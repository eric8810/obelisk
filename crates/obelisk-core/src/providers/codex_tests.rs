// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Rust port of tests/codex-parse.test.mjs, tests/codex-discover.test.mjs,
//! tests/codex-index.test.mjs, and tests/codex-replay-tool-identity.test.mjs
//! — the binding-independent codex adapter contract. The TS tests are the
//! spec; index-path tests run discover → parse → persist against an
//! in-memory database instead of the CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::codex::{parse, CodexProvider, NAME};
use crate::persist::persist;
use crate::providers::types::{
    DiscoverContext, IndexUnit, MessageRecord, ProviderAdapter, RawLookup, SessionCountMode,
    SessionRecord, StreamItem, ToolCallRecord, ToolResultRecord, TranscriptRecord,
};

const META_ID: &str = "019e8951-3e7d-7343-a3e3-05bff48a317d";
const GUARDIAN_ID: &str = "019ed5c4-8d52-7bc0-91f3-447a15e987d1";
const PLAIN_ID: &str = "019e8951-3e7d-7343-a3e3-05bff48a317d";
const INDEX_ID: &str = "019ed000-0000-7000-8000-000000000001";
const PARENT_ID: &str = "019ed000-0000-7000-8000-000000000101";
const REPLAY_ID: &str = "019ed000-0000-7000-8000-000000000102";

fn merge(base: Value, extra: Value) -> Value {
    let mut base = base;
    if let (Some(obj), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            obj.insert(key.clone(), value.clone());
        }
    }
    base
}

fn codex_meta(extra: Value) -> Value {
    merge(
        json!({
            "id": META_ID,
            "cwd": "/proj",
            "git": {"branch": "main"},
            "cli_version": "1.2",
            "timestamp": "2026-06-10T10:00:00Z",
        }),
        extra,
    )
}

fn guardian_meta() -> Value {
    json!({
        "thread_source": "subagent",
        "source": {"subagent": {"other": "guardian"}},
    })
}

fn write_lines(path: &Path, lines: &[Value]) {
    let body = lines
        .iter()
        .map(|l| serde_json::to_string(l).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, body + "\n").unwrap();
}

fn write_fixture(lines: &[Value]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    write_lines(&path, lines);
    (dir, path)
}

fn drain(items: Vec<StreamItem>) -> (Vec<TranscriptRecord>, Option<String>) {
    let mut records = Vec::new();
    let mut cursor = None;
    for item in items {
        match item {
            StreamItem::Record(record) => records.push(record),
            StreamItem::Cursor(value) => cursor = Some(value),
            StreamItem::Error(error) => panic!("provider error: {error}"),
        }
    }
    (records, cursor)
}

fn messages_of(records: &[TranscriptRecord]) -> Vec<&MessageRecord> {
    records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::Message(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn session_of(records: &[TranscriptRecord]) -> &SessionRecord {
    records
        .iter()
        .find_map(|r| match r {
            TranscriptRecord::Session(s) => Some(s),
            _ => None,
        })
        .unwrap()
}

fn unit_for(key: &Path) -> IndexUnit {
    IndexUnit {
        key: key.to_string_lossy().into_owned(),
        session_id: String::new(),
        ..Default::default()
    }
}

fn discover(
    provider: &CodexProvider,
    cursors: &HashMap<String, String>,
    changed: Option<&[String]>,
) -> Vec<IndexUnit> {
    let mut ctx = DiscoverContext {
        last_cursor: &|key: &str| cursors.get(key).cloned(),
        changed_paths: changed,
        indexed_sessions: None,
        report_incomplete_inventory: None,
    };
    provider.discover(&mut ctx)
}

fn open_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_SQL).unwrap();
    conn
}

fn scalar(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn scalar_param(conn: &rusqlite::Connection, sql: &str, param: &str) -> i64 {
    conn.query_row(sql, [param], |row| row.get(0)).unwrap()
}

fn text_param(conn: &rusqlite::Connection, sql: &str, param: &str) -> String {
    conn.query_row(sql, [param], |row| row.get(0)).unwrap()
}

fn set_file_times(path: &Path, secs: f64) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(secs);
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
    drop(file);
}

