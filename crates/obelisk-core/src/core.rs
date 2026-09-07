// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! The top-level Core API (port of packages/core/src/core.ts): the four
//! verbs' implementations behind the CLI. The CLI and later the desktop app
//! are thin shells over these; none re-implement retrieval or own the DB
//! lifecycle beyond what each function does.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::db;
use crate::indexer::{build_index, ensure_readable_schema, now_ms, BuildIndexOptions};
use crate::provider_settings::{
    create_configured_builtin_provider_runtime, read_persisted_provider_settings, SettingsRead,
};
use crate::providers::types::ProviderRegistry;
use crate::query::{AttuneApi, QueryApi};
use crate::sandbox::{run_sandbox, SandboxOptions, SandboxOutcome};
use crate::tx::{run_retryable_write_transaction, WriteRetryOptions};

/// One nonce candidate: the path first, then (strict) script content.
#[derive(Debug, Clone)]
pub struct NonceCandidate {
    pub value: String,
    pub strict: bool,
}

#[derive(Debug)]
pub struct CoreError {
    pub message: String,
}

impl CoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CoreError {}

// Recency bound for the tool_calls leg: the nonce's tool-call record is by
// definition written at invocation time — always recent.
const INVOCATION_RECENCY_MS: f64 = 15.0 * 60.0 * 1000.0;
// Epsilon for genuine concurrent collisions (seconds apart), versus
// sequential nonce reuses (minutes apart).
const INVOCATION_COLLISION_MS: f64 = 10_000.0;

const INVOCATION_POLL_INTERVAL_MS: u64 = 300;
const INVOCATION_POLL_CAP_MS: u64 = 4000;

fn report_incomplete_inventory(build: &crate::indexer::BuildResult) {
    for issue in &build.inventory_issues {
        eprintln!(
            "Warning: incomplete {} source inventory at {}: {}",
            issue.provider, issue.path, issue.error
        );
    }
}

/// The provider registry for a query pass: settings-aware. On unavailable
/// settings the index refresh is skipped with a warning (queries stay
/// read-only), mirroring the TS refreshQueryIndex.
enum RefreshOutcome {
    Registry(Arc<ProviderRegistry>),
    SchemaBlocked(String),
}

fn refresh_query_index(home: &Path, cwd: &Path) -> Result<RefreshOutcome, CoreError> {
    let settings = read_persisted_provider_settings(home);
    let (ok, persisted, error) = match settings {
        SettingsRead::Ok(persisted) => (true, persisted, String::new()),
        SettingsRead::Failed(error) => (false, Value::Null, error),
    };
    let runtime =
        create_configured_builtin_provider_runtime(home, cwd, &persisted, &Default::default());
    let registry = Arc::new(runtime.registry);
    if !ok {
        let schema =
            ensure_readable_schema(home).map_err(|error| CoreError::new(error.to_string()))?;
        if !schema.ready {
            let reason = schema.reason.unwrap_or("an unknown writer");
            return Ok(RefreshOutcome::SchemaBlocked(format!(
                "Obelisk index schema upgrade is blocked by {reason}"
            )));
        }
        eprintln!("Warning: {error}; index refresh skipped");
        return Ok(RefreshOutcome::Registry(registry));
    }
    let build = build_index(
        home,
        BuildIndexOptions {
            provider_registry: Some(registry.clone()),
            ..Default::default()
        },
    );
    report_incomplete_inventory(&build);
    Ok(RefreshOutcome::Registry(registry))
}

// ---- invoking-session resolution (core.ts) ----

struct IndexedTables {
    messages_fts: bool,
    messages: bool,
    tool_calls: bool,
}

fn indexed_tables(conn: &Connection) -> IndexedTables {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE name IN ('messages_fts', 'messages', 'tool_calls')",
        )
        .expect("sqlite_master read cannot fail on a healthy database");
    let mut tables = IndexedTables {
        messages_fts: false,
        messages: false,
        tool_calls: false,
    };
    if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
        for name in rows.flatten() {
            match name.as_str() {
                "messages_fts" => tables.messages_fts = true,
                "messages" => tables.messages = true,
                "tool_calls" => tables.tool_calls = true,
                _ => {}
            }
        }
    }
    tables
}

