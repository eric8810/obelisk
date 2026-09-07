// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Ported spec tests for the query/attune sandbox helper API.
//!
//! Sources:
//! - tests/query.test.mjs (31 tests) — the behavioral spec of every helper.
//! - tests/contract-helper-shapes.test.mjs (11 tests) — the JSON SHAPE
//!   contract (key sets/nesting) of every helper's return value, plus the
//!   api-reference.md doc-sync guard.
//!
//! Most tests build a database with raw SQL inserts and call the QueryApi /
//! AttuneApi methods directly (mirroring the TS direct-helper tests); a few
//! representative scripts run through `core::execute_query` /
//! `core::execute_attune` to cover the sandbox JSON boundary (args pass
//! through, results parse back, helper errors throw with the right message).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};

use crate::core::{execute_attune, execute_query, QueryOutcome};
use crate::provider_settings::create_builtin_provider_registry;
use crate::providers::types::{
    Cursor, DiscoverContext, IndexUnit, IndexedSession, ParseStream, ProviderAdapter,
    ProviderDescriptor, ProviderRegistry, RawLookup, RawRecord, StreamItem, WatchTarget,
};
use crate::query::{AttuneApi, QueryApi, MULTI_STATEMENT_SQL_MESSAGE, READ_ONLY_SQL_MESSAGE};
use crate::schema::SCHEMA_SQL;

const DEFAULT_PROJECT_PATH: &str = "/tmp/quiet-zero-test";

// ---------------------------------------------------------------------------
// Fixtures (ports of the TS memoryDb / searchDb / contract fixtures)
// ---------------------------------------------------------------------------

fn open_memory_db_with_schema() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory db opens");
    conn.execute_batch(SCHEMA_SQL).expect("schema applies");
    conn
}

/// TS memoryDb: three sessions across two projects and three memories.
fn seed_memory_fixture(conn: &Connection, project_path: &str) {
    let insert_session = "INSERT INTO sessions (id, title, project, project_path, started_at, ended_at, git_branch, message_count)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
    conn.execute(
        insert_session,
        params![
            "sid-1",
            "Older quiet-zero session",
            "quiet-zero",
            project_path,
            "2026-06-09T10:00:00Z",
            "2026-06-09T11:00:00Z",
            "main",
            12
        ],
    )
    .expect("seed sid-1");
    conn.execute(
        insert_session,
        params![
            "sid-2",
            "Memory layer session",
            "quiet-zero",
            project_path,
            "2026-06-10T10:00:00Z",
            "2026-06-10T11:00:00Z",
            "codex/memory-layer",
            23
        ],
    )
    .expect("seed sid-2");
    conn.execute(
        insert_session,
        params![
            "sid-3",
            "Other project session",
            "other-project",
            "/tmp/other-project",
            "2026-06-11T10:00:00Z",
            "2026-06-11T11:00:00Z",
            "main",
            5
        ],
    )
    .expect("seed sid-3");
    let insert_memory = "INSERT INTO memories (id, session_id, project, path, summary, created_at)
        VALUES (?, ?, ?, ?, ?, ?)";
    conn.execute(
        insert_memory,
        params![
            "mem-1",
            "sid-1",
            "quiet-zero",
            ".obelisk/memories/parallel-agents.md",
            "Decision: use parallel agents for independent review facets.",
            "2026-06-09T12:00:00Z"
        ],
    )
    .expect("seed mem-1");
    conn.execute(
        insert_memory,
        params![
            "mem-2",
            "sid-2",
            "quiet-zero",
            ".obelisk/memories/sqlite-memory.md",
            "Decision: store markdown memory records in SQLite.",
            "2026-06-10T12:00:00Z"
        ],
    )
    .expect("seed mem-2");
    conn.execute(
        insert_memory,
        params![
            "mem-3",
            "sid-3",
            "other-project",
            ".obelisk/memories/parallel-agents.md",
            "Other project note about parallel agents.",
            "2026-06-11T12:00:00Z"
        ],
    )
    .expect("seed mem-3");
    conn.execute(
        "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')",
        [],
    )
    .expect("rebuild memories fts");
}

fn memory_db(project_path: &str) -> Connection {
    let conn = open_memory_db_with_schema();
    seed_memory_fixture(&conn, project_path);
    conn
}

/// TS searchDb: one session with meta/text/thinking/inactive/hidden messages.
fn seed_search_fixture(conn: &Connection) {
    conn.execute(
        "INSERT INTO sessions (id, title, project, started_at) VALUES (?, ?, ?, ?)",
        params![
            "sid-search",
            "Search session",
            "quiet-zero",
            "2026-06-10T10:00:00Z"
        ],
    )
    .expect("seed search session");
    let insert = "INSERT INTO messages (uuid, session_id, text, role, timestamp, model, cwd, content_type, is_meta, visibility)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    conn.execute(
        insert,
        params![
            "msg-meta",
            "sid-search",
            "needle injected caveat",
            "user",
            "2026-06-10T10:00:30Z",
            None::<&str>,
            "/tmp/quiet-zero",
            "text",
            1,
            "visible"
        ],
    )
    .expect("seed msg-meta");
    conn.execute(
        insert,
        params![
            "msg-text",
            "sid-search",
            "needle visible reply",
            "assistant",
            "2026-06-10T10:01:00Z",
            "claude-opus",
            "/tmp/quiet-zero",
            "text",
            0,
            "visible"
        ],
    )
    .expect("seed msg-text");
    conn.execute(
        insert,
        params![
            "msg-meta-near",
            "sid-search",
            "<command-name>/exit</command-name>",
            "user",
            "2026-06-10T10:01:30Z",
            None::<&str>,
            "/tmp/quiet-zero",
            "text",
            1,
            "visible"
        ],
    )
    .expect("seed msg-meta-near");
    conn.execute(
        insert,
        params![
            "msg-thinking",
            "sid-search",
            "nearby reasoning trace",
            "assistant",
            "2026-06-10T10:02:00Z",
            "claude-opus",
            "/tmp/quiet-zero",
            "thinking",
            0,
            "visible"
        ],
    )
    .expect("seed msg-thinking");
    conn.execute(
        insert,
        params![
            "msg-inactive",
            "sid-search",
            "needle superseded experiment",
            "assistant",
            "2026-06-10T10:02:30Z",
            "claude-opus",
            "/tmp/quiet-zero",
            "text",
            0,
            "inactive"
        ],
    )
    .expect("seed msg-inactive");
    conn.execute(
        insert,
        params![
            "msg-inactive-meta",
            "sid-search",
            "needle superseded injected",
            "user",
            "2026-06-10T10:02:40Z",
            None::<&str>,
            "/tmp/quiet-zero",
            "text",
            1,
            "inactive"
        ],
    )
    .expect("seed msg-inactive-meta");
    conn.execute(
        insert,
        params![
            "msg-hidden",
            "sid-search",
            "needle abandoned branch",
            "assistant",
            "2026-06-10T10:03:00Z",
            "claude-opus",
            "/tmp/quiet-zero",
            "text",
            0,
            "hidden"
        ],
    )
    .expect("seed msg-hidden");
    conn.execute(
        "INSERT INTO messages_fts(messages_fts) VALUES('rebuild')",
        [],
    )
    .expect("rebuild messages fts");
}

fn search_db() -> Connection {
    let conn = open_memory_db_with_schema();
    seed_search_fixture(&conn);
    conn
}

