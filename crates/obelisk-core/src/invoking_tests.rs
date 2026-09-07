// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Spec tests for the nonce-based invoking-session resolution (port of
//! tests/invoking-session.test.mjs and tests/attune-contention.test.mjs).
//!
//! The TS unit fixtures pin the clock via `nowMs`; the Rust resolver reads
//! the real clock, so fixture timestamps are computed relative to the real
//! current time instead (same eras, same relative offsets).

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::core::{execute_attune, resolve_invoking_session_id, NonceCandidate};

const NONCE: &str = "obq-7f3c9a2e-4b1d-8e5f-unique";

// ---- fixtures ----

fn memory_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_SQL).unwrap();
    conn
}

/// ISO timestamp `offset_ms` from the real current time.
fn iso(offset_ms: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::milliseconds(offset_ms))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

const TEN_DAYS_MS: i64 = 10 * 24 * 60 * 60 * 1000;

fn insert_session(
    conn: &Connection,
    id: &str,
    title: &str,
    started_at: &str,
    ended_at: Option<&str>,
) {
    conn.execute(
        "INSERT INTO sessions (id, title, project, started_at, ended_at) VALUES (?,?,?,?,?)",
        rusqlite::params![id, title, "quiet-zero", started_at, ended_at],
    )
    .unwrap();
}

fn insert_message(
    conn: &Connection,
    uuid: &str,
    session_id: &str,
    role: &str,
    text: Option<&str>,
    timestamp: &str,
) {
    conn.execute(
        "INSERT INTO messages (uuid, session_id, type, role, text, timestamp, visibility, source)
         VALUES (?,?,?,?,?,?,?,?)",
        rusqlite::params![uuid, session_id, role, role, text, timestamp, "visible", "claude"],
    )
    .unwrap();
}

fn insert_tool_call(
    conn: &Connection,
    id: &str,
    message_uuid: &str,
    session_id: &str,
    name: &str,
    input: Value,
) {
    conn.execute(
        "INSERT INTO tool_calls (id, message_uuid, session_id, name, input_json) VALUES (?,?,?,?,?)",
        rusqlite::params![id, message_uuid, session_id, name, input.to_string()],
    )
    .unwrap();
}

/// The base fixture: sid-self holds the obelisk tool call carrying the nonce;
/// sid-history is old history with ordinary needle evidence.
fn invoking_db() -> Connection {
    let conn = memory_conn();
    insert_session(&conn, "sid-self", "Invoking session", &iso(-10_000), None);
    insert_session(
        &conn,
        "sid-history",
        "Historical session",
        &iso(-TEN_DAYS_MS),
        Some(&iso(-TEN_DAYS_MS + 3_600_000)),
    );
    insert_message(
        &conn,
        "msg-self-call",
        "sid-self",
        "assistant",
        None,
        &iso(-9_000),
    );
    insert_message(
        &conn,
        "msg-self",
        "sid-self",
        "assistant",
        Some("self needle reply"),
        &iso(-8_000),
    );
    insert_message(
        &conn,
        "msg-history",
        "sid-history",
        "assistant",
        Some("historical needle reply"),
        &iso(-TEN_DAYS_MS + 1_000),
    );
    insert_tool_call(
        &conn,
        "call-self",
        "msg-self-call",
        "sid-self",
        "Bash",
        json!({ "command": format!("obelisk --search \"needle\" --nonce {NONCE}") }),
    );
    conn
}

fn nonce_command(nonce: &str) -> String {
    format!("obelisk --search \"needle\" --nonce {nonce}")
}

/// A single session whose nonce-bearing tool call sits `offset_ms` from now.
fn nonce_db(session_id: &str, offset_ms: i64) -> Connection {
    let conn = memory_conn();
    insert_session(
        &conn,
        session_id,
        "Nonce session",
        &iso(offset_ms - 1_000),
        None,
    );
    insert_message(
        &conn,
        "msg-call",
        session_id,
        "assistant",
        None,
        &iso(offset_ms),
    );
    insert_tool_call(
        &conn,
        "call-nonce",
        "msg-call",
        session_id,
        "Bash",
        json!({ "command": nonce_command(NONCE) }),
    );
    conn
}