fn iso_from_ms(ms: f64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

/// JS `Date.parse` for ISO-8601 text; unparseable → None (NaN).
fn parse_iso_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn like_pattern(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    format!("%{out}%")
}

/// Resolve one nonce against the index (newest-wins, honest null on
/// ambiguity; strict mode additionally requires a unique CLI invoker).
fn resolve_single_invocation_nonce(
    conn: &Connection,
    nonce: &str,
    now: Option<f64>,
    strict: bool,
) -> Option<String> {
    let now = now.unwrap_or_else(now_ms);
    let cutoff = iso_from_ms(now - INVOCATION_RECENCY_MS);
    let tables = indexed_tables(conn);
    let mut newest_by_session: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut track = |session_id: Option<&str>, timestamp: Option<&str>| {
        if let (Some(session_id), Some(timestamp)) = (session_id, timestamp) {
            match newest_by_session.get(session_id) {
                Some(prev) if timestamp <= prev.as_str() => {}
                _ => {
                    newest_by_session.insert(session_id.to_string(), timestamp.to_string());
                }
            }
        }
    };
    // FTS narrows to candidate messages; exact containment filters tokenizer
    // false positives (hyphenated nonces are split into tokens).
    let re = crate::parsing::token_re();
    let tokens: Vec<&str> = re.find_iter(nonce).map(|m| m.as_str()).collect();
    let fts_query: String = tokens
        .iter()
        .take(12)
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" ");
    if !fts_query.is_empty() && tables.messages_fts && tables.messages {
        if let Ok(rows) = crate::query::run_rows(
            conn,
            "SELECT m.session_id AS session_id, m.text AS text, m.timestamp AS timestamp
             FROM messages_fts mf JOIN messages m ON m.uuid = mf.uuid
             WHERE mf.text MATCH ? AND m.timestamp >= ?",
            &[json!(fts_query), json!(cutoff)],
        ) {
            for row in rows {
                if let Some(text) = row.get("text").and_then(Value::as_str) {
                    if text.contains(nonce) {
                        track(
                            row.get("session_id").and_then(Value::as_str),
                            row.get("timestamp").and_then(Value::as_str),
                        );
                    }
                }
            }
        }
    }
    // The obelisk command line lands in tool_calls.input_json. input_json is
    // JSON-encoded, so a nonce with JSON-escaped characters is stored in
    // escaped form; match both spellings.
    if tables.tool_calls && tables.messages {
        let json_escaped = serde_json::to_string(nonce)
            .ok()
            .map(|s| s[1..s.len() - 1].to_string());
        let mut patterns = vec![like_pattern(nonce)];
        if let Some(escaped) = &json_escaped {
            if escaped != nonce {
                patterns.push(like_pattern(escaped));
            }
        }
        let like_clause = patterns
            .iter()
            .map(|_| "tc.input_json LIKE ? ESCAPE '\\'")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT tc.session_id AS session_id, m.timestamp AS timestamp
             FROM messages m CROSS JOIN tool_calls tc ON tc.message_uuid = m.uuid
             WHERE m.timestamp >= ? AND ({like_clause})"
        );
        let mut params = vec![json!(cutoff)];
        params.extend(patterns.iter().map(|p| json!(p)));
        if let Ok(rows) = crate::query::run_rows(conn, &sql, &params) {
            for row in rows {
                track(
                    row.get("session_id").and_then(Value::as_str),
                    row.get("timestamp").and_then(Value::as_str),
                );
            }
        }
    }
    if newest_by_session.is_empty() {
        return None;
    }
    if strict {
        // Content-derived candidates are not unique by construction, so
        // resolve only when exactly one recent session matches AND that
        // session itself holds a recent CLI invocation record.
        let invokers = recent_cli_invoker_sessions(conn, &cutoff, &tables);
        let eligible: Vec<&String> = newest_by_session
            .keys()
            .filter(|id| invokers.contains(*id))
            .collect();
        return if eligible.len() == 1 {
            Some(eligible[0].clone())
        } else {
            None
        };
    }
    let mut ranked: Vec<(&String, Option<i64>)> = newest_by_session
        .iter()
        .map(|(session_id, timestamp)| (session_id, parse_iso_ms(timestamp)))
        .collect();
    // An unparseable timestamp makes ordering unreliable: lean null.
    if ranked.iter().any(|(_, ms)| ms.is_none()) {
        return None;
    }
    ranked.sort_by_key(|&(_, ms)| std::cmp::Reverse(ms));
    if let (Some(&(_, Some(first_ms))), Some(&(_, Some(second_ms)))) =
        (ranked.first(), ranked.get(1))
    {
        if first_ms - second_ms <= INVOCATION_COLLISION_MS as i64 {
            return None;
        }
    }
    Some(ranked[0].0.clone())
}