/// TS contract-helper-shapes fixture: one session with a parent chain, a
/// failing tool call, a subagent, a workflow, a summary, and a memory.
fn contract_db(project_path: &str) -> Connection {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id, title, project, project_path, started_at, ended_at, git_branch, message_count, source)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            "sid-1",
            "Contract session",
            "quiet-zero",
            project_path,
            "2026-06-10T10:00:00Z",
            "2026-06-10T11:00:00Z",
            "main",
            5,
            "claude"
        ],
    )
    .expect("seed contract session");
    let insert_msg = "INSERT INTO messages
        (uuid, session_id, type, parent_uuid, timestamp, role, text, content_type, is_meta, model, agent_id, cwd, source)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
    conn.execute(
        insert_msg,
        params![
            "m-root",
            "sid-1",
            "user",
            None::<&str>,
            "2026-06-10T10:00:00Z",
            "user",
            "contract needle root",
            "text",
            0,
            None::<&str>,
            None::<&str>,
            project_path,
            "claude"
        ],
    )
    .expect("seed m-root");
    conn.execute(
        insert_msg,
        params![
            "m-child",
            "sid-1",
            "assistant",
            "m-root",
            "2026-06-10T10:00:10Z",
            "assistant",
            "contract needle child",
            "text",
            0,
            "claude-opus",
            "sub-A",
            project_path,
            "claude"
        ],
    )
    .expect("seed m-child");
    conn.execute(
        insert_msg,
        params![
            "m-fail",
            "sid-1",
            "assistant",
            "m-child",
            "2026-06-10T10:00:20Z",
            "assistant",
            "ran a command",
            "tool_use",
            0,
            "claude-opus",
            None::<&str>,
            project_path,
            "claude"
        ],
    )
    .expect("seed m-fail");
    conn.execute(
        insert_msg,
        params![
            "m-after",
            "sid-1",
            "assistant",
            "m-fail",
            "2026-06-10T10:00:30Z",
            "assistant",
            "recovered after failure",
            "text",
            0,
            "claude-opus",
            None::<&str>,
            project_path,
            "claude"
        ],
    )
    .expect("seed m-after");
    conn.execute(
        "INSERT INTO tool_calls (id, message_uuid, session_id, name, input_json, file_path)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            "tc-read",
            "m-child",
            "sid-1",
            "Read",
            r#"{"file_path":"/x/file.ts"}"#,
            "/x/file.ts"
        ],
    )
    .expect("seed tc-read");
    conn.execute(
        "INSERT INTO tool_calls (id, message_uuid, session_id, name, input_json, file_path)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            "tc-bash",
            "m-fail",
            "sid-1",
            "Bash",
            r#"{"command":"boom"}"#,
            None::<&str>
        ],
    )
    .expect("seed tc-bash");
    conn.execute(
        "INSERT INTO tool_results (tool_use_id, message_uuid, session_id, content, file_path, is_error)
         VALUES (?, ?, ?, ?, ?, ?)",
        params!["tc-bash", "m-fail", "sid-1", "command failed", None::<&str>, 1],
    )
    .expect("seed tc-bash result");
    conn.execute(
        "INSERT INTO subagents (agent_id, session_id, parent_tool_use_id, agent_type, description, duration_ms, total_tokens)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params!["sub-A", "sid-1", "tc-read", "general-purpose", "a subagent", 100, 200],
    )
    .expect("seed sub-A");
    conn.execute(
        "INSERT INTO workflows (run_id, session_id, task_id, script, result_json, timestamp, agent_count, status, workflow_name)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            "wf-1",
            "sid-1",
            "task-1",
            "export const meta={}",
            r#"{"ok":true}"#,
            "2026-06-10T10:05:00Z",
            1,
            "done",
            "demo"
        ],
    )
    .expect("seed wf-1");
    conn.execute(
        "INSERT INTO workflow_agents (agent_id, run_id, session_id, agent_type, description, phase, label, model, state, duration_ms, tokens, tool_calls)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            "wa-1", "wf-1", "sid-1", "worker", "agent", "Find", "find:x", "claude-opus", "done",
            50, 10, 2
        ],
    )
    .expect("seed wa-1");
    conn.execute(
        "INSERT INTO summaries (id, session_id, timestamp, source, content)
         VALUES (?, ?, ?, ?, ?)",
        params![
            "su-1",
            "sid-1",
            "2026-06-10T10:06:00Z",
            "away_summary",
            "a summary"
        ],
    )
    .expect("seed su-1");
    conn.execute(
        "INSERT INTO memories (id, session_id, project, path, summary, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            "mem-1",
            "sid-1",
            "quiet-zero",
            ".obelisk/memories/x.md",
            "Decision: contract fixture memory.",
            "2026-06-10T10:07:00Z"
        ],
    )
    .expect("seed contract memory");
    conn
}

/// A temp-file database with the schema applied (for tests that need two
/// connections over the same data: AttuneApi mutation + QueryApi recall).
fn temp_file_db() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("index.sqlite");
    let conn = Connection::open(&db_path).expect("temp db opens");
    conn.execute_batch(SCHEMA_SQL).expect("schema applies");
    (dir, db_path)
}

/// A real home directory with `.obelisk/obelisk.sqlite` seeded for
/// `execute_query` / `execute_attune` sandbox boundary tests.
fn query_home() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp home");
    let home = dir.path().to_path_buf();
    let obelisk_dir = home.join(crate::OBELISK_DIR_NAME);
    std::fs::create_dir_all(&obelisk_dir).expect("create .obelisk dir");
    let conn = Connection::open(obelisk_dir.join(crate::DB_FILE_NAME)).expect("open index db");
    conn.execute_batch(SCHEMA_SQL).expect("schema applies");
    seed_memory_fixture(&conn, "/tmp/quiet-zero-test");
    seed_search_fixture(&conn);
    (dir, home)
}

// ---------------------------------------------------------------------------
// QueryApi / helpers
// ---------------------------------------------------------------------------

fn empty_registry() -> Arc<ProviderRegistry> {
    Arc::new(ProviderRegistry::new(vec![]).expect("empty registry is consistent"))
}

fn query_api(conn: Connection) -> QueryApi {
    QueryApi {
        conn,
        registry: empty_registry(),
        invoking_session_id: None,
        cwd: PathBuf::from("/tmp"),
    }
}

fn query_api_at(conn: Connection, cwd: PathBuf) -> QueryApi {
    QueryApi {
        conn,
        registry: empty_registry(),
        invoking_session_id: None,
        cwd,
    }
}

fn query_api_with_registry(conn: Connection, registry: ProviderRegistry) -> QueryApi {
    QueryApi {
        conn,
        registry: Arc::new(registry),
        invoking_session_id: None,
        cwd: PathBuf::from("/tmp"),
    }
}

fn ids(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row.get("id").and_then(Value::as_str).unwrap_or_default())
        .collect()
}

fn uuids(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row.get("uuid").and_then(Value::as_str).unwrap_or_default())
        .collect()
}

fn uuid_visibility_pairs(rows: &[Value]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| {
            (
                row.get("uuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                row.get("visibility")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn message_uuid_visibility_pairs(rows: &[Value]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| {
            let message = row.get("message").unwrap_or(&Value::Null);
            (
                message
                    .get("uuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                message
                    .get("visibility")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

/// TS exactKeys: the object's key set must equal the documented set.
fn exact_keys(value: &Value, expected: &[&str], label: &str) {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("{label}: expected an object, got {value}"))
        .keys()
        .map(String::from)
        .collect();
    keys.sort();
    let mut want: Vec<String> = expected.iter().map(|key| key.to_string()).collect();
    want.sort();
    assert_eq!(keys, want, "{label}: key set drifted from api-reference.md");
}

/// Insertion-order keys (serde_json preserve_order mirrors the TS literals).
fn key_order(value: &Value) -> Vec<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("expected an object, got {value}"))
        .keys()
        .map(String::from)
        .collect()
}

/// TS hasKeys: every documented key must be present.
fn has_keys(value: &Value, expected: &[&str], label: &str) {
    let map = value
        .as_object()
        .unwrap_or_else(|| panic!("{label}: expected an object"));
    for key in expected {
        assert!(
            map.contains_key(*key),
            "{label}: missing documented key \"{key}\""
        );
    }
}

/// `YYYY-MM-DDT...` ISO prefix (TS `/^\d{4}-\d{2}-\d{2}T/`).
fn is_iso_date_prefix(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.len() >= 11
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'T'
}

/// `mem-<uuid v4>`: 36 hex/hyphen uuid chars after the prefix.
fn is_mem_uuid_id(id: &str) -> bool {
    let rest = id.strip_prefix("mem-").unwrap_or_default();
    rest.len() == 36 && rest.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

// ---------------------------------------------------------------------------
// Fake raw provider (ports the TS fake providerRegistry objects)
// ---------------------------------------------------------------------------

struct FakeRawProvider {
    id: &'static str,
    /// Fixed raw text; None → `raw:{message_uuid}` (TS visibility test).
    text: Option<String>,
    /// Unit key override (TS sessionUnitKey).
    unit_key: Option<String>,
    /// Records the cursor received by every raw() call.
    cursors: Arc<Mutex<Vec<Option<String>>>>,
}

impl FakeRawProvider {
    fn new(
        id: &'static str,
        text: Option<String>,
        unit_key: Option<String>,
        cursors: Arc<Mutex<Vec<Option<String>>>>,
    ) -> Self {
        Self {
            id,
            text,
            unit_key,
            cursors,
        }
    }
}

impl ProviderAdapter for FakeRawProvider {
    fn name(&self) -> &'static str {
        self.id
    }

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id,
            name: "Fake Raw Provider",
            vendor: "",
            default_root: String::new(),
            color: "",
            requires_explicit_root: false,
            root_resolution_reason: None,
        }
    }

    fn session_unit_key(&self, _session: &IndexedSession) -> Option<String> {
        self.unit_key.clone()
    }

    fn watch_targets(&self, _configured_root: &str) -> Vec<WatchTarget> {
        Vec::new()
    }

    fn discover<'a>(&self, _ctx: &mut DiscoverContext<'a>) -> Vec<IndexUnit> {
        Vec::new()
    }

    fn parse<'a>(&self, _unit: &'a IndexUnit, _cursor: Cursor) -> ParseStream<'a> {
        Box::new(std::iter::empty::<StreamItem>())
    }

    fn raw(&self, input: &RawLookup) -> Option<RawRecord> {
        self.cursors
            .lock()
            .unwrap()
            .push(input.cursor.map(str::to_string));
        let text = self
            .text
            .clone()
            .unwrap_or_else(|| format!("raw:{}", input.message_uuid));
        let total_length = text.chars().map(char::len_utf16).sum();
        Some(RawRecord {
            text,
            total_length: Some(total_length),
            offset: None,
            limit: None,
            has_more: None,
            message_text: None,
        })
    }
}

fn fake_raw_registry(provider: FakeRawProvider) -> ProviderRegistry {
    ProviderRegistry::new(vec![Arc::new(provider)]).expect("fake registry is consistent")
}

// ---------------------------------------------------------------------------
// search / thread (tests/query.test.mjs)
// ---------------------------------------------------------------------------

