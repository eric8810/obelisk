// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Rust port of tests/kimi-parse.test.mjs, tests/kimi-manifest.test.mjs and
//! the adapter-level assertions of tests/kimi-runtime.test.mjs /
//! tests/app-kimi-index.test.mjs — the binding-independent Kimi adapter
//! contract.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::kimi::{base64url, parse, sha256, KimiProvider, KIMI_CANONICAL_TRANSCRIPT_MARKER};
use crate::persist::persist;
use crate::providers::types::{
    DiscoverContext, IndexUnit, IndexedSession, InventoryIssue, MessageRecord, ProviderAdapter,
    SessionRecord, StreamItem, SubagentRecord, SummaryRecord, ToolCallRecord, ToolResultRecord,
    TranscriptRecord,
};

// ---- helpers ----

fn write_jsonl(path: &Path, records: &[Value]) {
    let body: String = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path, body + "\n").unwrap();
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

/// TS `drain` on a generator that may throw: the error surfaces instead.
fn drain_or_error(
    items: Vec<StreamItem>,
) -> Result<(Vec<TranscriptRecord>, Option<String>), String> {
    let mut records = Vec::new();
    let mut cursor = None;
    for item in items {
        match item {
            StreamItem::Record(record) => records.push(record),
            StreamItem::Cursor(value) => cursor = Some(value),
            StreamItem::Error(error) => return Err(error),
        }
    }
    Ok((records, cursor))
}

fn discover_with(
    provider: &KimiProvider,
    cursors: &HashMap<String, String>,
    changed_paths: Option<Vec<String>>,
    indexed: &[IndexedSession],
    issues: &mut Vec<InventoryIssue>,
) -> Vec<IndexUnit> {
    let last_cursor = |key: &str| cursors.get(key).cloned();
    let indexed_sessions = || indexed.to_vec();
    let mut report = |issue: InventoryIssue| issues.push(issue);
    let mut ctx = DiscoverContext {
        last_cursor: &last_cursor,
        changed_paths: changed_paths.as_deref(),
        indexed_sessions: Some(&indexed_sessions),
        report_incomplete_inventory: Some(&mut report),
    };
    provider.discover(&mut ctx)
}

fn discover_all(provider: &KimiProvider) -> Vec<IndexUnit> {
    discover_with(provider, &HashMap::new(), None, &[], &mut Vec::new())
}

fn unit_cursor(unit: &IndexUnit) -> String {
    unit.meta
        .as_ref()
        .and_then(|meta| meta.get("currentCursor"))
        .and_then(Value::as_str)
        .expect("unit meta carries currentCursor")
        .to_string()
}

fn unit_mode(unit: &IndexUnit) -> String {
    unit.meta
        .as_ref()
        .and_then(|meta| meta.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn assert_manifest_cursor(cursor: &str) {
    assert!(
        regex::Regex::new(r"^\d+:0:kimi-manifest-v1:[A-Za-z0-9_-]{43}$")
            .unwrap()
            .is_match(cursor),
        "cursor does not match the manifest format: {cursor}"
    );
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

fn open_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_SQL).unwrap();
    conn
}

/// tests/kimi-parse.test.mjs writeKimiFixture.
fn write_kimi_fixture() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-native-1");
    let main_dir = session_dir.join("agents").join("main");
    let child_dir = session_dir.join("agents").join("agent-7");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::create_dir_all(&child_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({
            "title": "Kimi fixture",
            "createdAt": "2026-07-20T10:00:00.000Z",
            "updatedAt": "2026-07-20T10:01:00.000Z",
            "workDir": "/tmp/kimi-project",
            "agents": {
                "main": { "type": "main" },
                "agent-7": { "type": "sub", "parentAgentId": "main", "labels": { "profile": "explore" } },
            },
        }))
        .unwrap(),
    )
    .unwrap();
    let main_records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1753005600000i64}),
        json!({"type": "config.update", "time": 1753005600100i64, "modelAlias": "kimi-k2"}),
        json!({"type": "context.append_message", "time": 1753005601000i64, "message": {"role": "user", "content": [{"type": "text", "text": "inspect the project"}], "toolCalls": [], "origin": {"kind": "user"}}}),
        json!({"type": "context.append_loop_event", "time": 1753005602000i64, "event": {"type": "step.begin", "uuid": "step-1", "turnId": "0"}}),
        json!({"type": "context.append_loop_event", "time": 1753005602100i64, "event": {"type": "content.part", "uuid": "thinking-1", "stepUuid": "step-1", "part": {"type": "thinking", "thinking": "I should inspect it"}}}),
        json!({"type": "context.append_loop_event", "time": 1753005602200i64, "event": {"type": "tool.call", "uuid": "tool-event-1", "stepUuid": "step-1", "toolCallId": "call-1", "name": "Read", "args": {"file_path": "/tmp/kimi-project/a.ts"}}}),
        json!({"type": "context.append_loop_event", "time": 1753005602300i64, "event": {"type": "tool.result", "parentUuid": "tool-result-1", "toolCallId": "call-1", "result": {"output": "agent_id: agent-7\nfile body", "isError": false}}}),
        json!({"type": "context.append_loop_event", "time": 1753005602500i64, "event": {"type": "content.part", "uuid": "text-1", "stepUuid": "step-1", "part": {"type": "text", "text": "done"}}}),
        json!({"type": "context.append_loop_event", "time": 1753005603000i64, "event": {"type": "step.end", "uuid": "step-1", "usage": {"inputOther": 7, "inputCacheRead": 3, "inputCacheCreation": 2, "output": 3}}}),
        json!({"type": "context.apply_compaction", "time": 1753005604000i64, "summary": "Earlier work summary", "compactedCount": 2}),
    ];
    write_jsonl(&main_dir.join("wire.jsonl"), &main_records);
    let child_records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1753005600000i64}),
        json!({"type": "context.append_message", "time": 1753005602400i64, "message": {"role": "user", "content": [{"type": "text", "text": "child prompt"}], "toolCalls": [], "origin": {"kind": "system_trigger", "name": "subagent"}}}),
    ];
    write_jsonl(&child_dir.join("wire.jsonl"), &child_records);
    (root, session_dir)
}

// ---- vendored hash sanity (guards the manifest cursor encoding) ----

#[test]
fn vendored_sha256_matches_known_vectors() {
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    // base64url(sha256("")) — the canonical empty-input digest.
    assert_eq!(
        base64url(&sha256(b"")),
        "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU"
    );
}

// ---- kimi-parse.test.mjs ----

