// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Rust port of tests/pi-parse.test.mjs (plus the randomized differential
//! from tests/pi-randomized-differential.test.mjs with its vendored Pi 0.83.0
//! context oracle) — the binding-independent Pi adapter contract.
//!
//! Ported TS test files and what was reduced:
//! - tests/pi-parse.test.mjs: all 47 cases ported. Assertions that go through
//!   assembleSessionDetail/query (fileHistory, failures, search, thread,
//!   summaries with includeInactive, raw visibility filtering) are replaced
//!   with equivalent persist/DB-row assertions and marked
//!   `TODO(session-detail)`.
//! - tests/pi-randomized-differential.test.mjs: ported in full (512 cases,
//!   fixed LCG seed, oracle totals pinned to the TS run).
//! - tests/pi-runtime.test.mjs and tests/app-pi-index.test.mjs: app/CLI-level
//!   integration (spawned processes, HOME env, settings); the root-resolution
//!   behavior they cover is asserted here against `resolve_default_pi_root`,
//!   but the CLI wiring itself is out of scope for obelisk-core.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::pi::{parse, pi_session_id, resolve_default_pi_root, resolve_pi_root, PiProvider, NAME};
use crate::persist::persist;
use crate::providers::types::{
    DiscoverContext, IndexUnit, IndexedSession, InventoryIssue, MessageRecord, MessageVisibility,
    ProviderAdapter, SessionCountMode, SessionRecord, StreamItem, SummaryRecord, ToolCallRecord,
    ToolResultRecord, TranscriptRecord,
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/pi");

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(name)).unwrap()
}

fn write_session(content: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("project").join("session.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    (dir, path)
}

fn write_session_at(root: &Path, relative_path: &str, content: &str) -> PathBuf {
    let path = root.join(relative_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

fn jsonl(records: &[Value]) -> String {
    let mut body = String::new();
    for record in records {
        body.push_str(&serde_json::to_string(record).unwrap());
        body.push('\n');
    }
    body
}

fn jsonl_records(content: &str) -> Vec<Value> {
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// `Date.parse` on the fixed ISO strings used by the tests.
fn date_ms(timestamp: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .timestamp_millis()
}

fn header(overrides: Value) -> Value {
    let mut base = json!({
        "type": "session",
        "version": 3,
        "id": "pi-test-session",
        "timestamp": "2026-08-02T10:00:00.000Z",
        "cwd": "/tmp/pi-test-project",
    });
    if let (Some(base), Some(overrides)) = (base.as_object_mut(), overrides.as_object()) {
        for (key, value) in overrides {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

fn user_entry(id: &str, parent: Option<&str>, text: &str) -> Value {
    user_entry_ts(id, parent, text, "2026-08-02T10:00:01.000Z")
}

fn user_entry_ts(id: &str, parent: Option<&str>, text: &str, ts: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "parentId": parent,
        "timestamp": ts,
        "message": {"role": "user", "content": text, "timestamp": date_ms(ts)},
    })
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

fn drain_err(items: Vec<StreamItem>) -> String {
    items
        .into_iter()
        .find_map(|item| match item {
            StreamItem::Error(error) => Some(error),
            _ => None,
        })
        .expect("expected provider error")
}

fn discover_ctx<'a>(
    last_cursor: &'a dyn Fn(&str) -> Option<String>,
    changed_paths: Option<&'a [String]>,
    indexed_sessions: Option<&'a dyn Fn() -> Vec<IndexedSession>>,
    report: Option<&'a mut dyn FnMut(InventoryIssue)>,
) -> DiscoverContext<'a> {
    DiscoverContext {
        last_cursor,
        changed_paths,
        indexed_sessions,
        report_incomplete_inventory: report,
    }
}

fn discover_all(provider: &PiProvider) -> Vec<IndexUnit> {
    let mut ctx = discover_ctx(&|_key| None, None, None, None);
    provider.discover(&mut ctx)
}

fn parse_only(root: &Path) -> (PiProvider, IndexUnit, Vec<TranscriptRecord>, Option<String>) {
    let provider = PiProvider::new(root.to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);
    let (records, cursor) = drain(parse(&units[0], None));
    (provider, units[0].clone(), records, cursor)
}

fn messages_of(records: &[TranscriptRecord]) -> Vec<&MessageRecord> {
    records
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::Message(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn summaries_of(records: &[TranscriptRecord]) -> Vec<&SummaryRecord> {
    records
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::Summary(summary) => Some(summary),
            _ => None,
        })
        .collect()
}

fn tool_calls_of(records: &[TranscriptRecord]) -> Vec<&ToolCallRecord> {
    records
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn tool_results_of(records: &[TranscriptRecord]) -> Vec<&ToolResultRecord> {
    records
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

fn session_of(records: &[TranscriptRecord]) -> &SessionRecord {
    records
        .iter()
        .find_map(|record| match record {
            TranscriptRecord::Session(session) => Some(session),
            _ => None,
        })
        .unwrap()
}

fn message_by_text<'a>(records: &'a [TranscriptRecord], text: &str) -> &'a MessageRecord {
    messages_of(records)
        .into_iter()
        .find(|message| message.text.as_deref() == Some(text))
        .unwrap()
}

fn visible_message_texts(records: &[TranscriptRecord]) -> Vec<String> {
    messages_of(records)
        .into_iter()
        .filter(|message| message.visibility == MessageVisibility::Visible)
        .filter_map(|message| message.text.clone())
        .collect()
}

fn open_db(units: &[IndexUnit]) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_SQL).unwrap();
    for unit in units {
        let items = parse(unit, None);
        persist(&conn, unit, items.into_iter()).unwrap();
    }
    conn
}

fn raw_lookup<'a>(
    provider: &'a PiProvider,
    message_uuid: &str,
    session: &'a Value,
    cursor: Option<&'a str>,
) -> Option<crate::providers::types::RawRecord> {
    provider.raw(&crate::providers::types::RawLookup {
        source: NAME,
        message_uuid,
        session: Some(session),
        agent_id: None,
        cursor,
        subagent: None,
        workflow_agent: None,
    })
}

// ---------------------------------------------------------------------------
// Root resolution (TS: resolveDefaultPiRoot precedence tests)
// ---------------------------------------------------------------------------

#[test]
fn root_resolution_follows_official_precedence_and_rejects_ambiguous_roots() {
    let home = tempfile::tempdir().unwrap();
    let project_cwd = home.path().join("project");
    std::fs::create_dir_all(&project_cwd).unwrap();
    let project_cwd = project_cwd.to_string_lossy().into_owned();
    let empty_env = |_: &str| -> Option<String> { None };

    let resolved = resolve_default_pi_root(&empty_env, home.path(), Some(&project_cwd));
    assert_eq!(
        resolved.root,
        home.path().join(".pi").join("agent").join("sessions")
    );
    assert!(!resolved.requires_explicit_root);
    assert!(resolved.reason.is_none());

    let agent_dir = home.path().join("custom-agent");
    let absolute_sessions = home.path().join("absolute-sessions");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string(&json!({"sessionDir": absolute_sessions})).unwrap(),
    )
    .unwrap();
    let agent_dir_env = |key: &str| -> Option<String> {
        (key == "PI_CODING_AGENT_DIR").then(|| agent_dir.to_string_lossy().into_owned())
    };
    let resolved = resolve_default_pi_root(&agent_dir_env, home.path(), Some(&project_cwd));
    assert_eq!(resolved.root, absolute_sessions);
    assert!(!resolved.requires_explicit_root);

    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string(&json!({"sessionDir": ".pi/relative-sessions"})).unwrap(),
    )
    .unwrap();
    let resolved = resolve_default_pi_root(&agent_dir_env, home.path(), Some(&project_cwd));
    assert!(resolved.requires_explicit_root);
    assert_eq!(resolved.root, agent_dir.join("sessions"));
    assert!(!resolved
        .root
        .to_string_lossy()
        .contains("relative-sessions"));

    let session_dir_env = |key: &str| -> Option<String> {
        (key == "PI_CODING_AGENT_SESSION_DIR").then(|| "cwd-relative-sessions".to_string())
    };
    let resolved = resolve_default_pi_root(&session_dir_env, home.path(), Some(&project_cwd));
    assert!(resolved.requires_explicit_root);
    assert_eq!(
        resolved.root,
        home.path().join(".pi").join("agent").join("sessions")
    );

    let env_sessions = home.path().join("env-sessions");
    let both_env = |key: &str| -> Option<String> {
        match key {
            "PI_CODING_AGENT_SESSION_DIR" => Some(env_sessions.to_string_lossy().into_owned()),
            "PI_CODING_AGENT_DIR" => Some(agent_dir.to_string_lossy().into_owned()),
            _ => None,
        }
    };
    let resolved = resolve_default_pi_root(&both_env, home.path(), Some(&project_cwd));
    assert_eq!(resolved.root, env_sessions);
    assert!(!resolved.requires_explicit_root);

    // createPiProvider() with an unresolvable env root reports and yields
    // nothing (TS mutates process.env; we inject the same env instead).
    let resolution = resolve_pi_root(None, None, &session_dir_env, home.path());
    let unresolved = PiProvider::from_resolution(resolution, None);
    assert!(unresolved.descriptor().requires_explicit_root);
    assert!(unresolved
        .descriptor()
        .root_resolution_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("PI_CODING_AGENT_SESSION_DIR")));
    assert!(unresolved
        .watch_targets(&unresolved.descriptor().default_root)
        .is_empty());
    let mut reports = Vec::new();
    {
        let mut report = |issue: InventoryIssue| reports.push(issue);
        let mut ctx = discover_ctx(&|_key| None, None, None, Some(&mut report));
        assert!(unresolved.discover(&mut ctx).is_empty());
    }
    assert_eq!(reports.len(), 1);

    let explicitly_selected = resolve_pi_root(
        Some(
            &home
                .path()
                .join(".pi")
                .join("agent")
                .join("sessions")
                .to_string_lossy(),
        ),
        None,
        &session_dir_env,
        home.path(),
    );
    assert!(!explicitly_selected.requires_explicit_root);
}

#[test]
fn project_settings_override_global_session_dir_using_official_launch_cwd() {
    let home = tempfile::tempdir().unwrap();
    let agent_dir = home.path().join("agent");
    let project_cwd = home.path().join("project");
    let global_sessions = home.path().join("global-sessions");
    let project_sessions = home.path().join("project-sessions");
    std::fs::create_dir_all(project_cwd.join(".pi")).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string(&json!({"sessionDir": global_sessions})).unwrap(),
    )
    .unwrap();
    std::fs::write(
        project_cwd.join(".pi").join("settings.json"),
        serde_json::to_string(&json!({"sessionDir": project_sessions})).unwrap(),
    )
    .unwrap();
    let env = |key: &str| -> Option<String> {
        (key == "PI_CODING_AGENT_DIR").then(|| agent_dir.to_string_lossy().into_owned())
    };
    let cwd = project_cwd.to_string_lossy().into_owned();

    let resolved = resolve_default_pi_root(&env, home.path(), Some(&cwd));
    assert_eq!(resolved.root, project_sessions);
    assert!(!resolved.requires_explicit_root);

    std::fs::write(
        project_cwd.join(".pi").join("settings.json"),
        serde_json::to_string(&json!({"sessionDir": ".pi/project-sessions"})).unwrap(),
    )
    .unwrap();
    let resolved = resolve_default_pi_root(&env, home.path(), Some(&cwd));
    assert_eq!(
        resolved.root,
        project_cwd.join(".pi").join("project-sessions")
    );
    assert!(!resolved.requires_explicit_root);
}

#[test]
fn malformed_settings_fall_back_to_remaining_official_scope() {
    let home = tempfile::tempdir().unwrap();
    let agent_dir = home.path().join("agent");
    let project_cwd = home.path().join("project");
    let project_settings_dir = project_cwd.join(".pi");
    let global_sessions = home.path().join("global-sessions");
    let project_sessions = home.path().join("project-sessions");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&project_settings_dir).unwrap();
    let env = |key: &str| -> Option<String> {
        (key == "PI_CODING_AGENT_DIR").then(|| agent_dir.to_string_lossy().into_owned())
    };
    let cwd = project_cwd.to_string_lossy().into_owned();

    std::fs::write(agent_dir.join("settings.json"), "{broken").unwrap();
    let resolved = resolve_default_pi_root(&env, home.path(), Some(&cwd));
    assert_eq!(resolved.root, agent_dir.join("sessions"));
    assert!(!resolved.requires_explicit_root);

    std::fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_string(&json!({"sessionDir": global_sessions})).unwrap(),
    )
    .unwrap();
    std::fs::write(project_settings_dir.join("settings.json"), "{broken").unwrap();
    let resolved = resolve_default_pi_root(&env, home.path(), Some(&cwd));
    assert_eq!(resolved.root, global_sessions);
    assert!(!resolved.requires_explicit_root);

    std::fs::write(agent_dir.join("settings.json"), "{broken").unwrap();
    std::fs::write(
        project_settings_dir.join("settings.json"),
        serde_json::to_string(&json!({"sessionDir": project_sessions})).unwrap(),
    )
    .unwrap();
    let resolved = resolve_default_pi_root(&env, home.path(), Some(&cwd));
    assert_eq!(resolved.root, project_sessions);
    assert!(!resolved.requires_explicit_root);
}

