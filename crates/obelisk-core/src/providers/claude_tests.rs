// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Rust port of tests/claude-parse.test.mjs — the binding-independent claude
//! adapter contract. If per-line parse behavior drifts, this fails before
//! persist ever runs.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::claude::{parse, ClaudeProvider, NAME};
use crate::parsing::file_signature;
use crate::persist::persist;
use crate::providers::types::{
    DiscoverContext, IndexUnit, MessageRecord, ProviderAdapter, SessionRecord, StreamItem,
    SummaryRecord, ToolCallRecord, ToolResultRecord, TranscriptRecord,
};

fn write_fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sid-x.jsonl");
    let lines = [
        json!({"type": "ai-title", "aiTitle": "My Session"}),
        json!({"uuid": "u1", "type": "user", "timestamp": "2026-06-10T10:00:00Z", "cwd": "/proj", "gitBranch": "main", "message": {"role": "user", "content": "hi"}}),
        json!({"uuid": "a1", "type": "assistant", "timestamp": "2026-06-10T10:00:05Z", "message": {"role": "assistant", "model": "claude-opus", "content": [{"type": "text", "text": "ok"}, {"type": "tool_use", "id": "tc1", "name": "Read", "input": {"file_path": "/f"}}], "usage": {"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 30}}}),
        json!({"type": "system", "subtype": "turn_duration", "parentUuid": "a1", "durationMs": 1234}),
        json!({"uuid": "u2", "type": "user", "timestamp": "2026-06-10T10:00:10Z", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tc1", "content": "file body", "is_error": false}]}}),
        json!({"type": "system", "subtype": "away_summary", "uuid": "s1", "timestamp": "2026-06-10T10:00:11Z", "content": "a summary"}),
    ];
    let body: String = lines
        .iter()
        .map(|l| serde_json::to_string(l).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, body + "\n").unwrap();
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

fn discover_all(provider: &ClaudeProvider) -> Vec<IndexUnit> {
    let mut ctx = DiscoverContext {
        last_cursor: &|_key| None,
        changed_paths: None,
        indexed_sessions: None,
        report_incomplete_inventory: None,
    };
    provider.discover(&mut ctx)
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

#[test]
fn parse_yields_expected_record_stream_for_main_session() {
    let (_dir, path) = write_fixture();
    let unit = IndexUnit {
        key: path.to_string_lossy().into_owned(),
        session_id: "sid-x".into(),
        project: Some("quiet-zero".into()),
        ..Default::default()
    };
    let (records, cursor) = drain(parse(&unit, None));

    let messages = messages_of(&records);
    assert_eq!(
        messages.iter().map(|m| m.uuid.as_str()).collect::<Vec<_>>(),
        vec!["u1", "a1", "u2"]
    );
    let a1 = messages.iter().find(|m| m.uuid == "a1").unwrap();
    assert_eq!(a1.model.as_deref(), Some("claude-opus"));
    assert_eq!(a1.input_tokens, Some(60));
    assert_eq!(a1.output_tokens, Some(5));
    assert!(messages.iter().all(|m| m.source == "claude"));

    let tool_calls: Vec<&ToolCallRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::ToolCall(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "tc1");
    assert_eq!(tool_calls[0].name, "Read");

    let tool_results: Vec<&ToolResultRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::ToolResult(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0].tool_use_id, "tc1");
    assert!(!tool_results[0].is_error);

    let durations: Vec<&TranscriptRecord> = records
        .iter()
        .filter(|r| matches!(r, TranscriptRecord::MessageTurnDuration { .. }))
        .collect();
    assert_eq!(durations.len(), 1);
    assert!(matches!(
        durations[0],
        TranscriptRecord::MessageTurnDuration { uuid, turn_duration_ms: Some(1234) }
            if uuid == "a1"
    ));

    let summaries: Vec<&SummaryRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::Summary(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(
        summaries.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["s1"]
    );

    let session = session_of(&records);
    assert_eq!(session.title.as_deref(), Some("My Session"));
    assert_eq!(session.message_count, 3);
    assert_eq!(session.started_at.as_deref(), Some("2026-06-10T10:00:00Z"));
    assert_eq!(session.ended_at.as_deref(), Some("2026-06-10T10:00:10Z"));
    assert_eq!(session.git_branch.as_deref(), Some("main"));

    let (mtime, size, ctime, ino) = file_signature(&path).unwrap();
    assert_eq!(
        cursor.as_deref(),
        Some(format!("{mtime}:6:{size}:{ctime}:{ino}").as_str())
    );
}

#[test]
fn parse_emits_no_session_record_for_subagent_transcript() {
    let (_dir, path) = write_fixture();
    let unit = IndexUnit {
        key: path.to_string_lossy().into_owned(),
        session_id: "sid-x".into(),
        is_subagent: true,
        agent_id: Some("agent-7".into()),
        ..Default::default()
    };
    let (records, _) = drain(parse(&unit, None));
    assert!(records
        .iter()
        .all(|r| !matches!(r, TranscriptRecord::Session(_))));
    let messages = messages_of(&records);
    assert!(messages
        .iter()
        .all(|m| m.agent_id.as_deref() == Some("agent-7")));
}

#[test]
fn parse_resumes_from_cursor_skipping_indexed_lines() {
    let (_dir, path) = write_fixture();
    let unit = IndexUnit {
        key: path.to_string_lossy().into_owned(),
        session_id: "sid-x".into(),
        project: Some("quiet-zero".into()),
        ..Default::default()
    };
    let (records, _) = drain(parse(&unit, Some("0:6".into())));
    assert!(records
        .iter()
        .all(|r| matches!(r, TranscriptRecord::Session(_))));
    let session = session_of(&records);
    assert_eq!(session.message_count, 0);
}

#[test]
fn workflow_artifacts_persist_with_explicit_canonical_tool_edge() {
    let root = tempfile::tempdir().unwrap();
    let project_dir = root.path().join("projects").join("-proj");
    let workflow_dir = project_dir.join("sid-workflow").join("workflows");
    let workflow_agent_dir = project_dir
        .join("sid-workflow")
        .join("subagents")
        .join("workflows")
        .join("run-workflow");
    std::fs::create_dir_all(&workflow_dir).unwrap();
    std::fs::create_dir_all(&workflow_agent_dir).unwrap();
    let main_lines = [
        json!({"uuid": "assistant-workflow", "type": "assistant", "timestamp": "2026-06-10T10:00:00Z", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "workflow-tool", "name": "Workflow", "input": {}}]}}),
        json!({"uuid": "workflow-result", "type": "user", "timestamp": "2026-06-10T10:00:01Z", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "workflow-tool", "content": "run-workflow complete"}]}}),
    ];
    let body: String = main_lines
        .iter()
        .map(|l| serde_json::to_string(l).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(project_dir.join("sid-workflow.jsonl"), body + "\n").unwrap();
    std::fs::write(
        workflow_dir.join("run-workflow.json"),
        serde_json::to_string(&json!({
            "runId": "run-workflow",
            "workflowName": "Review",
            "status": "complete",
            "workflowProgress": [{"type": "workflow_agent", "agentId": "7", "phaseTitle": "review", "label": "Reviewer"}],
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        workflow_agent_dir.join("agent-7.jsonl"),
        serde_json::to_string(&json!({
            "uuid": "workflow-agent-message", "type": "user", "timestamp": "2026-06-10T10:00:00Z",
            "message": {"role": "user", "content": "review it"},
        }))
        .unwrap()
            + "\n",
    )
    .unwrap();
    std::fs::write(
        workflow_agent_dir.join("agent-7.meta.json"),
        serde_json::to_string(&json!({
            "agentType": "reviewer", "description": "Review the implementation",
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("history.jsonl"),
        serde_json::to_string(
            &json!({"sessionId": "sid-workflow", "title": "History-owned title"}),
        )
        .unwrap()
            + "\n",
    )
    .unwrap();

    let provider = ClaudeProvider::new(root.path().to_path_buf());
    let units = discover_all(&provider);
    let mut all_records = Vec::new();
    for unit in &units {
        let (records, _) = drain(parse(unit, None));
        all_records.extend(records);
    }

    let workflow = all_records
        .iter()
        .find_map(|r| match r {
            TranscriptRecord::Workflow(w) => Some(w),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        workflow.parent_tool_use_id.as_deref(),
        Some("workflow-tool")
    );

    // Persist everything into an in-memory database and verify the
    // workflow + workflow_agent rows land.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_SQL).unwrap();
    for unit in &units {
        let items = parse(unit, None);
        persist(&conn, unit, items.into_iter()).unwrap();
    }
    let (run_id, parent, name, agent_count): (String, Option<String>, Option<String>, i64) = conn
        .query_row(
            "SELECT run_id, parent_tool_use_id, workflow_name, agent_count FROM workflows",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(run_id, "run-workflow");
    assert_eq!(parent.as_deref(), Some("workflow-tool"));
    assert_eq!(name.as_deref(), Some("Review"));
    assert_eq!(agent_count, 1);
    let (agent_id, label, agent_type): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT agent_id, label, agent_type FROM workflow_agents",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(agent_id, "agent-7");
    assert_eq!(label.as_deref(), Some("Reviewer"));
    assert_eq!(agent_type.as_deref(), Some("reviewer"));
    let title: Option<String> = conn
        .query_row("SELECT title FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(title.as_deref(), Some("History-owned title"));
}

#[test]
fn links_repeated_workflow_names_by_unique_run_id() {
    let root = tempfile::tempdir().unwrap();
    let project_dir = root.path().join("projects").join("-proj");
    let workflow_dir = project_dir.join("sid-workflow").join("workflows");
    std::fs::create_dir_all(&workflow_dir).unwrap();
    let main_lines = [
        json!({"uuid": "assistant-old", "type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "old-call", "name": "Workflow", "input": {}}]}}),
        json!({"uuid": "old-result", "type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "old-call", "content": "Run ID: old-run\nSummary: same-name"}]}}),
        json!({"uuid": "assistant-new", "type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "new-call", "name": "Workflow", "input": {}}]}}),
        json!({"uuid": "new-result", "type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "new-call", "content": "Run ID: new-run\nSummary: same-name"}]}}),
    ];
    let body: String = main_lines
        .iter()
        .map(|l| serde_json::to_string(l).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(project_dir.join("sid-workflow.jsonl"), body + "\n").unwrap();
    for run_id in ["old-run", "new-run"] {
        std::fs::write(
            workflow_dir.join(format!("{run_id}.json")),
            serde_json::to_string(&json!({
                "runId": run_id, "workflowName": "same-name", "status": "completed", "workflowProgress": [],
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let provider = ClaudeProvider::new(root.path().to_path_buf());
    let workflow_units: Vec<IndexUnit> = discover_all(&provider)
        .into_iter()
        .filter(|unit| {
            unit.meta
                .as_ref()
                .and_then(|m| m.get("kind"))
                .and_then(serde_json::Value::as_str)
                == Some("workflow")
        })
        .collect();
    assert_eq!(workflow_units.len(), 2);

    let mut parent_by_run: HashMap<String, Option<String>> = HashMap::new();
    for unit in &workflow_units {
        let (records, _) = drain(parse(unit, None));
        let workflow = records
            .iter()
            .find_map(|r| match r {
                TranscriptRecord::Workflow(w) => Some(w),
                _ => None,
            })
            .unwrap();
        parent_by_run.insert(workflow.run_id.clone(), workflow.parent_tool_use_id.clone());
    }
    assert_eq!(
        parent_by_run.get("old-run").unwrap().as_deref(),
        Some("old-call")
    );
    assert_eq!(
        parent_by_run.get("new-run").unwrap().as_deref(),
        Some("new-call")
    );
}

// ---- torn-tail cursor safety (#102) ----

fn write_torn_fixture() -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sid-torn.jsonl");
    let head = [
        serde_json::to_string(&json!({"uuid": "u1", "type": "user", "timestamp": "2026-06-10T10:00:00Z", "message": {"role": "user", "content": "hi"}})).unwrap(),
        serde_json::to_string(&json!({"uuid": "a1", "type": "assistant", "timestamp": "2026-06-10T10:00:05Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}})).unwrap(),
    ];
    (dir, path, head.join("\n"))
}

const COMPLETED_TAIL: &str = r#"{"uuid":"u2","type":"user","timestamp":"2026-06-10T10:00:10Z","message":{"role":"user","content":"done"}}"#;

fn torn_unit(path: &Path) -> IndexUnit {
    IndexUnit {
        key: path.to_string_lossy().into_owned(),
        session_id: "sid-torn".into(),
        ..Default::default()
    }
}

#[test]
fn torn_unterminated_tail_does_not_advance_cursor() {
    let (_dir, path, head) = write_torn_fixture();
    std::fs::write(
        &path,
        format!("{head}\n{{\"uuid\":\"u2\",\"type\":\"user\",\"mes"),
    )
    .unwrap();
    let (_, cursor) = drain(parse(&torn_unit(&path), None));
    assert_eq!(
        cursor.unwrap().split(':').nth(1),
        Some("2"),
        "the unparseable unterminated tail is not counted"
    );
}

#[test]
fn completed_tail_is_indexed_exactly_once_after_torn_parse() {
    let (_dir, path, head) = write_torn_fixture();
    std::fs::write(
        &path,
        format!("{head}\n{{\"uuid\":\"u2\",\"type\":\"user\",\"mes"),
    )
    .unwrap();
    let (_, torn_cursor) = drain(parse(&torn_unit(&path), None));

    std::fs::write(&path, format!("{head}\n{COMPLETED_TAIL}\n")).unwrap();
    let (records, cursor) = drain(parse(&torn_unit(&path), torn_cursor));
    let u2: Vec<&MessageRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::Message(m) if m.uuid == "u2" => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(u2.len(), 1, "the completed line is indexed exactly once");
    assert_eq!(cursor.unwrap().split(':').nth(1), Some("3"));
}

#[test]
fn newline_terminated_malformed_line_advances_cursor() {
    let (_dir, path, head) = write_torn_fixture();
    std::fs::write(&path, format!("{head}\nnot-json-at-all\n")).unwrap();
    let (_, cursor) = drain(parse(&torn_unit(&path), None));
    assert_eq!(cursor.unwrap().split(':').nth(1), Some("3"));
}

#[test]
fn legal_unterminated_final_json_line_advances_cursor() {
    let (_dir, path, head) = write_torn_fixture();
    std::fs::write(&path, format!("{head}\n{COMPLETED_TAIL}")).unwrap();
    let (_, cursor) = drain(parse(&torn_unit(&path), None));
    assert_eq!(cursor.unwrap().split(':').nth(1), Some("3"));
}

// ---- cursor signature: same-millisecond recovery through discover ----

#[test]
fn same_mtime_tail_completion_is_rediscovered_through_cursor_signature() {
    let root = tempfile::tempdir().unwrap();
    let project_dir = root.path().join("projects").join("-proj");
    std::fs::create_dir_all(&project_dir).unwrap();
    let path = project_dir.join("sid-torn.jsonl");
    let head = [
        serde_json::to_string(&json!({"uuid": "u1", "type": "user", "timestamp": "2026-06-10T10:00:00Z", "message": {"role": "user", "content": "hi"}})).unwrap(),
        serde_json::to_string(&json!({"uuid": "a1", "type": "assistant", "timestamp": "2026-06-10T10:00:05Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}})).unwrap(),
    ]
    .join("\n");
    std::fs::write(
        &path,
        format!("{head}\n{{\"uuid\":\"u2\",\"type\":\"user\",\"mes"),
    )
    .unwrap();
    let provider = ClaudeProvider::new(root.path().to_path_buf());

    let units = discover_all(&provider);
    assert_eq!(units.len(), 1);
    let (_, torn_cursor) = drain(parse(&units[0], None));
    let torn_cursor = torn_cursor.unwrap();
    assert_eq!(torn_cursor.split(':').nth(1), Some("2"));

    // The writer completes the line with the mtime pinned to the parse-time
    // value — an mtime-only gate would never reselect this file.
    std::fs::write(&path, format!("{head}\n{COMPLETED_TAIL}\n")).unwrap();
    let pinned_ms: f64 = torn_cursor.split(':').next().unwrap().parse().unwrap();
    let pinned = std::time::UNIX_EPOCH + std::time::Duration::from_millis(pinned_ms as u64);
    let file = File::options().write(true).open(&path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(pinned)
            .set_modified(pinned),
    )
    .unwrap();
    drop(file);

    let mut ctx = DiscoverContext {
        last_cursor: &|_key| Some(torn_cursor.clone()),
        changed_paths: None,
        indexed_sessions: None,
        report_incomplete_inventory: None,
    };
    let rediscovered = provider.discover(&mut ctx);
    assert_eq!(
        rediscovered.len(),
        1,
        "a same-mtime completion is rediscovered"
    );
    let (records, _) = drain(parse(&rediscovered[0], Some(torn_cursor)));
    let u2: Vec<&MessageRecord> = records
        .iter()
        .filter_map(|r| match r {
            TranscriptRecord::Message(m) if m.uuid == "u2" => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(u2.len(), 1, "the completed line is indexed exactly once");
}

#[test]
fn legacy_two_part_cursor_keeps_mtime_only_gate() {
    let root = tempfile::tempdir().unwrap();
    let project_dir = root.path().join("projects").join("-proj");
    std::fs::create_dir_all(&project_dir).unwrap();
    let path = project_dir.join("sid-legacy.jsonl");
    let lines = [
        serde_json::to_string(&json!({"uuid": "u1", "type": "user", "timestamp": "2026-06-10T10:00:00Z", "message": {"role": "user", "content": "hi"}})).unwrap(),
        serde_json::to_string(&json!({"uuid": "a1", "type": "assistant", "timestamp": "2026-06-10T10:00:05Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}})).unwrap(),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    let provider = ClaudeProvider::new(root.path().to_path_buf());
    let mtime = crate::parsing::file_mtime_ms(&path).unwrap();

    let mut ctx = DiscoverContext {
        last_cursor: &|_key| Some(format!("{mtime}:2")),
        changed_paths: None,
        indexed_sessions: None,
        report_incomplete_inventory: None,
    };
    assert_eq!(
        provider.discover(&mut ctx).len(),
        0,
        "a legacy cursor at the current mtime is not reselected"
    );

    let mut ctx = DiscoverContext {
        last_cursor: &|_key| Some(format!("{}:2", mtime - 1000.0)),
        changed_paths: None,
        indexed_sessions: None,
        report_incomplete_inventory: None,
    };
    assert_eq!(
        provider.discover(&mut ctx).len(),
        1,
        "a legacy cursor behind the current mtime is reselected"
    );
}

#[test]
fn raw_returns_source_line_and_message_text() {
    let (_dir, path) = write_fixture();
    let session = json!({"id": "sid-x", "jsonl_path": path.to_string_lossy()});
    let input = crate::providers::types::RawLookup {
        source: NAME,
        message_uuid: "a1",
        session: Some(&session),
        agent_id: None,
        cursor: None,
        subagent: None,
        workflow_agent: None,
    };
    let provider = ClaudeProvider::new(PathBuf::from("/nonexistent"));
    let raw = provider.raw(&input).expect("raw record found");
    assert!(raw.text.contains("\"a1\""));
    assert_eq!(raw.message_text.as_deref(), Some("ok"));
}