// ---- tests/codex-parse.test.mjs ----

#[test]
fn parse_yields_deduped_tool_aware_stream_with_total_session() {
    let (_dir, path) = write_fixture(&[
        json!({"type": "session_meta", "timestamp": "2026-06-10T10:00:00Z", "payload": codex_meta(json!({}))}),
        json!({"type": "event_msg", "timestamp": "2026-06-10T10:00:01Z", "payload": {"type": "user_message", "message": "hello codex"}}),
        json!({"type": "event_msg", "timestamp": "2026-06-10T10:00:02Z", "payload": {"type": "agent_message", "message": "hi there"}}),
        // Duplicate of the agent_message above — must be deduped (dropped).
        json!({"type": "response_item", "timestamp": "2026-06-10T10:00:02Z", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi there"}]}}),
        json!({"type": "response_item", "timestamp": "2026-06-10T10:00:03Z", "payload": {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{\"cmd\":\"ls\"}"}}),
        json!({"type": "response_item", "timestamp": "2026-06-10T10:00:04Z", "payload": {"type": "function_call_output", "call_id": "call_1", "output": "file listing"}}),
        json!({"type": "event_msg", "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 100, "output_tokens": 50}}}}),
        json!({"type": "event_msg", "timestamp": "2026-06-10T10:00:05Z", "payload": {"type": "task_complete", "duration_ms": 1500}}),
    ]);

    let (records, _) = drain(parse(&unit_for(&path), None));

    // Three messages: user, assistant text, assistant tool_use. The duplicate
    // response_item 'hi there' was deduped.
    let messages = messages_of(&records);
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.text.as_deref() == Some("hi there"))
            .count(),
        1,
        "agent_message deduped against response_item"
    );
    assert!(messages.iter().all(|m| m.source == "codex"));

    // token_count patched the last text-assistant message's tokens.
    let text_assistant = messages
        .iter()
        .find(|m| {
            m.role.as_deref() == Some("assistant") && m.content_type.as_deref() == Some("text")
        })
        .unwrap();
    assert_eq!(text_assistant.input_tokens, Some(100));
    assert_eq!(text_assistant.output_tokens, Some(50));

    // Tool call + result.
    let tool_calls: Vec<&ToolCallRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::ToolCall(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, format!("codex:{META_ID}:call_1"));
    assert_eq!(tool_calls[0].name, "shell");
    let tool_results: Vec<&ToolResultRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::ToolResult(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(
        tool_results[0].tool_use_id,
        format!("codex:{META_ID}:call_1")
    );

    // task_complete → turn duration on the text-assistant message.
    let durations: Vec<&TranscriptRecord> = records
        .iter()
        .filter(|r| matches!(r, TranscriptRecord::MessageTurnDuration { .. }))
        .collect();
    assert_eq!(durations.len(), 1);
    assert!(matches!(
        durations[0],
        TranscriptRecord::MessageTurnDuration {
            turn_duration_ms: Some(1500),
            ..
        }
    ));

    // One session record, full-reparse semantics.
    let sessions: Vec<&SessionRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::Session(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].source, "codex");
    assert_eq!(sessions[0].count_mode, SessionCountMode::Total);
    assert_eq!(sessions[0].message_count, 3);
    assert_eq!(sessions[0].git_branch.as_deref(), Some("main"));
}

#[test]
fn parse_retracts_guardian_thread_via_delete_session_and_emits_nothing_else() {
    let (_dir, path) = write_fixture(&[
        json!({"type": "session_meta", "timestamp": "2026-06-10T10:00:00Z", "payload": codex_meta(guardian_meta())}),
        json!({"type": "event_msg", "timestamp": "2026-06-10T10:00:01Z", "payload": {"type": "user_message", "message": "ignored"}}),
    ]);

    let (records, _) = drain(parse(&unit_for(&path), None));

    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0],
        TranscriptRecord::DeleteSession { session_id } if session_id.starts_with("codex:")
    ));
}