#[test]
fn non_string_session_dir_values_never_select_default_corpus() {
    let home = tempfile::tempdir().unwrap();
    let agent_dir = home.path().join("agent");
    let project_cwd = home.path().join("project");
    std::fs::create_dir_all(project_cwd.join(".pi")).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(project_cwd.join(".pi").join("settings.json"), "{}").unwrap();
    let env = |key: &str| -> Option<String> {
        (key == "PI_CODING_AGENT_DIR").then(|| agent_dir.to_string_lossy().into_owned())
    };
    let cwd = project_cwd.to_string_lossy().into_owned();

    for session_dir in [json!(false), json!(0)] {
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::to_string(&json!({"sessionDir": session_dir})).unwrap(),
        )
        .unwrap();
        let resolved = resolve_default_pi_root(&env, home.path(), Some(&cwd));
        assert!(resolved.requires_explicit_root);
        assert!(resolved
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("sessionDir must be a string")));
    }
}

#[test]
fn discovery_never_certifies_invalid_session_root_as_empty() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("sessions");
    std::fs::write(&root, "not a directory").unwrap();
    let provider = PiProvider::new(root.clone());
    let mut reports = Vec::new();
    {
        let mut report = |issue: InventoryIssue| reports.push(issue);
        let mut ctx = discover_ctx(&|_key| None, None, None, Some(&mut report));
        assert!(provider.discover(&mut ctx).is_empty());
    }
    assert_eq!(reports.len(), 1, "an invalid root is reported incomplete");
}

// ---------------------------------------------------------------------------
// Fixture-driven projection tests (TS: pi-parse tool/real-model fixtures)
// ---------------------------------------------------------------------------

#[test]
fn tool_fixture_projects_canonical_messages_usage_and_file_paths() {
    let (root, path) = write_session(&fixture("tool-session.jsonl"));
    let (provider, unit, records, cursor) = parse_only(root.path());

    let cursor = cursor.unwrap();
    let mut parts = cursor.split(':');
    assert!(parts.next().unwrap().parse::<f64>().is_ok());
    assert_eq!(parts.next().unwrap(), "0");
    assert_eq!(parts.next().unwrap(), "pi-snapshot-v1");

    assert!(matches!(
        &records[0],
        TranscriptRecord::DeleteSession { session_id } if *session_id == unit.session_id
    ));

    let messages = messages_of(&records);
    let projected: Vec<(&str, &str, Option<&str>)> = messages
        .iter()
        .map(|m| {
            (
                m.role.as_deref().unwrap_or(""),
                m.content_type.as_deref().unwrap_or(""),
                m.text.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        projected,
        vec![
            ("user", "text", Some("Read probe.txt and report it")),
            ("assistant", "tool_use", None),
            ("toolResult", "tool_result", Some("real-pi-tool-result\n")),
            (
                "assistant",
                "text",
                Some("The read tool returned real-pi-tool-result.")
            ),
        ]
    );
    assert_eq!(messages[1].input_tokens, Some(17));
    assert_eq!(messages[1].output_tokens, Some(5));

    let calls = tool_calls_of(&records);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, format!("{}:tool", messages[1].uuid));
    assert_eq!(calls[0].name, "read");
    assert_eq!(calls[0].file_path.as_deref(), Some("probe.txt"));
    assert_eq!(
        tool_results_of(&records)[0].file_path.as_deref(),
        Some("probe.txt")
    );

    let session = session_of(&records);
    assert_eq!(session.title.as_deref(), Some("Tool probe"));
    assert_eq!(session.message_count, 4);
    assert_eq!(session.count_mode, SessionCountMode::Total);

    let session_json = json!({"id": session.id, "jsonl_path": path.to_string_lossy()});
    let raw = raw_lookup(&provider, &messages[1].uuid, &session_json, Some(&cursor))
        .expect("raw toolCall block");
    assert!(raw.text.contains(r#""type":"toolCall""#));
    assert!(raw.message_text.is_none());

    // TODO(session-detail): TS also compares assembleSessionDetail(values)
    // against the persisted DB rows; assert the rows directly instead.
    let conn = open_db(&[unit]);
    let (title, count): (Option<String>, i64) = conn
        .query_row("SELECT title,message_count FROM sessions", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(title.as_deref(), Some("Tool probe"));
    assert_eq!(count, 4);
    let texts: Vec<Option<String>> = conn
        .prepare("SELECT text FROM messages ORDER BY timestamp,uuid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        texts,
        vec![
            Some("Read probe.txt and report it".to_string()),
            None,
            Some("real-pi-tool-result\n".to_string()),
            Some("The read tool returned real-pi-tool-result.".to_string()),
        ]
    );
    let (name, file_path): (String, Option<String>) = conn
        .query_row("SELECT name,file_path FROM tool_calls", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(name, "read");
    assert_eq!(file_path.as_deref(), Some("probe.txt"));
    let tool_results: i64 = conn
        .query_row("SELECT COUNT(*) FROM tool_results", [], |row| row.get(0))
        .unwrap();
    assert_eq!(tool_results, 1);
}

#[test]
fn raw_lookup_resolves_exact_block_and_rejects_replacement_provenance() {
    let long_text = "long-block-".repeat(1100);
    let records = vec![
        header(json!({"id": "raw-block-session"})),
        user_entry("raw-user", None, "raw prompt"),
        json!({
            "type": "message",
            "id": "raw-assistant",
            "parentId": "raw-user",
            "timestamp": "2026-08-02T10:00:02.000Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "exact private thought"},
                    {"type": "text", "text": long_text},
                    {"type": "toolCall", "id": "raw-call", "name": "read", "arguments": {"path": "raw.ts"}},
                ],
                "errorMessage": "exact synthetic error",
                "responseModel": "probe",
                "timestamp": date_ms("2026-08-02T10:00:02.000Z"),
            },
        }),
    ];
    let (dir, path) = write_session(&jsonl(&records));
    let (provider, _, values, cursor) = parse_only(dir.path());
    let session = session_of(&values);
    let session_json = json!({"id": session.id, "jsonl_path": path.to_string_lossy()});
    let cursor = cursor.unwrap();
    let assistant: Vec<&MessageRecord> = messages_of(&values)
        .into_iter()
        .filter(|m| m.role.as_deref() == Some("assistant"))
        .collect();
    let by_content_type = |ct: &str| {
        assistant
            .iter()
            .find(|m| m.content_type.as_deref() == Some(ct))
            .unwrap()
    };

    let thinking = by_content_type("thinking");
    assert_eq!(
        raw_lookup(&provider, &thinking.uuid, &session_json, Some(&cursor))
            .unwrap()
            .message_text
            .as_deref(),
        Some("exact private thought")
    );
    let text = by_content_type("text");
    assert_eq!(
        raw_lookup(&provider, &text.uuid, &session_json, Some(&cursor))
            .unwrap()
            .message_text
            .as_deref(),
        Some(long_text.as_str())
    );
    assert_eq!(text.text.as_deref().map(|t| t.chars().count()), Some(10000));
    assert!(raw_lookup(
        &provider,
        &by_content_type("tool_use").uuid,
        &session_json,
        Some(&cursor)
    )
    .unwrap()
    .message_text
    .is_none());
    assert_eq!(
        raw_lookup(
            &provider,
            &by_content_type("error").uuid,
            &session_json,
            Some(&cursor)
        )
        .unwrap()
        .message_text
        .as_deref(),
        Some("exact synthetic error")
    );
    assert!(raw_lookup(
        &provider,
        &format!("{}:entry:000002:message:block:9999", session.id),
        &session_json,
        Some(&cursor)
    )
    .is_none());
    let wrong_path = json!({"id": session.id, "jsonl_path": dir.path().to_string_lossy()});
    assert!(raw_lookup(&provider, &assistant[0].uuid, &wrong_path, Some(&cursor)).is_none());

    let replacement = jsonl(&[
        header(json!({"id": "raw-block-session"})),
        user_entry("replacement-user", None, "replacement lure"),
        user_entry_ts(
            "replacement-second",
            Some("replacement-user"),
            "unrelated same-header replacement",
            "2026-08-02T10:00:02.000Z",
        ),
    ]);
    std::fs::write(&path, replacement).unwrap();
    assert!(raw_lookup(&provider, &assistant[0].uuid, &session_json, Some(&cursor)).is_none());
}

// ---------------------------------------------------------------------------
// Tool-scope chain tests (TS: branch-local structured tools)
// ---------------------------------------------------------------------------

fn tool_id_reuse_records() -> Vec<Value> {
    vec![
        header(json!({"id": "branch-tool-id-reuse"})),
        user_entry("root", None, "shared prompt"),
        json!({
            "type": "message", "id": "active-call", "parentId": "root",
            "timestamp": "2026-08-02T10:00:02.000Z",
            "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "shared-native-id", "name": "read", "arguments": {"path": "active.ts"}}], "model": "probe", "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0}, "timestamp": date_ms("2026-08-02T10:00:02.000Z")},
        }),
        json!({
            "type": "message", "id": "active-result", "parentId": "active-call",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "message": {"role": "toolResult", "toolCallId": "shared-native-id", "toolName": "read", "content": [{"type": "text", "text": "ACTIVE RESULT"}], "isError": false, "timestamp": date_ms("2026-08-02T10:00:03.000Z")},
        }),
        json!({
            "type": "message", "id": "abandoned-call", "parentId": "root",
            "timestamp": "2026-08-02T10:00:04.000Z",
            "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "shared-native-id", "name": "read", "arguments": {"path": "abandoned.ts"}}], "model": "probe", "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0}, "timestamp": date_ms("2026-08-02T10:00:04.000Z")},
        }),
        json!({
            "type": "message", "id": "abandoned-result", "parentId": "abandoned-call",
            "timestamp": "2026-08-02T10:00:05.000Z",
            "message": {"role": "toolResult", "toolCallId": "shared-native-id", "toolName": "read", "content": [{"type": "text", "text": "ABANDONED RESULT"}], "isError": true, "timestamp": date_ms("2026-08-02T10:00:05.000Z")},
        }),
        json!({
            "type": "leaf", "id": "select-active", "parentId": "abandoned-result",
            "targetId": "active-result", "timestamp": "2026-08-02T10:00:06.000Z",
        }),
    ]
}

#[test]
fn structured_tools_remain_branch_local_when_pi_reuses_a_native_tool_id() {
    let (root, _) = write_session(&jsonl(&tool_id_reuse_records()));
    let (_, unit, values, _) = parse_only(root.path());
    let messages = messages_of(&values);
    let active_tool_use = messages
        .iter()
        .find(|m| {
            m.content_type.as_deref() == Some("tool_use")
                && m.visibility == MessageVisibility::Visible
        })
        .unwrap();
    let inactive_tool_use = messages
        .iter()
        .find(|m| {
            m.content_type.as_deref() == Some("tool_use")
                && m.visibility == MessageVisibility::Inactive
        })
        .unwrap();
    let calls = tool_calls_of(&values);
    let results = tool_results_of(&values);
    assert_eq!(calls.len(), 2);
    let active_call = calls
        .iter()
        .find(|c| c.file_path.as_deref() == Some("active.ts"))
        .unwrap();
    let inactive_call = calls
        .iter()
        .find(|c| c.file_path.as_deref() == Some("abandoned.ts"))
        .unwrap();
    assert_eq!(active_call.id, format!("{}:tool", active_tool_use.uuid));
    assert_eq!(active_call.message_uuid, active_tool_use.uuid);
    assert_eq!(inactive_call.id, format!("{}:tool", inactive_tool_use.uuid));
    assert_eq!(inactive_call.message_uuid, inactive_tool_use.uuid);

    let active_result = results
        .iter()
        .find(|r| r.content == "ACTIVE RESULT")
        .unwrap();
    let inactive_result = results
        .iter()
        .find(|r| r.content == "ABANDONED RESULT")
        .unwrap();
    assert_eq!(active_result.tool_use_id, active_call.id);
    assert_eq!(active_result.content, "ACTIVE RESULT");
    assert_eq!(active_result.file_path.as_deref(), Some("active.ts"));
    assert!(!active_result.is_error);
    assert_eq!(inactive_result.tool_use_id, inactive_call.id);
    assert_eq!(inactive_result.content, "ABANDONED RESULT");
    assert_eq!(inactive_result.file_path.as_deref(), Some("abandoned.ts"));
    assert!(inactive_result.is_error);

    // TODO(session-detail): TS additionally asserts query.fileHistory and
    // query.failures with includeInactive; assert persisted rows instead.
    let conn = open_db(&[unit]);
    let counts: (i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM tool_calls), (SELECT COUNT(*) FROM tool_results)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(counts, (2, 2));
    let file_paths: Vec<String> = conn
        .prepare("SELECT file_path FROM tool_calls ORDER BY file_path")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(file_paths, vec!["abandoned.ts", "active.ts"]);
}