fn resolve(conn: &Connection, candidates: &[(&str, bool)]) -> Option<String> {
    let list: Vec<NonceCandidate> = candidates
        .iter()
        .map(|(value, strict)| NonceCandidate {
            value: (*value).to_string(),
            strict: *strict,
        })
        .collect();
    resolve_invoking_session_id(conn, &list)
}

// ---- resolution (tests/invoking-session.test.mjs) ----

#[test]
fn resolver_finds_invoking_session_via_tool_call_command_line() {
    let conn = invoking_db();
    assert_eq!(
        resolve(&conn, &[(NONCE, false)]).as_deref(),
        Some("sid-self")
    );
}

#[test]
fn resolver_matches_json_escaped_nonce_in_tool_calls_input_json() {
    // Windows-style nonces contain backslashes, which JSON encoding doubles
    // in stored input_json; the raw LIKE spelling alone would miss the record.
    let conn = invoking_db();
    let win_nonce = "C:\\tmp\\obq.win789.mjs";
    insert_tool_call(
        &conn,
        "call-win",
        "msg-self-call",
        "sid-self",
        "Bash",
        json!({ "command": format!("obelisk --query {win_nonce}") }),
    );
    assert_eq!(
        resolve(&conn, &[(win_nonce, false)]).as_deref(),
        Some("sid-self")
    );
}

#[test]
fn resolver_finds_invoking_session_via_message_text() {
    let conn = invoking_db();
    let path_nonce = format!("/tmp/obq.{NONCE}.mjs");
    insert_message(
        &conn,
        "msg-self-nonce",
        "sid-self",
        "user",
        Some(&format!("ran obelisk --query {path_nonce}")),
        &iso(-7_000),
    );
    assert_eq!(
        resolve(&conn, &[(&path_nonce, false)]).as_deref(),
        Some("sid-self")
    );
}

#[test]
fn resolver_returns_none_for_missing_or_unknown_nonces() {
    let conn = invoking_db();
    assert_eq!(resolve(&conn, &[]), None);
    // An empty candidate value is skipped, like undefined/null in the TS spec.
    assert_eq!(resolve(&conn, &[("", false)]), None);
    assert_eq!(
        resolve(&conn, &[("obq-never-appears-anywhere", false)]),
        None
    );
}

#[test]
fn same_nonce_far_apart_in_time_resolves_to_newest_session() {
    // Matches far apart are unrelated history (a replayed command line):
    // newest wins rather than poisoning to null.
    let conn = invoking_db();
    insert_message(
        &conn,
        "msg-history-call",
        "sid-history",
        "assistant",
        None,
        &iso(-600_000),
    );
    insert_tool_call(
        &conn,
        "call-history",
        "msg-history-call",
        "sid-history",
        "Bash",
        json!({ "command": nonce_command(NONCE) }),
    );
    assert_eq!(
        resolve(&conn, &[(NONCE, false)]).as_deref(),
        Some("sid-self")
    );
}

#[test]
fn same_nonce_within_collision_epsilon_resolves_to_none() {
    // Two sessions whose newest matching records land seconds apart are a
    // genuine concurrent collision: honest unknown.
    let conn = invoking_db();
    // sid-self at now-9s vs sid-history at now-4s: 5s apart, within the 10s epsilon.
    insert_message(
        &conn,
        "msg-history-call",
        "sid-history",
        "assistant",
        None,
        &iso(-4_000),
    );
    insert_tool_call(
        &conn,
        "call-history",
        "msg-history-call",
        "sid-history",
        "Bash",
        json!({ "command": nonce_command(NONCE) }),
    );
    assert_eq!(resolve(&conn, &[(NONCE, false)]), None);
}

#[test]
fn quoting_only_session_loses_to_real_execution() {
    // sid-history mentions the nonce in message text two minutes BEFORE
    // sid-self executed it: newest-wins resolves to the executing session.
    let conn = invoking_db();
    insert_message(
        &conn,
        "msg-history-quote",
        "sid-history",
        "user",
        Some(&format!("what does {} do?", nonce_command(NONCE))),
        &iso(-120_000),
    );
    assert_eq!(
        resolve(&conn, &[(NONCE, false)]).as_deref(),
        Some("sid-self")
    );
}