#[test]
fn kimi_provider_discovers_a_changed_session_directory_and_returns_a_stable_cursor() {
    let (root, session_dir) = write_kimi_fixture();
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);

    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, session_dir.to_string_lossy());
    assert_eq!(units[0].session_id, "kimi:session-native-1");
    assert_manifest_cursor(&unit_cursor(&units[0]));
    assert_eq!(
        KIMI_CANONICAL_TRANSCRIPT_MARKER,
        "__kimi_canonical_transcript_v6__"
    );

    let cursors: HashMap<String, String> = [(units[0].key.clone(), unit_cursor(&units[0]))]
        .into_iter()
        .collect();
    assert!(
        discover_with(&provider, &cursors, None, &[], &mut Vec::new()).is_empty(),
        "a current cursor proves the session unchanged"
    );

    // A legacy cursor cannot prove that the session is unchanged.
    let legacy_cursor = unit_cursor(&units[0])
        .split(':')
        .take(2)
        .collect::<Vec<_>>()
        .join(":");
    let legacy: HashMap<String, String> = [(units[0].key.clone(), legacy_cursor)]
        .into_iter()
        .collect();
    let rediscovered = discover_with(&provider, &legacy, None, &[], &mut Vec::new());
    assert_eq!(
        rediscovered
            .iter()
            .map(|u| u.key.clone())
            .collect::<Vec<_>>(),
        vec![session_dir.to_string_lossy().into_owned()]
    );
}

#[test]
fn kimi_provider_folds_main_and_subagent_wire_logs_into_the_canonical_transcript_language() {
    let (root, session_dir) = write_kimi_fixture();
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    let (records, ret) = drain(parse(&units[0], None));

    // The TS test pins the complete yielded sequence with a sha256 golden
    // over JSON.stringify(records); that hash is TS-serialization-specific
    // and is not portable — the structural assertions below carry the spec.

    assert!(matches!(
        &records[0],
        TranscriptRecord::DeleteSession { session_id } if session_id == "kimi:session-native-1"
    ));
    assert_eq!(ret.as_deref(), Some(unit_cursor(&units[0]).as_str()));

    let session = session_of(&records);
    assert_eq!(session.id, "kimi:session-native-1");
    assert_eq!(session.title.as_deref(), Some("Kimi fixture"));
    assert_eq!(session.project.as_deref(), Some("-tmp-kimi-project"));
    assert_eq!(session.source, "kimi");
    assert_eq!(
        session.count_mode,
        crate::providers::types::SessionCountMode::Total
    );
    assert_eq!(
        session.jsonl_path,
        session_dir
            .join("agents")
            .join("main")
            .join("wire.jsonl")
            .to_string_lossy()
            .into_owned()
    );

    let messages = messages_of(&records);
    let shape: Vec<(Option<&str>, &str, Option<&str>)> = messages
        .iter()
        .map(|m| {
            (
                m.role.as_deref(),
                m.content_type.as_deref().unwrap_or(""),
                m.text.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (Some("user"), "text", Some("inspect the project")),
            (Some("assistant"), "thinking", Some("I should inspect it")),
            (Some("assistant"), "tool_use", None),
            (Some("assistant"), "text", Some("done")),
            (Some("user"), "text", Some("child prompt")),
        ]
    );
    let child = messages.last().unwrap();
    assert_eq!(
        child.agent_id.as_deref(),
        Some("kimi:session-native-1:agent-7")
    );
    assert!(child.is_sidechain);
    let done = messages
        .iter()
        .find(|m| m.text.as_deref() == Some("done"))
        .unwrap();
    assert_eq!(done.input_tokens, Some(12));
    assert_eq!(done.output_tokens, Some(3));
    assert_eq!(done.model.as_deref(), Some("kimi-k2"));

    let tool_calls: Vec<&ToolCallRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::ToolCall(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "kimi:session-native-1:main:call-1");
    assert_eq!(tool_calls[0].name, "Read");
    assert_eq!(
        tool_calls[0].file_path.as_deref(),
        Some("/tmp/kimi-project/a.ts")
    );

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
        "kimi:session-native-1:main:call-1"
    );
    assert!(!tool_results[0].is_error);
    // TODO(session-detail): the TS test asserts the assembled detail's
    // tool_calls[0].result.content includes 'file body'; the record-level
    // equivalent is asserted here.
    assert!(tool_results[0].content.contains("file body"));

    let summaries: Vec<&SummaryRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::Summary(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(
        summaries
            .iter()
            .map(|s| s.content.as_str())
            .collect::<Vec<_>>(),
        vec!["Earlier work summary"]
    );

    let subagents: Vec<&SubagentRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::Subagent(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0].agent_id, "kimi:session-native-1:agent-7");
    assert_eq!(
        subagents[0].parent_tool_use_id.as_deref(),
        Some("kimi:session-native-1:main:call-1")
    );
    assert_eq!(subagents[0].agent_type.as_deref(), Some("explore"));

    let durations: Vec<&TranscriptRecord> = records
        .iter()
        .filter(|r| matches!(r, TranscriptRecord::MessageTurnDuration { .. }))
        .collect();
    assert_eq!(durations.len(), 1);
    assert!(matches!(
        durations[0],
        TranscriptRecord::MessageTurnDuration { uuid, turn_duration_ms: Some(1000) }
            if uuid == "kimi:session-native-1:main:text-1"
    ));

    // TODO(session-detail): the TS test asserts assembleSessionDetail excludes
    // the sidechain 'child prompt' message; the record-level equivalent is its
    // agent_id/is_sidechain projection asserted above.
}

#[test]
fn kimi_provider_ignores_a_torn_final_wire_line_until_it_is_completed() {
    let (root, session_dir) = write_kimi_fixture();
    let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
    let torn =
        std::fs::read_to_string(&wire_path).unwrap() + "{\"type\":\"context.append_message\"";
    std::fs::write(&wire_path, torn).unwrap();
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);

    let (records, ret) = drain(parse(&units[0], None));
    assert_eq!(messages_of(&records).len(), 5);
    assert_eq!(ret.as_deref(), Some(unit_cursor(&units[0]).as_str()));
}