fn recent_cli_invoker_sessions(
    conn: &Connection,
    cutoff: &str,
    tables: &IndexedTables,
) -> std::collections::HashSet<String> {
    let mut invokers = std::collections::HashSet::new();
    let patterns = ["%obelisk --%", "%obelisk.js --%"];
    if tables.tool_calls && tables.messages {
        if let Ok(rows) = crate::query::run_rows(
            conn,
            "SELECT DISTINCT tc.session_id AS session_id
             FROM messages m CROSS JOIN tool_calls tc ON tc.message_uuid = m.uuid
             WHERE m.timestamp >= ? AND (tc.input_json LIKE ? OR tc.input_json LIKE ?)",
            &[json!(cutoff), json!(patterns[0]), json!(patterns[1])],
        ) {
            for row in rows {
                if let Some(session_id) = row.get("session_id").and_then(Value::as_str) {
                    invokers.insert(session_id.to_string());
                }
            }
        }
    }
    if tables.messages {
        if let Ok(rows) = crate::query::run_rows(
            conn,
            "SELECT DISTINCT session_id FROM messages
             WHERE timestamp >= ? AND (text LIKE ? OR text LIKE ?)",
            &[json!(cutoff), json!(patterns[0]), json!(patterns[1])],
        ) {
            for row in rows {
                if let Some(session_id) = row.get("session_id").and_then(Value::as_str) {
                    invokers.insert(session_id.to_string());
                }
            }
        }
    }
    invokers
}

/// Try nonce candidates in order; first match wins.
pub fn resolve_invoking_session_id(
    conn: &Connection,
    candidates: &[NonceCandidate],
) -> Option<String> {
    for candidate in candidates {
        if candidate.value.is_empty() {
            continue;
        }
        if let Some(hit) =
            resolve_single_invocation_nonce(conn, &candidate.value, None, candidate.strict)
        {
            return Some(hit);
        }
    }
    None
}

fn schema_ready_for_carve_out(home: &Path) -> bool {
    let Ok(conn) = db::open_read_db(home) else {
        return false;
    };
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) AS c FROM sqlite_master WHERE name IN ('messages_fts', 'messages', 'tool_calls')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if tables != 3 {
        return false;
    }
    // The carve-out build must not perform a schema upgrade: migrating a
    // legacy schema under a fresh daemon heartbeat remains daemon-owned.
    !crate::schema::core_schema_needs_migration(&conn)
}

/// Resolve the invoking session, closing the index-freshness gap first: the
/// pre-query refresh skips when a recent build or the app daemon owns
/// writes, so the current invocation's tool-call record may not be indexed
/// yet. On a first miss, run one incremental recovery build (bypassing both
/// the recent-build debounce and daemon policy ownership), then poll for a
/// concurrent writer to publish the nonce. Still unresolved after the cap
/// is honest null.
pub fn resolve_invoking_session_id_with_wait(
    home: &Path,
    candidates: &[NonceCandidate],
    registry: Arc<ProviderRegistry>,
) -> Option<String> {
    if candidates.iter().all(|c| c.value.is_empty()) {
        return None;
    }
    let try_resolve = || -> Option<String> {
        let conn = db::open_read_db(home).ok()?;
        let hit = resolve_invoking_session_id(&conn, candidates);
        drop(conn);
        hit
    };
    if let Some(immediate) = try_resolve() {
        return Some(immediate);
    }
    if schema_ready_for_carve_out(home) {
        let build = build_index(
            home,
            BuildIndexOptions {
                ignore_recent_build: true,
                ignore_daemon_ownership: true,
                provider_registry: Some(registry),
                ..Default::default()
            },
        );
        report_incomplete_inventory(&build);
        if let Some(after_build) = try_resolve() {
            return Some(after_build);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(INVOCATION_POLL_CAP_MS);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(INVOCATION_POLL_INTERVAL_MS));
        if let Some(hit) = try_resolve() {
            return Some(hit);
        }
    }
    None
}