#[test]
fn resolver_bounds_the_scan_to_the_recency_window() {
    // The TS spec pins nowMs past the fixture; the Rust resolver reads the
    // real clock, so the same edge is exercised from the fixture side: a
    // record 20 minutes old is outside the 15-minute window, one 1 minute
    // old is inside.
    let stale = nonce_db("sid-stale", -20 * 60 * 1000);
    assert_eq!(resolve(&stale, &[(NONCE, false)]), None);
    let fresh = nonce_db("sid-fresh", -60_000);
    assert_eq!(
        resolve(&fresh, &[(NONCE, false)]).as_deref(),
        Some("sid-fresh")
    );
}

#[test]
fn resolver_falls_back_to_script_content_when_the_path_misses() {
    // The documented mktemp flow hides the query path behind a shell
    // variable; the transcript then carries the heredoc body verbatim.
    let conn = invoking_db();
    let script =
        "const hits = search('content nonce needle', { limit: 3 });\nreturn hits.length;\n";
    insert_tool_call(
        &conn,
        "call-heredoc",
        "msg-self-call",
        "sid-self",
        "Bash",
        json!({ "command": format!("cat > \"$qfile\" <<'QUERY_EOF'\n{script}QUERY_EOF\nobelisk --query \"$qfile\"") }),
    );
    assert_eq!(
        resolve(
            &conn,
            &[("/tmp/obq.hidden/query.mjs", false), (script.trim(), false)]
        )
        .as_deref(),
        Some("sid-self")
    );
    assert_eq!(
        resolve(&conn, &[("/tmp/obq.hidden/query.mjs", false)]),
        None
    );
}

#[test]
fn resolver_matches_json_escaped_multiline_content_candidate() {
    // Write-style tool input JSON-escapes newlines, so the stored
    // input_json holds the escaped spelling; both spellings must match.
    let conn = invoking_db();
    let script = "const map = overview({ limit: 6 });\nreturn map.current_project;\n";
    insert_tool_call(
        &conn,
        "call-write",
        "msg-self-call",
        "sid-self",
        "Write",
        json!({ "file_path": "/tmp/obq.write/query.mjs", "content": script }),
    );
    assert_eq!(
        resolve(
            &conn,
            &[("/tmp/obq.absent/query.mjs", false), (script.trim(), false)]
        )
        .as_deref(),
        Some("sid-self")
    );
}

#[test]
fn both_legs_matching_one_session_dedupe_to_the_newest_record() {
    // The same session hit by the message-text leg (older) and the
    // tool_calls leg (newer) is one candidate at its newest timestamp, not
    // a self-collision.
    let conn = memory_conn();
    insert_session(&conn, "sid-self", "Both legs", &iso(-70_000), None);
    insert_message(
        &conn,
        "msg-quote",
        "sid-self",
        "user",
        Some(&format!("ran obelisk --query /tmp/obq.{NONCE}.mjs")),
        &iso(-60_000),
    );
    insert_message(
        &conn,
        "msg-call",
        "sid-self",
        "assistant",
        None,
        &iso(-5_000),
    );
    insert_tool_call(
        &conn,
        "call-self",
        "msg-call",
        "sid-self",
        "Bash",
        json!({ "command": nonce_command(NONCE) }),
    );
    assert_eq!(
        resolve(&conn, &[(NONCE, false)]).as_deref(),
        Some("sid-self")
    );
}

#[test]
fn strict_candidate_resolves_when_the_matching_session_invoked_the_cli() {
    // The heredoc body and the `obelisk --query` call land in the same
    // command record, so the invoker satisfies the invocation requirement.
    let conn = invoking_db();
    let script = "const hits = search('strict needle', { limit: 3 });\nreturn hits.length;\n";
    insert_tool_call(
        &conn,
        "call-strict",
        "msg-self-call",
        "sid-self",
        "Bash",
        json!({ "command": format!("cat > \"$qfile\" <<'QUERY_EOF'\n{script}QUERY_EOF\nobelisk --query \"$qfile\"") }),
    );
    assert_eq!(
        resolve(&conn, &[(script.trim(), true)]).as_deref(),
        Some("sid-self")
    );
}