#[test]
fn kimi_provider_normalizes_think_parts_and_drops_empty_thinking_placeholders() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-think-1");
    let main_dir = session_dir.join("agents").join("main");
    let wire_path = main_dir.join("wire.jsonl");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({"workDir": "/tmp/think"})).unwrap(),
    )
    .unwrap();
    let records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1}),
        json!({"type": "context.append_loop_event", "time": 2, "event": {"type": "step.begin", "uuid": "step-1"}}),
        json!({"type": "context.append_loop_event", "time": 3, "event": {"type": "content.part", "uuid": "think-1", "stepUuid": "step-1", "part": {"type": "think", "think": "private reasoning"}}}),
        json!({"type": "context.append_loop_event", "time": 4, "event": {"type": "content.part", "uuid": "think-empty", "stepUuid": "step-1", "part": {"type": "think", "think": ""}}}),
        json!({"type": "context.append_loop_event", "time": 5, "event": {"type": "content.part", "uuid": "answer-1", "stepUuid": "step-1", "part": {"type": "text", "text": "visible answer"}}}),
    ];
    write_jsonl(&wire_path, &records);
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);

    let (records, _) = drain(parse(&units[0], None));
    let messages = messages_of(&records);
    let shape: Vec<(Option<&str>, &str)> = messages
        .iter()
        .map(|m| (m.text.as_deref(), m.content_type.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Some("private reasoning"), "thinking"),
            (Some("visible answer"), "text")
        ]
    );
    assert_eq!(session_of(&records).message_count, 2);
    let raw = provider.raw(&crate::providers::types::RawLookup {
        source: "kimi",
        message_uuid: &messages[0].uuid,
        session: Some(&json!({"jsonl_path": wire_path.to_string_lossy()})),
        agent_id: None,
        cursor: None,
        subagent: None,
        workflow_agent: None,
    });
    assert_eq!(
        raw.unwrap().message_text.as_deref(),
        Some("private reasoning")
    );
}

#[test]
fn kimi_provider_removes_injection_messages_inside_an_undone_range() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-undo-1");
    let main_dir = session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({
            "workDir": "/tmp/kimi-undo",
            "agents": { "main": { "type": "main" } },
        }))
        .unwrap(),
    )
    .unwrap();
    let records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1753005600000i64}),
        json!({"type": "context.append_message", "time": 1, "message": {"role": "user", "content": "before clear", "toolCalls": [], "origin": {"kind": "user"}}}),
        json!({"type": "context.append_loop_event", "time": 2, "event": {"type": "content.part", "uuid": "before-answer", "stepUuid": "s1", "part": {"type": "text", "text": "kept answer"}}}),
        json!({"type": "context.clear", "time": 3}),
        json!({"type": "context.append_message", "time": 4, "message": {"role": "user", "content": "undone prompt", "toolCalls": [], "origin": {"kind": "user"}}}),
        json!({"type": "context.append_message", "time": 5, "message": {"role": "user", "content": "persistent injection", "toolCalls": [], "origin": {"kind": "injection"}}}),
        json!({"type": "context.append_message", "time": 6, "message": {"role": "user", "content": "ephemeral system trigger", "toolCalls": [], "origin": {"kind": "system_trigger"}}}),
        json!({"type": "context.append_loop_event", "time": 7, "event": {"type": "content.part", "uuid": "undone-answer", "stepUuid": "s2", "part": {"type": "text", "text": "undone answer"}}}),
        json!({"type": "context.undo", "time": 8, "count": 1}),
    ];
    write_jsonl(&main_dir.join("wire.jsonl"), &records);
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);

    let (records, _) = drain(parse(&units[0], None));
    assert_eq!(
        messages_of(&records)
            .iter()
            .map(|m| m.text.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("before clear"), Some("kept answer")]
    );
    assert_eq!(session_of(&records).message_count, 2);
}

#[test]
fn kimi_provider_removes_a_prompt_owned_injection_that_precedes_an_undone_prompt() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-undo-injection-1");
    let main_dir = session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({
            "workDir": "/tmp/kimi-undo-injection",
            "agents": { "main": { "type": "main" } },
        }))
        .unwrap(),
    )
    .unwrap();
    let records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1753005600000i64}),
        json!({"type": "context.append_message", "time": 1, "message": {"role": "user", "id": "p1", "content": "kept prompt", "toolCalls": [], "origin": {"kind": "user"}}}),
        json!({"type": "context.append_loop_event", "time": 2, "event": {"type": "content.part", "uuid": "kept-answer", "stepUuid": "s1", "part": {"type": "text", "text": "kept answer"}}}),
        json!({"type": "context.append_message", "time": 3, "message": {"role": "user", "id": "inj-1", "content": "compressed image context", "toolCalls": [], "origin": {"kind": "injection", "ownerPromptId": "X", "variant": "image_compression"}}}),
        json!({"type": "context.append_message", "time": 4, "message": {"role": "user", "id": "X", "content": "undone prompt", "toolCalls": [], "origin": {"kind": "user"}}}),
        json!({"type": "context.undo", "time": 5, "count": 1}),
    ];
    write_jsonl(&main_dir.join("wire.jsonl"), &records);
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);

    let (records, _) = drain(parse(&units[0], None));
    assert_eq!(
        messages_of(&records)
            .iter()
            .map(|m| m.text.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("kept prompt"), Some("kept answer")]
    );
    assert_eq!(session_of(&records).message_count, 2);
}

#[test]
fn kimi_provider_scopes_changed_path_discovery_to_one_session_and_bypasses_an_unchanged_cursor() {
    let root = tempfile::tempdir().unwrap();
    let first_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-1");
    let second_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-2");
    for session_dir in [&first_dir, &second_dir] {
        std::fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        std::fs::write(
            session_dir.join("state.json"),
            serde_json::to_string(&json!({"workDir": "/tmp/project"})).unwrap(),
        )
        .unwrap();
        std::fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            "{\"type\":\"metadata\"}\n",
        )
        .unwrap();
    }
    let provider = KimiProvider::new(root.path().to_path_buf());
    let initial = discover_all(&provider);
    assert_eq!(initial.len(), 2);
    let cursors: HashMap<String, String> = initial
        .iter()
        .map(|unit| (unit.key.clone(), unit_cursor(unit)))
        .collect();

    let units = discover_with(
        &provider,
        &cursors,
        Some(vec![first_dir
            .join("state.json")
            .to_string_lossy()
            .into_owned()]),
        &[],
        &mut Vec::new(),
    );
    assert_eq!(
        units.iter().map(|u| u.key.clone()).collect::<Vec<_>>(),
        vec![first_dir.to_string_lossy().into_owned()]
    );
}