/// Rethrow an error unless the schema is blocked: a stale schema under an
/// active daemon must surface the honest blocked reason instead.
fn rethrow_unless_schema_blocked(home: &Path, error: String) -> CoreError {
    let schema = ensure_readable_schema(home).ok();
    match schema {
        Some(schema) if !schema.ready => CoreError::new(format!(
            "Obelisk index schema upgrade is blocked by {}",
            schema.reason.unwrap_or("an unknown writer")
        )),
        _ => CoreError::new(error),
    }
}

/// FTS search over indexed message text. Refreshes the index, then queries.
pub fn search_text(
    home: &Path,
    cwd: &Path,
    text: &str,
    opts: Value,
    invocation_nonce: Option<&[NonceCandidate]>,
) -> Result<Value, CoreError> {
    let registry = match refresh_query_index(home, cwd)? {
        RefreshOutcome::Registry(registry) => registry,
        RefreshOutcome::SchemaBlocked(message) => return Err(CoreError::new(message)),
    };
    let invoking_session_id = invocation_nonce
        .map(|candidates| resolve_invoking_session_id_with_wait(home, candidates, registry.clone()))
        .unwrap_or_default();
    let conn = db::open_read_db(home)
        .map_err(|error| rethrow_unless_schema_blocked(home, error.to_string()))?;
    let api = QueryApi {
        conn,
        registry,
        invoking_session_id,
        cwd: PathBuf::from(cwd),
    };
    Ok(Value::Array(api.search(&json!(text), &opts)))
}

/// The result of a CodeAct script: `undefined` is distinct from null
/// (the TS CLI prints the literal `undefined` for it).
#[derive(Debug, Clone)]
pub enum QueryOutcome {
    Value(Value),
    Undefined,
}

const QUERY_HELPERS: &[&str] = &[
    "sql",
    "search",
    "context",
    "trace",
    "thread",
    "subagents",
    "workflows",
    "workflowTree",
    "fileHistory",
    "failures",
    "sessions",
    "recent",
    "summaries",
    "raw",
    "memories",
    "overview",
];

/// Execute a read-only CodeAct query script and resolve its returned value.
pub fn execute_query(
    home: &Path,
    cwd: &Path,
    script: &str,
    invocation_nonce: Option<&[NonceCandidate]>,
) -> Result<QueryOutcome, CoreError> {
    let registry = match refresh_query_index(home, cwd)? {
        RefreshOutcome::Registry(registry) => registry,
        RefreshOutcome::SchemaBlocked(message) => return Err(CoreError::new(message)),
    };
    let invoking_session_id = invocation_nonce
        .map(|candidates| resolve_invoking_session_id_with_wait(home, candidates, registry.clone()))
        .unwrap_or_default();
    let conn = db::open_read_db(home)
        .map_err(|error| rethrow_unless_schema_blocked(home, error.to_string()))?;
    let api = QueryApi {
        conn,
        registry,
        invoking_session_id,
        cwd: PathBuf::from(cwd),
    };
    let dispatch = move |name: &str, args: &Value| -> Result<Value, String> {
        let empty = Value::Null;
        let arg0 = args.get(0).unwrap_or(&empty);
        let arg1 = args.get(1).unwrap_or(&empty);
        let params: Vec<Value> = args
            .as_array()
            .map(|array| array.iter().skip(1).cloned().collect())
            .unwrap_or_default();
        match name {
            "sql" => api.sql(arg0, &params).map(Value::Array),
            "search" => Ok(Value::Array(api.search(arg0, arg1))),
            "context" => Ok(api.context(arg0, arg1).unwrap_or(Value::Null)),
            "trace" => Ok(Value::Array(api.trace(arg0, arg1))),
            "thread" => Ok(Value::Array(api.thread(arg0, arg1))),
            "subagents" => Ok(Value::Array(api.subagents(arg0))),
            "workflows" => Ok(Value::Array(api.workflows(arg0))),
            "workflowTree" => Ok(api.workflow_tree(arg0).unwrap_or(Value::Null)),
            "fileHistory" => Ok(Value::Array(api.file_history(arg0, arg1))),
            "failures" => Ok(Value::Array(api.failures(arg0))),
            "sessions" => Ok(Value::Array(api.sessions(arg0))),
            "recent" => Ok(Value::Array(api.recent(arg0))),
            "summaries" => Ok(Value::Array(api.summaries(arg0))),
            "raw" => Ok(api.raw(arg0, arg1).unwrap_or(Value::Null)),
            "memories" => api.memories(arg0).map(Value::Array),
            "overview" => Ok(api.overview(arg0)),
            other => Err(format!("unknown helper: {other}")),
        }
    };
    match run_sandbox(
        Box::new(dispatch),
        QUERY_HELPERS,
        script,
        SandboxOptions::default(),
    ) {
        SandboxOutcome::Value(value) => Ok(QueryOutcome::Value(value)),
        SandboxOutcome::Undefined => Ok(QueryOutcome::Undefined),
        SandboxOutcome::Error { message, .. } => Err(rethrow_unless_schema_blocked(home, message)),
        SandboxOutcome::Timeout { message } => Err(CoreError::new(message)),
    }
}