#[test]
fn search_falls_back_to_safe_tokenization_for_fts_special_input() {
    let api = query_api(search_db());

    // 'needle-reply' is FTS5 operator syntax (a hyphen). Raw MATCH would
    // throw; search() must fall back to safe per-token quoting
    // ("needle" "reply") and still find the message with both tokens.
    let rows = api.search(&json!("needle-reply"), &json!({ "limit": 5 }));

    assert_eq!(message_uuid_visibility_pairs(&rows).len(), 1);
    assert_eq!(rows[0]["message"]["uuid"], json!("msg-text"));
}

#[test]
fn search_exposes_content_type_on_hits_and_temporal_context() {
    let api = query_api(search_db());

    let rows = api.search(&json!("needle"), &json!({ "limit": 1 }));

    assert_eq!(rows[0]["message"]["uuid"], json!("msg-text"));
    assert_eq!(rows[0]["message"]["content_type"], json!("text"));
    assert_eq!(rows[0]["message"]["is_meta"], json!(0));
    assert_eq!(rows[0]["context"][0]["uuid"], json!("msg-thinking"));
    assert_eq!(rows[0]["context"][0]["content_type"], json!("thinking"));
    assert_eq!(rows[0]["context"][0]["is_meta"], json!(0));
}

#[test]
fn search_and_thread_omit_meta_messages_by_default_and_expose_them_on_request() {
    let api = query_api(search_db());

    assert!(api
        .search(&json!("injected"), &json!({ "limit": 5 }))
        .is_empty());

    let with_meta = api.search(
        &json!("injected"),
        &json!({ "includeMeta": true, "limit": 5 }),
    );
    assert_eq!(with_meta[0]["message"]["uuid"], json!("msg-meta"));
    assert_eq!(with_meta[0]["message"]["is_meta"], json!(1));
    assert!(api
        .search(
            &json!("abandoned"),
            &json!({ "includeMeta": true, "limit": 5 })
        )
        .is_empty());

    assert_eq!(
        uuids(&api.thread(&json!("sid-search"), &Value::Null)),
        vec!["msg-text", "msg-thinking"]
    );
    assert_eq!(
        uuids(&api.thread(&json!("sid-search"), &json!({ "includeMeta": true }))),
        vec!["msg-meta", "msg-text", "msg-meta-near", "msg-thinking"]
    );
    assert_eq!(
        uuid_visibility_pairs(
            &api.thread(&json!("sid-search"), &json!({ "includeInactive": true }))
        ),
        vec![
            ("msg-text".to_string(), "visible".to_string()),
            ("msg-thinking".to_string(), "visible".to_string()),
            ("msg-inactive".to_string(), "inactive".to_string()),
        ]
    );
}

#[test]
fn inactive_search_is_opt_in_orthogonal_to_meta_filtering_and_always_labeled() {
    let api = query_api(search_db());

    assert!(api
        .search(&json!("superseded"), &json!({ "limit": 5 }))
        .is_empty());
    let inactive = api.search(
        &json!("superseded"),
        &json!({ "includeInactive": true, "limit": 5 }),
    );
    assert_eq!(
        message_uuid_visibility_pairs(&inactive),
        vec![("msg-inactive".to_string(), "inactive".to_string())]
    );
    assert!(inactive[0]["context"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| {
            row["visibility"] == json!("visible") || row["visibility"] == json!("inactive")
        }));

    let with_meta = api.search(
        &json!("superseded"),
        &json!({ "includeInactive": true, "includeMeta": true, "limit": 5 }),
    );
    let mut pairs = message_uuid_visibility_pairs(&with_meta);
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("msg-inactive".to_string(), "inactive".to_string()),
            ("msg-inactive-meta".to_string(), "inactive".to_string()),
        ]
    );
    assert!(api
        .search(
            &json!("abandoned"),
            &json!({ "includeInactive": true, "includeMeta": true })
        )
        .is_empty());
}

// ---------------------------------------------------------------------------
// context / trace visibility chains
// ---------------------------------------------------------------------------

#[test]
fn context_and_trace_reject_hidden_targets_and_omit_hidden_ancestors() {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id,title,source) VALUES (?,?,?)",
        params!["sid-chain", "Visibility chain", "pi"],
    )
    .expect("seed session");
    let insert = "INSERT INTO messages (uuid,session_id,type,parent_uuid,role,text,timestamp,visibility,source)
        VALUES (?,?,?,?,?,?,?,?,?)";
    conn.execute(
        insert,
        params![
            "visible-root",
            "sid-chain",
            "user",
            None::<&str>,
            "user",
            "root",
            "2026-08-02T10:00:00Z",
            "visible",
            "pi"
        ],
    )
    .expect("seed visible-root");
    conn.execute(
        insert,
        params![
            "hidden-parent",
            "sid-chain",
            "assistant",
            "visible-root",
            "assistant",
            "secret",
            "2026-08-02T10:00:01Z",
            "hidden",
            "pi"
        ],
    )
    .expect("seed hidden-parent");
    conn.execute(
        insert,
        params![
            "visible-child",
            "sid-chain",
            "user",
            "hidden-parent",
            "user",
            "continue",
            "2026-08-02T10:00:02Z",
            "visible",
            "pi"
        ],
    )
    .expect("seed visible-child");
    conn.execute(
        insert,
        params![
            "inactive-child",
            "sid-chain",
            "assistant",
            "visible-root",
            "assistant",
            "superseded",
            "2026-08-02T10:00:03Z",
            "inactive",
            "pi"
        ],
    )
    .expect("seed inactive-child");
    let api = query_api(conn);

    assert_eq!(api.context(&json!("hidden-parent"), &Value::Null), None);
    assert_eq!(
        api.context(&json!("hidden-parent"), &json!({ "includeInactive": true })),
        None
    );
    assert!(api.trace(&json!("hidden-parent"), &Value::Null).is_empty());
    assert!(api
        .trace(&json!("hidden-parent"), &json!({ "includeInactive": true }))
        .is_empty());
    assert_eq!(api.context(&json!("inactive-child"), &Value::Null), None);
    assert!(api.trace(&json!("inactive-child"), &Value::Null).is_empty());
    assert_eq!(
        uuid_visibility_pairs(
            api.context(
                &json!("inactive-child"),
                &json!({ "includeInactive": true })
            )
            .unwrap()["parentChain"]
                .as_array()
                .unwrap()
        ),
        vec![("visible-root".to_string(), "visible".to_string())]
    );
    assert_eq!(
        uuid_visibility_pairs(&api.trace(
            &json!("inactive-child"),
            &json!({ "includeInactive": true })
        )),
        vec![
            ("visible-root".to_string(), "visible".to_string()),
            ("inactive-child".to_string(), "inactive".to_string()),
        ]
    );
    assert_eq!(
        uuids(
            api.context(&json!("visible-child"), &Value::Null).unwrap()["parentChain"]
                .as_array()
                .unwrap()
        ),
        vec!["visible-root"]
    );
    assert_eq!(
        uuids(&api.trace(&json!("visible-child"), &Value::Null)),
        vec!["visible-root", "visible-child"]
    );
}

// ---------------------------------------------------------------------------
// raw (provider projection, visibility, cursors, UTF-16 slicing)
// ---------------------------------------------------------------------------

#[test]
fn raw_rejects_hidden_targets_and_labels_explicitly_included_inactive_evidence() {
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let registry = fake_raw_registry(FakeRawProvider::new("claude", None, None, cursors));
    let api = query_api_with_registry(search_db(), registry);

    assert_eq!(api.raw(&json!("msg-hidden"), &Value::Null), None);
    assert_eq!(
        api.raw(&json!("msg-hidden"), &json!({ "includeInactive": true })),
        None
    );
    assert_eq!(api.raw(&json!("msg-inactive"), &Value::Null), None);
    assert_eq!(
        api.raw(&json!("msg-inactive"), &json!({ "includeInactive": true }))
            .unwrap(),
        json!({
            "text": "raw:msg-inactive",
            "totalLength": 16,
            "offset": 0,
            "limit": 10000,
            "hasMore": false,
            "visibility": "inactive",
        })
    );
    let visible = api.raw(&json!("msg-text"), &Value::Null).unwrap();
    assert_eq!(visible["text"], json!("raw:msg-text"));
    assert_eq!(visible["visibility"], json!("visible"));
}

#[test]
fn raw_slices_text_by_utf16_code_units() {
    // JS String.slice operates on UTF-16 code units: the emoji is 2 units.
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let registry = fake_raw_registry(FakeRawProvider::new(
        "claude",
        Some("a\u{1f600}b".into()),
        None,
        cursors,
    ));
    let api = query_api_with_registry(search_db(), registry);

    let result = api
        .raw(&json!("msg-text"), &json!({ "offset": 1, "limit": 2 }))
        .unwrap();
    assert_eq!(
        result,
        json!({
            "text": "\u{1f600}",
            "totalLength": 4,
            "offset": 1,
            "limit": 2,
            "hasMore": true,
            "visibility": "visible",
        })
    );
}