#[test]
fn kimi_provider_presents_user_slash_activations_as_real_user_prompts() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-user-slash-1");
    let main_dir = session_dir.join("agents").join("main");
    let wire_path = main_dir.join("wire.jsonl");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({"workDir": "/tmp/user-slash"})).unwrap(),
    )
    .unwrap();
    let records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1}),
        json!({"type": "context.append_message", "time": 2, "message": {
            "role": "user", "content": "User activated the skill and loaded its full instructions.", "toolCalls": [],
            "origin": {"kind": "skill_activation", "trigger": "user-slash", "skillName": "obelisk", "skillArgs": "  synthesize my history  "},
        }}),
        json!({"type": "context.append_message", "time": 3, "message": {
            "role": "user", "content": "Expanded plugin command implementation.", "toolCalls": [],
            "origin": {"kind": "plugin_command", "trigger": "user-slash", "pluginId": "demo", "commandName": "ship", "commandArgs": "  --fast  "},
        }}),
        json!({"type": "context.append_message", "time": 4, "message": {
            "role": "user", "content": "Model-triggered skill instructions.", "toolCalls": [],
            "origin": {"kind": "skill_activation", "trigger": "model-tool", "skillName": "review"},
        }}),
    ];
    write_jsonl(&wire_path, &records);
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);

    let (records, _) = drain(parse(&units[0], None));
    let messages = messages_of(&records);
    let shape: Vec<(Option<&str>, bool)> = messages
        .iter()
        .map(|m| (m.text.as_deref(), m.is_meta))
        .collect();
    assert_eq!(
        shape,
        vec![
            (Some("/obelisk synthesize my history"), false),
            (Some("/demo:ship --fast"), false),
            (Some("Model-triggered skill instructions."), true),
        ]
    );
    let raw = provider.raw(&crate::providers::types::RawLookup {
        source: "kimi",
        message_uuid: &messages[0].uuid,
        session: Some(&json!({"jsonl_path": wire_path.to_string_lossy()})),
        agent_id: None,
        cursor: None,
        subagent: None,
        workflow_agent: None,
    });
    assert_eq!(
        raw.unwrap().message_text.as_deref(),
        Some("/obelisk synthesize my history")
    );
}

#[test]
fn kimi_provider_maps_protocol_1_0_embedded_tool_calls_and_results() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("session-tools-1");
    let main_dir = session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({"workDir": "/tmp/tools"})).unwrap(),
    )
    .unwrap();
    let records = [
        json!({"type": "metadata", "protocol_version": "1.0", "created_at": 1}),
        json!({"type": "context.append_message", "time": 2, "message": {
            "role": "assistant", "content": [],
            "toolCalls": [{"type": "function", "id": "legacy-call", "function": {"name": "Read", "arguments": "{\"file_path\":\"/tmp/tools/a.ts\"}"}}],
        }}),
        json!({"type": "context.append_message", "time": 3, "message": {
            "role": "tool", "content": [{"type": "text", "text": "legacy result"}], "toolCalls": [], "toolCallId": "legacy-call",
        }}),
    ];
    write_jsonl(&main_dir.join("wire.jsonl"), &records);
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);

    let (records, _) = drain(parse(&units[0], None));
    let tool_calls: Vec<&ToolCallRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::ToolCall(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "kimi:session-tools-1:main:legacy-call");
    assert_eq!(tool_calls[0].name, "Read");
    assert_eq!(tool_calls[0].file_path.as_deref(), Some("/tmp/tools/a.ts"));
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
        "kimi:session-tools-1:main:legacy-call"
    );
    assert_eq!(tool_results[0].content, "legacy result");
}

// ---- kimi-manifest.test.mjs ----
//
// The TS suite intercepts node:fs to inject mid-discovery races (a member
// added during a stat, a directory moved during the census, a wire appended
// during projection). Those hooks have no Rust equivalent; the equivalent
// stable-state behaviors (member added after discovery, tombstone witness
// revalidation, mtime-restore detection) are ported below instead.

/// tests/fixtures/kimi/manifest-session — sanitized real session shape.
const MANIFEST_STATE: &str = r#"{"id":"session_fixture_manifest","version":2,"cwd":"/tmp/kimi-manifest-fixture","createdAt":1787850634251,"updatedAt":1787850634251,"archived":false,"agents":{"main":{"homedir":"/tmp/kimi-manifest-fixture/agents/main","type":"main"}},"custom":{},"isCustomTitle":false}"#;
const MANIFEST_METADATA_LINE: &str =
    r#"{"type":"metadata","protocol_version":"1.5","created_at":1787850634278}"#;
const MANIFEST_WIRE: &str = concat!(
    r#"{"type":"metadata","protocol_version":"1.5","created_at":1787850634278}"#,
    "\n",
    r#"{"type":"runtime.set_binding","workspaceId":"wd_fixture_manifest","runtimeId":"local","agentId":"main","time":1787850634294}"#,
    "\n",
);

fn write_manifest_session(root: &Path) -> PathBuf {
    write_manifest_session_in(root, "workspace-1", "session-1")
}

fn write_manifest_session_in(root: &Path, workspace: &str, session: &str) -> PathBuf {
    let session_dir = root.join("sessions").join(workspace).join(session);
    let main_dir = session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(session_dir.join("state.json"), MANIFEST_STATE).unwrap();
    std::fs::write(main_dir.join("wire.jsonl"), MANIFEST_WIRE).unwrap();
    session_dir
}

fn indexed(session_id: &str, jsonl_path: &Path) -> IndexedSession {
    IndexedSession {
        session_id: session_id.to_string(),
        jsonl_path: jsonl_path.to_string_lossy().into_owned(),
    }
}

#[test]
fn kimi_parse_rejects_a_member_added_after_discovery() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);

    let child_dir = session_dir.join("agents").join("child-1");
    std::fs::create_dir_all(&child_dir).unwrap();
    std::fs::write(
        child_dir.join("wire.jsonl"),
        MANIFEST_METADATA_LINE.to_string() + "\n",
    )
    .unwrap();

    let error = drain_or_error(parse(&units[0], None))
        .expect_err("parse must reject the session changed between discovery and parse");
    assert!(
        error.contains("Kimi session changed while indexing"),
        "{error}"
    );
}