const ATTUNE_HELPERS: &[&str] = &["remember", "forget"];

/// Execute a memory-mutation CodeAct script (remember/forget only). Memory
/// writes are independent of index writes by design: no settings read, no
/// pre-write index build, no daemon-ownership check, no writer lease
/// (ADR-0006 amendment); each mutation is a single short retryable write
/// transaction.
pub fn execute_attune(home: &Path, cwd: &Path, script: &str) -> Result<Value, CoreError> {
    let conn = db::open_attune_db(home).map_err(CoreError::new)?;
    let api = AttuneApi {
        conn,
        cwd: PathBuf::from(cwd),
    };
    // Verify the recall half of the memory layer actually works before
    // accepting mutations, inside the same retryable wrapper.
    run_retryable_write_transaction(
        &api.conn,
        || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            db::probe_attune_memory_layer(&api.conn).map_err(|message| {
                Box::new(std::io::Error::other(message)) as Box<dyn std::error::Error + Send + Sync>
            })
        },
        crate::tx::WriteTxOptions {
            label: Some("attune".to_string()),
        },
        &WriteRetryOptions {
            retry_on_begin_busy: true,
            budget_ms: 5000,
            retry_delay_ms: 100,
            max_attempts: 10,
        },
    )
    .map_err(|error| CoreError::new(error.to_string()))?;

    let dispatch = move |name: &str, args: &Value| -> Result<Value, String> {
        let arg0 = args.get(0).cloned().unwrap_or(Value::Null);
        // Each mutation runs in its own short retryable write transaction.
        let mutation = |api: &AttuneApi, name: &str, arg0: &Value| -> Result<Value, String> {
            match name {
                "remember" => api.remember(arg0),
                "forget" => api.forget(arg0),
                other => Err(format!("unknown helper: {other}")),
            }
        };
        run_retryable_write_transaction(
            &api.conn,
            || -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
                mutation(&api, name, &arg0).map_err(|message| {
                    Box::new(std::io::Error::other(message))
                        as Box<dyn std::error::Error + Send + Sync>
                })
            },
            crate::tx::WriteTxOptions {
                label: Some("attune".to_string()),
            },
            &WriteRetryOptions {
                retry_on_begin_busy: true,
                budget_ms: 5000,
                retry_delay_ms: 100,
                max_attempts: 10,
            },
        )
        .map_err(|error| error.to_string())
    };
    match run_sandbox(
        Box::new(dispatch),
        ATTUNE_HELPERS,
        script,
        SandboxOptions::default(),
    ) {
        SandboxOutcome::Value(value) => Ok(value),
        SandboxOutcome::Undefined => Ok(Value::Null),
        SandboxOutcome::Error { message, .. } => Err(CoreError::new(message)),
        SandboxOutcome::Timeout { message } => Err(CoreError::new(message)),
    }
}