#[test]
fn empty_retained_tail_checkpoint_does_not_link_later_result_to_discarded_scope() {
    let records = vec![
        header(json!({"id": "checkpoint-tool-scope"})),
        json!({
            "type": "message", "id": "discarded-call", "parentId": null,
            "timestamp": "2026-08-02T10:00:01.000Z",
            "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "discarded-native-id", "name": "read", "arguments": {"path": "discarded.ts"}}], "model": "probe", "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0}, "timestamp": date_ms("2026-08-02T10:00:01.000Z")},
        }),
        json!({
            "type": "compaction", "id": "empty-checkpoint", "parentId": "discarded-call",
            "timestamp": "2026-08-02T10:00:02.000Z",
            "summary": "No retained tool context.", "firstKeptEntryId": "discarded-call",
            "tokensBefore": 10, "retainedTail": [],
        }),
        json!({
            "type": "message", "id": "unresolved-result", "parentId": "empty-checkpoint",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "message": {"role": "toolResult", "toolCallId": "discarded-native-id", "toolName": "read", "content": [{"type": "text", "text": "UNRESOLVED RESULT"}], "isError": false, "timestamp": date_ms("2026-08-02T10:00:03.000Z")},
        }),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());

    let calls = tool_calls_of(&values);
    assert_eq!(calls.len(), 1);
    let discarded_call = calls[0];
    assert_eq!(
        messages_of(&values)
            .iter()
            .find(|m| m.uuid == discarded_call.message_uuid)
            .unwrap()
            .visibility,
        MessageVisibility::Inactive
    );
    assert!(tool_results_of(&values).is_empty());
    assert_eq!(
        message_by_text(&values, "UNRESOLVED RESULT").visibility,
        MessageVisibility::Visible
    );
}

#[test]
fn legacy_compaction_does_not_link_visible_result_to_inactive_tool_call() {
    let records = vec![
        header(json!({"id": "legacy-tool-scope"})),
        json!({
            "type": "message", "id": "discarded-call", "parentId": null,
            "timestamp": "2026-08-02T10:00:01.000Z",
            "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "discarded-native-id", "name": "read", "arguments": {"path": "/secret/inactive"}}], "model": "probe", "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0}, "timestamp": date_ms("2026-08-02T10:00:01.000Z")},
        }),
        user_entry_ts(
            "kept-user",
            Some("discarded-call"),
            "kept context",
            "2026-08-02T10:00:02.000Z",
        ),
        json!({
            "type": "compaction", "id": "legacy-compaction", "parentId": "kept-user",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "summary": "Discard the tool call.", "firstKeptEntryId": "kept-user", "tokensBefore": 10,
        }),
        json!({
            "type": "message", "id": "standalone-result", "parentId": "legacy-compaction",
            "timestamp": "2026-08-02T10:00:04.000Z",
            "message": {"role": "toolResult", "toolCallId": "discarded-native-id", "toolName": "read", "content": [{"type": "text", "text": "VISIBLE STANDALONE FAILURE"}], "isError": true, "timestamp": date_ms("2026-08-02T10:00:04.000Z")},
        }),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());
    let discarded_call = tool_calls_of(&values)[0];
    assert_eq!(
        messages_of(&values)
            .iter()
            .find(|m| m.uuid == discarded_call.message_uuid)
            .unwrap()
            .visibility,
        MessageVisibility::Inactive
    );
    assert_eq!(
        message_by_text(&values, "VISIBLE STANDALONE FAILURE").visibility,
        MessageVisibility::Visible
    );
    assert!(tool_results_of(&values).is_empty());

    // TODO(session-detail): TS asserts query.failures/fileHistory; assert the
    // persisted visibility columns instead.
    let conn = open_db(&[unit]);
    let (visibility,): (String,) = conn
        .query_row(
            "SELECT visibility FROM messages WHERE uuid = ?1",
            [&discarded_call.message_uuid],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap();
    assert_eq!(visibility, "inactive");
}

#[test]
fn real_model_fixture_preserves_error_image_cache_and_reasoning_semantics() {
    let (root, _) = write_session(&fixture("real-model-session.jsonl"));
    let (_, _, values, _) = parse_only(root.path());
    let messages = messages_of(&values);
    let projected: Vec<(&str, &str, Option<&str>)> = messages
        .iter()
        .map(|m| {
            (
                m.role.as_deref().unwrap_or(""),
                m.content_type.as_deref().unwrap_or(""),
                m.text.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        projected,
        vec![
            ("user", "text", Some("Reply with the probe marker")),
            ("user", "image", Some("[image image/png; base64 chars=8]")),
            ("assistant", "error", Some("404 not found")),
            ("user", "text", Some("Try the responses protocol")),
            ("assistant", "text", Some("REAL_PI_PROBE_OK")),
        ]
    );
    let success = messages.last().unwrap();
    assert_eq!(success.input_tokens, Some(4818));
    assert_eq!(success.output_tokens, Some(20));
    assert!(!messages
        .iter()
        .any(|m| m.content_type.as_deref() == Some("thinking")));
    assert_eq!(session_of(&values).message_count, 5);
}

#[test]
fn durable_leaf_and_null_leaf_control_active_visibility() {
    let source = write_session(&fixture("harness-source.jsonl"));
    let (_, unit, parsed_source, _) = parse_only(source.0.path());
    let source_messages = messages_of(&parsed_source);
    assert_eq!(
        source_messages.len(),
        6,
        "complete retainedTail must not be projected twice"
    );
    assert_eq!(
        visible_message_texts(&parsed_source),
        vec!["Harness first user turn"]
    );
    assert_eq!(session_of(&parsed_source).message_count, 1);

    // TODO(session-detail): TS compares assembleSessionDetail(...).messages;
    // assert the persisted row instead.
    let conn = open_db(&[unit]);
    let count: i64 = conn
        .query_row("SELECT message_count FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let null_leaf = write_session(&fixture("harness-null-leaf.jsonl"));
    let (_, _, parsed_null, _) = parse_only(null_leaf.0.path());
    assert_eq!(
        messages_of(&parsed_null)[0].visibility,
        MessageVisibility::Inactive
    );
    assert_eq!(session_of(&parsed_null).message_count, 0);
    assert_eq!(
        session_of(&parsed_null).title.as_deref(),
        Some("This physical message is no longer active")
    );
}

#[test]
fn empty_latest_session_name_clears_it_and_falls_back_to_physical_first_user() {
    let records = vec![
        header(json!({"id": "physical-session-title"})),
        json!({
            "type": "message", "id": "physical-user", "parentId": null,
            "timestamp": "2026-08-02T10:00:01.000Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                    {"type": "text", "text": "physical"},
                    {"type": "text", "text": "fallback"},
                ],
                "timestamp": date_ms("2026-08-02T10:00:01.000Z"),
            },
        }),
        json!({"type": "session_info", "id": "named", "parentId": "physical-user", "timestamp": "2026-08-02T10:00:02.000Z", "name": "  Named session  "}),
        json!({"type": "session_info", "id": "blank-name", "parentId": "named", "timestamp": "2026-08-02T10:00:03.000Z", "name": "   "}),
        json!({"type": "leaf", "id": "null-leaf", "parentId": "blank-name", "targetId": null, "timestamp": "2026-08-02T10:00:04.000Z"}),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());

    assert_eq!(
        session_of(&values).title.as_deref(),
        Some("physical fallback")
    );
    assert_eq!(
        message_by_text(&values, "physical").visibility,
        MessageVisibility::Inactive
    );
}

#[test]
fn retained_tail_replaces_complete_ancestors_when_compaction_is_active() {
    let mut records = jsonl_records(&fixture("harness-source.jsonl"));
    records.retain(|record| record.get("type").and_then(Value::as_str) != Some("leaf"));
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());
    let messages = messages_of(&values);
    let visible = visible_message_texts(&values);
    assert_eq!(
        messages.len(),
        8,
        "physical ancestors and active retained context are both preserved"
    );
    assert_eq!(
        visible,
        vec![
            "Harness retained user turn",
            "Harness retained assistant turn",
            "Harness post-compaction user turn",
            "Harness post-compaction assistant turn",
        ]
    );
    assert_eq!(
        message_by_text(&values, "Harness first user turn").visibility,
        MessageVisibility::Inactive
    );
    let retained_physical = messages
        .iter()
        .find(|m| {
            m.text.as_deref() == Some("Harness retained user turn") && !m.uuid.contains(":tail:")
        })
        .unwrap();
    assert_eq!(retained_physical.visibility, MessageVisibility::Inactive);
    assert_eq!(
        summaries_of(&values)[0].visibility,
        Some(MessageVisibility::Visible)
    );
    let totals = messages.iter().fold((0i64, 0i64), |(input, output), m| {
        (
            input + m.input_tokens.unwrap_or(0),
            output + m.output_tokens.unwrap_or(0),
        )
    });
    assert_eq!(
        totals,
        (369, 69),
        "retained context copies must not duplicate model usage"
    );

    // TODO(session-detail): TS asserts query.search/thread visibility
    // filtering; assert the persisted visible rows instead.
    let conn = open_db(&[unit]);
    let db_visible: Vec<Option<String>> = conn
        .prepare("SELECT text FROM messages WHERE visibility='visible' ORDER BY timestamp,uuid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        db_visible,
        vec![
            Some("Harness retained user turn".to_string()),
            Some("Harness retained assistant turn".to_string()),
            Some("Harness post-compaction user turn".to_string()),
            Some("Harness post-compaction assistant turn".to_string()),
        ]
    );
}