#[test]
fn kimi_discovery_retracts_an_indexed_session_that_loses_its_last_wire() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let main_wire = session_dir.join("agents").join("main").join("wire.jsonl");
    let provider = KimiProvider::new(root.path().to_path_buf());
    let initial = discover_all(&provider);
    assert_eq!(initial.len(), 1);
    let stored_cursor = unit_cursor(&initial[0]);
    std::fs::remove_file(&main_wire).unwrap();

    let cursors = [(initial[0].key.clone(), stored_cursor.clone())]
        .into_iter()
        .collect::<HashMap<String, String>>();
    let units = discover_with(
        &provider,
        &cursors,
        None,
        &[indexed("kimi:session-1", &main_wire)],
        &mut Vec::new(),
    );
    assert_eq!(units.len(), 1);
    let tombstone = &units[0];
    assert_eq!(tombstone.key, session_dir.to_string_lossy());
    assert_eq!(
        tombstone
            .meta
            .as_ref()
            .and_then(|meta| meta.get("wireFiles"))
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0)
    );
    assert_eq!(unit_mode(tombstone), "tombstone");
    assert_eq!(tombstone.retract_session_ids, vec!["kimi:session-1"]);

    let (records, cursor) = drain(parse(tombstone, Some(stored_cursor)));
    assert!(records.is_empty());
    assert_manifest_cursor(cursor.as_deref().unwrap());
}

#[test]
fn kimi_discovery_ignores_an_empty_session_that_was_never_indexed() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root
        .path()
        .join("sessions")
        .join("workspace-1")
        .join("empty-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("state.json"), MANIFEST_STATE).unwrap();
    let provider = KimiProvider::new(root.path().to_path_buf());
    assert!(discover_all(&provider).is_empty());
}

#[test]
fn kimi_full_replay_retracts_an_indexed_session_after_its_last_wire_disappears() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let main_wire = session_dir.join("agents").join("main").join("wire.jsonl");
    std::fs::remove_file(&main_wire).unwrap();
    let provider = KimiProvider::new(root.path().to_path_buf());

    let units = discover_with(
        &provider,
        &HashMap::new(),
        None,
        &[indexed("kimi:session-1", &main_wire)],
        &mut Vec::new(),
    );
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, session_dir.to_string_lossy());
    assert_eq!(units[0].retract_session_ids, vec!["kimi:session-1"]);
    let (records, _) = drain(parse(&units[0], None));
    assert!(records.is_empty());
}

#[test]
fn kimi_discovery_reports_a_member_that_disappears_during_snapshotting() {
    // The TS test mocks statSync; a dangling symlink reproduces the same
    // race without interception — the directory listing contains the member
    // but its stat fails with ENOENT.
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
    let provider = KimiProvider::new(root.path().to_path_buf());
    let initial = discover_all(&provider);
    assert_eq!(initial.len(), 1);

    std::fs::remove_file(&wire_path).unwrap();
    std::os::unix::fs::symlink("vanished-target.jsonl", &wire_path).unwrap();

    let cursors = [(initial[0].key.clone(), unit_cursor(&initial[0]))]
        .into_iter()
        .collect::<HashMap<String, String>>();
    let mut issues = Vec::new();
    let units = discover_with(&provider, &cursors, None, &[], &mut issues);
    assert!(units.is_empty());
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, session_dir.to_string_lossy());
    assert!(
        issues[0].error.contains("No such file or directory") || issues[0].error.contains("ENOENT"),
        "{}",
        issues[0].error
    );
}

#[test]
fn kimi_discovery_emits_a_tombstone_when_an_indexed_session_directory_is_deleted() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let main_wire = session_dir.join("agents").join("main").join("wire.jsonl");
    let provider = KimiProvider::new(root.path().to_path_buf());
    let initial = discover_all(&provider);
    assert_eq!(initial.len(), 1);
    let stored_cursor = unit_cursor(&initial[0]);
    std::fs::remove_dir_all(&session_dir).unwrap();

    let cursors = [(initial[0].key.clone(), stored_cursor.clone())]
        .into_iter()
        .collect::<HashMap<String, String>>();
    let units = discover_with(
        &provider,
        &cursors,
        None,
        &[indexed("kimi:session-1", &main_wire)],
        &mut Vec::new(),
    );
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, session_dir.to_string_lossy());
    assert_eq!(unit_mode(&units[0]), "tombstone");
    assert_eq!(units[0].retract_session_ids, vec!["kimi:session-1"]);
    let (records, _) = drain(parse(&units[0], Some(stored_cursor)));
    assert!(records.is_empty());
}

#[test]
fn kimi_discovery_fails_closed_when_two_directories_share_one_session_identity() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let main_wire = session_dir.join("agents").join("main").join("wire.jsonl");
    let duplicate_dir = root
        .path()
        .join("sessions")
        .join("workspace-2")
        .join("session-1");
    std::fs::create_dir_all(root.path().join("sessions").join("workspace-2")).unwrap();
    copy_dir_recursive(&session_dir, &duplicate_dir);

    let provider = KimiProvider::new(root.path().to_path_buf());
    let mut issues = Vec::new();
    let units = discover_with(
        &provider,
        &HashMap::new(),
        None,
        &[indexed("kimi:session-1", &main_wire)],
        &mut issues,
    );
    assert!(units.is_empty());
    assert_eq!(issues.len(), 1);
    assert!(
        issues[0]
            .error
            .contains("Multiple live Kimi session directories share identity kimi:session-1"),
        "{}",
        issues[0].error
    );
}

fn copy_dir_recursive(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}

#[test]
fn kimi_discovery_ignores_a_stale_empty_duplicate_when_one_live_identity_remains() {
    let root = tempfile::tempdir().unwrap();
    let indexed_session_dir = write_manifest_session(root.path());
    let indexed_wire = indexed_session_dir
        .join("agents")
        .join("main")
        .join("wire.jsonl");
    let live_session_dir = root
        .path()
        .join("sessions")
        .join("workspace-2")
        .join("session-1");
    std::fs::create_dir_all(root.path().join("sessions").join("workspace-2")).unwrap();
    std::fs::rename(&indexed_session_dir, &live_session_dir).unwrap();
    std::fs::create_dir_all(&indexed_session_dir).unwrap();

    let provider = KimiProvider::new(root.path().to_path_buf());
    let mut issues = Vec::new();
    let units = discover_with(
        &provider,
        &HashMap::new(),
        None,
        &[indexed("kimi:session-1", &indexed_wire)],
        &mut issues,
    );
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].key, live_session_dir.to_string_lossy());
    assert_eq!(unit_mode(&units[0]), "replay");
    assert!(issues.is_empty());
}