#[test]
fn discover_folds_session_index_metadata_into_canonical_session_record() {
    let root = tempfile::tempdir().unwrap();
    let sessions_dir = root
        .path()
        .join("sessions")
        .join("2026")
        .join("06")
        .join("10");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let path = sessions_dir.join(format!("rollout-{META_ID}.jsonl"));
    write_lines(
        &path,
        &[
            json!({"type": "session_meta", "timestamp": "2026-06-10T10:00:00Z", "payload": codex_meta(json!({}))}),
        ],
    );
    let index_path = root.path().join("session_index.jsonl");
    std::fs::write(
        &index_path,
        serde_json::to_string(&json!({
            "id": META_ID, "thread_name": "Indexed title", "updated_at": "2026-06-10T11:00:00Z",
        }))
        .unwrap()
            + "\n",
    )
    .unwrap();
    let provider = CodexProvider::new(root.path().to_path_buf());
    let changed = vec![index_path.to_string_lossy().into_owned()];
    let mut cursors = HashMap::new();
    cursors.insert(
        path.to_string_lossy().into_owned(),
        "9999999999999:1".to_string(),
    );
    let units = discover(&provider, &cursors, Some(&changed));

    assert_eq!(units.len(), 1);
    let (records, _) = drain(parse(&units[0], None));
    let session = session_of(&records);
    assert_eq!(session.title.as_deref(), Some("Indexed title"));
    assert_eq!(session.ended_at.as_deref(), Some("2026-06-10T11:00:00Z"));
}

#[test]
fn discover_watches_and_reads_archived_sessions() {
    let root = tempfile::tempdir().unwrap();
    let archive_dir = root.path().join("archived_sessions");
    std::fs::create_dir_all(&archive_dir).unwrap();
    let path = archive_dir.join(format!("rollout-{META_ID}.jsonl"));
    write_lines(
        &path,
        &[
            json!({"type": "session_meta", "timestamp": "2026-06-10T10:00:00Z", "payload": codex_meta(json!({}))}),
            json!({"type": "event_msg", "timestamp": "2026-06-10T10:00:01Z", "payload": {"type": "user_message", "message": "archived Codex sentinel"}}),
        ],
    );

    let provider = CodexProvider::new(root.path().to_path_buf());
    let mut cursors = HashMap::new();
    cursors.insert(
        path.to_string_lossy().into_owned(),
        "9999999999999:1".to_string(),
    );
    let changed = vec![path.to_string_lossy().into_owned()];
    let units = discover(&provider, &cursors, Some(&changed));

    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, path.to_string_lossy());

    let targets = provider.watch_targets(&root.path().to_string_lossy());
    assert_eq!(targets.len(), 3);
    assert!(matches!(
        targets[0].kind,
        crate::providers::types::WatchTargetKind::Tree
    ));
    assert_eq!(
        targets[0].path,
        root.path().join("sessions").to_string_lossy()
    );
    assert!(matches!(
        targets[1].kind,
        crate::providers::types::WatchTargetKind::Tree
    ));
    assert_eq!(targets[1].path, archive_dir.to_string_lossy());
    assert!(matches!(
        targets[2].kind,
        crate::providers::types::WatchTargetKind::File
    ));
    assert_eq!(
        targets[2].path,
        root.path().join("session_index.jsonl").to_string_lossy()
    );

    let input = RawLookup {
        source: NAME,
        message_uuid: &format!("codex:{META_ID}:000002"),
        session: None,
        agent_id: Some("codex:archive-agent"),
        cursor: None,
        subagent: None,
        workflow_agent: None,
    };
    let raw = provider.raw(&input).expect("raw record found");
    assert!(raw.text.contains("archived Codex sentinel"));
}

// ---- tests/codex-discover.test.mjs (#114 / #121 / #123) ----