#[test]
fn strict_candidate_rejects_session_that_never_invoked_the_cli() {
    // A stranger who merely wrote the same content has no CLI invocation
    // record: honest null instead of mis-marking their session.
    let conn = invoking_db();
    let script = "const hits = search('strict needle', { limit: 3 });\nreturn hits.length;\n";
    insert_message(
        &conn,
        "msg-quote",
        "sid-history",
        "assistant",
        None,
        &iso(-5_000),
    );
    insert_tool_call(
        &conn,
        "call-quote",
        "msg-quote",
        "sid-history",
        "Write",
        json!({ "file_path": "/tmp/obq.quote/query.mjs", "content": script }),
    );
    assert_eq!(resolve(&conn, &[(script.trim(), true)]), None);
}

#[test]
fn strict_candidate_rejects_content_matched_by_multiple_sessions() {
    // Shared boilerplate: two sessions hold the same script within the
    // window. Newest-wins would guess; strict mode refuses.
    let conn = invoking_db();
    let script = "const map = overview({ limit: 6 });\nreturn map.current_project;\n";
    let heredoc =
        format!("cat > \"$qfile\" <<'QUERY_EOF'\n{script}QUERY_EOF\nobelisk --query \"$qfile\"");
    insert_message(
        &conn,
        "msg-history-call",
        "sid-history",
        "assistant",
        None,
        &iso(-70_000),
    );
    insert_tool_call(
        &conn,
        "call-history",
        "msg-history-call",
        "sid-history",
        "Bash",
        json!({ "command": heredoc }),
    );
    insert_tool_call(
        &conn,
        "call-self-strict",
        "msg-self-call",
        "sid-self",
        "Bash",
        json!({ "command": heredoc }),
    );
    assert_eq!(resolve(&conn, &[(script.trim(), true)]), None);
}

#[test]
fn resolver_tolerates_partially_built_index() {
    // A read-only query can face a DB with only index_state: the nonce
    // cannot resolve — honest null, no panic.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE index_state (jsonl_path TEXT PRIMARY KEY, mtime REAL, lines_processed INTEGER)",
    )
    .unwrap();
    assert_eq!(resolve(&conn, &[(NONCE, false)]), None);
}

#[test]
fn resolver_returns_honest_null_on_unparseable_timestamps() {
    // An unparseable matching timestamp makes ordering unreliable: lean
    // null instead of guessing a newest session.
    let conn = memory_conn();
    insert_session(&conn, "sid-broken", "Broken clock", &iso(-10_000), None);
    insert_message(
        &conn,
        "msg-broken",
        "sid-broken",
        "user",
        Some(&format!("ran obelisk --query /tmp/obq.{NONCE}.mjs")),
        "not-a-timestamp",
    );
    assert_eq!(resolve(&conn, &[(NONCE, false)]), None);
}

// ---- resolve_invoking_session_id_with_wait ----

#[test]
fn with_wait_resolves_immediately_on_a_preseeded_index() {
    let home = tempfile::tempdir().unwrap();
    {
        let conn = crate::db::open_db(home.path()).unwrap();
        insert_session(&conn, "sid-self", "Invoking session", &iso(-5_000), None);
        insert_message(
            &conn,
            "msg-self-call",
            "sid-self",
            "assistant",
            None,
            &iso(-4_000),
        );
        insert_tool_call(
            &conn,
            "call-self",
            "msg-self-call",
            "sid-self",
            "Bash",
            json!({ "command": nonce_command(NONCE) }),
        );
    }
    let registry = std::sync::Arc::new(crate::provider_settings::create_builtin_provider_registry(
        home.path(),
        &Default::default(),
        home.path(),
    ));
    let hit = crate::core::resolve_invoking_session_id_with_wait(
        home.path(),
        &[NonceCandidate {
            value: NONCE.to_string(),
            strict: false,
        }],
        registry,
    );
    assert_eq!(hit.as_deref(), Some("sid-self"));
}