#[test]
fn kimi_discovery_retracts_an_old_identity_replaced_at_the_same_path() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let main_wire = session_dir.join("agents").join("main").join("wire.jsonl");

    let provider = KimiProvider::new(root.path().to_path_buf());
    let mut issues = Vec::new();
    let units = discover_with(
        &provider,
        &HashMap::new(),
        None,
        &[indexed("kimi:replaced-session", &main_wire)],
        &mut issues,
    );
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].session_id, "kimi:session-1");
    assert_eq!(units[0].retract_session_ids, vec!["kimi:replaced-session"]);
    assert!(issues.is_empty());
}

#[test]
fn kimi_changed_path_replacement_preserves_the_old_identity_live_at_another_path() {
    let root = tempfile::tempdir().unwrap();
    let old_identity_dir = write_manifest_session_in(root.path(), "workspace-2", "old-session");
    let replacement_dir = write_manifest_session_in(root.path(), "workspace-1", "new-session");
    let replacement_wire = replacement_dir
        .join("agents")
        .join("main")
        .join("wire.jsonl");

    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_with(
        &provider,
        &HashMap::new(),
        Some(vec![replacement_dir.to_string_lossy().into_owned()]),
        &[indexed("kimi:old-session", &replacement_wire)],
        &mut Vec::new(),
    );
    let shape: Vec<(String, String, Vec<String>)> = units
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
        shape,
        vec![
            (
                replacement_dir.to_string_lossy().into_owned(),
                "kimi:new-session".to_string(),
                vec![]
            ),
            (
                old_identity_dir.to_string_lossy().into_owned(),
                "kimi:old-session".to_string(),
                vec![]
            ),
        ]
    );
}

#[test]
fn kimi_tombstone_parse_rejects_an_identity_that_becomes_live_after_discovery() {
    let root = tempfile::tempdir().unwrap();
    let sessions_dir = root.path().join("sessions");
    let empty_session_dir = sessions_dir.join("workspace-2").join("session-1");
    let old_session_dir = sessions_dir.join("workspace-1").join("session-1");
    std::fs::create_dir_all(&empty_session_dir).unwrap();
    std::fs::write(empty_session_dir.join("state.json"), MANIFEST_STATE).unwrap();

    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_with(
        &provider,
        &HashMap::new(),
        None,
        &[indexed(
            "kimi:session-1",
            &old_session_dir
                .join("agents")
                .join("main")
                .join("wire.jsonl"),
        )],
        &mut Vec::new(),
    );
    assert_eq!(units.len(), 1);

    let main_dir = empty_session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        main_dir.join("wire.jsonl"),
        MANIFEST_METADATA_LINE.to_string() + "\n",
    )
    .unwrap();

    let error = drain_or_error(parse(&units[0], None))
        .expect_err("tombstone parse must reject the identity becoming live");
    assert!(
        error.contains("Kimi session identity changed while indexing"),
        "{error}"
    );
}

#[test]
fn kimi_moved_replay_rejects_a_duplicate_that_becomes_live_after_discovery() {
    let root = tempfile::tempdir().unwrap();
    let indexed_session_dir = write_manifest_session(root.path());
    let indexed_wire = indexed_session_dir
        .join("agents")
        .join("main")
        .join("wire.jsonl");
    let live_session_dir = root
        .path()
        .join("sessions")
        .join("workspace-2")
        .join("session-1");
    std::fs::create_dir_all(root.path().join("sessions").join("workspace-2")).unwrap();
    std::fs::rename(&indexed_session_dir, &live_session_dir).unwrap();
    std::fs::create_dir_all(&indexed_session_dir).unwrap();

    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_with(
        &provider,
        &HashMap::new(),
        None,
        &[indexed("kimi:session-1", &indexed_wire)],
        &mut Vec::new(),
    );
    assert_eq!(units.len(), 1);

    let main_dir = indexed_session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        main_dir.join("wire.jsonl"),
        MANIFEST_METADATA_LINE.to_string() + "\n",
    )
    .unwrap();

    let error = drain_or_error(parse(&units[0], None))
        .expect_err("moved replay must reject the duplicate becoming live");
    assert!(
        error.contains("Kimi session identity changed while indexing"),
        "{error}"
    );
}

#[test]
fn kimi_discovery_detects_an_append_even_when_mtime_is_restored() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
    let provider = KimiProvider::new(root.path().to_path_buf());
    let initial = discover_all(&provider);
    assert_eq!(initial.len(), 1);
    let before = std::fs::metadata(&wire_path).unwrap();

    let mut content = std::fs::read_to_string(&wire_path).unwrap();
    content.push_str(MANIFEST_METADATA_LINE);
    content.push('\n');
    std::fs::write(&wire_path, content).unwrap();
    let file = std::fs::File::options()
        .write(true)
        .open(&wire_path)
        .unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(before.accessed().unwrap())
            .set_modified(before.modified().unwrap()),
    )
    .unwrap();
    drop(file);

    let cursors = [(initial[0].key.clone(), unit_cursor(&initial[0]))]
        .into_iter()
        .collect::<HashMap<String, String>>();
    let units = discover_with(&provider, &cursors, None, &[], &mut Vec::new());
    assert_eq!(
        units.iter().map(|u| u.key.clone()).collect::<Vec<_>>(),
        vec![session_dir.to_string_lossy().into_owned()]
    );
}

#[test]
fn kimi_discovery_detects_a_same_size_rewrite_with_restored_mtime() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
    let provider = KimiProvider::new(root.path().to_path_buf());
    let initial = discover_all(&provider);
    assert_eq!(initial.len(), 1);
    let before = std::fs::metadata(&wire_path).unwrap();

    let original = std::fs::read_to_string(&wire_path).unwrap();
    let rewritten = original.replace("\"runtimeId\":\"local\"", "\"runtimeId\":\"focal\"");
    assert_eq!(rewritten.len(), original.len());
    std::fs::write(&wire_path, rewritten).unwrap();
    let file = std::fs::File::options()
        .write(true)
        .open(&wire_path)
        .unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(before.accessed().unwrap())
            .set_modified(before.modified().unwrap()),
    )
    .unwrap();
    drop(file);

    let cursors = [(initial[0].key.clone(), unit_cursor(&initial[0]))]
        .into_iter()
        .collect::<HashMap<String, String>>();
    let units = discover_with(&provider, &cursors, None, &[], &mut Vec::new());
    assert_eq!(
        units.iter().map(|u| u.key.clone()).collect::<Vec<_>>(),
        vec![session_dir.to_string_lossy().into_owned()]
    );
}