fn write_rollout(root: &Path, id: &str, meta_extra: Value) -> PathBuf {
    let dir = root.join("sessions").join("2026").join("06").join("15");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("rollout-2026-06-15T10-00-00-{id}.jsonl"));
    let payload = merge(
        json!({
            "id": id,
            "timestamp": "2026-06-15T10:00:00Z",
            "cwd": "/tmp/cdx",
            "cli_version": "1.0",
        }),
        meta_extra,
    );
    write_lines(
        &path,
        &[
            json!({"type": "session_meta", "timestamp": "2026-06-15T10:00:00Z", "payload": payload}),
            json!({"type": "event_msg", "timestamp": "2026-06-15T10:00:01Z", "payload": {"type": "user_message", "message": "hello"}}),
        ],
    );
    path
}

// `${mtime}:${lines}:${size}:${ctimeMs}:${ino}` — see cursor_signature_differs.
fn cursor_for(path: &Path, size_override: Option<i64>) -> String {
    let (mtime, size, ctime, ino) = crate::parsing::file_signature(path).unwrap();
    format!("{mtime}:2:{}:{ctime}:{ino}", size_override.unwrap_or(size))
}

fn cursor_map(entries: &[(PathBuf, String)]) -> HashMap<String, String> {
    entries
        .iter()
        .map(|(path, cursor)| (path.to_string_lossy().into_owned(), cursor.clone()))
        .collect()
}

#[test]
fn discovery_skips_guardian_and_plain_transcripts_with_matching_cursor_signatures() {
    let root = tempfile::tempdir().unwrap();
    let guardian_path = write_rollout(root.path(), GUARDIAN_ID, guardian_meta());
    let plain_path = write_rollout(root.path(), PLAIN_ID, json!({}));
    let cursors = cursor_map(&[
        (guardian_path.clone(), cursor_for(&guardian_path, None)),
        (plain_path.clone(), cursor_for(&plain_path, None)),
    ]);

    let provider = CodexProvider::new(root.path().to_path_buf());
    assert!(
        discover(&provider, &cursors, None).is_empty(),
        "both cursor-clean files are skipped"
    );
}

#[test]
fn discovery_replans_guardian_transcript_rewritten_at_same_mtime() {
    let root = tempfile::tempdir().unwrap();
    let guardian_path = write_rollout(root.path(), GUARDIAN_ID, guardian_meta());
    // Same mtime, different size: a same-millisecond append must not be skipped.
    let (mtime, size, ctime, ino) = crate::parsing::file_signature(&guardian_path).unwrap();
    let cursors = cursor_map(&[(
        guardian_path.clone(),
        format!("{mtime}:2:{}:{ctime}:{ino}", size + 1),
    )]);

    let provider = CodexProvider::new(root.path().to_path_buf());
    let units = discover(&provider, &cursors, None);
    assert_eq!(units.len(), 1, "a changed signature forces re-planning");
    assert_eq!(
        units[0].session_id, "",
        "guardian units carry no session id"
    );
    assert_eq!(
        units[0]
            .meta
            .as_ref()
            .and_then(|m| m.get("guardian"))
            .and_then(Value::as_bool),
        Some(true),
        "guardian detection runs for re-planned files"
    );
}