#[test]
fn raw_looks_up_cursors_by_provider_unit_identity_instead_of_source_path() {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id,title,jsonl_path,source) VALUES (?,?,?,?)",
        params![
            "sid-raw-key",
            "Raw key",
            "/alpha/agents/main/wire.jsonl",
            "alpha"
        ],
    )
    .expect("seed session");
    conn.execute(
        "INSERT INTO messages (uuid,session_id,type,role,text,content_type,visibility,source)
         VALUES (?,?,?,?,?,?,?,?)",
        params![
            "msg-raw-key",
            "sid-raw-key",
            "user",
            "user",
            "raw key",
            "text",
            "visible",
            "alpha"
        ],
    )
    .expect("seed message");
    conn.execute(
        "INSERT INTO index_state (jsonl_path,mtime,lines_processed,cursor) VALUES (?,?,?,?)",
        params!["alpha:unit", 10.0, 1, "10:1"],
    )
    .expect("seed index_state");
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let registry = fake_raw_registry(FakeRawProvider::new(
        "alpha",
        Some("raw".into()),
        Some("alpha:unit".into()),
        cursors.clone(),
    ));
    let api = query_api_with_registry(conn, registry);

    assert_eq!(
        api.raw(&json!("msg-raw-key"), &Value::Null).unwrap()["text"],
        json!("raw")
    );
    assert_eq!(*cursors.lock().unwrap(), vec![Some("10:1".to_string())]);
}

// ---------------------------------------------------------------------------
// failures
// ---------------------------------------------------------------------------

#[test]
fn failures_next_messages_does_not_leak_hidden_branch_messages() {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id,title,source) VALUES (?,?,?)",
        params!["sid-failure", "Failure branch", "pi"],
    )
    .expect("seed session");
    let insert_message =
        "INSERT INTO messages (uuid,session_id,type,role,text,timestamp,visibility,source)
        VALUES (?,?,?,?,?,?,?,?)";
    conn.execute(
        insert_message,
        params![
            "failure-result",
            "sid-failure",
            "user",
            "toolResult",
            "failed",
            "2026-08-02T10:00:00Z",
            "visible",
            "pi"
        ],
    )
    .expect("seed failure-result");
    conn.execute(
        insert_message,
        params![
            "hidden-next",
            "sid-failure",
            "assistant",
            "assistant",
            "abandoned",
            "2026-08-02T10:00:01Z",
            "hidden",
            "pi"
        ],
    )
    .expect("seed hidden-next");
    conn.execute(
        insert_message,
        params![
            "inactive-next",
            "sid-failure",
            "assistant",
            "assistant",
            "superseded",
            "2026-08-02T10:00:02Z",
            "inactive",
            "pi"
        ],
    )
    .expect("seed inactive-next");
    conn.execute(
        insert_message,
        params![
            "visible-next",
            "sid-failure",
            "assistant",
            "assistant",
            "recovered",
            "2026-08-02T10:00:03Z",
            "visible",
            "pi"
        ],
    )
    .expect("seed visible-next");
    conn.execute(
        "INSERT INTO tool_calls (id,message_uuid,session_id,name,input_json) VALUES (?,?,?,?,?)",
        params![
            "call-failure",
            "failure-result",
            "sid-failure",
            "read",
            "{}"
        ],
    )
    .expect("seed tool call");
    conn.execute(
        "INSERT INTO tool_results (tool_use_id,message_uuid,session_id,content,is_error) VALUES (?,?,?,?,?)",
        params!["call-failure", "failure-result", "sid-failure", "failed", 1],
    )
    .expect("seed tool result");
    let api = query_api(conn);

    let row = &api.failures(&json!("sid-failure"))[0];
    assert_eq!(
        uuids(row["nextMessages"].as_array().unwrap()),
        vec!["visible-next"]
    );
    assert_eq!(row["visibility"], json!("visible"));
    let with_inactive =
        &api.failures(&json!({ "sessionId": "sid-failure", "includeInactive": true }))[0];
    assert_eq!(
        uuid_visibility_pairs(with_inactive["nextMessages"].as_array().unwrap()),
        vec![
            ("inactive-next".to_string(), "inactive".to_string()),
            ("visible-next".to_string(), "visible".to_string()),
        ]
    );
}