#[test]
fn kimi_discovery_detects_removal_of_one_member_from_a_multi_wire_session() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_manifest_session(root.path());
    let child_dir = session_dir.join("agents").join("child-1");
    std::fs::create_dir_all(&child_dir).unwrap();
    std::fs::write(
        child_dir.join("wire.jsonl"),
        MANIFEST_METADATA_LINE.to_string() + "\n",
    )
    .unwrap();
    let provider = KimiProvider::new(root.path().to_path_buf());
    let initial = discover_all(&provider);
    assert_eq!(initial.len(), 1);

    std::fs::remove_dir_all(&child_dir).unwrap();

    let cursors = [(initial[0].key.clone(), unit_cursor(&initial[0]))]
        .into_iter()
        .collect::<HashMap<String, String>>();
    let units = discover_with(&provider, &cursors, None, &[], &mut Vec::new());
    assert_eq!(
        units.iter().map(|u| u.key.clone()).collect::<Vec<_>>(),
        vec![session_dir.to_string_lossy().into_owned()]
    );
}

// ---- app-kimi-index.test.mjs (adapter-level assertions) ----

/// app-kimi-index writeSession.
fn write_index_session(root: &Path) -> PathBuf {
    let session_dir = root
        .join("sessions")
        .join("workspace-1")
        .join("session-index-1");
    let main_dir = session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({
            "title": "Indexed Kimi session",
            "workDir": "/tmp/indexed-kimi",
            "createdAt": "2026-07-20T10:00:00.000Z",
            "updatedAt": "2026-07-20T10:01:00.000Z",
            "agents": { "main": { "type": "main" } },
        }))
        .unwrap(),
    )
    .unwrap();
    let records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1753005600000i64}),
        json!({"type": "context.append_message", "time": 1753005601000i64, "message": {"role": "user", "content": [{"type": "text", "text": "kimi index needle"}], "toolCalls": [], "origin": {"kind": "user"}}}),
    ];
    write_jsonl(&main_dir.join("wire.jsonl"), &records);
    session_dir
}

/// app-kimi-index writePlaceholderSession.
fn write_placeholder_session(root: &Path, user_prompt: bool) -> PathBuf {
    let session_dir = root
        .join("sessions")
        .join("workspace-1")
        .join("session-placeholder-1");
    let main_dir = session_dir.join("agents").join("main");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::write(
        session_dir.join("state.json"),
        serde_json::to_string(&json!({
            "title": "New Session",
            "workDir": "/tmp/indexed-kimi",
            "createdAt": "2026-07-20T10:00:00.000Z",
            "updatedAt": "2026-07-20T10:00:00.000Z",
            "agents": { "main": { "type": "main" } },
        }))
        .unwrap(),
    )
    .unwrap();
    let mut records = vec![
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1753005600000i64}),
        json!({"type": "config.update", "profileName": "agent", "systemPrompt": "Kimi Code CLI"}),
        json!({"type": "tools.set_active_tools", "tools": []}),
        json!({"type": "config.update", "modelAlias": "kimi-code/kimi-for-coding"}),
    ];
    if user_prompt {
        records.push(json!({
            "type": "context.append_message", "time": 1753005601000i64,
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "real prompt before metadata catches up"}],
                "toolCalls": [], "origin": {"kind": "user"},
            },
        }));
    }
    write_jsonl(&main_dir.join("wire.jsonl"), &records);
    session_dir
}

#[test]
fn kimi_parse_excludes_never_started_placeholder_sessions() {
    let root = tempfile::tempdir().unwrap();
    write_placeholder_session(root.path(), false);
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);

    let (records, cursor) = drain(parse(&units[0], None));
    // Kimi persists a titled session before the first prompt; it stays
    // retracted until user evidence exists.
    assert_eq!(records.len(), 1);
    assert!(matches!(
        &records[0],
        TranscriptRecord::DeleteSession { session_id } if session_id == "kimi:session-placeholder-1"
    ));
    assert_manifest_cursor(cursor.as_deref().unwrap());

    let conn = open_db();
    let items = parse(&units[0], None);
    persist(&conn, &units[0], items.into_iter()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0, "placeholder session is not persisted");
}

#[test]
fn kimi_parse_keeps_a_prompted_session_while_placeholder_metadata_catches_up() {
    let root = tempfile::tempdir().unwrap();
    write_placeholder_session(root.path(), true);
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);

    let conn = open_db();
    let items = parse(&units[0], None);
    persist(&conn, &units[0], items.into_iter()).unwrap();
    let (id, title, message_count): (String, Option<String>, i64) = conn
        .query_row(
            "SELECT id, title, message_count FROM sessions WHERE source='kimi'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(id, "kimi:session-placeholder-1");
    assert_eq!(title.as_deref(), Some("New Session"));
    assert_eq!(message_count, 1);
}