#[test]
fn discovery_fails_closed_on_legacy_mtime_only_cursor() {
    let root = tempfile::tempdir().unwrap();
    let guardian_path = write_rollout(root.path(), GUARDIAN_ID, guardian_meta());
    // A two-part cursor cannot prove the file is unchanged, so it re-parses
    // once and upgrades to the full signature — even when mtimes match.
    let mtime = crate::parsing::file_mtime_ms(&guardian_path).unwrap();
    let cursors = cursor_map(&[(guardian_path, format!("{mtime}:2"))]);

    let provider = CodexProvider::new(root.path().to_path_buf());
    let units = discover(&provider, &cursors, None);
    assert_eq!(
        units.len(),
        1,
        "a legacy cursor at the current mtime is re-planned"
    );
    assert_eq!(
        units[0]
            .meta
            .as_ref()
            .and_then(|m| m.get("guardian"))
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn discovery_honors_exact_changed_paths_report_over_matching_cursor() {
    let root = tempfile::tempdir().unwrap();
    let guardian_path = write_rollout(root.path(), GUARDIAN_ID, guardian_meta());
    let plain_path = write_rollout(root.path(), PLAIN_ID, json!({}));
    let cursors = cursor_map(&[
        (guardian_path.clone(), cursor_for(&guardian_path, None)),
        (plain_path.clone(), cursor_for(&plain_path, None)),
    ]);

    // The watcher path: an exact change report must re-plan the file even
    // when its cursor signature still matches.
    let provider = CodexProvider::new(root.path().to_path_buf());
    let changed = vec![guardian_path.to_string_lossy().into_owned()];
    let units = discover(&provider, &cursors, Some(&changed));
    assert_eq!(
        units.iter().map(|u| u.key.as_str()).collect::<Vec<_>>(),
        vec![guardian_path.to_string_lossy().as_ref()],
        "only the reported file is re-planned"
    );
}

// ---- tests/codex-index.test.mjs (#104) ----

fn index_meta_line() -> Value {
    json!({
        "type": "session_meta",
        "timestamp": "2026-06-15T10:00:00Z",
        "payload": {
            "id": INDEX_ID,
            "timestamp": "2026-06-15T10:00:00Z",
            "cwd": "/tmp/cdx",
            "cli_version": "1.0",
        },
    })
}

fn evt(event_type: &str, message: &str, ts: &str) -> Value {
    json!({"type": "event_msg", "timestamp": ts, "payload": {"type": event_type, "message": message}})
}

fn index_fixture(root: &Path, user_text: &str) -> PathBuf {
    let dir = root.join("sessions").join("2026").join("06").join("15");
    std::fs::create_dir_all(&dir).unwrap();
    let jsonl = dir.join(format!("rollout-2026-06-15T10-00-00-{INDEX_ID}.jsonl"));
    write_lines(
        &jsonl,
        &[
            index_meta_line(),
            evt("user_message", user_text, "2026-06-15T10:00:01Z"),
            evt("agent_message", "codex reply", "2026-06-15T10:00:02Z"),
        ],
    );
    jsonl
}

fn index_pass(
    conn: &rusqlite::Connection,
    provider: &CodexProvider,
    cursors: &HashMap<String, String>,
) -> HashMap<String, String> {
    let units = discover(provider, cursors, None);
    let mut new_cursors = HashMap::new();
    for unit in &units {
        let cursor = cursors.get(&unit.key).cloned();
        let items = parse(unit, cursor);
        if let Some(cursor) = persist(conn, unit, items.into_iter()).unwrap() {
            new_cursors.insert(unit.key.clone(), cursor);
        }
    }
    new_cursors
}

#[test]
fn full_build_then_incremental_rebuild_replaces_total_count_without_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let jsonl = index_fixture(root.path(), "codex hello");
    let provider = CodexProvider::new(root.path().to_path_buf());
    let conn = open_db();

    let cursors = index_pass(&conn, &provider, &HashMap::new());
    assert_eq!(
        scalar(&conn, "SELECT COUNT(*) FROM sessions WHERE source='codex'"),
        1,
        "one codex session indexed"
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT message_count FROM sessions WHERE source='codex'"
        ),
        2,
        "two messages counted"
    );
    assert_eq!(
        scalar(&conn, "SELECT COUNT(*) FROM messages WHERE source='codex'"),
        2
    );

    // Append a third message; bump mtime; incremental rebuild (full-reparse).
    let mut body = std::fs::read_to_string(&jsonl).unwrap();
    body.push_str(
        &serde_json::to_string(&evt(
            "user_message",
            "codex followup",
            "2026-06-15T10:01:00Z",
        ))
        .unwrap(),
    );
    body.push('\n');
    std::fs::write(&jsonl, body).unwrap();
    let t = crate::parsing::file_mtime_ms(&jsonl).unwrap() / 1000.0 + 10.0;
    set_file_times(&jsonl, t);

    index_pass(&conn, &provider, &cursors);
    // 'total' replace: 3, not 5 (2+3) and not a stale 2.
    assert_eq!(
        scalar(
            &conn,
            "SELECT message_count FROM sessions WHERE source='codex'"
        ),
        3,
        "message_count replaced with the new total"
    );
    assert_eq!(
        scalar(&conn, "SELECT COUNT(*) FROM messages WHERE source='codex'"),
        3,
        "exactly three messages, upserted (no duplicates)"
    );
    // TS also asserts the appended message is searchable (FTS surface,
    // binding-level here); the row text is asserted instead.
    assert_eq!(
        scalar(
            &conn,
            "SELECT COUNT(*) FROM messages WHERE source='codex' AND text='codex followup'"
        ),
        1,
        "the appended message is indexed"
    );
}