#[test]
fn legacy_compaction_exposes_only_first_kept_ancestors_and_later_descendants() {
    let records = vec![
        header(json!({"id": "first-kept-context"})),
        user_entry("old-root", None, "old root"),
        user_entry_ts(
            "old-near",
            Some("old-root"),
            "old near",
            "2026-08-02T10:00:02.000Z",
        ),
        user_entry_ts(
            "kept-start",
            Some("old-near"),
            "kept start",
            "2026-08-02T10:00:03.000Z",
        ),
        user_entry_ts(
            "kept-end",
            Some("kept-start"),
            "kept end",
            "2026-08-02T10:00:04.000Z",
        ),
        json!({
            "type": "compaction", "id": "legacy-compaction", "parentId": "kept-end",
            "timestamp": "2026-08-02T10:00:05.000Z",
            "summary": "Earlier context summary", "firstKeptEntryId": "kept-start", "tokensBefore": 100,
        }),
        user_entry_ts(
            "post-compaction",
            Some("legacy-compaction"),
            "post compaction",
            "2026-08-02T10:00:06.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(
        visible_message_texts(&values),
        vec!["kept start", "kept end", "post compaction"]
    );
    let inactive: Vec<Option<&str>> = messages_of(&values)
        .iter()
        .filter(|m| m.visibility == MessageVisibility::Inactive)
        .map(|m| m.text.as_deref())
        .collect();
    assert_eq!(inactive, vec![Some("old root"), Some("old near")]);
    assert_eq!(
        summaries_of(&values)[0].visibility,
        Some(MessageVisibility::Visible)
    );
}

#[test]
fn later_legacy_compaction_can_retain_context_across_earlier_compaction() {
    let records = vec![
        header(json!({"id": "nested-compaction-context"})),
        user_entry("root", None, "root"),
        user_entry_ts(
            "near-first",
            Some("root"),
            "near first",
            "2026-08-02T10:00:02.000Z",
        ),
        json!({
            "type": "compaction", "id": "first-compaction", "parentId": "near-first",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "summary": "First summary", "firstKeptEntryId": "near-first", "tokensBefore": 50,
        }),
        user_entry_ts(
            "after-first",
            Some("first-compaction"),
            "after first",
            "2026-08-02T10:00:04.000Z",
        ),
        json!({
            "type": "compaction", "id": "second-compaction", "parentId": "after-first",
            "timestamp": "2026-08-02T10:00:05.000Z",
            "summary": "Second summary", "firstKeptEntryId": "root", "tokensBefore": 100,
        }),
        user_entry_ts(
            "after-second",
            Some("second-compaction"),
            "after second",
            "2026-08-02T10:00:06.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(
        visible_message_texts(&values),
        vec!["root", "near first", "after first", "after second"]
    );
    let visible_summaries: Vec<&str> = summaries_of(&values)
        .iter()
        .filter(|s| s.visibility == Some(MessageVisibility::Visible))
        .map(|s| s.content.as_str())
        .collect();
    assert_eq!(visible_summaries, vec!["First summary", "Second summary"]);
}

#[test]
fn retained_tail_checkpoint_bounds_a_later_legacy_compaction() {
    let records = vec![
        header(json!({"id": "mixed-compaction-context"})),
        user_entry("root", None, "discarded root"),
        json!({
            "type": "compaction", "id": "checkpoint", "parentId": "root",
            "timestamp": "2026-08-02T10:00:02.000Z",
            "summary": "Checkpoint summary", "firstKeptEntryId": "root", "tokensBefore": 50,
            "retainedTail": [{"role": "user", "content": "checkpoint tail", "timestamp": date_ms("2026-08-02T10:00:02.000Z")}],
        }),
        user_entry_ts(
            "after-checkpoint",
            Some("checkpoint"),
            "discarded after checkpoint",
            "2026-08-02T10:00:03.000Z",
        ),
        json!({
            "type": "compaction", "id": "later-legacy", "parentId": "after-checkpoint",
            "timestamp": "2026-08-02T10:00:04.000Z",
            "summary": "Later legacy summary", "firstKeptEntryId": "root", "tokensBefore": 100,
        }),
        user_entry_ts(
            "head",
            Some("later-legacy"),
            "active head",
            "2026-08-02T10:00:05.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(visible_message_texts(&values), vec!["active head"]);
    let visible_summaries: Vec<&str> = summaries_of(&values)
        .iter()
        .filter(|s| s.visibility == Some(MessageVisibility::Visible))
        .map(|s| s.content.as_str())
        .collect();
    assert_eq!(visible_summaries, vec!["Later legacy summary"]);
}

#[test]
fn legacy_checkpoint_fork_accepts_truncated_parent_before_first_kept() {
    let records = vec![
        header(json!({"id": "legacy-checkpoint-fork"})),
        user_entry("kept-start", Some("omitted-parent"), "kept start"),
        user_entry_ts(
            "kept-end",
            Some("kept-start"),
            "kept end",
            "2026-08-02T10:00:02.000Z",
        ),
        json!({
            "type": "compaction", "id": "legacy-compaction", "parentId": "kept-end",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "summary": "Omitted source context summary", "firstKeptEntryId": "kept-start", "tokensBefore": 100,
        }),
        user_entry_ts(
            "post-compaction",
            Some("legacy-compaction"),
            "post compaction",
            "2026-08-02T10:00:04.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(
        visible_message_texts(&values),
        vec!["kept start", "kept end", "post compaction"]
    );
    assert_eq!(
        summaries_of(&values)[0].visibility,
        Some(MessageVisibility::Visible)
    );
}

#[test]
fn checkpoint_fork_materializes_retained_tail_once_and_reconnects_parent_identity() {
    let (root, path) = write_session(&fixture("harness-checkpoint-fork.jsonl"));
    let (provider, _, values, cursor) = parse_only(root.path());
    let messages = messages_of(&values);
    let projected: Vec<(&str, Option<&str>)> = messages
        .iter()
        .map(|m| (m.content_type.as_deref().unwrap_or(""), m.text.as_deref()))
        .collect();
    assert_eq!(
        projected,
        vec![
            ("text", Some("Harness retained user turn")),
            ("thinking", Some("retained reasoning")),
            ("text", Some("Harness retained assistant turn")),
            ("text", Some("Harness post-compaction user turn")),
            ("text", Some("Harness post-compaction assistant turn")),
        ]
    );
    assert_eq!(messages[1].input_tokens, None);
    assert_eq!(messages[2].input_tokens, None);
    assert_eq!(messages[2].output_tokens, None);
    assert_eq!(messages[4].input_tokens, Some(123));
    assert_eq!(messages[4].output_tokens, Some(23));
    assert_eq!(messages[3].parent_uuid, Some(messages[2].uuid.clone()));
    assert_eq!(summaries_of(&values).len(), 1);

    let session = session_of(&values);
    let session_json = json!({"id": session.id, "jsonl_path": path.to_string_lossy()});
    let cursor = cursor.unwrap();
    let raw_tail_thinking =
        raw_lookup(&provider, &messages[1].uuid, &session_json, Some(&cursor)).unwrap();
    assert!(!raw_tail_thinking.text.contains("retainedTail"));
    let parsed: Value = serde_json::from_str(&raw_tail_thinking.text).unwrap();
    assert_eq!(
        parsed.get("role").and_then(Value::as_str),
        Some("assistant")
    );
    assert_eq!(
        raw_tail_thinking.message_text.as_deref(),
        Some("retained reasoning")
    );
    assert_eq!(
        raw_lookup(&provider, &messages[2].uuid, &session_json, Some(&cursor))
            .unwrap()
            .message_text
            .as_deref(),
        Some("Harness retained assistant turn")
    );
}

#[test]
fn retained_tail_raw_evidence_never_exposes_hidden_sibling_messages() {
    let retained_tail = json!([
        {"role": "user", "content": "visible retained evidence", "timestamp": date_ms("2026-08-02T10:00:02.000Z")},
        {"role": "custom", "content": "HIDDEN RETAINED SIBLING", "display": false, "timestamp": date_ms("2026-08-02T10:00:03.000Z")},
    ]);
    let records = vec![
        header(json!({"id": "retained-raw-visibility"})),
        user_entry("root", None, "discarded root"),
        json!({
            "type": "compaction", "id": "checkpoint", "parentId": "root",
            "timestamp": "2026-08-02T10:00:02.000Z",
            "summary": "Checkpoint summary", "firstKeptEntryId": "root", "tokensBefore": 50,
            "retainedTail": retained_tail,
        }),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (provider, unit, values, cursor) = parse_only(root.path());
    let visible = messages_of(&values)
        .into_iter()
        .find(|m| {
            m.visibility == MessageVisibility::Visible
                && m.text.as_deref() == Some("visible retained evidence")
        })
        .unwrap();
    let hidden = messages_of(&values)
        .into_iter()
        .find(|m| m.visibility == MessageVisibility::Hidden)
        .unwrap();

    let session = session_of(&values);
    let session_json = json!({"id": session.id, "jsonl_path": root.path().join("project").join("session.jsonl").to_string_lossy()});
    let raw = raw_lookup(&provider, &visible.uuid, &session_json, cursor.as_deref()).unwrap();
    assert_eq!(
        raw.text,
        serde_json::to_string(&retained_tail[0]).unwrap(),
        "raw evidence is the exact retained message JSON"
    );
    assert!(!raw.text.contains("HIDDEN RETAINED SIBLING"));

    // TODO(query): TS asserts query.raw(hidden.uuid) === null — the query
    // layer filters hidden messages before the provider lookup. Assert the
    // persisted hidden visibility instead.
    let conn = open_db(&[unit]);
    let (visibility,): (String,) = conn
        .query_row(
            "SELECT visibility FROM messages WHERE uuid = ?1",
            [&hidden.uuid],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap();
    assert_eq!(visibility, "hidden");
}

#[test]
fn retained_tail_beginning_with_tool_result_remains_standalone_active_evidence() {
    let records = vec![
        header(json!({"id": "retained-tool-result"})),
        json!({
            "type": "message", "id": "physical-call", "parentId": null,
            "timestamp": "2026-08-02T10:00:01.000Z",
            "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "split-call", "name": "read", "arguments": {"path": "probe.txt"}}], "usage": {"input": 5, "output": 2}, "timestamp": 1785664801000i64},
        }),
        json!({
            "type": "compaction", "id": "checkpoint", "parentId": "physical-call",
            "timestamp": "2026-08-02T10:00:02.000Z",
            "summary": "The preceding tool turn was split.", "firstKeptEntryId": "physical-call",
            "tokensBefore": 100,
            "retainedTail": [{"role": "toolResult", "toolCallId": "split-call", "toolName": "read", "content": [{"type": "text", "text": "RETAINED RESULT"}], "isError": false, "timestamp": 1785664802000i64}],
        }),
        user_entry_ts(
            "after-checkpoint",
            Some("checkpoint"),
            "after",
            "2026-08-02T10:00:03.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());
    assert_eq!(
        visible_message_texts(&values),
        vec!["RETAINED RESULT", "after"]
    );
    assert!(tool_results_of(&values).is_empty());

    // TODO(session-detail): TS compares assembleSessionDetail ordering; the
    // persisted visible order carries the same contract.
    let conn = open_db(&[unit]);
    let visible: Vec<Option<String>> = conn
        .prepare("SELECT text FROM messages WHERE visibility='visible' ORDER BY timestamp,uuid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        visible,
        vec![
            Some("RETAINED RESULT".to_string()),
            Some("after".to_string())
        ]
    );
}

#[test]
fn full_tree_marks_abandoned_messages_inactive_and_indexes_summaries_and_bash() {
    let records = vec![
        header(json!({})),
        user_entry("root-user", None, "shared root"),
        user_entry_ts(
            "abandoned-user",
            Some("root-user"),
            "abandoned branch",
            "2026-08-02T10:00:02.000Z",
        ),
        json!({
            "type": "message", "id": "abandoned-assistant", "parentId": "abandoned-user",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "discarded answer"}], "model": "probe", "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0}, "timestamp": date_ms("2026-08-02T10:00:03.000Z")},
        }),
        json!({
            "type": "custom_message", "id": "inactive-custom", "parentId": "abandoned-assistant",
            "timestamp": "2026-08-02T10:00:03.500Z", "customType": "probe",
            "content": "superseded extension context", "display": true,
        }),
        json!({
            "type": "branch_summary", "id": "branch-summary", "parentId": "root-user",
            "timestamp": "2026-08-02T10:00:04.000Z", "fromId": "abandoned-assistant",
            "summary": "The abandoned branch tried a discarded answer.",
        }),
        json!({
            "type": "custom_message", "id": "hidden-custom", "parentId": "branch-summary",
            "timestamp": "2026-08-02T10:00:05.000Z", "customType": "probe",
            "content": "hidden extension context", "display": false,
        }),
        json!({
            "type": "custom_message", "id": "visible-custom", "parentId": "hidden-custom",
            "timestamp": "2026-08-02T10:00:06.000Z", "customType": "probe",
            "content": "visible extension context", "display": true,
        }),
        json!({
            "type": "message", "id": "bash", "parentId": "visible-custom",
            "timestamp": "2026-08-02T10:00:07.000Z",
            "message": {
                "role": "bashExecution", "command": "printf probe",
                "output": "x".repeat(12_000), "exitCode": 0, "cancelled": false,
                "truncated": true, "fullOutputPath": "/tmp/must-not-be-read",
                "excludeFromContext": true, "timestamp": date_ms("2026-08-02T10:00:07.000Z"),
            },
        }),
        json!({
            "type": "compaction", "id": "compaction", "parentId": "bash",
            "timestamp": "2026-08-02T10:00:08.000Z",
            "summary": "Earlier active work.", "firstKeptEntryId": "root-user", "tokensBefore": 42,
        }),
        user_entry_ts(
            "active-user",
            Some("compaction"),
            "active request",
            "2026-08-02T10:00:09.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());

    assert_eq!(
        message_by_text(&values, "abandoned branch").visibility,
        MessageVisibility::Inactive
    );
    assert!(!message_by_text(&values, "abandoned branch").is_meta);
    assert_eq!(
        message_by_text(&values, "superseded extension context").visibility,
        MessageVisibility::Inactive
    );
    assert_eq!(
        message_by_text(&values, "hidden extension context").visibility,
        MessageVisibility::Hidden
    );
    assert_eq!(
        message_by_text(&values, "visible extension context").visibility,
        MessageVisibility::Visible
    );
    let bash = messages_of(&values)
        .into_iter()
        .find(|m| m.role.as_deref() == Some("bashExecution"))
        .unwrap();
    assert_eq!(bash.r#type, "user");
    assert_eq!(bash.content_type.as_deref(), Some("bash_execution"));
    assert!(!bash.is_meta);
    let bash_text = bash.text.as_deref().unwrap();
    assert_eq!(bash_text.chars().count(), 10_000);
    assert!(bash_text.ends_with("[Output truncated. Full output: /tmp/must-not-be-read]"));
    assert!(tool_calls_of(&values).is_empty());
    assert_eq!(
        summaries_of(&values)
            .iter()
            .map(|s| s.source.as_str())
            .collect::<Vec<_>>(),
        vec!["pi:branch_summary", "pi:compaction"]
    );

    // TODO(session-detail): TS asserts query.search/thread visibility
    // filtering; assert persisted visibility columns instead.
    let conn = open_db(&[unit]);
    let visibility_by_text: Vec<(Option<String>, String)> = conn
        .prepare("SELECT text,visibility FROM messages ORDER BY uuid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let lookup = |text: &str| -> &str {
        visibility_by_text
            .iter()
            .find(|(t, _)| t.as_deref() == Some(text))
            .map(|(_, v)| v.as_str())
            .unwrap()
    };
    assert_eq!(lookup("abandoned branch"), "inactive");
    assert_eq!(lookup("superseded extension context"), "inactive");
    assert_eq!(lookup("hidden extension context"), "hidden");
    assert_eq!(lookup("visible extension context"), "visible");
    assert_eq!(lookup("active request"), "visible");
}

#[test]
fn inactive_summaries_remain_available_for_accounting() {
    let records = vec![
        header(json!({"id": "summary-visibility"})),
        user_entry("root", None, "shared root"),
        user_entry_ts(
            "abandoned",
            Some("root"),
            "abandoned prompt",
            "2026-08-02T10:00:02.000Z",
        ),
        json!({
            "type": "branch_summary", "id": "abandoned-summary", "parentId": "abandoned",
            "timestamp": "2026-08-02T10:00:03.000Z", "fromId": "abandoned",
            "summary": "ABANDONED SUMMARY SECRET",
            "usage": {"input": 9, "output": 2, "cacheRead": 3, "cacheWrite": 4},
        }),
        user_entry_ts(
            "active",
            Some("root"),
            "active prompt",
            "2026-08-02T10:00:04.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());
    let summary = summaries_of(&values)[0];
    assert_eq!(summary.visibility, Some(MessageVisibility::Inactive));
    assert_eq!(summary.input_tokens, Some(16));

    // TODO(session-detail): TS asserts assembleSessionDetail/query summaries
    // filtering; assert the persisted row instead.
    let conn = open_db(&[unit]);
    let (content, visibility, input, output): (String, String, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT content,visibility,input_tokens,output_tokens FROM summaries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(content, "ABANDONED SUMMARY SECRET");
    assert_eq!(visibility, "inactive");
    assert_eq!(input, Some(16));
    assert_eq!(output, Some(2));
}

#[test]
fn lowercase_read_edit_write_arguments_path_populates_call_and_result_paths() {
    let mut records = vec![header(json!({}))];
    let mut parent: Option<String> = None;
    for (index, name) in ["read", "edit", "write"].iter().enumerate() {
        let call_entry = format!("call-entry-{index}");
        let call_id = format!("call-{index}");
        records.push(json!({
            "type": "message", "id": call_entry, "parentId": parent,
            "timestamp": format!("2026-08-02T10:00:0{}.000Z", index + 1),
            "message": {"role": "assistant", "content": [{"type": "toolCall", "id": call_id, "name": name, "arguments": {"path": format!("src/{name}.ts")}}], "model": "probe", "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0}, "timestamp": 1785664800000i64 + index as i64},
        }));
        records.push(json!({
            "type": "message", "id": format!("result-entry-{index}"), "parentId": call_entry,
            "timestamp": format!("2026-08-02T10:00:1{}.000Z", index + 1),
            "message": {"role": "toolResult", "toolCallId": call_id, "toolName": name, "content": [{"type": "text", "text": format!("{name} result")}], "isError": false, "timestamp": 1785664801000i64 + index as i64},
        }));
        parent = Some(format!("result-entry-{index}"));
    }
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(
        tool_calls_of(&values)
            .iter()
            .map(|call| call.file_path.clone())
            .collect::<Vec<_>>(),
        vec![
            Some("src/read.ts".to_string()),
            Some("src/edit.ts".to_string()),
            Some("src/write.ts".to_string()),
        ]
    );
    assert_eq!(
        tool_results_of(&values)
            .iter()
            .map(|result| result.file_path.clone())
            .collect::<Vec<_>>(),
        vec![
            Some("src/read.ts".to_string()),
            Some("src/edit.ts".to_string()),
            Some("src/write.ts".to_string()),
        ]
    );
}

#[test]
fn tool_result_nested_model_usage_is_retained_on_its_canonical_message() {
    let records = vec![
        header(json!({})),
        json!({
            "type": "message", "id": "result", "parentId": null,
            "timestamp": "2026-08-02T10:00:01.000Z",
            "message": {"role": "toolResult", "toolCallId": "nested-call", "toolName": "agent", "content": [{"type": "text", "text": "nested result"}], "usage": {"input": 11, "output": 7, "cacheRead": 3, "cacheWrite": 2}, "isError": false, "timestamp": 1785664801000i64},
        }),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());
    let result = messages_of(&values)[0];
    assert_eq!(result.input_tokens, Some(16));
    assert_eq!(result.output_tokens, Some(7));
}

#[test]
fn compaction_and_branch_summary_usage_survives_canonical_persistence() {
    let records = vec![
        header(json!({"id": "summary-usage"})),
        user_entry("user", None, "summarize this"),
        json!({
            "type": "branch_summary", "id": "branch-summary", "parentId": "user",
            "timestamp": "2026-08-02T10:00:02.000Z", "fromId": "user",
            "summary": "Branch summary",
            "usage": {"input": 10, "output": 4, "cacheRead": 2, "cacheWrite": 3},
        }),
        json!({
            "type": "compaction", "id": "compaction", "parentId": "branch-summary",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "summary": "Compaction summary", "firstKeptEntryId": "user", "tokensBefore": 100,
            "usage": {"input": 20, "output": 5, "cacheRead": 6, "cacheWrite": 7},
        }),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());
    let summary_usage: Vec<(&str, Option<i64>, Option<i64>)> = summaries_of(&values)
        .iter()
        .map(|s| (s.source.as_str(), s.input_tokens, s.output_tokens))
        .collect();
    assert_eq!(
        summary_usage,
        vec![
            ("pi:branch_summary", Some(15), Some(4)),
            ("pi:compaction", Some(33), Some(5)),
        ]
    );

    let conn = open_db(&[unit]);
    let rows: Vec<(Option<i64>, Option<i64>)> = conn
        .prepare("SELECT input_tokens,output_tokens FROM summaries ORDER BY timestamp")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec![(Some(15), Some(4)), (Some(33), Some(5))]);
}

#[test]
fn retained_tail_summaries_keep_distinct_canonical_identities() {
    let records = vec![
        header(json!({"id": "retained-summary-identities"})),
        user_entry("root", None, "summarize retained context"),
        json!({
            "type": "compaction", "id": "checkpoint", "parentId": "root",
            "timestamp": "2026-08-02T10:00:03.000Z",
            "summary": "Outer compaction", "firstKeptEntryId": "root", "tokensBefore": 100,
            "retainedTail": [
                {"role": "branchSummary", "summary": "First retained summary", "timestamp": date_ms("2026-08-02T10:00:01.000Z")},
                {"role": "branchSummary", "summary": "Second retained summary", "timestamp": date_ms("2026-08-02T10:00:02.000Z")},
            ],
        }),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());
    let retained: Vec<&SummaryRecord> = summaries_of(&values)
        .into_iter()
        .filter(|s| s.source == "pi:branch_summary")
        .collect();
    assert_eq!(
        retained
            .iter()
            .map(|s| s.content.as_str())
            .collect::<Vec<_>>(),
        vec!["First retained summary", "Second retained summary"]
    );
    let unique_ids: std::collections::HashSet<&str> =
        retained.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(unique_ids.len(), 2);

    let conn = open_db(&[unit]);
    let contents: Vec<String> = conn
        .prepare(
            "SELECT content FROM summaries WHERE source='pi:branch_summary' ORDER BY timestamp",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        contents,
        vec![
            "First retained summary".to_string(),
            "Second retained summary".to_string()
        ]
    );
}

#[test]
fn malformed_lines_follow_pi_while_structural_errors_fail_atomically() {
    // No trailing newline: the final unterminated JSON line still parses.
    let complete = jsonl(&[header(json!({})), user_entry("user", None, "no newline")])
        .trim_end_matches('\n')
        .to_string();
    let (root, _) = write_session(&complete);
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(messages_of(&values).len(), 1);

    // Mid-file garbage line is skipped like Pi's loader.
    let completed_corruption = [
        serde_json::to_string(&header(json!({}))).unwrap(),
        serde_json::to_string(&user_entry("before", None, "before malformed line")).unwrap(),
        "{broken}".to_string(),
        serde_json::to_string(&user_entry_ts(
            "after",
            Some("before"),
            "after malformed line",
            "2026-08-02T10:00:02.000Z",
        ))
        .unwrap(),
        String::new(),
    ]
    .join("\n");
    let (root, _) = write_session(&completed_corruption);
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(
        messages_of(&values)
            .iter()
            .filter_map(|m| m.text.clone())
            .collect::<Vec<_>>(),
        vec!["before malformed line", "after malformed line"]
    );

    // Terminal garbage after a newline-terminated body is skipped.
    let terminal_garbage =
        jsonl(&[header(json!({})), user_entry("user", None, "prefix")]) + "not-json";
    let (root, _) = write_session(&terminal_garbage);
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(
        messages_of(&values)
            .iter()
            .filter_map(|m| m.text.clone())
            .collect::<Vec<_>>(),
        vec!["prefix"]
    );

    // A parseable non-object value is structural corruption.
    let (root, path) = write_session(&format!(
        "{}\n42\n",
        serde_json::to_string(&header(json!({}))).unwrap()
    ));
    let provider = PiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, path.to_string_lossy());
    let error = drain_err(parse(&units[0], None));
    assert!(
        error.contains("Malformed Pi JSONL value at line 2"),
        "{error}"
    );

    // A null message body is structural corruption.
    let invalid_message = jsonl(&[
        header(json!({})),
        json!({
            "type": "message", "id": "invalid-message", "parentId": null,
            "timestamp": "2026-08-02T10:00:01.000Z", "message": null,
        }),
    ]);
    let (root, _) = write_session(&invalid_message);
    let provider = PiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    let error = drain_err(parse(&units[0], None));
    assert!(error.contains("Malformed Pi message"), "{error}");
}

#[test]
fn v1_migration_preserves_identities_and_v2_hook_message_becomes_custom() {
    let v1 = [
        json!({"type": "session", "id": "legacy-v1"}),
        json!({"type": "message", "timestamp": "2026-08-02T10:00:01.000Z", "message": {"role": "user", "content": "legacy user", "timestamp": 1785664800000i64}}),
        json!({"type": "compaction", "timestamp": "2026-08-02T10:00:02.000Z", "summary": "legacy summary", "firstKeptEntryIndex": 1, "tokensBefore": 100}),
        json!({"type": "message", "timestamp": "2026-08-02T10:00:03.000Z", "message": {"role": "user", "content": "legacy tail", "timestamp": 1785664803000i64}}),
    ];
    // v1 files separate records with blank lines; the loader skips them.
    let v1_body = v1
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n";
    let (dir, path) = write_session(&v1_body);
    let (first_provider, _, first, _) = parse_only(dir.path());

    let mut migrated_compaction = v1[2].clone();
    migrated_compaction["id"] = json!("random-migrated-2");
    migrated_compaction["parentId"] = json!("random-migrated-1");
    migrated_compaction["firstKeptEntryId"] = json!("random-migrated-1");
    if let Some(record) = migrated_compaction.as_object_mut() {
        record.remove("firstKeptEntryIndex");
    }
    let migrated = vec![
        {
            let mut record = v1[0].clone();
            record["version"] = json!(3);
            record
        },
        {
            let mut record = v1[1].clone();
            record["id"] = json!("random-migrated-1");
            record["parentId"] = json!(null);
            record
        },
        migrated_compaction,
        {
            let mut record = v1[3].clone();
            record["id"] = json!("random-migrated-3");
            record["parentId"] = json!("random-migrated-2");
            record
        },
    ];
    std::fs::write(&path, jsonl(&migrated)).unwrap();
    let (_, _, second, second_cursor) = parse_only(dir.path());

    assert_eq!(
        messages_of(&first)
            .iter()
            .map(|m| m.uuid.as_str())
            .collect::<Vec<_>>(),
        messages_of(&second)
            .iter()
            .map(|m| m.uuid.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        summaries_of(&first)
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        summaries_of(&second)
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(session_of(&first).version.as_deref(), Some("session-v1"));
    assert_eq!(session_of(&second).version.as_deref(), Some("session-v3"));
    assert_eq!(session_of(&first).started_at, None);
    assert_eq!(session_of(&first).project, None);
    assert_eq!(session_of(&first).id, session_of(&second).id);
    assert!(messages_of(&first).iter().all(|m| m.cwd.is_none()));

    let first_message = messages_of(&first)[0];
    let session_json = json!({"id": session_of(&second).id, "jsonl_path": path.to_string_lossy()});
    let raw_after_migration = raw_lookup(
        &first_provider,
        &first_message.uuid,
        &session_json,
        second_cursor.as_deref(),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&raw_after_migration.text).unwrap(),
        migrated[1]["message"]
    );
    assert_eq!(
        raw_after_migration.message_text.as_deref(),
        Some("legacy user")
    );

    let v2 = vec![
        header(json!({"version": 2, "id": "legacy-v2"})),
        json!({
            "type": "message", "id": "hook", "parentId": null,
            "timestamp": "2026-08-02T10:00:01.000Z",
            "message": {"role": "hookMessage", "customType": "legacy-hook", "content": "extension context", "display": true, "timestamp": 1785664800000i64},
        }),
    ];
    let (root, _) = write_session(&jsonl(&v2));
    let (_, _, values, _) = parse_only(root.path());
    let hook = messages_of(&values)[0];
    assert_eq!(hook.r#type, "system");
    assert_eq!(hook.role.as_deref(), Some("custom"));
    assert_eq!(hook.content_type.as_deref(), Some("custom"));
    assert!(hook.is_meta);
    assert_eq!(hook.visibility, MessageVisibility::Visible);
}

#[test]
fn invalid_tree_identity_cycle_and_leaf_target_fail_closed() {
    let cases: Vec<(Vec<Value>, &str)> = vec![
        (
            vec![
                header(json!({})),
                user_entry("same", None, "one"),
                user_entry("same", Some("same"), "two"),
            ],
            "Duplicate Pi entry id",
        ),
        (
            vec![
                header(json!({})),
                user_entry("a", Some("b"), "a"),
                user_entry_ts("b", Some("a"), "b", "2026-08-02T10:00:02.000Z"),
            ],
            "cycle",
        ),
        (
            vec![
                header(json!({})),
                user_entry("a", Some("b"), "a"),
                user_entry_ts("b", Some("a"), "b", "2026-08-02T10:00:02.000Z"),
                json!({
                    "type": "compaction", "id": "unrelated-compaction", "parentId": null,
                    "timestamp": "2026-08-02T10:00:03.000Z",
                    "summary": "Must not mask the cycle", "firstKeptEntryId": "a", "tokensBefore": 10,
                }),
            ],
            "cycle",
        ),
        (
            vec![
                header(json!({})),
                user_entry("a", None, "a"),
                json!({"type": "leaf", "id": "leaf", "parentId": "a", "targetId": "missing", "timestamp": "2026-08-02T10:00:02.000Z"}),
            ],
            "leaf target missing does not exist",
        ),
    ];
    for (records, pattern) in cases {
        let (root, _) = write_session(&jsonl(&records));
        let provider = PiProvider::new(root.path().to_path_buf());
        let units = discover_all(&provider);
        let error = drain_err(parse(&units[0], None));
        assert!(error.contains(pattern), "expected {pattern} in {error}");
    }
}

#[test]
fn missing_parents_form_official_pi_orphan_roots() {
    let records = vec![
        header(json!({"id": "orphan-root"})),
        user_entry("inactive-root", None, "inactive root"),
        user_entry_ts(
            "orphan",
            Some("omitted-parent"),
            "orphan root",
            "2026-08-02T10:00:02.000Z",
        ),
        user_entry_ts(
            "orphan-child",
            Some("orphan"),
            "orphan child",
            "2026-08-02T10:00:03.000Z",
        ),
    ];
    let (root, _) = write_session(&jsonl(&records));
    let (_, unit, values, _) = parse_only(root.path());
    assert_eq!(
        visible_message_texts(&values),
        vec!["orphan root", "orphan child"]
    );
    assert_eq!(
        message_by_text(&values, "inactive root").visibility,
        MessageVisibility::Inactive
    );

    // TODO(session-detail): TS asserts query.thread ordering; the persisted
    // visible ordering carries the same contract.
    let conn = open_db(&[unit]);
    let visible: Vec<Option<String>> = conn
        .prepare("SELECT text FROM messages WHERE visibility='visible' ORDER BY timestamp,uuid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        visible,
        vec![
            Some("orphan root".to_string()),
            Some("orphan child".to_string())
        ]
    );
}

// ---------------------------------------------------------------------------
// Discovery reconciliation tests (TS: dedup, symlinks, tombstones, cursors)
// ---------------------------------------------------------------------------

#[test]
fn discovery_deduplicates_identical_copies_and_rejects_divergent_copies() {
    let root = tempfile::tempdir().unwrap();
    let original = fixture("tool-session.jsonl");
    write_session_at(root.path(), "a/session.jsonl", &original);
    write_session_at(root.path(), "b/session.jsonl", &original);
    let provider = PiProvider::new(root.path().to_path_buf());
    assert_eq!(discover_all(&provider).len(), 1);

    let divergent = format!(
        "{original}{}\n",
        serde_json::to_string(&json!({
            "type": "session_info", "id": "divergent-name", "parentId": "9db96a87",
            "timestamp": "2026-08-02T09:42:39.000Z", "name": "Diverged copy",
        }))
        .unwrap()
    );
    write_session_at(root.path(), "b/session.jsonl", &divergent);
    let divergent_units = discover_all(&provider);
    assert_eq!(divergent_units.len(), 2);
    for unit in &divergent_units {
        let error = drain_err(parse(unit, None));
        assert!(
            error.contains("Divergent Pi session copies"),
            "expected collision error in {error}"
        );
    }
}

#[cfg(unix)]
#[test]
fn discovery_follows_readable_jsonl_file_symlinks_without_retracting_provenance() {
    let (dir, path) = write_session(&jsonl(&[
        header(json!({"id": "symlink-session"})),
        user_entry("symlink-user", None, "symlink evidence"),
    ]));
    let provider = PiProvider::new(dir.path().to_path_buf());
    let original = discover_all(&provider);
    let (records, cursor) = drain(parse(&original[0], None));

    let target_dir = tempfile::tempdir().unwrap();
    let target = target_dir.path().join("session.jsonl");
    std::fs::rename(&path, &target).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();

    let changed = vec![path.to_string_lossy().into_owned()];
    let indexed = vec![IndexedSession {
        session_id: original[0].session_id.clone(),
        jsonl_path: path.to_string_lossy().into_owned(),
    }];
    let units = {
        let last_cursor = |key: &str| -> Option<String> {
            (key == path.to_string_lossy()).then(|| cursor.clone().unwrap())
        };
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&last_cursor, Some(&changed), Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };

    assert!(
        !units
            .iter()
            .any(|unit| unit.meta.as_ref().and_then(|m| m.get("kind"))
                == Some(&json!("pi-tombstone")))
    );
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].session_id, original[0].session_id);
    let (values, _) = drain(parse(&units[0], None));
    assert_eq!(
        messages_of(&values)[0].text.as_deref(),
        Some("symlink evidence")
    );
    let _ = records;
}

#[test]
fn malformed_file_does_not_force_unrelated_unchanged_sessions_to_reparse() {
    let root = tempfile::tempdir().unwrap();
    let damaged = write_session_at(
        root.path(),
        "a/session.jsonl",
        &jsonl(&[
            header(json!({"id": "damaged-session"})),
            user_entry("damaged-user", None, "damaged source"),
        ]),
    );
    let unchanged = write_session_at(
        root.path(),
        "b/session.jsonl",
        &jsonl(&[
            header(json!({"id": "unchanged-session"})),
            user_entry("unchanged-user", None, "unchanged source"),
        ]),
    );
    let provider = PiProvider::new(root.path().to_path_buf());
    let mut cursors: HashMap<String, String> = HashMap::new();
    let mut indexed = Vec::new();
    for unit in discover_all(&provider) {
        let (_, cursor) = drain(parse(&unit, None));
        cursors.insert(unit.key.clone(), cursor.unwrap());
        indexed.push(IndexedSession {
            session_id: unit.session_id.clone(),
            jsonl_path: unit.key.clone(),
        });
    }

    std::fs::write(&damaged, "{broken}\n").unwrap();
    let units = {
        let last_cursor = |key: &str| cursors.get(key).cloned();
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&last_cursor, None, Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };

    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, damaged.to_string_lossy());
    let error = drain_err(parse(&units[0], None));
    assert!(error.contains("Empty Pi session"), "{error}");
    let _ = unchanged;
}

#[test]
fn moved_session_with_unreadable_identity_cannot_retract_its_last_good_snapshot() {
    let (dir, path) = write_session(&jsonl(&[
        header(json!({"id": "moved-torn-session"})),
        user_entry("moved-user", None, "last good evidence"),
    ]));
    let provider = PiProvider::new(dir.path().to_path_buf());
    let original = discover_all(&provider);
    let (_, cursor) = drain(parse(&original[0], None));

    let moved_path = dir.path().join("moved").join("session.jsonl");
    std::fs::create_dir_all(moved_path.parent().unwrap()).unwrap();
    std::fs::rename(&path, &moved_path).unwrap();
    std::fs::write(&moved_path, "{torn header\n").unwrap();
    let mut reports = Vec::new();
    let indexed = vec![IndexedSession {
        session_id: original[0].session_id.clone(),
        jsonl_path: path.to_string_lossy().into_owned(),
    }];
    let units = {
        let last_cursor = |key: &str| -> Option<String> {
            (key == path.to_string_lossy()).then(|| cursor.clone().unwrap())
        };
        let indexed_fn = || indexed.clone();
        let mut report = |issue: InventoryIssue| reports.push(issue);
        let mut ctx = discover_ctx(&last_cursor, None, Some(&indexed_fn), Some(&mut report));
        provider.discover(&mut ctx)
    };

    assert!(
        reports.is_empty(),
        "a bad identity is not an incomplete filesystem traversal"
    );
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, moved_path.to_string_lossy());
    assert!(units[0].retract_session_ids.is_empty());
    let error = drain_err(parse(&units[0], None));
    assert!(error.contains("Empty Pi session"), "{error}");
}

#[test]
fn bad_selected_copy_forces_unchanged_valid_duplicate_to_refresh_provenance() {
    let root = tempfile::tempdir().unwrap();
    let content = fixture("tool-session.jsonl");
    let second = write_session_at(root.path(), "b/session.jsonl", &content);
    let provider = PiProvider::new(root.path().to_path_buf());
    let initial_unit = discover_all(&provider)[0].clone();
    let (_, initial) = drain(parse(&initial_unit, None));
    let mut cursors: HashMap<String, String> = HashMap::new();
    cursors.insert(
        second.to_string_lossy().into_owned(),
        initial.clone().unwrap(),
    );

    let first = write_session_at(root.path(), "a/session.jsonl", &content);
    let selected = {
        let last_cursor = |key: &str| cursors.get(key).cloned();
        let mut ctx = discover_ctx(&last_cursor, None, None, None);
        provider.discover(&mut ctx)
    };
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].key, first.to_string_lossy());
    let (_, selected_parse) = drain(parse(&selected[0], None));
    cursors.insert(
        first.to_string_lossy().into_owned(),
        selected_parse.clone().unwrap(),
    );

    std::fs::write(&first, "{broken}\n").unwrap();
    let indexed = vec![IndexedSession {
        session_id: initial_unit.session_id.clone(),
        jsonl_path: first.to_string_lossy().into_owned(),
    }];
    let recovery = {
        let last_cursor = |key: &str| cursors.get(key).cloned();
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&last_cursor, None, Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    let survivor = recovery
        .iter()
        .find(|unit| unit.key == second.to_string_lossy())
        .expect("the valid copy must bypass its unchanged cursor");
    assert_eq!(survivor.session_id, initial_unit.session_id);
    let (values, _) = drain(parse(survivor, None));
    assert_eq!(
        session_of(&values).jsonl_path,
        second.to_string_lossy().into_owned()
    );
}

#[test]
fn same_project_local_session_id_stays_distinct_across_cwd_namespaces() {
    let root = tempfile::tempdir().unwrap();
    let first_header = header(json!({"id": "shared-custom-id", "cwd": "/tmp/pi-project-a"}));
    let second_header = header(json!({"id": "shared-custom-id", "cwd": "/tmp/pi-project-b"}));
    write_session_at(
        root.path(),
        "a/session.jsonl",
        &jsonl(&[
            first_header.clone(),
            user_entry("project-a-user", None, "project A evidence"),
        ]),
    );
    write_session_at(
        root.path(),
        "b/session.jsonl",
        &jsonl(&[
            second_header.clone(),
            user_entry("project-b-user", None, "project B evidence"),
        ]),
    );

    let provider = PiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 2);
    let session_ids: std::collections::HashSet<String> =
        units.iter().map(|unit| unit.session_id.clone()).collect();
    assert_eq!(
        session_ids,
        [pi_session_id(&first_header), pi_session_id(&second_header)]
            .into_iter()
            .collect::<std::collections::HashSet<String>>()
    );

    let conn = open_db(&units);
    let db_ids: std::collections::HashSet<String> = conn
        .prepare("SELECT id FROM sessions WHERE source='pi'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(db_ids, session_ids);
}

#[test]
fn identity_migration_retracts_legacy_row_attached_to_non_selected_copy() {
    let root = tempfile::tempdir().unwrap();
    let content = fixture("tool-session.jsonl");
    let first = write_session_at(root.path(), "a/session.jsonl", &content);
    let second = write_session_at(root.path(), "b/session.jsonl", &content);
    let provider = PiProvider::new(root.path().to_path_buf());
    let initial_unit = discover_all(&provider)[0].clone();
    let (_, initial) = drain(parse(&initial_unit, None));
    let source_header: Value = serde_json::from_str(content.split('\n').next().unwrap()).unwrap();
    let legacy_id = format!("pi:{}", source_header["id"].as_str().unwrap());

    let indexed = vec![IndexedSession {
        session_id: legacy_id.clone(),
        jsonl_path: second.to_string_lossy().into_owned(),
    }];
    let migration = {
        let last_cursor = |key: &str| -> Option<String> {
            (key == first.to_string_lossy()).then(|| initial.clone().unwrap())
        };
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&last_cursor, None, Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    assert_eq!(migration.len(), 1);
    assert_eq!(migration[0].key, first.to_string_lossy());
    assert_eq!(migration[0].session_id, pi_session_id(&source_header));
    assert_eq!(migration[0].retract_session_ids, vec![legacy_id]);

    let (values, _) = drain(parse(&migration[0], None));
    let deletions: Vec<&String> = values
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::DeleteSession { session_id } => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(deletions, vec![&pi_session_id(&source_header)]);
}

#[test]
fn unlinking_deduplicated_copy_reparses_surviving_source() {
    let root = tempfile::tempdir().unwrap();
    let content = fixture("tool-session.jsonl");
    let first = write_session_at(root.path(), "a/session.jsonl", &content);
    let second = write_session_at(root.path(), "b/session.jsonl", &content);
    let provider = PiProvider::new(root.path().to_path_buf());
    let original = discover_all(&provider)[0].clone();
    let (_, cursor) = drain(parse(&original, None));

    std::fs::remove_file(&first).unwrap();
    let changed = vec![first.to_string_lossy().into_owned()];
    let indexed = vec![IndexedSession {
        session_id: original.session_id.clone(),
        jsonl_path: first.to_string_lossy().into_owned(),
    }];
    let survivors = {
        let last_cursor = |key: &str| -> Option<String> {
            (key == first.to_string_lossy()).then(|| cursor.clone().unwrap())
        };
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&last_cursor, Some(&changed), Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    assert_eq!(survivors.len(), 1);
    assert_eq!(survivors[0].key, second.to_string_lossy());
    assert!(survivors[0].retract_session_ids.is_empty());
    let (values, _) = drain(parse(&survivors[0], None));
    assert_eq!(
        session_of(&values).jsonl_path,
        second.to_string_lossy().into_owned()
    );
}

#[test]
fn replacing_one_copy_preserves_original_identity_when_another_copy_survives() {
    let root = tempfile::tempdir().unwrap();
    let content = fixture("tool-session.jsonl");
    let first = write_session_at(root.path(), "a/session.jsonl", &content);
    let second = write_session_at(root.path(), "b/session.jsonl", &content);
    let provider = PiProvider::new(root.path().to_path_buf());
    let original = discover_all(&provider)[0].clone();
    drain(parse(&original, None));

    write_session_at(
        root.path(),
        "a/session.jsonl",
        &jsonl(&[
            header(json!({"id": "replacement-copy"})),
            user_entry("replacement-user", None, "replacement"),
        ]),
    );
    let changed = vec![first.to_string_lossy().into_owned()];
    let indexed = vec![IndexedSession {
        session_id: original.session_id.clone(),
        jsonl_path: first.to_string_lossy().into_owned(),
    }];
    let units = {
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&|_key| None, Some(&changed), Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };

    let replacement_header = header(json!({"id": "replacement-copy"}));
    let by_key: Vec<(String, String, Vec<String>)> = units
        .iter()
        .map(|unit| {
            (
                unit.key.clone(),
                unit.session_id.clone(),
                unit.retract_session_ids.clone(),
            )
        })
        .collect();
    assert_eq!(
        by_key,
        vec![
            (
                first.to_string_lossy().into_owned(),
                pi_session_id(&replacement_header),
                Vec::new()
            ),
            (
                second.to_string_lossy().into_owned(),
                original.session_id.clone(),
                Vec::new()
            ),
        ]
    );
}

#[test]
fn changed_path_discovery_bypasses_cursors_while_passive_pull_is_skipped() {
    let (dir, path) = write_session(&fixture("tool-session.jsonl"));
    let provider = PiProvider::new(dir.path().to_path_buf());
    let units = discover_all(&provider);
    let (_, cursor) = drain(parse(&units[0], None));
    let cursor = cursor.unwrap();

    let passive = {
        let last_cursor = |_key: &str| Some(cursor.clone());
        let mut ctx = discover_ctx(&last_cursor, None, None, None);
        provider.discover(&mut ctx)
    };
    assert!(passive.is_empty());

    let changed = vec![path.to_string_lossy().into_owned()];
    let bypassed = {
        let last_cursor = |_key: &str| Some(cursor.clone());
        let mut ctx = discover_ctx(&last_cursor, Some(&changed), None, None);
        provider.discover(&mut ctx)
    };
    assert_eq!(
        bypassed
            .iter()
            .map(|unit| unit.key.clone())
            .collect::<Vec<_>>(),
        vec![path.to_string_lossy().into_owned()]
    );
}

fn replace_at_same_mtime(path: &Path, content: &str) {
    let before = std::fs::metadata(path).unwrap();
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, content).unwrap();
    let file = File::options().write(true).open(&replacement).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(before.accessed().unwrap())
            .set_modified(before.modified().unwrap()),
    )
    .unwrap();
    drop(file);
    std::fs::rename(&replacement, path).unwrap();
    let file = File::options().write(true).open(path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(before.accessed().unwrap())
            .set_modified(before.modified().unwrap()),
    )
    .unwrap();
}

#[test]
fn snapshot_cursors_detect_same_identity_and_replacement_writes_preserving_mtime() {
    let (dir, path) = write_session(&jsonl(&[
        header(json!({"id": "preserved-mtime"})),
        user_entry("before", None, "before"),
    ]));
    let provider = PiProvider::new(dir.path().to_path_buf());
    let initial_unit = discover_all(&provider)[0].clone();
    let (_, initial) = drain(parse(&initial_unit, None));
    let initial_cursor = initial.unwrap();

    replace_at_same_mtime(
        &path,
        &jsonl(&[
            header(json!({"id": "preserved-mtime"})),
            user_entry_ts("after", None, "after!", "2026-08-02T10:00:02.000Z"),
        ]),
    );
    let indexed = vec![IndexedSession {
        session_id: initial_unit.session_id.clone(),
        jsonl_path: path.to_string_lossy().into_owned(),
    }];
    let same_identity = {
        let last_cursor = |_key: &str| Some(initial_cursor.clone());
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&last_cursor, None, Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    assert_eq!(same_identity.len(), 1);
    assert_eq!(same_identity[0].session_id, initial_unit.session_id);
    let (_, updated) = drain(parse(&same_identity[0], Some(initial_cursor.clone())));
    let updated_cursor = updated.unwrap();

    let replacement_header = header(json!({"id": "replacement-mtime"}));
    replace_at_same_mtime(
        &path,
        &jsonl(&[
            replacement_header.clone(),
            user_entry("replacement", None, "replacement"),
        ]),
    );
    let replacement = {
        let last_cursor = |_key: &str| Some(updated_cursor.clone());
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&last_cursor, None, Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    assert_eq!(replacement.len(), 1);
    assert_eq!(
        replacement[0].session_id,
        pi_session_id(&replacement_header)
    );
    assert_eq!(
        replacement[0].retract_session_ids,
        vec![initial_unit.session_id.clone()]
    );
}

#[test]
fn incomplete_inventories_never_infer_deletion_from_changed_paths() {
    let parent = tempfile::tempdir().unwrap();
    let unreadable_root = parent.path().join("sessions");
    std::fs::write(&unreadable_root, "not a directory").unwrap();
    let indexed_path = unreadable_root.join("project").join("session.jsonl");
    let provider = PiProvider::new(unreadable_root.clone());
    let changed = vec![unreadable_root.to_string_lossy().into_owned()];
    let indexed = vec![IndexedSession {
        session_id: "pi:preserve-me".to_string(),
        jsonl_path: indexed_path.to_string_lossy().into_owned(),
    }];
    let units = {
        let indexed_fn = || indexed.clone();
        let mut ctx = discover_ctx(&|_key| None, Some(&changed), Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    assert!(units.is_empty());
}

#[test]
fn file_disappearing_during_discovery_marks_inventory_incomplete() {
    let (dir, path) = write_session(&jsonl(&[
        header(json!({"id": "discovery-race"})),
        user_entry("message", None, "vanishing source"),
    ]));
    let provider = PiProvider::new(dir.path().to_path_buf());
    let mut reports = Vec::new();

    let units = {
        let indexed_fn = move || -> Vec<IndexedSession> {
            std::fs::remove_file(&path).unwrap();
            Vec::new()
        };
        let mut report = |issue: InventoryIssue| reports.push(issue);
        let mut ctx = discover_ctx(&|_key| None, None, Some(&indexed_fn), Some(&mut report));
        provider.discover(&mut ctx)
    };

    assert!(units.is_empty());
    assert_eq!(reports.len(), 1, "the vanished file is reported incomplete");
}

#[test]
fn indexed_provenance_retracts_replaced_identity_and_emits_unlink_tombstone() {
    let (dir, path) = write_session(&jsonl(&[
        header(json!({"id": "before"})),
        user_entry("a", None, "before"),
    ]));
    let provider = PiProvider::new(dir.path().to_path_buf());
    let before_unit = discover_all(&provider)[0].clone();
    let (before_values, before_cursor) = drain(parse(&before_unit, None));
    let before_session = session_of(&before_values).id.clone();

    write_session_at(
        dir.path(),
        "project/session.jsonl",
        &jsonl(&[
            header(json!({"id": "after"})),
            user_entry_ts("b", None, "after", "2026-08-02T10:00:02.000Z"),
        ]),
    );
    let changed = vec![path.to_string_lossy().into_owned()];
    let after_units = {
        let last_cursor = |_key: &str| Some(before_cursor.clone().unwrap());
        let indexed_fn = || {
            vec![IndexedSession {
                session_id: before_session.clone(),
                jsonl_path: path.to_string_lossy().into_owned(),
            }]
        };
        let mut ctx = discover_ctx(&last_cursor, Some(&changed), Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    assert_eq!(
        after_units[0].retract_session_ids,
        vec![before_session.clone()]
    );
    let (after_values, after_cursor) = drain(parse(&after_units[0], None));
    let after_session = session_of(&after_values).id.clone();
    assert_ne!(after_session, before_session);
    let deletions: Vec<&String> = after_values
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::DeleteSession { session_id } => Some(session_id),
            _ => None,
        })
        .collect();
    assert_eq!(deletions, vec![&after_session]);

    std::fs::remove_file(&path).unwrap();
    let tombstones = {
        let last_cursor = |_key: &str| Some(after_cursor.clone().unwrap());
        let indexed_fn = || {
            vec![IndexedSession {
                session_id: after_session.clone(),
                jsonl_path: path.to_string_lossy().into_owned(),
            }]
        };
        let mut ctx = discover_ctx(&last_cursor, Some(&changed), Some(&indexed_fn), None);
        provider.discover(&mut ctx)
    };
    assert_eq!(tombstones.len(), 1);
    assert_eq!(
        tombstones[0].retract_session_ids,
        vec![after_session.clone()]
    );
    let (tombstone_records, tombstone_cursor) = drain(parse(&tombstones[0], after_cursor));
    assert!(tombstone_records.is_empty());
    assert_eq!(tombstone_cursor.as_deref(), Some("0:0"));
}

#[test]
fn session_identity_combines_project_local_id_with_stable_cwd_namespace() {
    let source: Value =
        serde_json::from_str(fixture("tool-session.jsonl").split('\n').next().unwrap()).unwrap();
    let mut migrated = source.clone();
    migrated["version"] = if source["version"] == json!(3) {
        json!(2)
    } else {
        json!(3)
    };
    migrated["timestamp"] = json!("2030-01-01T00:00:00.000Z");
    migrated["parentSession"] = json!("/tmp/parent.jsonl");
    assert_eq!(pi_session_id(&source), pi_session_id(&migrated));

    let mut different_id = source.clone();
    different_id["id"] = json!("different-session");
    assert_ne!(pi_session_id(&source), pi_session_id(&different_id));

    let mut different_cwd = source.clone();
    different_cwd["cwd"] = json!("/tmp/other-project");
    assert_ne!(pi_session_id(&source), pi_session_id(&different_cwd));
}

#[test]
fn long_linear_pi_trees_validate_without_repeated_prefix_walks() {
    let mut records = vec![header(json!({"id": "long-linear"}))];
    let mut parent: Option<String> = None;
    for index in 0..5000 {
        let id = format!("entry-{index}");
        let ts = format!(
            "2026-08-02T10:{:02}:{:02}.000Z",
            (index / 60) % 60,
            index % 60
        );
        records.push(user_entry_ts(
            &id,
            parent.as_deref(),
            &format!("message {index}"),
            &ts,
        ));
        parent = Some(id);
    }
    let (root, _) = write_session(&jsonl(&records));
    let (_, _, values, _) = parse_only(root.path());
    assert_eq!(messages_of(&values).len(), 5000);
}

// ---------------------------------------------------------------------------
// Randomized differential (TS: tests/pi-randomized-differential.test.mjs)
//
// Test-only transcription of the Pi 0.83.0 context algorithms:
// https://github.com/earendil-works/pi/blob/v0.83.0/packages/coding-agent/src/core/session-manager.ts
// https://github.com/earendil-works/pi/blob/v0.83.0/packages/agent/src/harness/session/session.ts
// (MIT License, Copyright (c) 2025 Mario Zechner — see the TS fixture header.)
// ---------------------------------------------------------------------------

const PI_CONTEXT_ORACLE_VERSION: &str = "0.83.0";

fn oracle_by_id(entries: &[Value]) -> HashMap<String, Value> {
    entries
        .iter()
        .map(|entry| (entry["id"].as_str().unwrap().to_string(), entry.clone()))
        .collect()
}

fn pi083_leaf_id(entries: &[Value]) -> Option<String> {
    let mut leaf_id = None;
    for entry in entries {
        leaf_id = if entry["type"] == json!("leaf") {
            entry["targetId"].as_str().map(str::to_string)
        } else {
            Some(entry["id"].as_str().unwrap().to_string())
        };
    }
    leaf_id
}

fn oracle_walk_stop(entry: &Value, stop_at_entry_id: &Option<String>) -> bool {
    stop_at_entry_id
        .as_deref()
        .is_some_and(|stop| entry["id"].as_str() == Some(stop))
}

fn build_coding_agent_session_path(
    entries: &[Value],
    leaf_id: Option<&str>,
    by_id: &HashMap<String, Value>,
) -> Vec<Value> {
    let Some(leaf_id) = leaf_id else {
        return Vec::new();
    };
    let leaf = by_id
        .get(leaf_id)
        .or_else(|| entries.last())
        .expect("leaf entry exists");

    let mut path: Vec<Value> = Vec::new();
    let mut current = Some(leaf);
    while let Some(entry) = current {
        path.push(entry.clone());
        current = entry["parentId"]
            .as_str()
            .and_then(|parent| by_id.get(parent));
    }
    path.reverse();
    path
}

// pi-agent-core 0.83.0 JsonlSessionStorage.getPathToRootOrCompaction().
// The checked fixtures may contain synthetic orphan edges; those terminate
// the path just as the coding-agent oracle above does instead of exercising
// the storage API's separate invalid-session error.
fn build_agent_core_session_path(
    entries: &[Value],
    leaf_id: Option<&str>,
    by_id: &HashMap<String, Value>,
) -> Vec<Value> {
    let Some(leaf_id) = leaf_id else {
        return Vec::new();
    };
    let mut current = by_id
        .get(leaf_id)
        .or_else(|| entries.last())
        .expect("leaf entry exists");
    let mut path: Vec<Value> = Vec::new();
    let mut stop_at_entry_id: Option<String> = None;
    loop {
        path.insert(0, current.clone());
        if oracle_walk_stop(current, &stop_at_entry_id) {
            break;
        }
        if current["type"] == json!("compaction") {
            if current.get("retainedTail").is_some_and(Value::is_array) {
                break;
            }
            stop_at_entry_id = current["firstKeptEntryId"].as_str().map(str::to_string);
        }
        let Some(parent) = current["parentId"].as_str() else {
            break;
        };
        match by_id.get(parent) {
            Some(next) => current = next,
            None => break,
        }
    }
    path
}

fn oracle_last_compaction(path: &[Value]) -> Option<&Value> {
    path.iter()
        .rev()
        .find(|entry| entry["type"] == json!("compaction"))
}

fn build_coding_agent_context_entries(
    entries: &[Value],
    leaf_id: Option<&str>,
    by_id: &HashMap<String, Value>,
) -> Vec<Value> {
    let path = build_coding_agent_session_path(entries, leaf_id, by_id);
    let Some(compaction) = oracle_last_compaction(&path) else {
        return path;
    };
    let compaction_index = path
        .iter()
        .position(|entry| entry["id"] == compaction["id"])
        .unwrap();
    let mut context = vec![compaction.clone()];
    let mut found_first_kept = false;
    for entry in &path[..compaction_index] {
        if entry["id"] == compaction["firstKeptEntryId"] {
            found_first_kept = true;
        }
        if found_first_kept {
            context.push(entry.clone());
        }
    }
    context.extend(path[compaction_index + 1..].iter().cloned());
    context
}

fn default_agent_core_context_entry_transform(path_entries: &[Value]) -> Vec<Value> {
    let Some(compaction) = oracle_last_compaction(path_entries) else {
        return path_entries.to_vec();
    };
    let mut entries_out = vec![compaction.clone()];
    let compaction_index = path_entries
        .iter()
        .position(|entry| entry["type"] == json!("compaction") && entry["id"] == compaction["id"])
        .unwrap();
    if compaction.get("retainedTail").is_some_and(Value::is_array) {
        entries_out.extend(path_entries[compaction_index + 1..].iter().cloned());
        return entries_out;
    }
    if compaction["firstKeptEntryId"].is_string() {
        let mut found_first_kept = false;
        for entry in &path_entries[..compaction_index] {
            if entry["id"] == compaction["firstKeptEntryId"] {
                found_first_kept = true;
            }
            if found_first_kept {
                entries_out.push(entry.clone());
            }
        }
    }
    entries_out.extend(path_entries[compaction_index + 1..].iter().cloned());
    entries_out
}

fn evidence_json(kind: &str, source: Value, role: Value, content: Value) -> String {
    json!({"kind": kind, "source": source, "role": role, "content": content}).to_string()
}

// The Pi oracles select the active context entries. Obelisk stores those
// entries in durable physical order for its evidence timeline, while
// retainedTail messages remain immediately after their owning compaction
// summary.
fn project_canonical_evidence(
    physical_entries: &[Value],
    context_entries: &[Value],
) -> Vec<String> {
    let active_ids: std::collections::HashSet<&str> = context_entries
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    let mut evidence = Vec::new();
    for entry in physical_entries
        .iter()
        .filter(|entry| active_ids.contains(entry["id"].as_str().unwrap()))
    {
        match entry["type"].as_str().unwrap() {
            "message" => evidence.push(evidence_json(
                "message",
                Value::Null,
                entry["message"]["role"].clone(),
                entry["message"]["content"].clone(),
            )),
            "branch_summary" => evidence.push(evidence_json(
                "summary",
                json!("pi:branch_summary"),
                Value::Null,
                entry["summary"].clone(),
            )),
            "compaction" => {
                evidence.push(evidence_json(
                    "summary",
                    json!("pi:compaction"),
                    Value::Null,
                    entry["summary"].clone(),
                ));
                for message in entry
                    .get("retainedTail")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    evidence.push(evidence_json(
                        "message",
                        Value::Null,
                        message["role"].clone(),
                        message["content"].clone(),
                    ));
                }
            }
            _ => {}
        }
    }
    evidence
}

fn actual_evidence(records: &[TranscriptRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::Message(message)
                if message.visibility == MessageVisibility::Visible =>
            {
                Some(evidence_json(
                    "message",
                    Value::Null,
                    message
                        .role
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    message
                        .text
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                ))
            }
            TranscriptRecord::Summary(summary)
                if summary.visibility == Some(MessageVisibility::Visible) =>
            {
                Some(evidence_json(
                    "summary",
                    json!(summary.source),
                    Value::Null,
                    json!(summary.content),
                ))
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fixed-seed differential driver
// ---------------------------------------------------------------------------

use chrono::TimeZone;

struct Lcg(u32);

impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        self.0 as f64 / 0x1_0000_0000u64 as f64
    }
}

fn oracle_integer(random: &mut Lcg, min: i64, max: i64) -> i64 {
    min + (random.next_f64() * (max - min + 1) as f64).floor() as i64
}

fn differential_timestamp(case_index: u32, entry_index: u32) -> String {
    let base = chrono::Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
    let dt = base
        + chrono::Duration::seconds(12 * 3600 + (case_index % 30) as i64 * 60 + entry_index as i64);
    format!("{}Z", dt.format("%Y-%m-%dT%H:%M:%S%.3f"))
}

#[derive(Debug, Default, PartialEq)]
struct OracleTotals {
    cases: i64,
    entries: i64,
    messages: i64,
    leaves: i64,
    null_leaves: i64,
    compactions: i64,
    retained_tail_compactions: i64,
    branch_summaries: i64,
    orphan_parents: i64,
    retained_checkpoint_path_cases: i64,
    mixed_checkpoint_legacy_cases: i64,
}

fn create_oracle_case(random: &mut Lcg, case_index: u32, totals: &mut OracleTotals) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut head_id: Option<String> = None;
    let entry_count = oracle_integer(random, 8, 64);
    for entry_index in 0..entry_count {
        let id = format!("case-{case_index}-entry-{entry_index}");
        let time = differential_timestamp(case_index, entry_index as u32);
        let roll = random.next_f64();
        let entry;
        if roll < 0.08 && !ids.is_empty() {
            let target_id = if random.next_f64() < 0.12 {
                None
            } else {
                Some(ids[oracle_integer(random, 0, ids.len() as i64 - 1) as usize].clone())
            };
            entry = json!({
                "type": "leaf", "id": id, "parentId": head_id,
                "timestamp": time, "targetId": target_id,
            });
            head_id = target_id;
            totals.leaves += 1;
            if head_id.is_none() {
                totals.null_leaves += 1;
            }
        } else {
            let mut parent_id = head_id.clone();
            if random.next_f64() < 0.06 {
                parent_id = Some(format!("omitted-{case_index}-{entry_index}"));
                totals.orphan_parents += 1;
            } else if !ids.is_empty() && random.next_f64() < 0.28 {
                parent_id =
                    Some(ids[oracle_integer(random, 0, ids.len() as i64 - 1) as usize].clone());
            }

            if roll < 0.72 {
                entry = json!({
                    "type": "message", "id": id, "parentId": parent_id, "timestamp": time,
                    "message": {"role": "user", "content": format!("entry:{id}"), "timestamp": date_ms(&time)},
                });
                totals.messages += 1;
            } else if roll < 0.84 {
                entry = json!({
                    "type": "branch_summary", "id": id, "parentId": parent_id, "timestamp": time,
                    "fromId": parent_id.clone().unwrap_or_else(|| id.clone()),
                    "summary": format!("branch:{id}"),
                });
                totals.branch_summaries += 1;
            } else if roll < 0.96 {
                let retained = random.next_f64() < 0.5;
                totals.compactions += 1;
                if retained {
                    totals.retained_tail_compactions += 1;
                }
                entry = if retained {
                    json!({
                        "type": "compaction", "id": id, "parentId": parent_id, "timestamp": time,
                        "summary": format!("compaction:{id}"), "tokensBefore": oracle_integer(random, 0, 100_000),
                        "retainedTail": [{"role": "user", "content": format!("tail:{id}"), "timestamp": date_ms(&time)}],
                    })
                } else {
                    json!({
                        "type": "compaction", "id": id, "parentId": parent_id, "timestamp": time,
                        "summary": format!("compaction:{id}"), "tokensBefore": oracle_integer(random, 0, 100_000),
                        "firstKeptEntryId": parent_id,
                    })
                };
            } else {
                entry = json!({
                    "type": "model_change", "id": id, "parentId": parent_id, "timestamp": time,
                    "provider": "probe", "modelId": "probe",
                });
            }
            head_id = Some(id.clone());
        }
        entries.push(entry);
        ids.push(id);
        totals.entries += 1;
    }
    entries
}

#[test]
fn fixed_seed_randomized_differential_matches_vendored_pi_0_83_context_oracles() {
    assert_eq!(PI_CONTEXT_ORACLE_VERSION, "0.83.0");
    const CASES: usize = 512;
    const SEED: u32 = 0x5eedc0de;
    let mut random = Lcg(SEED);
    let root = tempfile::tempdir().unwrap();
    let mut generated: Vec<(Value, Vec<Value>)> = Vec::new();
    let mut totals = OracleTotals {
        cases: CASES as i64,
        ..Default::default()
    };

    for case_index in 0..CASES {
        let entries = create_oracle_case(&mut random, case_index as u32, &mut totals);
        let case_header = json!({
            "type": "session",
            "version": 3,
            "id": format!("differential-{case_index}"),
            "timestamp": differential_timestamp(case_index as u32, 0),
            "cwd": format!("/tmp/obelisk-pi-differential/project-{case_index}"),
        });
        let dir = root.path().join(format!("case-{case_index}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = serde_json::to_string(&case_header).unwrap() + "\n";
        for entry in &entries {
            body.push_str(&serde_json::to_string(entry).unwrap());
            body.push('\n');
        }
        std::fs::write(dir.join("session.jsonl"), body).unwrap();
        generated.push((case_header, entries));
    }

    let provider = PiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), CASES);
    for unit in &units {
        let marker = "case-";
        let start = unit.key.rfind(marker).unwrap() + marker.len();
        let rest = &unit.key[start..];
        let case_index: usize = rest[..rest.find('/').unwrap()].parse().unwrap();
        let (case_header, entries) = &generated[case_index];
        let leaf_id = pi083_leaf_id(entries);
        let by_id = oracle_by_id(entries);
        let full_path = build_coding_agent_session_path(entries, leaf_id.as_deref(), &by_id);
        let coding_agent_context =
            build_coding_agent_context_entries(entries, leaf_id.as_deref(), &by_id);
        let agent_core_context = default_agent_core_context_entry_transform(
            &build_agent_core_session_path(entries, leaf_id.as_deref(), &by_id),
        );
        // Pi 0.83's CLI owns legacy firstKeptEntryId semantics. retainedTail
        // is the agent-core storage checkpoint format, so mixed/new chains
        // use its bounded storage path before the context transform.
        let mut checkpoint_index: Option<usize> = None;
        for (index, entry) in full_path.iter().enumerate() {
            if entry["type"] == json!("compaction") && entry.get("retainedTail").is_some() {
                checkpoint_index = Some(index);
            }
        }
        if let Some(index) = checkpoint_index {
            totals.retained_checkpoint_path_cases += 1;
            if full_path[index + 1..].iter().any(|entry| {
                entry["type"] == json!("compaction") && entry.get("retainedTail").is_none()
            }) {
                totals.mixed_checkpoint_legacy_cases += 1;
            }
        }
        let expected_context = if checkpoint_index.is_some() {
            &agent_core_context
        } else {
            &coding_agent_context
        };
        let (records, _) = drain(parse(unit, None));
        assert_eq!(
            actual_evidence(&records),
            project_canonical_evidence(entries, expected_context),
            "seed 0x{SEED:x}, case {case_index}"
        );
        let _ = case_header;
    }

    assert_eq!(
        totals,
        OracleTotals {
            cases: 512,
            entries: 18124,
            messages: 11586,
            leaves: 1457,
            null_leaves: 167,
            compactions: 2230,
            retained_tail_compactions: 1137,
            branch_summaries: 2191,
            orphan_parents: 1060,
            retained_checkpoint_path_cases: 175,
            mixed_checkpoint_legacy_cases: 32,
        }
    );
}
