// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Query and attune sandbox helper API (port of packages/core/src/query.ts).
//!
//! Every helper takes serde_json arguments (the JS engine boundary serializes
//! to JSON strings; see sandbox.rs) and returns serde_json values with
//! object keys in the same insertion order the TS rows produce
//! (serde_json preserve_order).

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::provider_indexing::stored_session_cursor;
use crate::providers::types::ProviderRegistry;

const BASH_EXIT_PAT: &str = "Exit code %";

// #107: the read-only contract follows the statement's actual database
// effects instead of scanning SQL text for mutation keywords. The lexical
// SELECT/WITH prefix check stays as the cheap entry contract; the read-only
// connection remains the final mutation boundary (rusqlite rejects
// multi-statement input at prepare, mapping TS assertSingleStatement).
pub const READ_ONLY_SQL_MESSAGE: &str = "sql() only supports read-only SELECT/WITH queries";
pub const MULTI_STATEMENT_SQL_MESSAGE: &str =
    "sql() accepts exactly one SQL statement per call; split multiple statements into separate sql() calls";

fn assert_read_only_sql_prefix(text: &str) -> Result<(), String> {
    let re = crate::parsing::readonly_prefix_re();
    if !re.is_match(text) {
        return Err(READ_ONLY_SQL_MESSAGE.to_string());
    }
    Ok(())
}

/// Byte index just past the first `;` that is outside string literals,
/// quoted identifiers, and comments — i.e. where the first statement ends.
fn first_statement_end(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let rest = &sql[i..];
        if rest.starts_with("--") {
            {
                let nl = rest.find('\n')?;
                i += nl + 1
            }
            continue;
        }
        if rest.starts_with("/*") {
            {
                let end = rest.find("*/")?;
                i += end + 2
            }
            continue;
        }
        let ch = bytes[i];
        if ch == b';' {
            return Some(i + 1);
        }
        if ch == b'\'' || ch == b'"' || ch == b'`' {
            let quote = ch as char;
            i += 1;
            while i < bytes.len() {
                if bytes[i] as char == quote {
                    if quote != b'\'' as char {
                        break;
                    }
                    // '' is an escaped quote inside a string literal.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
        }
        i += 1;
    }
    None
}

/// TS `isBlankOrCommentTail`: whitespace and complete comments only — an
/// unterminated `/*` tail is NOT inert (it would be swallowed as
/// comment-to-EOF by SQLite's prepare, hiding a truncated second statement).
fn is_blank_or_comment_tail(text: &str) -> bool {
    let mut i = 0usize;
    while i < text.len() {
        let rest = &text[i..];
        let ch = rest.chars().next().expect("non-empty");
        if matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{c}' | '\u{b}') {
            i += ch.len_utf8();
            continue;
        }
        if rest.starts_with("--") {
            match rest.find('\n') {
                Some(nl) => i += nl + 1,
                None => return true,
            }
            continue;
        }
        if rest.starts_with("/*") {
            match rest.find("*/") {
                Some(end) => i += end + 2,
                None => return false,
            }
            continue;
        }
        return false;
    }
    true
}

/// TS `assertSingleStatement`: exactly one statement per sql() call.
fn assert_single_statement(sql_text: &str) -> Result<(), String> {
    if let Some(end) = first_statement_end(sql_text) {
        if !is_blank_or_comment_tail(&sql_text[end..]) {
            return Err(MULTI_STATEMENT_SQL_MESSAGE.to_string());
        }
    }
    Ok(())
}

fn normalized_visibility(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_str) {
        None | Some("visible") => "visible",
        Some("inactive") => "inactive",
        Some(_) => "hidden",
    }
}

fn with_visibility(row: Value) -> Value {
    if let Value::Object(map) = row {
        let mut out = Map::new();
        let visibility = map.get("visibility").cloned().unwrap_or(Value::Null);
        for (key, value) in map {
            out.insert(key, value);
        }
        out.insert(
            "visibility".to_string(),
            Value::String(normalized_visibility(Some(&visibility)).to_string()),
        );
        Value::Object(out)
    } else {
        row
    }
}

// Subagent total tokens (ADR-0010): a provider-stored value wins; when the
// provider did not store one, derive it from sidechain message usage.
fn derive_subagent_tokens(conn: &Connection, agent_id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT SUM(COALESCE(input_tokens,0)+COALESCE(output_tokens,0)) AS t, COUNT(*) AS n
         FROM messages WHERE agent_id=?1 AND (input_tokens IS NOT NULL OR output_tokens IS NOT NULL)",
        [agent_id],
        |row| {
            let n: i64 = row.get(1)?;
            let t: Option<f64> = row.get(0)?;
            Ok((n, t))
        },
    )
    .ok()
    .and_then(|(n, t)| if n > 0 { t.map(|t| t as i64) } else { None })
}

fn is_queryable_message(row: Option<&Value>, include_inactive: bool) -> bool {
    let Some(row) = row else { return false };
    match normalized_visibility(row.get("visibility")) {
        "visible" => true,
        "inactive" => include_inactive,
        _ => false,
    }
}

fn visibility_sql(alias: &str, include_inactive: bool) -> String {
    let column = format!("{alias}.visibility");
    if include_inactive {
        format!("COALESCE({column},'visible') IN ('visible','inactive')")
    } else {
        format!("COALESCE({column},'visible')='visible'")
    }
}

fn cjk_text_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        // \p{Script=Han} etc. — the regex crate supports script classes.
        regex::Regex::new(
            r"[\p{script=Han}\p{script=Hiragana}\p{script=Katakana}\p{script=Hangul}]",
        )
        .expect("CJK script regex compiles")
    })
}

fn assert_english_memory_text(value: &Value, label: &str) -> Result<(), String> {
    let text = value.as_str().unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(());
    }
    if cjk_text_re().is_match(text) {
        let requirement = if label.contains("query") {
            "must use English terms"
        } else {
            "must be written in English"
        };
        return Err(format!(
            "{label} {requirement}; translate user-language terms before using the memory layer"
        ));
    }
    Ok(())
}

pub fn build_safe_fts_query(text: &Value) -> String {
    let text = text.as_str().unwrap_or_default();
    let re = crate::parsing::token_re();
    let mut tokens: Vec<&str> = re.find_iter(text).map(|m| m.as_str()).collect();
    tokens.truncate(12);
    tokens
        .iter()
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize helper opts: null/undefined → {}, string → {scalarKey}, number
/// → {limit} (TS normalizeOpts).
fn normalize_opts(opts: &Value, scalar_key: &str) -> Map<String, Value> {
    match opts {
        Value::Null => Map::new(),
        Value::String(s) => {
            let mut map = Map::new();
            map.insert(scalar_key.to_string(), Value::String(s.clone()));
            map
        }
        Value::Number(n) => {
            let mut map = Map::new();
            map.insert("limit".to_string(), Value::Number(n.clone()));
            map
        }
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    }
}

fn opt_str<'a>(opts: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    opts.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn opt_bool(opts: &Map<String, Value>, key: &str) -> bool {
    opts.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn opt_limit(opts: &Map<String, Value>, default: i64) -> i64 {
    opts.get("limit").and_then(Value::as_i64).unwrap_or(default)
}

fn json_to_sql(value: &Value) -> rusqlite::Result<rusqlite::types::Value> {
    use rusqlite::types::Value as SqlValue;
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::Real(f)
            } else {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "unsupported number".into(),
                ));
            }
        }
        Value::String(s) => SqlValue::Text(s.clone()),
        // The legacy CLI contract rejects structured params; mirror
        // that honestly (TS parity).
        Value::Array(_) | Value::Object(_) => {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "Invalid argument type".into(),
            ))
        }
    })
}