#[test]
fn incremental_rebuild_detects_real_append_forced_to_same_mtime() {
    let root = tempfile::tempdir().unwrap();
    let jsonl = index_fixture(root.path(), "codex hello");
    let provider = CodexProvider::new(root.path().to_path_buf());
    let conn = open_db();
    let cursors = index_pass(&conn, &provider, &HashMap::new());
    assert_eq!(
        scalar(&conn, "SELECT COUNT(*) FROM messages WHERE source='codex'"),
        2
    );

    let orig = crate::parsing::file_mtime_ms(&jsonl).unwrap() / 1000.0;
    let mut body = std::fs::read_to_string(&jsonl).unwrap();
    body.push_str(
        &serde_json::to_string(&evt(
            "user_message",
            "same-mtime followup",
            "2026-06-15T10:01:00Z",
        ))
        .unwrap(),
    );
    body.push('\n');
    std::fs::write(&jsonl, body).unwrap();
    set_file_times(&jsonl, orig); // force the mtime back — same-millisecond append

    index_pass(&conn, &provider, &cursors);
    assert_eq!(
        scalar(&conn, "SELECT COUNT(*) FROM messages WHERE source='codex'"),
        3,
        "the same-mtime append is re-parsed"
    );
}

#[test]
fn incremental_rebuild_detects_same_size_rewrite_forced_to_same_mtime() {
    let root = tempfile::tempdir().unwrap();
    // 'alpha' and 'omega' are equal length, keeping size identical.
    let jsonl = index_fixture(root.path(), "codex alpha value");
    let provider = CodexProvider::new(root.path().to_path_buf());
    let conn = open_db();
    let cursors = index_pass(&conn, &provider, &HashMap::new());

    let orig = crate::parsing::file_mtime_ms(&jsonl).unwrap() / 1000.0;
    write_lines(
        &jsonl,
        &[
            index_meta_line(),
            evt("user_message", "codex omega value", "2026-06-15T10:00:01Z"),
            evt("agent_message", "codex reply", "2026-06-15T10:00:02Z"),
        ],
    );
    set_file_times(&jsonl, orig);

    index_pass(&conn, &provider, &cursors);
    assert_eq!(
        scalar(
            &conn,
            "SELECT COUNT(*) FROM messages WHERE source='codex' AND text LIKE '%alpha value%'"
        ),
        0
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT COUNT(*) FROM messages WHERE source='codex' AND text LIKE '%omega value%'"
        ),
        1,
        "the rewritten content replaces the old text"
    );
}