#[test]
fn kimi_units_persist_and_unchanged_discovery_returns_nothing() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_index_session(root.path());
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);

    let conn = open_db();
    let mut cursors: HashMap<String, String> = HashMap::new();
    for unit in &units {
        let items = parse(unit, None);
        let cursor = persist(&conn, unit, items.into_iter())
            .unwrap()
            .expect("cursor persisted");
        cursors.insert(unit.key.clone(), cursor);
    }
    let (id, title, source, message_count): (String, Option<String>, String, i64) = conn
        .query_row(
            "SELECT id, title, source, message_count FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(id, "kimi:session-index-1");
    assert_eq!(title.as_deref(), Some("Indexed Kimi session"));
    assert_eq!(source, "kimi");
    assert_eq!(message_count, 1);
    let text: Option<String> = conn
        .query_row("SELECT text FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(text.as_deref(), Some("kimi index needle"));
    let (index_cursor,): (String,) = conn
        .query_row(
            "SELECT cursor FROM index_state WHERE jsonl_path = ?1",
            [session_dir.to_string_lossy().into_owned()],
            |row| Ok((row.get(0)?,)),
        )
        .unwrap();
    assert_manifest_cursor(&index_cursor);

    // The second build affects nothing: the stored cursor proves the session
    // unchanged (app test: affectedSessionIds == []).
    assert!(discover_with(&provider, &cursors, None, &[], &mut Vec::new()).is_empty());
}

#[test]
fn kimi_undo_and_clear_replace_the_indexed_session_instead_of_leaving_stale_rows() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = write_index_session(root.path());
    let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
    let records = [
        json!({"type": "metadata", "protocol_version": "1.5", "created_at": 1753005600000i64}),
        json!({"type": "context.append_message", "time": 1753005601000i64, "message": {"role": "user", "content": [{"type": "text", "text": "kimi index needle"}], "toolCalls": [], "origin": {"kind": "user"}}}),
    ];
    let assistant = json!({
        "type": "context.append_loop_event", "time": 1753005602000i64,
        "event": {"type": "content.part", "uuid": "answer-1", "stepUuid": "step-1", "part": {"type": "text", "text": "answer removed by undo"}},
    });

    let provider = KimiProvider::new(root.path().to_path_buf());
    let conn = open_db();
    let mut cursors: HashMap<String, String> = HashMap::new();

    let rebuild =
        |wire: &[Value], conn: &rusqlite::Connection, cursors: &mut HashMap<String, String>| {
            write_jsonl(&wire_path, wire);
            let units = discover_with(&provider, cursors, None, &[], &mut Vec::new());
            for unit in &units {
                let items = parse(unit, None);
                if let Some(cursor) = persist(conn, unit, items.into_iter()).unwrap() {
                    cursors.insert(unit.key.clone(), cursor);
                }
            }
            units.len()
        };

    // Initial build: 1 message.
    rebuild(&records, &conn, &mut cursors);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);

    // Kimi undo shrinks the durable wire transcript.
    let mut grown = records.to_vec();
    grown.push(assistant.clone());
    rebuild(&grown, &conn, &mut cursors);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let gone: i64 = count_text_like(&conn, "%removed by undo%");
    assert_eq!(gone, 1);

    rebuild(&records, &conn, &mut cursors);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    let gone: i64 = count_text_like(&conn, "%removed by undo%");
    assert_eq!(gone, 0);

    // Clear retains the session container but removes all projected messages.
    rebuild(&records[..1], &conn, &mut cursors);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let message_count: i64 = conn
        .query_row(
            "SELECT message_count FROM sessions WHERE id=?",
            ["kimi:session-index-1"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(message_count, 0);
}

fn count_text_like(conn: &rusqlite::Connection, pattern: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE text LIKE ?",
        [pattern],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn kimi_full_fixture_persists_all_record_kinds() {
    let (root, session_dir) = write_kimi_fixture();
    let provider = KimiProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);

    let conn = open_db();
    let items = parse(&units[0], None);
    persist(&conn, &units[0], items.into_iter()).unwrap();

    let (title, project, message_count, jsonl_path): (Option<String>, Option<String>, i64, String) =
        conn.query_row(
            "SELECT title, project, message_count, jsonl_path FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(title.as_deref(), Some("Kimi fixture"));
    assert_eq!(project.as_deref(), Some("-tmp-kimi-project"));
    assert_eq!(
        message_count, 4,
        "only the main wire counts toward the session"
    );
    assert_eq!(
        jsonl_path,
        session_dir
            .join("agents")
            .join("main")
            .join("wire.jsonl")
            .to_string_lossy()
            .into_owned()
    );

    let messages: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(messages, 5);
    let sidechain: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE agent_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sidechain, 1);
    let turn_duration: Option<i64> = conn
        .query_row(
            "SELECT turn_duration_ms FROM messages WHERE text='done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(turn_duration, Some(1000));
    let (name, file_path): (String, Option<String>) = conn
        .query_row("SELECT name, file_path FROM tool_calls", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(name, "Read");
    assert_eq!(file_path.as_deref(), Some("/tmp/kimi-project/a.ts"));
    let (content,): (String,) = conn
        .query_row("SELECT content FROM tool_results", [], |row| {
            Ok((row.get(0)?,))
        })
        .unwrap();
    assert!(content.contains("file body"));
    let summaries: i64 = conn
        .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))
        .unwrap();
    assert_eq!(summaries, 1);
    let (agent_id, parent_tool_use_id, agent_type): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT agent_id, parent_tool_use_id, agent_type FROM subagents",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(agent_id, "kimi:session-native-1:agent-7");
    assert_eq!(
        parent_tool_use_id.as_deref(),
        Some("kimi:session-native-1:main:call-1")
    );
    assert_eq!(agent_type.as_deref(), Some("explore"));
}

#[test]
fn kimi_provider_contract_matches_the_typescript_descriptor() {
    let root = tempfile::tempdir().unwrap();
    let provider = KimiProvider::new(root.path().to_path_buf());
    assert_eq!(provider.name(), "kimi");
    let descriptor = provider.descriptor();
    assert_eq!(descriptor.id, "kimi");
    assert_eq!(descriptor.name, "Kimi Code");
    assert_eq!(descriptor.vendor, "Moonshot AI");
    assert_eq!(descriptor.color, "#6d6afc");
    assert_eq!(
        descriptor.default_root,
        root.path().to_string_lossy().into_owned()
    );
    assert_eq!(
        provider.index_version_marker(),
        Some(KIMI_CANONICAL_TRANSCRIPT_MARKER)
    );
    let targets = provider.watch_targets(&root.path().to_string_lossy());
    assert_eq!(targets.len(), 2);
    assert_eq!(
        targets[0].kind,
        crate::providers::types::WatchTargetKind::Tree
    );
    assert_eq!(
        targets[0].path,
        root.path().join("sessions").to_string_lossy().into_owned()
    );
    assert_eq!(
        targets[1].kind,
        crate::providers::types::WatchTargetKind::File
    );
    assert_eq!(
        targets[1].path,
        root.path()
            .join("session_index.jsonl")
            .to_string_lossy()
            .into_owned()
    );
    // sessionUnitKey: the session directory, not the wire file.
    let session = IndexedSession {
        session_id: "kimi:session-1".to_string(),
        jsonl_path: root
            .path()
            .join("sessions")
            .join("workspace-1")
            .join("session-1")
            .join("agents")
            .join("main")
            .join("wire.jsonl")
            .to_string_lossy()
            .into_owned(),
    };
    assert_eq!(
        provider.session_unit_key(&session).as_deref(),
        Some(
            root.path()
                .join("sessions")
                .join("workspace-1")
                .join("session-1")
                .to_string_lossy()
                .into_owned()
                .as_str()
        )
    );
}

// TODO(session-detail): tests/kimi-parse.test.mjs asserts the assembled
// session detail excludes the sidechain child prompt and exposes the tool
// result content through tool_calls — those live in session-detail.ts and are
// not part of this adapter port; the record-level equivalents are asserted in
// kimi_provider_folds_main_and_subagent_wire_logs_into_the_canonical_transcript_language.
//
// Not ported (require node:fs interception): unchanged discovery never reads
// session bodies; a member added while its snapshot is being captured; a
// directory move during the identity census; a wire appearing during the
// second identity census; a wire append racing projection; EACCES stat
// failures. The Rust ports cover the same invariants at stable states.