fn run_query(conn: &Connection, sql: &str, params: &[Value]) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let sql_params: Vec<rusqlite::types::Value> = params
        .iter()
        .map(json_to_sql)
        .collect::<rusqlite::Result<_>>()?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = sql_params
        .iter()
        .map(|p| p as &dyn rusqlite::ToSql)
        .collect();
    let mut rows = stmt.query(param_refs.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut map = Map::new();
        for (index, name) in column_names.iter().enumerate() {
            let value = match row.get_ref(index)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(i) => Value::Number(i.into()),
                rusqlite::types::ValueRef::Real(f) => number::from_f64_lossy(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                rusqlite::types::ValueRef::Text(t) => {
                    Value::String(String::from_utf8_lossy(t).into_owned())
                }
                rusqlite::types::ValueRef::Blob(b) => {
                    // No BLOB columns exist in the Obelisk schema; map to a
                    // lossy string rather than crash.
                    Value::String(String::from_utf8_lossy(b).into_owned())
                }
            };
            map.insert(name.clone(), value);
        }
        out.push(Value::Object(map));
    }
    Ok(out)
}

/// JS prints integral floats without a fraction (`2.0` → `2`); mirror that
/// for SQLite REAL columns so CLI output shapes match the TS build.
mod number {
    use serde_json::Number;

    pub fn from_f64_lossy(f: f64) -> Option<Number> {
        if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
            Number::from_i128(f as i128)
        } else {
            Number::from_f64(f)
        }
    }
}

fn one(conn: &Connection, sql: &str, params: &[Value]) -> Option<Value> {
    run_query(conn, sql, params).ok().and_then(|mut rows| {
        if rows.is_empty() {
            None
        } else {
            Some(rows.remove(0))
        }
    })
}

/// Public row reader for internal callers (invocation resolution).
pub fn run_rows(conn: &Connection, sql: &str, params: &[Value]) -> rusqlite::Result<Vec<Value>> {
    run_query(conn, sql, params)
}

/// (where clause, params) builder (TS buildWhere).
#[allow(clippy::too_many_arguments)]
fn build_where(
    opts: &Map<String, Value>,
    session_id_col: &str,
    project_col: &str,
    timestamp_col: &str,
    timestamp_after_col: Option<&str>,
    timestamp_before_col: Option<&str>,
    branch_col: &str,
    source_col: Option<&str>,
) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    if let Some(session_id) = opt_str(opts, "sessionId") {
        clauses.push(format!("{session_id_col} = ?"));
        params.push(json!(session_id));
    }
    if let Some(sessions) = opts.get("sessions").and_then(Value::as_array) {
        if !sessions.is_empty() {
            let placeholders = sessions.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            clauses.push(format!("{session_id_col} IN ({placeholders})"));
            params.extend(sessions.iter().cloned());
        }
    }
    if let Some(project) = opt_str(opts, "project") {
        clauses.push(format!("{project_col} LIKE ?"));
        params.push(json!(project));
    }
    if let Some(after) = opt_str(opts, "after") {
        clauses.push(format!(
            "{} > ?",
            timestamp_after_col.unwrap_or(timestamp_col)
        ));
        params.push(json!(after));
    }
    if let Some(before) = opt_str(opts, "before") {
        clauses.push(format!(
            "{} < ?",
            timestamp_before_col.unwrap_or(timestamp_col)
        ));
        params.push(json!(before));
    }
    if let Some(branch) = opt_str(opts, "branch") {
        clauses.push(format!("{branch_col} = ?"));
        params.push(json!(branch));
    }
    let source = opt_str(opts, "source");
    if let (Some(source), Some(source_col)) = (source, source_col) {
        if source != "all" {
            clauses.push(format!("COALESCE({source_col}, 'claude') = ?"));
            params.push(json!(source));
        }
    }
    (
        if clauses.is_empty() {
            "1=1".to_string()
        } else {
            clauses.join(" AND ")
        },
        params,
    )
}

pub struct QueryApi {
    /// Owned connection: the sandbox host functions require 'static
    /// dispatchers, so the API owns the read-only handle it queries with.
    pub conn: Connection,
    pub registry: std::sync::Arc<ProviderRegistry>,
    pub invoking_session_id: Option<String>,
    pub cwd: PathBuf,
}

impl QueryApi {
    /// The raw sql() helper: read-only, single-statement SELECT/WITH.
    pub fn sql(&self, text: &Value, params: &[Value]) -> Result<Vec<Value>, String> {
        let text = text.as_str().unwrap_or_default();
        assert_read_only_sql_prefix(text)?;
        assert_single_statement(text)?;
        match self.conn.prepare(text) {
            Ok(_) => {}
            Err(rusqlite::Error::MultipleStatement) => {
                return Err(MULTI_STATEMENT_SQL_MESSAGE.to_string())
            }
            Err(error) => {
                let message = error.to_string();
                if message.contains("not authorized") {
                    return Err(READ_ONLY_SQL_MESSAGE.to_string());
                }
                return Err(message);
            }
        }
        run_query(&self.conn, text, params).map_err(|error| {
            let message = error.to_string();
            if message.contains("attempt to write") {
                READ_ONLY_SQL_MESSAGE.to_string()
            } else {
                message
            }
        })
    }

    pub fn search(&self, text: &Value, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 20);
        let include_meta = opt_bool(&opts, "includeMeta");
        let include_inactive = opt_bool(&opts, "includeInactive");
        let text_str = text.as_str().unwrap_or_default();