#[test]
fn failures_gates_both_result_and_linked_call_message_visibility() {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id,title,source) VALUES (?,?,?)",
        params!["sid-edge-visibility", "Tool edge visibility", "pi"],
    )
    .expect("seed session");
    let insert_message =
        "INSERT INTO messages (uuid,session_id,type,role,text,timestamp,visibility,source)
        VALUES (?,?,?,?,?,?,?,?)";
    let insert_call =
        "INSERT INTO tool_calls (id,message_uuid,session_id,name,input_json,file_path)
        VALUES (?,?,?,?,?,?)";
    let insert_result =
        "INSERT INTO tool_results (tool_use_id,message_uuid,session_id,content,is_error)
        VALUES (?,?,?,?,?)";
    for (index, call_visibility) in ["visible", "inactive", "hidden"].iter().enumerate() {
        let call_id = format!("call-{call_visibility}");
        conn.execute(
            insert_message,
            params![
                format!("message-{call_visibility}"),
                "sid-edge-visibility",
                "assistant",
                "assistant",
                None::<&str>,
                format!("2026-08-02T10:00:0{}Z", index * 2),
                call_visibility,
                "pi"
            ],
        )
        .expect("seed call message");
        conn.execute(
            insert_message,
            params![
                format!("result-{call_visibility}"),
                "sid-edge-visibility",
                "user",
                "toolResult",
                format!("failed-{call_visibility}"),
                format!("2026-08-02T10:00:0{}Z", index * 2 + 1),
                "visible",
                "pi"
            ],
        )
        .expect("seed result message");
        conn.execute(
            insert_call,
            params![
                call_id,
                format!("message-{call_visibility}"),
                "sid-edge-visibility",
                "read",
                format!("{{\"path\":\"/{call_visibility}\"}}"),
                format!("/{call_visibility}")
            ],
        )
        .expect("seed tool call");
        conn.execute(
            insert_result,
            params![
                call_id,
                format!("result-{call_visibility}"),
                "sid-edge-visibility",
                format!("failed-{call_visibility}"),
                1
            ],
        )
        .expect("seed tool result");
    }
    let api = query_api(conn);

    assert_eq!(
        api.failures(&json!("sid-edge-visibility"))
            .iter()
            .map(|record| record["toolCall"]["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["call-visible"]
    );
    let mut pairs: Vec<(String, String)> = api
        .failures(&json!({ "sessionId": "sid-edge-visibility", "includeInactive": true }))
        .iter()
        .map(|record| {
            (
                record["toolCall"]["id"].as_str().unwrap().to_string(),
                record["visibility"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("call-inactive".to_string(), "inactive".to_string()),
            ("call-visible".to_string(), "visible".to_string()),
        ]
    );
}

#[test]
fn failures_preserves_orphaned_error_results_without_linked_messages_or_calls() {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id,title,source) VALUES (?,?,?)",
        params!["sid-orphan-failure", "Orphan failure", "codex"],
    )
    .expect("seed session");
    conn.execute(
        "INSERT INTO tool_results (tool_use_id,message_uuid,session_id,content,is_error) VALUES (?,?,?,?,?)",
        params!["missing-call", "", "sid-orphan-failure", "orphaned failure", 1],
    )
    .expect("seed orphan result");
    let api = query_api(conn);

    let rows = api.failures(&json!("sid-orphan-failure"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["toolCall"], Value::Null);
    assert_eq!(rows[0]["result"]["content"], json!("orphaned failure"));
    assert_eq!(rows[0]["visibility"], json!("visible"));
    assert_eq!(rows[0]["nextMessages"], json!([]));
}

#[test]
fn summaries_file_history_and_failures_expose_inactive_rows_only_on_request() {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id,title,source) VALUES (?,?,?)",
        params!["sid-structured", "Structured visibility", "pi"],
    )
    .expect("seed session");
    let insert_message =
        "INSERT INTO messages (uuid,session_id,type,role,text,timestamp,visibility,source)
        VALUES (?,?,?,?,?,?,?,?)";
    let insert_call =
        "INSERT INTO tool_calls (id,message_uuid,session_id,name,input_json,file_path)
        VALUES (?,?,?,?,?,?)";
    let insert_result =
        "INSERT INTO tool_results (tool_use_id,message_uuid,session_id,content,is_error)
        VALUES (?,?,?,?,?)";
    let insert_summary = "INSERT INTO summaries (id,session_id,timestamp,source,content,visibility)
        VALUES (?,?,?,?,?,?)";
    for (index, visibility) in ["visible", "inactive", "hidden"].iter().enumerate() {
        let suffix = visibility;
        let uuid = format!("message-{suffix}");
        let call_id = format!("call-{suffix}");
        let timestamp = format!("2026-08-02T10:00:0{index}Z");
        conn.execute(
            insert_message,
            params![
                uuid,
                "sid-structured",
                "user",
                "toolResult",
                suffix,
                timestamp,
                visibility,
                "pi"
            ],
        )
        .expect("seed message");
        conn.execute(
            insert_call,
            params![
                call_id,
                uuid,
                "sid-structured",
                "read",
                "{}",
                "/tmp/visibility.ts"
            ],
        )
        .expect("seed call");
        conn.execute(
            insert_result,
            params![
                call_id,
                uuid,
                "sid-structured",
                format!("failed-{suffix}"),
                1
            ],
        )
        .expect("seed result");
        conn.execute(
            insert_summary,
            params![
                format!("summary-{suffix}"),
                "sid-structured",
                timestamp,
                "pi:branch_summary",
                suffix,
                visibility
            ],
        )
        .expect("seed summary");
    }
    let api = query_api(conn);

    assert_eq!(
        api.file_history(&json!("/tmp/visibility.ts"), &Value::Null)
            .iter()
            .map(|row| row["visibility"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["visible"]
    );
    assert_eq!(
        api.file_history(
            &json!("/tmp/visibility.ts"),
            &json!({ "includeInactive": true })
        )
        .iter()
        .map(|row| row["visibility"].as_str().unwrap())
        .collect::<Vec<_>>(),
        vec!["visible", "inactive"]
    );
    assert_eq!(
        api.failures(&json!("sid-structured"))
            .iter()
            .map(|row| row["visibility"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["visible"]
    );
    assert_eq!(
        api.failures(&json!({ "sessionId": "sid-structured", "includeInactive": true }))
            .iter()
            .map(|row| {
                (
                    row["visibility"].as_str().unwrap().to_string(),
                    row["result"]["visibility"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("inactive".to_string(), "inactive".to_string()),
            ("visible".to_string(), "visible".to_string()),
        ]
    );
    assert_eq!(
        api.summaries(&json!("sid-structured"))
            .iter()
            .map(|row| row["visibility"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["visible"]
    );
    assert_eq!(
        api.summaries(&json!({ "sessionId": "sid-structured", "includeInactive": true }))
            .iter()
            .map(|row| row["visibility"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["inactive", "visible"]
    );
}

// ---------------------------------------------------------------------------
// memories (list opts, FTS recall, English-only enforcement)
// ---------------------------------------------------------------------------

#[test]
fn memories_follows_list_helper_scalar_opts_and_filters_by_query_within_scope() {
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    assert_eq!(ids(&api.memories(&json!("sid-1")).unwrap()), vec!["mem-1"]);
    assert_eq!(ids(&api.memories(&json!(1)).unwrap()), vec!["mem-3"]);
    assert_eq!(
        ids(&api
            .memories(&json!({ "project": "%quiet-zero%", "query": "parallel agents", "limit": 5 }))
            .unwrap()),
        vec!["mem-1"]
    );
}

#[test]
fn memories_requires_english_query_terms() {
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    // The TS spec: a CJK query errors with the English-only contract
    // message instead of silently becoming an FTS miss.
    let error = api
        .memories(&json!({ "query": "记忆层", "limit": 5 }))
        .expect_err("non-English query must be rejected");
    assert!(
        error.contains("memories() query must use English terms"),
        "{error}"
    );
}

#[test]
fn memories_uses_fts_recall_with_safe_english_tokenization_and_rank() {
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    let rows = api
        .memories(&json!({ "project": "%quiet-zero%", "query": "sqlite-memory", "limit": 5 }))
        .unwrap();

    assert_eq!(ids(&rows), vec!["mem-2"]);
    assert!(rows[0]["rank"].is_number());
}

#[test]
fn memories_does_not_broaden_punctuation_only_fts_queries_into_full_recall() {
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    assert!(api
        .memories(&json!({ "project": "%quiet-zero%", "query": "---", "limit": 5 }))
        .unwrap()
        .is_empty());
}

// ---------------------------------------------------------------------------
// overview
// ---------------------------------------------------------------------------

#[test]
fn overview_returns_a_compact_current_project_map_with_bounded_sessions() {
    let project_dir = tempfile::tempdir().expect("temp project dir");
    let cwd = project_dir.path().to_path_buf();
    let api = query_api_at(memory_db(&cwd.to_string_lossy()), cwd.clone());

    let view = api.overview(&json!({ "limit": 1, "projectLimit": 5 }));

    assert_eq!(view["current"]["cwd"], json!(cwd.to_string_lossy()));
    assert_eq!(view["current"]["project"]["project"], json!("quiet-zero"));
    assert_eq!(
        view["current"]["project"]["source"],
        json!("cwd_project_path")
    );
    assert_eq!(view["current"]["project"]["confidence"], json!("exact"));
    assert!(!view["current"].as_object().unwrap().contains_key("session"));
    assert_eq!(view["current_project"]["session_total"], json!(2));
    assert_eq!(
        view["current_project"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["sid-2"]
    );
    assert_eq!(view["current_project"]["memory_total"], json!(2));
    assert_eq!(
        ids(view["current_project"]["memories"].as_array().unwrap()),
        vec!["mem-2", "mem-1"]
    );
    assert_eq!(view["totals"]["projects"], json!(2));
    assert_eq!(view["totals"]["sessions"], json!(3));
    assert_eq!(view["totals"]["memories"], json!(3));
    assert!(view["projects"].as_array().unwrap().iter().any(|p| {
        p["project"] == json!("quiet-zero")
            && p["session_count"] == json!(2)
            && p["memory_count"] == json!(2)
    }));
}

// ---------------------------------------------------------------------------
// sql() read-only contract
// ---------------------------------------------------------------------------

#[test]
fn query_api_sql_is_read_only() {
    // TS also asserts the query api does not expose attune helpers; in Rust
    // that is the compile-time surface (QueryApi has no remember/forget).
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    assert!(api.overview(&Value::Null)["totals"].is_object());
    assert_eq!(
        ids(&api
            .sql(&json!("SELECT id FROM memories ORDER BY id"), &[])
            .unwrap()),
        vec!["mem-1", "mem-2", "mem-3"]
    );
    assert_eq!(
        api.sql(
            &json!("INSERT INTO memories (id, path, summary) VALUES ('mem-x', '/tmp/x.md', 'x')"),
            &[]
        )
        .unwrap_err(),
        READ_ONLY_SQL_MESSAGE
    );
}

#[test]
fn sql_accepts_blocked_keywords_in_literals_comments_and_quoted_identifiers() {
    // The issue #107 repro: all of these are read-only and must not be
    // rejected for merely containing a blocked word.
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    assert_eq!(
        api.sql(&json!("SELECT 'live update' AS text"), &[])
            .unwrap()[0]["text"],
        json!("live update")
    );
    assert_eq!(
        api.sql(&json!(r#"SELECT 1 AS "delete""#), &[]).unwrap()[0]["delete"],
        json!(1)
    );
    assert_eq!(
        api.sql(&json!("SELECT 1 AS x -- INSERT INTO t"), &[])
            .unwrap()[0]["x"],
        json!(1)
    );
    assert_eq!(
        api.sql(&json!("SELECT 1 AS x /* DROP TABLE memories */"), &[])
            .unwrap()[0]["x"],
        json!(1)
    );
    // A blocked word inside a LIKE literal: no rows match, and that is the
    // point — the query executes instead of being rejected.
    assert!(api
        .sql(
            &json!("SELECT id FROM memories WHERE summary LIKE '%update%' ORDER BY id"),
            &[]
        )
        .unwrap()
        .is_empty());
    // Recursive CTEs and pragma table-valued functions are read-only too.
    assert_eq!(
        api.sql(
            &json!("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<3) SELECT x FROM c"),
            &[]
        )
        .unwrap()
        .len(),
        3
    );
    assert!(!api
        .sql(
            &json!("SELECT name FROM pragma_table_info('memories')"),
            &[]
        )
        .unwrap()
        .is_empty());
}

#[test]
fn read_only_connection_fails_writes_and_never_mutates_the_index() {
    // The TS suite had two tests here: an authorizer-driven prepare-time
    // rejection and the read-only connection boundary. rusqlite has no
    // authorizer, so this port covers the boundary path — the one the TS
    // test fell back to on runtimes without an authorizer — plus the
    // multi-statement rejection.
    let (dir, db_path) = temp_file_db();
    {
        let conn = Connection::open(&db_path).expect("seed connection opens");
        seed_memory_fixture(&conn, "/tmp/quiet-zero-test");
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("read-only connection opens");
    let api = query_api(conn);

    // Multi-statement input is rejected at prepare, before anything runs.
    assert_eq!(
        api.sql(&json!("SELECT 1; DROP TABLE memories"), &[])
            .unwrap_err(),
        MULTI_STATEMENT_SQL_MESSAGE
    );
    // A write prefixed with a valid WITH CTE passes the lexical entry check;
    // the read-only connection is the final mutation boundary.
    assert_eq!(
        api.sql(
            &json!("WITH c AS (SELECT 1) INSERT INTO memories (id, path, summary) SELECT 'mem-x', '/tmp/x.md', 'x' FROM c"),
            &[]
        )
        .unwrap_err(),
        READ_ONLY_SQL_MESSAGE
    );
    assert_eq!(
        api.sql(&json!("WITH c AS (SELECT 1) DELETE FROM memories"), &[])
            .unwrap_err(),
        READ_ONLY_SQL_MESSAGE
    );
    // The index is unchanged by every rejected write.
    assert_eq!(
        api.sql(&json!("SELECT COUNT(*) AS c FROM memories"), &[])
            .unwrap()[0]["c"],
        json!(3)
    );
    drop(api);
    drop(dir);
}

#[test]
fn sql_enforces_one_statement_per_call_with_clear_failures() {
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    for bad in ["SELECT 1; SELECT 2", "SELECT 1;; SELECT 2"] {
        assert_eq!(
            api.sql(&json!(bad), &[]).unwrap_err(),
            MULTI_STATEMENT_SQL_MESSAGE,
            "input: {bad}"
        );
    }

    // A trailing semicolon and trailing comments are still a single statement.
    assert_eq!(api.sql(&json!("SELECT 1;"), &[]).unwrap()[0]["1"], json!(1));
    assert_eq!(
        api.sql(&json!("SELECT 1; -- trailing comment"), &[])
            .unwrap()[0]["1"],
        json!(1)
    );
    assert_eq!(
        api.sql(&json!("SELECT 1; /* trailing comment */"), &[])
            .unwrap()[0]["1"],
        json!(1)
    );
}

#[test]
fn sql_rejects_unterminated_comment_tail() {
    let api = query_api(memory_db(DEFAULT_PROJECT_PATH));

    assert_eq!(
        api.sql(&json!("SELECT 1; /* unterminated"), &[])
            .unwrap_err(),
        MULTI_STATEMENT_SQL_MESSAGE
    );
}

// ---------------------------------------------------------------------------
// attune api (remember / forget)
// ---------------------------------------------------------------------------

#[test]
fn attune_api_exposes_only_memory_mutation_helpers() {
    // TS asserts Object.keys(api) is exactly ['forget', 'remember'] and that
    // search/sql are undefined. In Rust that is the compile-time surface:
    // AttuneApi has exactly the two mutation methods. Smoke-test them.
    let api = AttuneApi {
        conn: memory_db(DEFAULT_PROJECT_PATH),
        cwd: PathBuf::from("/tmp"),
    };

    assert_eq!(
        api.forget(&json!({})).unwrap_err(),
        "forget() requires id and reason"
    );
    assert_eq!(
        api.remember(&json!({})).unwrap_err(),
        "remember() requires path and summary"
    );
}

#[test]
fn remember_inserts_a_fresh_id_and_never_overwrites_existing_rows() {
    // TS fakes the DB driver to force a PRIMARY KEY collision and observe
    // the regenerate-and-retry loop. A rusqlite Connection cannot be mocked,
    // so this port locks the observable contract instead: a plain INSERT
    // with a freshly generated mem-<uuid> id that never replaces rows.
    let (dir, db_path) = temp_file_db();
    let memory_path = dir.path().join("memory.md");
    std::fs::write(&memory_path, "# Memory\n").expect("write memory file");
    let id;
    {
        let conn = Connection::open(&db_path).expect("db opens");
        conn.execute(
            "INSERT INTO memories (id, path, summary, created_at) VALUES (?, ?, ?, ?)",
            params![
                "mem-existing",
                "/m.md",
                "seed memory",
                "2026-08-01T00:00:00Z"
            ],
        )
        .expect("seed existing memory");
        let api = AttuneApi {
            conn,
            cwd: dir.path().to_path_buf(),
        };
        let result = api
            .remember(&json!({
                "path": memory_path.to_string_lossy(),
                "project": "collision-test",
                "summary": "Decision: memory ids regenerate on collision instead of overwriting.",
            }))
            .expect("remember succeeds");
        id = result["id"].as_str().expect("id is a string").to_string();
        assert_ne!(id, "mem-existing");
        assert!(is_mem_uuid_id(&id), "id must be mem-<uuid v4>: {id}");
    }
    let check = Connection::open(&db_path).expect("verify connection opens");
    let count: i64 = check
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .expect("count memories");
    assert_eq!(count, 2, "plain INSERT keeps the existing row");
    let (summary,): (String,) = check
        .query_row(
            "SELECT summary FROM memories WHERE id='mem-existing'",
            [],
            |row| Ok((row.get(0)?,)),
        )
        .expect("existing row survives");
    assert_eq!(summary, "seed memory");
}

#[test]
fn remember_stores_absolute_project_relative_memory_path() {
    let project_dir = tempfile::tempdir().expect("temp project dir");
    let memory_dir = project_dir.path().join(".obelisk").join("memories");
    std::fs::create_dir_all(&memory_dir).expect("create memory dir");
    let memory_path = memory_dir.join("decision.md");
    std::fs::write(&memory_path, "# Decision\n").expect("write memory file");
    let (dir, db_path) = temp_file_db();
    let id;
    {
        let conn = Connection::open(&db_path).expect("db opens");
        seed_memory_fixture(&conn, &project_dir.path().to_string_lossy());
        let api = AttuneApi {
            conn,
            cwd: PathBuf::from("/tmp"),
        };
        let result = api
            .remember(&json!({
                "path": ".obelisk/memories/decision.md",
                "session_id": "sid-1",
                "summary": "Decision: store normalized memory paths.",
            }))
            .expect("remember succeeds");
        assert_eq!(result["path"], json!(memory_path.to_string_lossy()));
        id = result["id"].as_str().expect("id is a string").to_string();
    }
    let check = Connection::open(&db_path).expect("verify connection opens");
    let stored_path: String = check
        .query_row("SELECT path FROM memories WHERE id=?", params![id], |row| {
            row.get(0)
        })
        .expect("stored row has the path");
    assert_eq!(stored_path, memory_path.to_string_lossy());
    drop(dir);
}

#[test]
fn remember_updates_fts_recall_for_the_registered_memory_immediately() {
    let project_dir = tempfile::tempdir().expect("temp project dir");
    let memory_dir = project_dir.path().join(".obelisk").join("memories");
    std::fs::create_dir_all(&memory_dir).expect("create memory dir");
    let memory_path = memory_dir.join("query-plan.md");
    std::fs::write(&memory_path, "# Query Plan\n").expect("write memory file");
    let (dir, db_path) = temp_file_db();
    let registered_id;
    {
        let conn = Connection::open(&db_path).expect("db opens");
        seed_memory_fixture(&conn, &project_dir.path().to_string_lossy());
        let api = AttuneApi {
            conn,
            cwd: PathBuf::from("/tmp"),
        };
        let registered = api
            .remember(&json!({
                "path": ".obelisk/memories/query-plan.md",
                "session_id": "sid-2",
                "summary": "Decision: use faceted query plans for synthesis recall.",
            }))
            .expect("remember succeeds");
        registered_id = registered["id"]
            .as_str()
            .expect("id is a string")
            .to_string();
    }
    let api = query_api(Connection::open(&db_path).expect("recall connection opens"));
    let rows = api
        .memories(&json!({
            "project": "%quiet-zero%",
            "query": "faceted query plans",
            "limit": 5,
        }))
        .expect("memories recall succeeds");

    assert_eq!(ids(&rows), vec![registered_id.as_str()]);
    assert!(rows[0]["rank"].is_number());
    drop(dir);
}

#[test]
fn forget_soft_deletes_memory_records_from_active_recall() {
    let (dir, db_path) = temp_file_db();
    let deleted_at;
    {
        let conn = Connection::open(&db_path).expect("db opens");
        seed_memory_fixture(&conn, DEFAULT_PROJECT_PATH);
        let api = AttuneApi {
            conn,
            cwd: PathBuf::from("/tmp"),
        };
        let result = api
            .forget(&json!({ "id": "mem-1", "reason": "Outdated project guidance." }))
            .expect("forget succeeds");
        assert_eq!(result["id"], json!("mem-1"));
        assert_eq!(
            result["deleted_reason"],
            json!("Outdated project guidance.")
        );
        deleted_at = result["deleted_at"]
            .as_str()
            .expect("deleted_at is a string")
            .to_string();
        assert!(
            is_iso_date_prefix(&deleted_at),
            "deleted_at is ISO: {deleted_at}"
        );
    }
    let check = Connection::open(&db_path).expect("verify connection opens");
    let (stored_at, stored_reason): (String, String) = check
        .query_row(
            "SELECT deleted_at, deleted_reason FROM memories WHERE id='mem-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("soft-deleted row survives");
    assert_eq!(stored_at, deleted_at);
    assert_eq!(stored_reason, "Outdated project guidance.");
    let api = query_api(Connection::open(&db_path).expect("recall connection opens"));
    assert_eq!(
        ids(&api
            .memories(&json!({ "project": "%quiet-zero%", "limit": 10 }))
            .expect("memories recall succeeds")),
        vec!["mem-2"]
    );
    drop(dir);
}

#[test]
fn remember_requires_english_summaries() {
    let project_dir = tempfile::tempdir().expect("temp project dir");
    let memory_dir = project_dir.path().join(".obelisk").join("memories");
    std::fs::create_dir_all(&memory_dir).expect("create memory dir");
    let memory_path = memory_dir.join("decision.md");
    std::fs::write(&memory_path, "# Decision\n").expect("write memory file");
    let api = AttuneApi {
        conn: memory_db(&project_dir.path().to_string_lossy()),
        cwd: PathBuf::from("/tmp"),
    };

    let error = api
        .remember(&json!({
            "path": ".obelisk/memories/decision.md",
            "session_id": "sid-1",
            "summary": "决策：记忆摘要必须使用英文。",
        }))
        .unwrap_err();
    assert!(
        error.contains("remember() summary must be written in English"),
        "unexpected error: {error}"
    );
}

#[test]
fn remember_rejects_missing_memory_files() {
    let project_dir = tempfile::tempdir().expect("temp project dir");
    let api = AttuneApi {
        conn: memory_db(&project_dir.path().to_string_lossy()),
        cwd: PathBuf::from("/tmp"),
    };

    let error = api
        .remember(&json!({
            "path": ".obelisk/memories/missing.md",
            "session_id": "sid-1",
            "summary": "Decision: this should not be registered.",
        }))
        .unwrap_err();
    assert!(
        error.starts_with("remember() memory file does not exist"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// subagents
// ---------------------------------------------------------------------------

#[test]
fn subagents_after_before_narrow_by_the_activity_interval_not_session_ids() {
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id,title,started_at) VALUES (?,?,?)",
        params!["sid-agents", "Subagent session", "2026-06-01T09:00:00Z"],
    )
    .expect("seed session");
    let insert_agent =
        "INSERT INTO subagents (agent_id,session_id,agent_type,description) VALUES (?,?,?,?)";
    conn.execute(
        insert_agent,
        params!["agent-early", "sid-agents", "explore", "Early agent"],
    )
    .expect("seed agent-early");
    conn.execute(
        insert_agent,
        params!["agent-late", "sid-agents", "coder", "Late agent"],
    )
    .expect("seed agent-late");
    conn.execute(
        insert_agent,
        params![
            "agent-spanning",
            "sid-agents",
            "coder",
            "Agent active across the bound"
        ],
    )
    .expect("seed agent-spanning");
    let insert_message = "INSERT INTO messages (uuid,session_id,type,role,text,timestamp,agent_id) VALUES (?,?,?,?,?,?,?)";
    conn.execute(
        insert_message,
        params![
            "m-early-1",
            "sid-agents",
            "assistant",
            "assistant",
            "early start",
            "2026-06-01T10:00:00Z",
            "agent-early"
        ],
    )
    .expect("seed m-early-1");
    conn.execute(
        insert_message,
        params![
            "m-early-2",
            "sid-agents",
            "assistant",
            "assistant",
            "early end",
            "2026-06-01T10:05:00Z",
            "agent-early"
        ],
    )
    .expect("seed m-early-2");
    conn.execute(
        insert_message,
        params![
            "m-late-1",
            "sid-agents",
            "assistant",
            "assistant",
            "late start",
            "2026-06-03T10:00:00Z",
            "agent-late"
        ],
    )
    .expect("seed m-late-1");
    conn.execute(
        insert_message,
        params![
            "m-span-1",
            "sid-agents",
            "assistant",
            "assistant",
            "spanning start",
            "2026-06-01T12:00:00Z",
            "agent-spanning"
        ],
    )
    .expect("seed m-span-1");
    conn.execute(
        insert_message,
        params![
            "m-span-2",
            "sid-agents",
            "assistant",
            "assistant",
            "spanning end",
            "2026-06-03T12:00:00Z",
            "agent-spanning"
        ],
    )
    .expect("seed m-span-2");
    let api = query_api(conn);

    let agent_ids = |opts: Value| {
        let mut ids: Vec<String> = api
            .subagents(&opts)
            .iter()
            .map(|row| row["agent_id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    };

    // `after` keeps agents still active past the bound: the spanning agent's
    // interval crosses 06-02 even though it started before it.
    assert_eq!(
        agent_ids(json!({ "after": "2026-06-02T00:00:00Z" })),
        vec!["agent-late", "agent-spanning"]
    );
    // `before` keeps agents already started by the bound.
    assert_eq!(
        agent_ids(json!({ "before": "2026-06-02T00:00:00Z" })),
        vec!["agent-early", "agent-spanning"]
    );
    // Combined bounds select every agent active during the window.
    assert_eq!(
        agent_ids(json!({ "after": "2026-06-01T09:00:00Z", "before": "2026-06-04T00:00:00Z" })),
        vec!["agent-early", "agent-late", "agent-spanning"]
    );
    // A window inside the spanning agent's interval matches it alone.
    assert_eq!(
        api.subagents(
            &json!({ "after": "2026-06-02T00:00:00Z", "before": "2026-06-03T00:00:00Z" })
        )
        .iter()
        .map(|row| row["agent_id"].as_str().unwrap())
        .collect::<Vec<_>>(),
        vec!["agent-spanning"]
    );
}

#[test]
fn subagents_derives_total_tokens_from_sidechain_usage_when_unstored() {
    // ADR-0010: the stored row carries no total_tokens; the value is derived
    // at query time from sidechain message usage.
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id, title, source) VALUES ('sid-sub', 'sub parent', 'deepseek')",
        [],
    )
    .expect("seed session");
    conn.execute(
        "INSERT INTO subagents (agent_id, session_id, agent_type, description) VALUES ('deepseek:agent-1:scope', 'sid-sub', 'deepseek-official', 'helper')",
        [],
    )
    .expect("seed subagent");
    let insert_msg = "INSERT INTO messages (uuid, session_id, type, role, text, timestamp, agent_id, input_tokens, output_tokens, source) VALUES (?,?,?,?,?,?,?,?,?,?)";
    conn.execute(
        insert_msg,
        params![
            "sub-m1",
            "sid-sub",
            "assistant",
            "assistant",
            "a",
            "2026-06-01T00:00:00Z",
            "deepseek:agent-1:scope",
            10,
            5,
            "deepseek"
        ],
    )
    .expect("seed sub-m1");
    conn.execute(
        insert_msg,
        params![
            "sub-m2",
            "sid-sub",
            "assistant",
            "assistant",
            "b",
            "2026-06-01T00:01:00Z",
            "deepseek:agent-1:scope",
            3,
            2,
            "deepseek"
        ],
    )
    .expect("seed sub-m2");
    let api = query_api(conn);

    let row = &api.subagents(&Value::Null)[0];
    assert_eq!(row["total_tokens"], json!(20)); // (10+5) + (3+2)
    assert_eq!(row["messageCount"], json!(2));
    let ctx = api
        .context(&json!("sub-m1"), &Value::Null)
        .expect("context resolves");
    assert_eq!(ctx["subagent"]["total_tokens"], json!(20));
}

#[test]
fn subagents_never_overrides_a_provider_stored_total_tokens() {
    // Codex stores total_tokens authoritatively at persist time; the derived
    // message sum (15) must not replace the stored value (20).
    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id, title, source) VALUES ('sid-codex', 'codex parent', 'codex')",
        [],
    )
    .expect("seed session");
    conn.execute(
        "INSERT INTO subagents (agent_id, session_id, agent_type, total_tokens) VALUES ('codex:agent-1', 'sid-codex', 'worker', 20)",
        [],
    )
    .expect("seed subagent");
    conn.execute(
        "INSERT INTO messages (uuid, session_id, type, role, text, timestamp, agent_id, input_tokens, output_tokens, source) VALUES ('cx-m1', 'sid-codex', 'assistant', 'assistant', 'a', '2026-06-01T00:00:00Z', 'codex:agent-1', 10, 5, 'codex')",
        [],
    )
    .expect("seed cx-m1");
    let api = query_api(conn);

    assert_eq!(api.subagents(&Value::Null)[0]["total_tokens"], json!(20));
    let ctx = api
        .context(&json!("cx-m1"), &Value::Null)
        .expect("context resolves");
    assert_eq!(ctx["subagent"]["total_tokens"], json!(20));
}

// ---------------------------------------------------------------------------
// Contract shapes (tests/contract-helper-shapes.test.mjs)
// ---------------------------------------------------------------------------

#[test]
fn search_hit_shape_matches_api_reference() {
    let api = query_api(contract_db("/tmp/contract-cwd"));
    let rows = api.search(&json!("needle"), &json!({ "limit": 1 }));
    let hit = &rows[0];

    exact_keys(
        hit,
        &["message", "session", "rank", "context"],
        "search() hit",
    );
    exact_keys(
        &hit["message"],
        &[
            "uuid",
            "text",
            "content_type",
            "is_meta",
            "role",
            "timestamp",
            "model",
            "cwd",
            "visibility",
            "source",
        ],
        "search() hit.message",
    );
    exact_keys(
        &hit["session"],
        &["id", "title", "project", "started_at", "source"],
        "search() hit.session",
    );
    assert!(
        hit["context"].is_array(),
        "search() hit.context is an array"
    );
    // Insertion order matches the TS literals (serde_json preserve_order).
    assert_eq!(
        key_order(hit),
        vec!["message", "session", "rank", "context"]
    );
}

#[test]
fn context_shape_matches_api_reference() {
    let api = query_api(contract_db("/tmp/contract-cwd"));
    let ctx = api
        .context(&json!("m-child"), &Value::Null)
        .expect("context resolves");

    exact_keys(
        &ctx,
        &["message", "parentChain", "session", "subagent", "workflow"],
        "context()",
    );
    assert!(
        ctx["parentChain"].is_array(),
        "context() parentChain is an array"
    );
    assert_eq!(ctx["parentChain"][0]["uuid"], json!("m-root"));
    assert_eq!(
        key_order(&ctx),
        vec!["message", "parentChain", "session", "subagent", "workflow"]
    );
}

#[test]
fn file_history_row_shape_matches_api_reference() {
    let api = query_api(contract_db("/tmp/contract-cwd"));
    let rows = api.file_history(&json!("/x/file.ts"), &Value::Null);
    let row = &rows[0];

    exact_keys(
        row,
        &["toolCall", "session", "timestamp", "visibility"],
        "fileHistory() row",
    );
    exact_keys(
        &row["toolCall"],
        &["id", "message_uuid", "name", "input_json"],
        "fileHistory() row.toolCall",
    );
    exact_keys(
        &row["session"],
        &["id", "title", "project"],
        "fileHistory() row.session",
    );
    assert_eq!(
        key_order(row),
        vec!["toolCall", "session", "timestamp", "visibility"]
    );
}

#[test]
fn failures_row_shape_matches_api_reference() {
    let api = query_api(contract_db("/tmp/contract-cwd"));
    let rows = api.failures(&Value::Null);
    let row = &rows[0];

    exact_keys(
        row,
        &[
            "toolCall",
            "result",
            "session",
            "nextMessages",
            "visibility",
        ],
        "failures() row",
    );
    assert!(
        row["nextMessages"].is_array(),
        "failures() row.nextMessages is an array"
    );
    assert_eq!(row["nextMessages"][0]["uuid"], json!("m-after"));
    assert_eq!(
        key_order(row),
        vec![
            "toolCall",
            "result",
            "session",
            "nextMessages",
            "visibility"
        ]
    );
}

#[test]
fn subagents_row_carries_message_count() {
    let api = query_api(contract_db("/tmp/contract-cwd"));
    let row = &api.subagents(&Value::Null)[0];

    has_keys(row, &["messageCount"], "subagents() row");
    assert_eq!(row["messageCount"], json!(1));
}

#[test]
fn workflow_tree_shape_carries_result_and_agents_with_message_count() {
    let api = query_api(contract_db("/tmp/contract-cwd"));
    let tree = api
        .workflow_tree(&json!("wf-1"))
        .expect("workflow tree resolves");

    has_keys(&tree, &["result", "agents"], "workflowTree()");
    assert_eq!(tree["result"], json!({ "ok": true }));
    assert!(
        tree["agents"].is_array(),
        "workflowTree() agents is an array"
    );
    has_keys(
        &tree["agents"][0],
        &["messageCount"],
        "workflowTree() agent",
    );
    // The workflows() list helper (SELECT * rows, sessionId filter).
    let workflows = api.workflows(&json!({ "sessionId": "sid-1" }));
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0]["run_id"], json!("wf-1"));
}

#[test]
fn summaries_row_carries_session_title_and_project() {
    let api = query_api(contract_db("/tmp/contract-cwd"));
    let row = &api.summaries(&Value::Null)[0];

    has_keys(row, &["session_title", "project"], "summaries() row");
}

#[test]
fn overview_shape_matches_api_reference() {
    let cwd = "/tmp/contract-cwd";
    let api = query_api_at(contract_db(cwd), PathBuf::from(cwd));
    let view = api.overview(&json!({ "limit": 5 }));

    exact_keys(
        &view,
        &["current", "current_project", "projects", "totals"],
        "overview()",
    );
    exact_keys(
        &view["current"],
        &["cwd", "project", "session_id"],
        "overview() current",
    );
    assert_eq!(
        view["current"]["session_id"],
        Value::Null,
        "no invocation nonce: session_id is null"
    );
    exact_keys(
        &view["totals"],
        &["projects", "sessions", "memories", "sources"],
        "overview() totals",
    );
    assert_eq!(
        key_order(&view),
        vec!["current", "current_project", "projects", "totals"]
    );
    assert_eq!(
        key_order(&view["current"]),
        vec!["cwd", "project", "session_id"]
    );
    assert_eq!(
        key_order(&view["totals"]),
        vec!["projects", "sessions", "memories", "sources"]
    );
}

#[test]
fn forget_result_shape_matches_api_reference() {
    let api = AttuneApi {
        conn: contract_db("/tmp/contract-cwd"),
        cwd: PathBuf::from("/tmp"),
    };

    let forgotten = api
        .forget(&json!({ "id": "mem-1", "reason": "contract test" }))
        .expect("forget succeeds");
    exact_keys(
        &forgotten,
        &["id", "deleted_at", "deleted_reason"],
        "forget()",
    );
    assert_eq!(
        key_order(&forgotten),
        vec!["id", "deleted_at", "deleted_reason"]
    );
}

#[test]
fn raw_shape_matches_api_reference() {
    let dir = tempfile::tempdir().expect("temp jsonl dir");
    let jsonl_path = dir.path().join("session.jsonl");
    let line =
        r#"{"uuid":"m-raw","type":"user","message":{"role":"user","content":"raw line body"}}"#;
    std::fs::write(&jsonl_path, format!("{line}\n")).expect("write jsonl");

    let conn = open_memory_db_with_schema();
    conn.execute(
        "INSERT INTO sessions (id, title, project, project_path, jsonl_path, source)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            "sid-raw",
            "Raw session",
            "quiet-zero",
            dir.path().to_string_lossy(),
            jsonl_path.to_string_lossy(),
            "claude"
        ],
    )
    .expect("seed raw session");
    conn.execute(
        "INSERT INTO messages (uuid, session_id, type, role, text, content_type, source)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            "m-raw",
            "sid-raw",
            "user",
            "user",
            "raw line body",
            "text",
            "claude"
        ],
    )
    .expect("seed raw message");

    let home = tempfile::tempdir().expect("temp home");
    let roots: HashMap<String, PathBuf> = HashMap::new();
    let registry = create_builtin_provider_registry(home.path(), &roots, Path::new("/tmp"));
    let api = query_api_with_registry(conn, registry);

    let result = api
        .raw(&json!("m-raw"), &Value::Null)
        .expect("raw resolves");
    exact_keys(
        &result,
        &[
            "text",
            "totalLength",
            "offset",
            "limit",
            "hasMore",
            "visibility",
        ],
        "raw()",
    );
    assert_eq!(result["text"], json!(line));
    assert_eq!(result["totalLength"], json!(line.len()));
    assert_eq!(
        key_order(&result),
        vec![
            "text",
            "totalLength",
            "offset",
            "limit",
            "hasMore",
            "visibility"
        ]
    );
}

#[test]
fn doc_sync_guard_every_asserted_contract_key_appears_in_api_reference() {
    // Runs the same guard as the TS suite: every key asserted by the shape
    // tests above must still be documented in api-reference.md.
    const API_REFERENCE: &str = include_str!("../../../skill-doc/references/api-reference.md");
    let asserted = [
        "message",
        "session",
        "rank",
        "context",
        "uuid",
        "text",
        "content_type",
        "is_meta",
        "role",
        "timestamp",
        "model",
        "cwd",
        "visibility",
        "source",
        "id",
        "title",
        "project",
        "started_at",
        "parentChain",
        "subagent",
        "workflow",
        "toolCall",
        "message_uuid",
        "name",
        "input_json",
        "result",
        "nextMessages",
        "messageCount",
        "agents",
        "session_title",
        "current",
        "current_project",
        "projects",
        "totals",
        "session_id",
        "memories",
        "sessions",
        "deleted_at",
        "deleted_reason",
        "totalLength",
        "offset",
        "limit",
        "hasMore",
    ];
    let missing: Vec<&str> = asserted
        .iter()
        .copied()
        .filter(|key| !API_REFERENCE.contains(key))
        .collect();
    assert!(
        missing.is_empty(),
        "keys asserted in tests but absent from api-reference.md: {missing:?}"
    );
}

// ---------------------------------------------------------------------------
// Sandbox boundary (execute_query / execute_attune)
// ---------------------------------------------------------------------------

fn run_query_script(script: &str) -> Value {
    let (home, cwd) = query_home();
    match execute_query(home.path(), &cwd, script, None) {
        Ok(QueryOutcome::Value(value)) => value,
        other => panic!("expected value outcome, got {other:?}"),
    }
}

#[test]
fn execute_query_passes_sql_arguments_and_results_through_the_json_boundary() {
    let value = run_query_script(
        "const rows = await sql('SELECT id FROM memories ORDER BY id'); return rows.map(r => r.id);",
    );
    assert_eq!(value, json!(["mem-1", "mem-2", "mem-3"]));
}

#[test]
fn execute_query_passes_search_opts_through_the_json_boundary() {
    let value = run_query_script(
        "const rows = await search('needle', { limit: 1 }); return rows[0].message.uuid;",
    );
    assert_eq!(value, json!("msg-text"));
}

#[test]
fn execute_query_helper_errors_throw_with_the_contract_message() {
    let (home, cwd) = query_home();
    let script = r#"return await sql("INSERT INTO memories (id, path, summary) VALUES ('mem-x', '/tmp/x.md', 'x')");"#;
    let error = execute_query(home.path(), &cwd, script, None)
        .expect_err("write helper must throw inside the sandbox");
    assert_eq!(error.message, READ_ONLY_SQL_MESSAGE);
}

#[test]
fn execute_query_resolves_overview_and_recent_results() {
    // The home fixture seeds three memory-fixture sessions plus the search
    // fixture's sid-search session.
    let overview = run_query_script("const view = await overview(); return view.totals.sessions;");
    assert_eq!(overview, json!(4));

    let recent = run_query_script("const rows = await recent(2); return rows.map(s => s.id);");
    assert_eq!(recent, json!(["sid-3", "sid-2"]));
}

#[test]
fn execute_attune_remember_runs_through_the_sandbox_boundary() {
    let (home, cwd) = query_home();
    std::fs::write(cwd.join("memory.md"), "# Memory\n").expect("write memory file");
    let script = r#"return await remember({ path: 'memory.md', summary: 'Decision: sandbox attune boundary.' });"#;

    let result = execute_attune(home.path(), &cwd, script).expect("attune script succeeds");
    let id = result["id"].as_str().expect("id is a string");
    assert!(is_mem_uuid_id(id), "id must be mem-<uuid v4>: {id}");
    assert_eq!(
        result["path"],
        json!(cwd.join("memory.md").to_string_lossy())
    );

    let db_path = home
        .path()
        .join(crate::OBELISK_DIR_NAME)
        .join(crate::DB_FILE_NAME);
    let check = Connection::open(&db_path).expect("verify connection opens");
    let summary: String = check
        .query_row(
            "SELECT summary FROM memories WHERE id=?",
            params![id],
            |row| row.get(0),
        )
        .expect("memory row persisted");
    assert_eq!(summary, "Decision: sandbox attune boundary.");
}