// ---- execute_attune (tests/attune-contention.test.mjs) ----

fn attune_home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    {
        let _conn = crate::db::open_db(home.path()).unwrap();
    }
    let memory = home.path().join("memory.md");
    std::fs::write(&memory, "# Attune memory\n").unwrap();
    (home, memory)
}

fn quoted(path: &std::path::Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}

// The three execute_attune tests below were #[ignore]d while the sandbox
// whitelist sweep deleted the __obelisk_* globals; that is fixed (the keep
// set covers the host surface), so they run again.

#[test]
fn two_sequential_attunes_do_not_collide() {
    let (home, memory) = attune_home();
    let first = execute_attune(
        home.path(),
        home.path(),
        &format!(
            "return remember({{ path: {}, project: 'seq-attune', summary: 'First sequential decision recorded.' }});",
            quoted(&memory)
        ),
    )
    .unwrap();
    let second = execute_attune(
        home.path(),
        home.path(),
        &format!(
            "return remember({{ path: {}, project: 'seq-attune', summary: 'Second sequential decision recorded.' }});",
            quoted(&memory)
        ),
    )
    .unwrap();
    let id_first = first["id"].as_str().unwrap();
    let id_second = second["id"].as_str().unwrap();
    assert_ne!(id_first, id_second, "sequential memories get unique ids");
    assert_eq!(first["project"].as_str(), Some("seq-attune"));

    let conn = crate::db::open_db(home.path()).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE project='seq-attune'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "both mutations persisted");
}

#[test]
fn remember_and_forget_inside_one_attune_script_is_idempotent() {
    let (home, memory) = attune_home();
    let made = execute_attune(
        home.path(),
        home.path(),
        &format!(
            "const m = remember({{ path: {}, project: 'rf-attune', summary: 'Decision later superseded.' }});\nreturn forget({{ id: m.id, reason: 'superseded by rewrite' }});",
            quoted(&memory)
        ),
    )
    .unwrap();
    assert!(
        made["deleted_at"].is_string(),
        "forget ran inside the same script"
    );
    let id = serde_json::to_string(made["id"].as_str().unwrap()).unwrap();

    let again = execute_attune(
        home.path(),
        home.path(),
        &format!("return forget({{ id: {id}, reason: 'second forget stays idempotent' }});"),
    )
    .unwrap();
    assert_eq!(again["already_deleted"].as_bool(), Some(true));
    assert_eq!(
        again["deleted_reason"].as_str(),
        Some("superseded by rewrite")
    );
}

#[test]
fn attune_mutation_waits_out_a_held_write_lock_via_the_retry_budget() {
    // Port of the TS cross-process contention test: the retry layer (not the
    // 250ms connection busy_timeout) owns the waiting. The TS suite needs a
    // child process because its retry backoff blocks the event loop; Rust
    // threads block independently, so the holder runs in-process.
    let (home, memory) = attune_home();
    let db_path = crate::db::db_path(home.path());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA busy_timeout=0; BEGIN IMMEDIATE")
            .unwrap();
        let _ = ready_tx.send(());
        std::thread::sleep(std::time::Duration::from_millis(1500));
        conn.execute_batch("ROLLBACK").unwrap();
    });
    ready_rx.recv().unwrap();

    let result = execute_attune(
        home.path(),
        home.path(),
        &format!(
            "return remember({{ path: {}, project: 'contention-test', summary: 'Decision: attune waits out a bounded lock hold and still writes.' }});",
            quoted(&memory)
        ),
    );
    holder.join().unwrap();
    let remembered = result.unwrap();
    assert!(remembered["id"].as_str().is_some());

    let conn = crate::db::open_db(home.path()).unwrap();
    let summary: String = conn
        .query_row(
            "SELECT summary FROM memories WHERE project='contention-test'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(summary.contains("waits out a bounded lock hold"));
}