        let mut where_clause = "WHERE mf.text MATCH ?".to_string();
        let mut filter_params: Vec<Value> = Vec::new();
        if let Some(session_id) = opt_str(&opts, "sessionId") {
            where_clause += " AND mf.session_id=?";
            filter_params.push(json!(session_id));
        }
        if let Some(project) = opt_str(&opts, "project") {
            where_clause += " AND s.project LIKE ?";
            filter_params.push(json!(project));
        }
        if let Some(after) = opt_str(&opts, "after") {
            where_clause += " AND m.timestamp>?";
            filter_params.push(json!(after));
        }
        if let Some(before) = opt_str(&opts, "before") {
            where_clause += " AND m.timestamp<?";
            filter_params.push(json!(before));
        }
        if let Some(cwd) = opt_str(&opts, "cwd") {
            where_clause += " AND m.cwd LIKE ?";
            filter_params.push(json!(cwd));
        }
        let source = opt_str(&opts, "source");
        if let Some(source) = source.filter(|s| *s != "all") {
            where_clause += " AND COALESCE(m.source, s.source, 'claude')=?";
            filter_params.push(json!(source));
        }
        if !include_meta {
            where_clause += " AND COALESCE(m.is_meta,0)=0";
        }
        where_clause += &format!(" AND {}", visibility_sql("m", include_inactive));
        let sql = format!(
            "SELECT m.uuid,m.session_id,m.text,m.content_type,m.is_meta,m.role,m.timestamp,m.model,m.cwd,
                    COALESCE(m.visibility,'visible') AS visibility,m.source as m_source,
                    s.id as s_id,s.title as s_title,s.project as s_project,s.started_at as s_started,
                    s.source as s_source,
                    rank
             FROM messages_fts mf JOIN messages m ON m.uuid=mf.uuid LEFT JOIN sessions s ON s.id=m.session_id
             {where_clause} ORDER BY rank LIMIT ?"
        );
        let run_match = |match_text: &str| -> rusqlite::Result<Vec<Value>> {
            let mut all_params = vec![json!(match_text)];
            all_params.extend(filter_params.iter().cloned());
            all_params.push(json!(limit));
            run_query(&self.conn, &sql, &all_params)
        };
        // Honor raw FTS5 syntax when the query is valid, but never crash on
        // ordinary input (hyphens, punctuation) that FTS5 would parse as
        // operators: fall back to safe per-token quoting.
        let rows = match run_match(text_str) {
            Ok(rows) => rows,
            Err(_) => {
                let safe = build_safe_fts_query(text);
                if safe.is_empty() {
                    Vec::new()
                } else {
                    run_match(&safe).unwrap_or_default()
                }
            }
        };
        rows.into_iter()
            .map(|r| {
                let meta_clause = if include_meta {
                    ""
                } else {
                    "AND COALESCE(is_meta,0)=0"
                };
                let ctx_sql = format!(
                    "SELECT uuid,text,content_type,is_meta,role,timestamp,model,
                            COALESCE(visibility,'visible') AS visibility,
                            COALESCE(source, 'claude') as source
                     FROM messages
                     WHERE session_id=? AND uuid!=? {meta_clause}
                       AND {}
                     ORDER BY ABS(JULIANDAY(timestamp)-JULIANDAY(?))
                     LIMIT 6",
                    visibility_sql("messages", include_inactive)
                );
                let session_id = r.get("session_id").cloned().unwrap_or(Value::Null);
                let uuid = r.get("uuid").cloned().unwrap_or(Value::Null);
                let timestamp = r.get("timestamp").cloned().unwrap_or(Value::Null);
                let mut ctx = run_query(
                    &self.conn,
                    &ctx_sql,
                    &[session_id.clone(), uuid.clone(), timestamp.clone()],
                )
                .unwrap_or_default();
                ctx = ctx.into_iter().map(with_visibility).collect();
                ctx.sort_by(|a, b| {
                    let a = a.get("timestamp").and_then(Value::as_str).unwrap_or("");
                    let b = b.get("timestamp").and_then(Value::as_str).unwrap_or("");
                    if a < b {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                });
                let m_source = r.get("m_source").cloned().unwrap_or(Value::Null);
                let s_source = r.get("s_source").cloned().unwrap_or(Value::Null);
                let source_value = match (&m_source, &s_source) {
                    (Value::String(s), _) => s.clone(),
                    (_, Value::String(s)) => s.clone(),
                    _ => "claude".to_string(),
                };
                let s_id = r.get("s_id").cloned().unwrap_or(Value::Null);
                let mut session = json!({
                    "id": s_id.clone(),
                    "title": r.get("s_title").cloned().unwrap_or(Value::Null),
                    "project": r.get("s_project").cloned().unwrap_or(Value::Null),
                    "started_at": r.get("s_started").cloned().unwrap_or(Value::Null),
                    "source": if s_source.is_string() { s_source.clone() } else { Value::String(source_value.clone()) },
                });
                if let (Some(invoking), Value::String(s_id_str)) =
                    (&self.invoking_session_id, &s_id)
                {
                    if invoking == s_id_str {
                        if let Value::Object(map) = &mut session {
                            map.insert("is_invoking".to_string(), Value::Bool(true));
                        }
                    }
                }
                json!({
                    "message": {
                        "uuid": r.get("uuid").cloned().unwrap_or(Value::Null),
                        "text": r.get("text").cloned().unwrap_or(Value::Null),
                        "content_type": r.get("content_type").cloned().unwrap_or(Value::Null),
                        "is_meta": r.get("is_meta").cloned().unwrap_or(json!(0)),
                        "role": r.get("role").cloned().unwrap_or(Value::Null),
                        "timestamp": r.get("timestamp").cloned().unwrap_or(Value::Null),
                        "model": r.get("model").cloned().unwrap_or(Value::Null),
                        "cwd": r.get("cwd").cloned().unwrap_or(Value::Null),
                        "visibility": Value::String(normalized_visibility(r.get("visibility")).to_string()),
                        "source": Value::String(source_value),
                    },
                    "session": session,
                    "rank": r.get("rank").cloned().unwrap_or(Value::Null),
                    "context": ctx,
                })
            })
            .collect()
    }