#[test]
fn incremental_rebuild_detects_rename_replacement_forced_to_same_mtime() {
    let root = tempfile::tempdir().unwrap();
    let jsonl = index_fixture(root.path(), "codex alpha value");
    let provider = CodexProvider::new(root.path().to_path_buf());
    let conn = open_db();
    let cursors = index_pass(&conn, &provider, &HashMap::new());

    let orig = crate::parsing::file_mtime_ms(&jsonl).unwrap() / 1000.0;
    let tmp = jsonl.parent().unwrap().join("replacement.tmp");
    write_lines(
        &tmp,
        &[
            index_meta_line(),
            evt("user_message", "codex omega value", "2026-06-15T10:00:01Z"),
            evt("agent_message", "codex reply", "2026-06-15T10:00:02Z"),
        ],
    );
    set_file_times(&tmp, orig);
    std::fs::remove_file(&jsonl).unwrap();
    std::fs::rename(&tmp, &jsonl).unwrap();

    index_pass(&conn, &provider, &cursors);
    assert_eq!(
        scalar(
            &conn,
            "SELECT COUNT(*) FROM messages WHERE source='codex' AND text LIKE '%alpha value%'"
        ),
        0
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT COUNT(*) FROM messages WHERE source='codex' AND text LIKE '%omega value%'"
        ),
        1,
        "the replacement content replaces the old text"
    );
}

// ---- tests/codex-replay-tool-identity.test.mjs ----

fn write_replay_rollout(path: &Path, meta_payload: Value, source: &str) {
    write_lines(
        path,
        &[
            json!({"timestamp": "2026-06-15T10:00:00Z", "type": "session_meta", "payload": meta_payload}),
            json!({"timestamp": "2026-06-15T10:00:01Z", "type": "response_item", "payload": {"type": "custom_tool_call", "call_id": "call_shared", "name": "exec", "input": source}}),
            json!({"timestamp": "2026-06-15T10:00:02Z", "type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "call_shared", "output": "done"}}),
        ],
    );
}

#[test]
fn replayed_codex_call_cannot_steal_visible_message_tool_association() {
    let dir = tempfile::tempdir().unwrap();
    let parent_path = dir.path().join("parent.jsonl");
    let replay_path = dir.path().join("replay.jsonl");
    write_replay_rollout(
        &parent_path,
        json!({"id": PARENT_ID, "timestamp": "2026-06-15T10:00:00Z", "cwd": "/proj"}),
        "text(\"parent\")",
    );
    write_replay_rollout(
        &replay_path,
        json!({"id": REPLAY_ID, "forked_from_id": PARENT_ID, "timestamp": "2026-06-15T10:00:00Z", "cwd": "/proj"}),
        "text(\"replay\")",
    );

    let conn = open_db();
    for path in [&parent_path, &replay_path] {
        let unit = IndexUnit {
            key: path.to_string_lossy().into_owned(),
            session_id: String::new(),
            meta: Some(json!({"source": "codex"})),
            ..Default::default()
        };
        let items = parse(&unit, None);
        persist(&conn, &unit, items.into_iter()).unwrap();
    }

    let session_id = format!("codex:{PARENT_ID}");
    assert_eq!(
        scalar_param(
            &conn,
            "SELECT COUNT(*) FROM messages WHERE session_id=?1 AND agent_id IS NULL",
            &session_id
        ),
        1,
        "the replay remains outside the visible session timeline"
    );
    assert_eq!(
        scalar_param(
            &conn,
            "SELECT COUNT(*) FROM tool_calls WHERE session_id=?1",
            &session_id
        ),
        2,
        "each rollout owns an independently addressable tool call"
    );
    assert_eq!(
        scalar_param(
            &conn,
            "SELECT COUNT(*) FROM tool_results WHERE session_id=?1",
            &session_id
        ),
        2,
        "each rollout owns an independently addressable tool result"
    );
    // The parent's call input is the un-parseable string kept verbatim.
    assert_eq!(
        text_param(
            &conn,
            "SELECT input_json FROM tool_calls WHERE id=?1",
            &format!("codex:{PARENT_ID}:call_shared")
        ),
        "\"text(\\\"parent\\\")\""
    );
    assert_eq!(
        text_param(
            &conn,
            "SELECT content FROM tool_results WHERE tool_use_id=?1",
            &format!("codex:{PARENT_ID}:call_shared")
        ),
        "done"
    );
    // TODO(session-detail): the TS test also asserts, through
    // assembleSessionDetail, that the visible message's tool_calls list
    // carries only the parent rollout's call and that its result content is
    // 'done' — port when session-detail assembly lands in Rust.
}