    pub fn context(&self, uuid: &Value, opts: &Value) -> Option<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let include_inactive = opt_bool(&opts, "includeInactive");
        let uuid = uuid.as_str()?;
        let msg = one(
            &self.conn,
            "SELECT * FROM messages WHERE uuid=?",
            &[json!(uuid)],
        )?;
        if !is_queryable_message(Some(&msg), include_inactive) {
            return None;
        }
        let message = with_visibility(msg.clone());
        let session_id = msg.get("session_id").cloned().unwrap_or(Value::Null);
        let session = one(
            &self.conn,
            "SELECT * FROM sessions WHERE id=?",
            std::slice::from_ref(&session_id),
        );
        let mut chain: Vec<Value> = Vec::new();
        let mut cur = msg.clone();
        while let Some(parent_uuid) = cur.get("parent_uuid").and_then(Value::as_str) {
            let parent = one(
                &self.conn,
                "SELECT * FROM messages WHERE uuid=?",
                &[json!(parent_uuid)],
            );
            match parent {
                Some(parent) => {
                    let queryable = is_queryable_message(Some(&parent), include_inactive);
                    if queryable {
                        chain.insert(0, with_visibility(parent.clone()));
                    }
                    cur = parent;
                }
                None => break,
            }
        }
        let agent_id = msg.get("agent_id").and_then(Value::as_str);
        let mut subagent = agent_id.and_then(|agent_id| {
            one(
                &self.conn,
                "SELECT * FROM subagents WHERE agent_id=?",
                &[json!(agent_id)],
            )
        });
        if let (Some(subagent), Some(agent_id)) = (&mut subagent, agent_id) {
            if subagent
                .get("total_tokens")
                .map(Value::is_null)
                .unwrap_or(true)
            {
                if let Some(tokens) = derive_subagent_tokens(&self.conn, agent_id) {
                    if let Value::Object(map) = subagent {
                        map.insert("total_tokens".to_string(), json!(tokens));
                    }
                }
            }
        }
        let mut workflow = Value::Null;
        if let Some(agent_id) = agent_id {
            if let Some(wa) = one(
                &self.conn,
                "SELECT * FROM workflow_agents WHERE agent_id=?",
                &[json!(agent_id)],
            ) {
                let run_id = wa.get("run_id").cloned().unwrap_or(Value::Null);
                workflow = one(
                    &self.conn,
                    "SELECT * FROM workflows WHERE run_id=?",
                    &[run_id],
                )
                .unwrap_or(Value::Null);
            }
        }
        Some(json!({
            "message": message,
            "parentChain": chain,
            "session": session,
            "subagent": subagent,
            "workflow": workflow,
        }))
    }

    pub fn trace(&self, uuid: &Value, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let include_inactive = opt_bool(&opts, "includeInactive");
        let Some(uuid) = uuid.as_str() else {
            return Vec::new();
        };
        let mut chain: Vec<Value> = Vec::new();
        let mut cur = one(
            &self.conn,
            "SELECT * FROM messages WHERE uuid=?",
            &[json!(uuid)],
        );
        if !cur
            .as_ref()
            .map(|c| is_queryable_message(Some(c), include_inactive))
            .unwrap_or(false)
        {
            return chain;
        }
        while let Some(row) = cur {
            let parent_uuid = row
                .get("parent_uuid")
                .and_then(Value::as_str)
                .map(str::to_string);
            if is_queryable_message(Some(&row), include_inactive) {
                chain.insert(0, with_visibility(row.clone()));
            }
            cur = parent_uuid.and_then(|parent| {
                one(
                    &self.conn,
                    "SELECT * FROM messages WHERE uuid=?",
                    &[json!(parent)],
                )
            });
        }
        chain
    }

    pub fn thread(&self, sid: &Value, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let include_meta = opt_bool(&opts, "includeMeta");
        let include_inactive = opt_bool(&opts, "includeInactive");
        let Some(sid) = sid.as_str() else {
            return Vec::new();
        };
        let meta_clause = if include_meta {
            ""
        } else {
            "AND COALESCE(is_meta,0)=0"
        };
        let sql = format!(
            "SELECT * FROM messages
             WHERE session_id=? {meta_clause}
               AND {}
             ORDER BY timestamp",
            visibility_sql("messages", include_inactive)
        );
        run_query(&self.conn, &sql, &[json!(sid)])
            .unwrap_or_default()
            .into_iter()
            .map(with_visibility)
            .collect()
    }

    pub fn subagents(&self, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 100);
        let needs_join = opts.contains_key("project")
            || opts.contains_key("branch")
            || opts.contains_key("source");
        let first_message_at =
            "(SELECT MIN(m.timestamp) FROM messages m WHERE m.agent_id = sa.agent_id)";
        let last_message_at =
            "(SELECT MAX(m.timestamp) FROM messages m WHERE m.agent_id = sa.agent_id)";
        let (where_clause, mut params) = build_where(
            &opts,
            "sa.session_id",
            "s.project",
            first_message_at,
            Some(last_message_at),
            Some(first_message_at),
            "s.git_branch",
            Some("s.source"),
        );
        params.push(json!(limit));
        let join = if needs_join {
            "LEFT JOIN sessions s ON s.id=sa.session_id"
        } else {
            ""
        };
        let rows = run_query(
            &self.conn,
            &format!("SELECT sa.* FROM subagents sa {join} WHERE {where_clause} LIMIT ?"),
            &params,
        )
        .unwrap_or_default();
        rows.into_iter()
            .map(|r| {
                let agent_id = r.get("agent_id").cloned().unwrap_or(Value::Null);
                let c = one(
                    &self.conn,
                    "SELECT COUNT(*) as c FROM messages WHERE agent_id=?",
                    std::slice::from_ref(&agent_id),
                );
                let message_count = c
                    .as_ref()
                    .and_then(|c| c.get("c"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let mut out = r;
                if let Value::Object(map) = &mut out {
                    map.insert("messageCount".to_string(), json!(message_count));
                    if map.get("total_tokens").map(Value::is_null).unwrap_or(true) {
                        if let Some(agent_id) = agent_id.as_str() {
                            if let Some(tokens) = derive_subagent_tokens(&self.conn, agent_id) {
                                map.insert("total_tokens".to_string(), json!(tokens));
                            }
                        }
                    }
                }
                out
            })
            .collect()
    }

    pub fn workflows(&self, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 100);
        let needs_join = opts.contains_key("project")
            || opts.contains_key("branch")
            || opts.contains_key("source");
        let (where_clause, mut params) = build_where(
            &opts,
            "w.session_id",
            "s.project",
            "w.timestamp",
            None,
            None,
            "s.git_branch",
            Some("s.source"),
        );
        params.push(json!(limit));
        let join = if needs_join {
            "LEFT JOIN sessions s ON s.id=w.session_id"
        } else {
            ""
        };
        run_query(
            &self.conn,
            &format!(
                "SELECT w.* FROM workflows w {join} WHERE {where_clause} ORDER BY w.timestamp DESC LIMIT ?"
            ),
            &params,
        )
        .unwrap_or_default()
    }

    pub fn workflow_tree(&self, run_id: &Value) -> Option<Value> {
        let run_id = run_id.as_str()?;
        let wf = one(
            &self.conn,
            "SELECT * FROM workflows WHERE run_id=?",
            &[json!(run_id)],
        )?;
        let result_json = wf.get("result_json").and_then(Value::as_str);
        let result = result_json
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .unwrap_or(Value::Null);
        let agents = run_query(
            &self.conn,
            "SELECT * FROM workflow_agents WHERE run_id=?",
            &[json!(run_id)],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            let agent_id = a.get("agent_id").cloned().unwrap_or(Value::Null);
            let mc = one(
                &self.conn,
                "SELECT COUNT(*) as c FROM messages WHERE agent_id=?",
                &[agent_id],
            );
            let message_count = mc
                .as_ref()
                .and_then(|c| c.get("c"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let mut out = a;
            if let Value::Object(map) = &mut out {
                map.insert("messageCount".to_string(), json!(message_count));
            }
            out
        })
        .collect::<Vec<_>>();
        let mut out = wf;
        if let Value::Object(map) = &mut out {
            map.insert("result".to_string(), result);
            map.insert("agents".to_string(), Value::Array(agents));
        }
        Some(out)
    }

    pub fn file_history(&self, fp: &Value, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 200);
        let include_inactive = opt_bool(&opts, "includeInactive");
        let Some(fp) = fp.as_str() else {
            return Vec::new();
        };
        let mut where_clause = format!(
            "tc.file_path=? AND {}",
            visibility_sql("m", include_inactive)
        );
        let mut params = vec![json!(fp)];
        if let Some(after) = opt_str(&opts, "after") {
            where_clause += " AND m.timestamp > ?";
            params.push(json!(after));
        }
        if let Some(before) = opt_str(&opts, "before") {
            where_clause += " AND m.timestamp < ?";
            params.push(json!(before));
        }
        let source = opt_str(&opts, "source");
        if let Some(source) = source.filter(|s| *s != "all") {
            where_clause += " AND COALESCE(s.source, 'claude') = ?";
            params.push(json!(source));
        }
        params.push(json!(limit));
        let rows = run_query(
            &self.conn,
            &format!(
                "SELECT tc.*,s.title as s_title,s.project as s_project,m.timestamp as ts,
                        COALESCE(m.visibility,'visible') AS visibility
                 FROM tool_calls tc
                 LEFT JOIN sessions s ON s.id=tc.session_id
                 LEFT JOIN messages m ON m.uuid=tc.message_uuid
                 WHERE {where_clause}
                 ORDER BY m.timestamp
                 LIMIT ?"
            ),
            &params,
        )
        .unwrap_or_default();
        rows.into_iter()
            .map(|r| {
                json!({
                    "toolCall": {
                        "id": r.get("id").cloned().unwrap_or(Value::Null),
                        "message_uuid": r.get("message_uuid").cloned().unwrap_or(Value::Null),
                        "name": r.get("name").cloned().unwrap_or(Value::Null),
                        "input_json": r.get("input_json").cloned().unwrap_or(Value::Null),
                    },
                    "session": {
                        "id": r.get("session_id").cloned().unwrap_or(Value::Null),
                        "title": r.get("s_title").cloned().unwrap_or(Value::Null),
                        "project": r.get("s_project").cloned().unwrap_or(Value::Null),
                    },
                    "timestamp": r.get("ts").cloned().unwrap_or(Value::Null),
                    "visibility": Value::String(normalized_visibility(r.get("visibility")).to_string()),
                })
            })
            .collect()
    }

    pub fn failures(&self, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 50);
        let include_inactive = opt_bool(&opts, "includeInactive");
        let needs_join = opts.contains_key("project")
            || opts.contains_key("branch")
            || opts.contains_key("source");
        let (where_clause, filter_params) = build_where(
            &opts,
            "tr.session_id",
            "s.project",
            "rm.timestamp",
            None,
            None,
            "s.git_branch",
            Some("s.source"),
        );
        let join = if needs_join {
            "LEFT JOIN sessions s ON s.id=tr.session_id"
        } else {
            ""
        };
        let error_cond = format!(
            "(tr.is_error = 1 OR tr.content LIKE '{BASH_EXIT_PAT}') AND {} AND {}",
            visibility_sql("rm", include_inactive),
            visibility_sql("cm", include_inactive)
        );
        let mut all_params = filter_params;
        all_params.push(json!(limit));
        let rows = run_query(
            &self.conn,
            &format!(
                "SELECT tr.*,
                   CASE
                     WHEN COALESCE(rm.visibility,'visible') = 'inactive'
                       OR COALESCE(cm.visibility,'visible') = 'inactive'
                     THEN 'inactive'
                     ELSE 'visible'
                   END AS visibility
                 FROM tool_results tr
                 LEFT JOIN messages rm ON rm.uuid=tr.message_uuid
                 LEFT JOIN tool_calls tc ON tc.id=tr.tool_use_id
                 LEFT JOIN messages cm ON cm.uuid=tc.message_uuid
                 {join}
                 WHERE {error_cond} AND {where_clause}
                 ORDER BY rm.timestamp DESC
                 LIMIT ?"
            ),
            &all_params,
        )
        .unwrap_or_default();
        rows.into_iter()
            .map(|r| {
                let tool_use_id = r.get("tool_use_id").cloned().unwrap_or(Value::Null);
                let session_id = r.get("session_id").cloned().unwrap_or(Value::Null);
                let message_uuid = r.get("message_uuid").cloned().unwrap_or(Value::Null);
                let visibility =
                    Value::String(normalized_visibility(r.get("visibility")).to_string());
                let result = with_visibility(r);
                let tc = one(
                    &self.conn,
                    "SELECT * FROM tool_calls WHERE id=?",
                    &[tool_use_id],
                );
                let session = one(
                    &self.conn,
                    "SELECT * FROM sessions WHERE id=?",
                    std::slice::from_ref(&session_id),
                );
                let rm_row = one(
                    &self.conn,
                    "SELECT * FROM messages WHERE uuid=?",
                    &[message_uuid],
                );
                let rm = rm_row.map(with_visibility);
                let next: Vec<Value> = rm
                    .as_ref()
                    .and_then(|rm| rm.get("timestamp").and_then(Value::as_str))
                    .map(|timestamp| {
                        run_query(
                            &self.conn,
                            &format!(
                                "SELECT * FROM messages
                                 WHERE session_id=? AND timestamp>?
                                   AND {}
                                 ORDER BY timestamp
                                 LIMIT 3",
                                visibility_sql("messages", include_inactive)
                            ),
                            &[session_id, json!(timestamp)],
                        )
                        .unwrap_or_default()
                        .into_iter()
                        .map(with_visibility)
                        .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "toolCall": tc,
                    "result": result,
                    "session": session,
                    "nextMessages": next,
                    "visibility": visibility,
                })
            })
            .collect()
    }

    pub fn sessions(&self, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 50);
        let (where_clause, mut params) = build_where(
            &opts,
            "s.id",
            "s.project",
            "s.started_at",
            None,
            None,
            "s.git_branch",
            Some("s.source"),
        );
        params.push(json!(limit));
        run_query(
            &self.conn,
            &format!(
                "SELECT * FROM sessions s WHERE {where_clause} ORDER BY ended_at DESC LIMIT ?"
            ),
            &params,
        )
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            if let (Some(invoking), Value::String(id)) = (
                &self.invoking_session_id,
                row.get("id").unwrap_or(&Value::Null),
            ) {
                if invoking == id {
                    let mut out = row;
                    if let Value::Object(map) = &mut out {
                        map.insert("is_invoking".to_string(), Value::Bool(true));
                    }
                    return out;
                }
            }
            row
        })
        .collect()
    }

    pub fn recent(&self, n: &Value) -> Vec<Value> {
        let mut opts = Map::new();
        opts.insert(
            "limit".to_string(),
            n.as_i64().map(|v| json!(v)).unwrap_or(json!(10)),
        );
        self.sessions(&Value::Object(opts))
    }

    pub fn summaries(&self, opts: &Value) -> Vec<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 100);
        let include_inactive = opt_bool(&opts, "includeInactive");
        let (where_clause, mut params) = build_where(
            &opts,
            "su.session_id",
            "s.project",
            "su.timestamp",
            None,
            None,
            "s.git_branch",
            Some("s.source"),
        );
        params.push(json!(limit));
        run_query(
            &self.conn,
            &format!(
                "SELECT su.*, s.title as session_title, s.project
                 FROM summaries su
                 LEFT JOIN sessions s ON s.id=su.session_id
                 WHERE {where_clause} AND {}
                 ORDER BY su.timestamp DESC
                 LIMIT ?",
                visibility_sql("su", include_inactive)
            ),
            &params,
        )
        .unwrap_or_default()
        .into_iter()
        .map(with_visibility)
        .collect()
    }

    pub fn overview(&self, opts: &Value) -> Value {
        let mut normalized = Map::new();
        match opts {
            Value::String(project) => {
                normalized.insert("project".to_string(), json!(project));
            }
            Value::Number(n) => {
                normalized.insert("limit".to_string(), Value::Number(n.clone()));
            }
            Value::Object(map) => normalized = map.clone(),
            _ => {}
        }
        let cwd = self.cwd.to_string_lossy().into_owned();
        let session_limit = normalized.get("limit").and_then(Value::as_i64).unwrap_or(8);
        let project_limit = normalized
            .get("projectLimit")
            .and_then(Value::as_i64)
            .unwrap_or(20);
        let memory_limit = normalized
            .get("memoryLimit")
            .and_then(Value::as_i64)
            .unwrap_or(100);

        let latest_project_by_pattern = |pattern: &str| -> Option<Value> {
            one(
                &self.conn,
                "SELECT project, project_path
                 FROM sessions
                 WHERE project LIKE ?
                 ORDER BY COALESCE(ended_at, started_at) DESC
                 LIMIT 1",
                &[json!(pattern)],
            )
            .or_else(|| {
                one(
                    &self.conn,
                    "SELECT project, NULL AS project_path
                     FROM memories
                     WHERE project LIKE ?
                     ORDER BY created_at DESC
                     LIMIT 1",
                    &[json!(pattern)],
                )
            })
        };

        let project_descriptor = |row: &Value, source: &str, confidence: &str| -> Value {
            json!({
                "project": row.get("project").cloned().unwrap_or(Value::Null),
                "project_path": row.get("project_path").cloned().unwrap_or(Value::Null),
                "source": source,
                "confidence": confidence,
            })
        };

        let resolve_current_project = || -> Option<Value> {
            if let Some(project) = opt_str(&normalized, "project") {
                let row = latest_project_by_pattern(project);
                let has_wildcard = project.contains('%') || project.contains('_');
                let confidence = match &row {
                    Some(_) => {
                        if has_wildcard {
                            "inferred"
                        } else {
                            "exact"
                        }
                    }
                    None => "unknown",
                };
                return Some(project_descriptor(
                    &row.clone()
                        .unwrap_or(json!({"project": project, "project_path": Value::Null})),
                    "opts",
                    confidence,
                ));
            }
            let paths = run_query(
                &self.conn,
                "SELECT project, project_path, MAX(COALESCE(ended_at, started_at)) AS last_seen
                 FROM sessions
                 WHERE project IS NOT NULL AND project_path IS NOT NULL AND project_path != ''
                 GROUP BY project, project_path",
                &[],
            )
            .unwrap_or_default();
            let cwd_path = Path::new(&cwd);
            let mut by_project_path: Vec<&Value> = paths
                .iter()
                .filter(|r| {
                    let project_path = r.get("project_path").and_then(Value::as_str);
                    match project_path {
                        // TS: cwd === pp || cwd.startsWith(pp + sep) — Path's
                        // component-wise starts_with is the same predicate
                        // (a sibling like /x/synth-2 never matches /x/synth).
                        Some(pp) => cwd == pp || cwd_path.starts_with(Path::new(pp)),
                        None => false,
                    }
                })
                .collect();
            by_project_path.sort_by(|a, b| {
                let a_len = a
                    .get("project_path")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
                let b_len = b
                    .get("project_path")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
                b_len.cmp(&a_len)
            });
            if let Some(row) = by_project_path.first() {
                return Some(project_descriptor(row, "cwd_project_path", "exact"));
            }
            let by_message_cwd = one(
                &self.conn,
                "SELECT s.project, s.project_path, MAX(m.timestamp) AS last_seen
                 FROM messages m
                 LEFT JOIN sessions s ON s.id=m.session_id
                 WHERE m.cwd = ? AND s.project IS NOT NULL
                 GROUP BY s.project, s.project_path
                 ORDER BY last_seen DESC
                 LIMIT 1",
                &[json!(cwd)],
            );
            if let Some(row) = by_message_cwd {
                return Some(project_descriptor(&row, "cwd_messages", "inferred"));
            }
            None
        };

        let projects = run_query(
            &self.conn,
            "WITH names AS (
               SELECT project FROM sessions WHERE project IS NOT NULL GROUP BY project
               UNION
               SELECT project FROM memories WHERE project IS NOT NULL AND deleted_at IS NULL GROUP BY project
             ),
             session_stats AS (
               SELECT project, COUNT(*) AS session_count, MAX(COALESCE(ended_at, started_at)) AS last_session_at
               FROM sessions
               WHERE project IS NOT NULL
               GROUP BY project
             ),
             memory_stats AS (
               SELECT project, COUNT(*) AS memory_count, MAX(created_at) AS last_memory_at
               FROM memories
               WHERE project IS NOT NULL AND deleted_at IS NULL
               GROUP BY project
             )
             SELECT
               n.project,
               (
                 SELECT s2.project_path
                 FROM sessions s2
                 WHERE s2.project = n.project AND s2.project_path IS NOT NULL
                 ORDER BY COALESCE(s2.ended_at, s2.started_at) DESC
                 LIMIT 1
               ) AS project_path,
               COALESCE(ss.session_count, 0) AS session_count,
               COALESCE(ms.memory_count, 0) AS memory_count,
               ss.last_session_at,
               ms.last_memory_at
             FROM names n
             LEFT JOIN session_stats ss ON ss.project = n.project
             LEFT JOIN memory_stats ms ON ms.project = n.project
             ORDER BY COALESCE(ss.last_session_at, ms.last_memory_at) DESC
             LIMIT ?",
            &[json!(project_limit)],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            let project = row.get("project").cloned().unwrap_or(Value::Null);
            let branches = run_query(
                &self.conn,
                "SELECT git_branch
                 FROM sessions
                 WHERE project = ? AND git_branch IS NOT NULL AND git_branch != ''
                 GROUP BY git_branch
                 ORDER BY MAX(COALESCE(ended_at, started_at)) DESC
                 LIMIT 5",
                &[project],
            )
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.get("git_branch").cloned().unwrap_or(Value::Null))
            .collect::<Vec<_>>();
            let mut out = row;
            if let Value::Object(map) = &mut out {
                map.insert("recent_branches".to_string(), Value::Array(branches));
            }
            out
        })
        .collect::<Vec<_>>();

        let current_project_descriptor = resolve_current_project();
        let mut current_project = Value::Null;
        if let Some(descriptor) = &current_project_descriptor {
            if let Some(project) = descriptor.get("project").and_then(Value::as_str) {
                let session_total: i64 = one(
                    &self.conn,
                    "SELECT COUNT(*) AS c FROM sessions WHERE project = ?",
                    &[json!(project)],
                )
                .and_then(|row| row.get("c").and_then(Value::as_i64))
                .unwrap_or(0);
                let sessions_for_project = run_query(
                    &self.conn,
                    "SELECT id, title, project, project_path, started_at, ended_at, git_branch, message_count, COALESCE(source, 'claude') AS source
                     FROM sessions
                     WHERE project = ?
                     ORDER BY COALESCE(ended_at, started_at) DESC
                     LIMIT ?",
                    &[json!(project), json!(session_limit)],
                )
                .unwrap_or_default();
                let memory_total: i64 = one(
                    &self.conn,
                    "SELECT COUNT(*) AS c FROM memories WHERE project = ? AND deleted_at IS NULL",
                    &[json!(project)],
                )
                .and_then(|row| row.get("c").and_then(Value::as_i64))
                .unwrap_or(0);
                let memories_for_project = run_query(
                    &self.conn,
                    "SELECT id, path, anchors, summary, session_id, project, created_at
                     FROM memories
                     WHERE project = ? AND deleted_at IS NULL
                     ORDER BY created_at DESC
                     LIMIT ?",
                    &[json!(project), json!(memory_limit)],
                )
                .unwrap_or_default();
                current_project = json!({
                    "project": descriptor.get("project").cloned().unwrap_or(Value::Null),
                    "project_path": descriptor.get("project_path").cloned().unwrap_or(Value::Null),
                    "session_total": session_total,
                    "sessions": sessions_for_project,
                    "memory_total": memory_total,
                    "memories": memories_for_project,
                });
            }
        }

        let total_projects: i64 = one(
            &self.conn,
            "SELECT COUNT(*) AS c
             FROM (
               SELECT project FROM sessions WHERE project IS NOT NULL GROUP BY project
               UNION
               SELECT project FROM memories WHERE project IS NOT NULL AND deleted_at IS NULL GROUP BY project
             )",
            &[],
        )
        .and_then(|row| row.get("c").and_then(Value::as_i64))
        .unwrap_or(0);
        let total_sessions: i64 = one(&self.conn, "SELECT COUNT(*) AS c FROM sessions", &[])
            .and_then(|row| row.get("c").and_then(Value::as_i64))
            .unwrap_or(0);
        let total_memories: i64 = one(
            &self.conn,
            "SELECT COUNT(*) AS c FROM memories WHERE deleted_at IS NULL",
            &[],
        )
        .and_then(|row| row.get("c").and_then(Value::as_i64))
        .unwrap_or(0);
        let sources = run_query(
            &self.conn,
            "SELECT COALESCE(source, 'claude') AS source,
                   COUNT(*) AS session_count,
                   MAX(COALESCE(ended_at, started_at)) AS last_session_at
            FROM sessions
            GROUP BY COALESCE(source, 'claude')
            ORDER BY last_session_at DESC",
            &[],
        )
        .unwrap_or_default();

        json!({
            "current": {
                "cwd": cwd,
                "project": current_project_descriptor,
                "session_id": self.invoking_session_id.clone().map(Value::String).unwrap_or(Value::Null),
            },
            "current_project": current_project,
            "projects": projects,
            "totals": {
                "projects": total_projects,
                "sessions": total_sessions,
                "memories": total_memories,
                "sources": sources,
            },
        })
    }

    pub fn raw(&self, message_uuid: &Value, opts: &Value) -> Option<Value> {
        let opts = normalize_opts(opts, "sessionId");
        let include_inactive = opt_bool(&opts, "includeInactive");
        let offset = opts
            .get("offset")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0) as usize;
        let limit = opts
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(10000)
            .max(0) as usize;
        let message_uuid = message_uuid.as_str()?;
        let message = one(
            &self.conn,
            "SELECT * FROM messages WHERE uuid=?",
            &[json!(message_uuid)],
        )?;
        if !is_queryable_message(Some(&message), include_inactive) {
            return None;
        }
        let session_id = message.get("session_id").cloned().unwrap_or(Value::Null);
        let session = one(
            &self.conn,
            "SELECT * FROM sessions WHERE id=?",
            std::slice::from_ref(&session_id),
        );
        let agent_id = message.get("agent_id").and_then(Value::as_str);
        let subagent = agent_id.and_then(|agent_id| {
            one(
                &self.conn,
                "SELECT * FROM subagents WHERE agent_id=?",
                &[json!(agent_id)],
            )
        });
        let workflow_agent = agent_id.and_then(|agent_id| {
            one(
                &self.conn,
                "SELECT * FROM workflow_agents WHERE agent_id=?",
                &[json!(agent_id)],
            )
        });
        let message_source = message.get("source").and_then(Value::as_str);
        let session_source = session
            .as_ref()
            .and_then(|s| s.get("source"))
            .and_then(Value::as_str);
        let source = message_source
            .or(session_source)
            .unwrap_or("claude")
            .to_string();
        let cursor = stored_session_cursor(&self.conn, &self.registry, session.as_ref());
        let lookup = crate::providers::types::RawLookup {
            source: &source,
            message_uuid,
            session: session.as_ref(),
            agent_id,
            cursor: cursor.as_deref(),
            subagent: subagent.as_ref(),
            workflow_agent: workflow_agent.as_ref(),
        };
        let record = self.registry.raw(&lookup)?;
        let text = record.text;
        let total_length = record
            .total_length
            .unwrap_or(text.chars().map(char::len_utf16).sum::<usize>());
        let slice: String = {
            // JS slice operates on UTF-16 code units.
            let start = offset.min(total_length);
            let end = (offset + limit).min(total_length);
            utf16_slice(&text, start, end)
        };
        Some(json!({
            "text": slice,
            "totalLength": total_length,
            "offset": offset,
            "limit": limit,
            "hasMore": offset + limit < total_length,
            "visibility": Value::String(normalized_visibility(message.get("visibility")).to_string()),
        }))
    }

    pub fn memories(&self, opts: &Value) -> Result<Vec<Value>, String> {
        let opts = normalize_opts(opts, "sessionId");
        let limit = opt_limit(&opts, 50);
        let query = opts.get("query").cloned().unwrap_or(Value::Null);
        assert_english_memory_text(&query, "memories() query")?;
        let needs_join = opts.contains_key("branch") || opts.contains_key("source");
        let (base_where, mut params) = build_where(
            &opts,
            "mem.session_id",
            "mem.project",
            "mem.created_at",
            None,
            None,
            "s.git_branch",
            Some("s.source"),
        );
        let where_clause = format!("{base_where} AND mem.deleted_at IS NULL");
        let join = if needs_join {
            "LEFT JOIN sessions s ON s.id=mem.session_id"
        } else {
            ""
        };
        let has_query = query
            .as_str()
            .map(|q| !q.trim().is_empty())
            .unwrap_or(false);
        let fts_query = build_safe_fts_query(&query);
        if !has_query {
            params.push(json!(limit));
            return run_query(
                &self.conn,
                &format!("SELECT mem.* FROM memories mem {join} WHERE {where_clause} ORDER BY mem.created_at DESC LIMIT ?"),
                &params,
            )
            .map_err(|error| error.to_string());
        }
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        params.insert(0, json!(fts_query));
        params.push(json!(limit));
        run_query(
            &self.conn,
            &format!(
                "SELECT mem.*, mf.rank AS rank
                 FROM memories_fts mf
                 JOIN memories mem ON mem.rowid = mf.rowid
                 {join}
                 WHERE memories_fts MATCH ? AND {where_clause}
                 ORDER BY mf.rank, mem.created_at DESC
                 LIMIT ?"
            ),
            &params,
        )
        .map_err(|error| error.to_string())
    }
}

fn utf16_slice(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let len = ch.len_utf16();
        if units >= end {
            break;
        }
        if units + len > start {
            out.push(ch);
        }
        units += len;
    }
    out
}

/// The remember/forget mutation surface (TS createAttuneApi).
pub struct AttuneApi {
    /// Owned connection: the sandbox host functions require 'static
    /// dispatchers.
    pub conn: Connection,
    pub cwd: PathBuf,
}

impl AttuneApi {
    fn resolve_memory_path(
        &self,
        memory_path: &str,
        session_id: Option<&str>,
    ) -> Result<String, String> {
        let mut base: Option<PathBuf> = None;
        if let Some(session_id) = session_id {
            base = one(
                &self.conn,
                "SELECT project_path FROM sessions WHERE id=?",
                &[json!(session_id)],
            )
            .and_then(|row| {
                row.get("project_path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty());
        }
        let path = Path::new(memory_path);
        let resolved = if path.is_absolute() {
            PathBuf::from(crate::parsing::normalize_path(memory_path))
        } else {
            base.unwrap_or_else(|| self.cwd.clone()).join(memory_path)
        };
        let metadata = std::fs::metadata(&resolved).map_err(|_| {
            format!(
                "remember() memory file does not exist: {}",
                resolved.display()
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "remember() memory path is not a file: {}",
                resolved.display()
            ));
        }
        Ok(resolved.to_string_lossy().into_owned())
    }

    fn normalize_anchors(&self, anchors: Option<&Value>) -> Result<Option<String>, String> {
        let Some(anchors) = anchors else {
            return Ok(None);
        };
        if anchors.is_null() {
            return Ok(None);
        }
        let parsed = match anchors {
            Value::String(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                serde_json::from_str::<Value>(trimmed)
                    .map_err(|_| "remember() anchors must be a JSON array".to_string())?
            }
            other => other.clone(),
        };
        let Some(array) = parsed.as_array() else {
            return Err("remember() anchors must be an array".to_string());
        };
        for anchor in array {
            if !anchor.is_object() {
                return Err("remember() anchors entries must be objects".to_string());
            }
        }
        if array.is_empty() {
            return Ok(None);
        }
        Ok(Some(parsed.to_string()))
    }

    pub fn remember(&self, input: &Value) -> Result<Value, String> {
        let memory_path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let summary = input
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if memory_path.is_empty() || summary.is_empty() {
            return Err("remember() requires path and summary".to_string());
        }
        assert_english_memory_text(&json!(summary), "remember() summary")?;
        let session_id = input.get("session_id").and_then(Value::as_str);
        let normalized_path = self.resolve_memory_path(memory_path, session_id)?;
        let normalized_anchors = self.normalize_anchors(input.get("anchors"))?;
        let project = input
            .get("project")
            .and_then(Value::as_str)
            .map(str::to_string);
        let proj = match project {
            Some(project) if !project.is_empty() => Some(project),
            _ => session_id
                .and_then(|sid| {
                    one(
                        &self.conn,
                        "SELECT project FROM sessions WHERE id=?",
                        &[json!(sid)],
                    )
                })
                .and_then(|row| {
                    row.get("project")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
        };
        let created_at = crate::indexer::now_ms();
        let created_at_iso = iso_ms_to_string(created_at);
        let message_start = input
            .get("message_start")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let message_end = input
            .get("message_end")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        // Plain INSERT, never OR REPLACE: a memory id must not silently
        // overwrite an existing memory. Collisions regenerate instead of
        // losing data.
        let mut id = format!("mem-{}", uuid::Uuid::new_v4());
        for attempt in 0..3 {
            match self.conn.execute(
                "INSERT INTO memories (id, session_id, project, message_start, message_end, path, anchors, summary, created_at) VALUES (?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    id,
                    session_id,
                    proj,
                    message_start,
                    message_end,
                    normalized_path,
                    normalized_anchors,
                    summary,
                    created_at_iso
                ],
            ) {
                Ok(_) => break,
                Err(rusqlite::Error::SqliteFailure(ffi, _))
                    if attempt < 2
                        && ffi.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
                {
                    id = format!("mem-{}", uuid::Uuid::new_v4());
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(json!({
            "id": id,
            "path": normalized_path,
            "project": proj,
            "anchors": normalized_anchors,
            "created_at": created_at_iso,
        }))
    }

    pub fn forget(&self, input: &Value) -> Result<Value, String> {
        let id = input.get("id").and_then(Value::as_str).unwrap_or_default();
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if id.is_empty() || reason.is_empty() {
            return Err("forget() requires id and reason".to_string());
        }
        // Read, decide, and update in one write transaction: a concurrent
        // forget must observe the deleted state, not overwrite another's reason.
        let row = one(
            &self.conn,
            "SELECT id, deleted_at, deleted_reason FROM memories WHERE id=?",
            &[json!(id)],
        )
        .ok_or_else(|| format!("forget() memory not found: {id}"))?;
        if let Some(deleted_at) = row.get("deleted_at").and_then(Value::as_str) {
            return Ok(json!({
                "id": id,
                "deleted_at": deleted_at,
                "deleted_reason": row.get("deleted_reason").cloned().unwrap_or(Value::Null),
                "already_deleted": true,
            }));
        }
        let deleted_at = iso_ms_to_string(crate::indexer::now_ms());
        self.conn
            .execute(
                "UPDATE memories SET deleted_at=?, deleted_reason=? WHERE id=?",
                rusqlite::params![deleted_at, reason, id],
            )
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "id": id,
            "deleted_at": deleted_at,
            "deleted_reason": reason,
        }))
    }
}

fn iso_ms_to_string(ms: f64) -> String {
    // JS new Date().toISOString(): millisecond precision UTC.
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}
