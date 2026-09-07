// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Rust port of tests/session-detail-assembly.test.mjs and
//! tests/provider-session-detail.test.mjs — the TS tests are the spec.

use crate::persist::persist;
use crate::providers::codex::parse;
use crate::providers::types::{
    IndexUnit, MessageRecord, MessageVisibility, SessionCountMode, SessionRecord, StreamItem,
    SummaryRecord, TranscriptRecord,
};
use crate::query::run_rows;
use crate::session_detail::{
    assemble_session_detail, SessionDetailRows, SessionDetailSnapshot, SessionDetailSource,
    SessionDetailWorkflowAgent,
};
use serde_json::{json, Value};

fn rows_from_json(value: Value) -> SessionDetailRows {
    serde_json::from_value(value).unwrap()
}

fn messages_text(detail: &SessionDetailSnapshot) -> Vec<String> {
    detail
        .messages
        .iter()
        .map(|message| message.text.clone().unwrap_or_default())
        .collect()
}

fn summaries_content(detail: &SessionDetailSnapshot) -> Vec<String> {
    detail
        .summaries
        .iter()
        .map(|summary| summary.content.clone())
        .collect()
}

// session-detail-assembly.test.mjs

#[test]
fn session_assembly_preserves_thinking_and_attaches_evidence() {
    let assembled = assemble_session_detail(SessionDetailSource::Rows(rows_from_json(json!({
        "messages": [
            { "uuid": "thinking-1", "timestamp": "2026-06-10T10:00:00Z",
              "type": "assistant", "content_type": "thinking", "text": "reasoning" },
            { "uuid": "answer-1", "timestamp": "2026-06-10T10:00:01Z",
              "type": "assistant", "content_type": "text", "text": "answer" },
            { "uuid": "tool-1", "timestamp": "2026-06-10T10:00:02Z",
              "type": "assistant", "content_type": "tool_use", "text": "" },
            { "uuid": "result-1", "timestamp": "2026-06-10T10:00:03Z",
              "type": "user", "content_type": "tool_result", "text": "" },
        ],
        "toolCalls": [
            { "id": "call-1", "message_uuid": "tool-1", "name": "Agent",
              "input_json": "{\"description\":\"inspect\"}" },
        ],
        "toolResults": [
            { "tool_use_id": "call-1", "message_uuid": "result-1", "content": "done", "is_error": 0 },
        ],
        "subagents": [
            { "agent_id": "agent-1", "parent_tool_use_id": "call-1",
              "agent_type": "reviewer", "description": "inspect" },
        ],
        "workflows": [],
    }))))
    .unwrap()
    .messages;

    assert_eq!(assembled.len(), 1);
    assert_eq!(assembled[0].uuid, "answer-1");
    assert_eq!(assembled[0].thinking.as_deref(), Some("reasoning"));
    let call = &assembled[0].tool_calls.as_ref().unwrap()[0];
    assert_eq!(call.result.as_ref().unwrap().content, "done");
    assert_eq!(call.subagent.as_ref().unwrap().agent_id, "agent-1");
}

#[test]
fn orphan_tool_results_stay_out_of_the_timeline() {
    let assembled = assemble_session_detail(SessionDetailSource::Rows(rows_from_json(json!({
        "messages": [
            { "uuid": "answer", "type": "assistant", "content_type": "text", "text": "answer" },
            { "uuid": "orphan-result", "type": "user", "content_type": "tool_result", "text": "" },
            { "uuid": "tool", "type": "assistant", "content_type": "tool_use", "text": "" },
        ],
        "toolCalls": [
            { "id": "call", "message_uuid": "tool", "name": "Read",
              "input_json": "{\"path\":\"/tmp/file\"}" },
        ],
        "toolResults": [
            { "tool_use_id": "missing-call", "message_uuid": "",
              "content": "orphaned failure", "is_error": 1 },
        ],
    }))))
    .unwrap()
    .messages;

    assert_eq!(assembled.len(), 1);
    assert_eq!(assembled[0].uuid, "answer");
    assert_eq!(
        assembled[0]
            .tool_calls
            .as_ref()
            .unwrap()
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        ["call"]
    );
}

#[test]
fn skill_evidence_stays_standalone_and_workflow_agents_embed() {
    let assembled = assemble_session_detail(SessionDetailSource::Rows(rows_from_json(json!({
        "messages": [
            { "uuid": "skill-1", "type": "assistant", "content_type": "tool_use", "text": "" },
            { "uuid": "skill-md", "type": "user", "content_type": "skill_instructions",
              "is_meta": 1, "text": "# Skill instructions" },
            { "uuid": "workflow-1", "type": "assistant", "content_type": "tool_use", "text": "" },
        ],
        "toolCalls": [
            { "id": "call-skill", "message_uuid": "skill-1", "name": "Skill",
              "presentation": "skill", "input_json": "{\"skill\":\"obelisk\"}" },
            { "id": "call-workflow", "message_uuid": "workflow-1", "name": "Workflow",
              "presentation": "default", "input_json": "{}" },
        ],
        "toolResults": [
            { "tool_use_id": "call-workflow", "content": "complete", "is_error": 0 },
        ],
        "subagents": [],
        "workflows": [
            {
                "run_id": "run-1",
                "parent_tool_use_id": "call-workflow",
                "workflow_name": "review",
                "status": "complete",
                "agents": [
                    { "agent_id": "agent-1", "phase": "review", "label": "Reviewer",
                      "state": "complete" },
                ],
            },
        ],
    }))))
    .unwrap()
    .messages;

    assert_eq!(
        assembled[0].skill_md.as_deref(),
        Some("# Skill instructions")
    );
    let workflow = &assembled[1].tool_calls.as_ref().unwrap()[0]
        .workflow
        .as_ref()
        .unwrap();
    assert_eq!(workflow.run_id, "run-1");
    assert_eq!(
        workflow.agents,
        vec![SessionDetailWorkflowAgent {
            agent_id: "agent-1".to_string(),
            phase: Some("review".to_string()),
            label: Some("Reviewer".to_string()),
            state: Some("complete".to_string()),
            tokens: None,
            duration_ms: None,
        }]
    );
}

#[test]
fn canonical_classification_wins_over_parsing_provider_text() {
    let detail = assemble_session_detail(SessionDetailSource::Rows(rows_from_json(json!({
        "messages": [
            { "uuid": "provider-owned-classification", "type": "user",
              "content_type": "text", "is_meta": 0,
              "text": "<system-reminder>text alone does not define presentation semantics</system-reminder>" },
        ],
    }))))
    .unwrap();

    assert!(!detail.messages[0].is_meta);
}

#[test]
fn canonical_ordering_is_stable_across_provider_and_sqlite_iteration() {
    let message = |uuid: &str, text: &str| {
        TranscriptRecord::Message(MessageRecord {
            uuid: uuid.to_string(),
            session_id: "session".to_string(),
            r#type: "user".to_string(),
            parent_uuid: None,
            timestamp: Some("2026-06-10T10:00:00Z".to_string()),
            role: Some("user".to_string()),
            text: Some(text.to_string()),
            content_type: Some("text".to_string()),
            is_meta: false,
            visibility: MessageVisibility::Visible,
            model: None,
            is_sidechain: false,
            agent_id: None,
            input_tokens: None,
            output_tokens: None,
            cwd: None,
            skill: None,
            source: "test".to_string(),
        })
    };
    let records = vec![message("b", "second"), message("a", "first")];
    let detail = assemble_session_detail(SessionDetailSource::Records(&records)).unwrap();

    assert_eq!(
        detail
            .messages
            .iter()
            .map(|message| message.uuid.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn session_detail_remains_active_only_across_visibility_values() {
    let message = |uuid: &str, visibility: MessageVisibility| {
        TranscriptRecord::Message(MessageRecord {
            uuid: uuid.to_string(),
            session_id: "session".to_string(),
            r#type: "user".to_string(),
            parent_uuid: None,
            timestamp: Some(format!("2026-06-10T10:00:0{}Z", uuid.len())),
            role: Some("user".to_string()),
            text: Some(uuid.to_string()),
            content_type: Some("text".to_string()),
            is_meta: false,
            visibility,
            model: None,
            is_sidechain: false,
            agent_id: None,
            input_tokens: None,
            output_tokens: None,
            cwd: None,
            skill: None,
            source: "pi".to_string(),
        })
    };
    let summary = |id: &str, visibility: MessageVisibility| {
        TranscriptRecord::Summary(SummaryRecord {
            id: id.to_string(),
            session_id: "session".to_string(),
            timestamp: None,
            source: "pi:branch_summary".to_string(),
            content: id.to_string(),
            visibility: Some(visibility),
            input_tokens: None,
            output_tokens: None,
        })
    };
    let records = vec![
        message("visible", MessageVisibility::Visible),
        message("inactive", MessageVisibility::Inactive),
        message("hidden", MessageVisibility::Hidden),
        summary("visible-summary", MessageVisibility::Visible),
        summary("inactive-summary", MessageVisibility::Inactive),
        summary("hidden-summary", MessageVisibility::Hidden),
    ];
    let direct = assemble_session_detail(SessionDetailSource::Records(&records)).unwrap();
    assert_eq!(messages_text(&direct), ["visible"]);
    assert_eq!(summaries_content(&direct), ["visible-summary"]);

    let message_row = |uuid: &str, visibility: &str| {
        json!({
            "uuid": uuid, "session_id": "session", "type": "user", "parent_uuid": null,
            "timestamp": format!("2026-06-10T10:00:0{}Z", uuid.len()), "role": "user",
            "text": uuid, "content_type": "text", "is_meta": 0, "visibility": visibility,
            "model": null, "is_sidechain": 0, "agent_id": null, "input_tokens": null,
            "output_tokens": null, "cwd": null, "skill": null, "source": "pi",
        })
    };
    let summary_row = |id: &str, visibility: &str| {
        json!({
            "id": id, "session_id": "session", "timestamp": null,
            "source": "pi:branch_summary", "content": id, "visibility": visibility,
            "input_tokens": null, "output_tokens": null,
        })
    };
    let persisted = assemble_session_detail(SessionDetailSource::Rows(rows_from_json(json!({
        "messages": [
            message_row("visible", "visible"),
            message_row("inactive", "inactive"),
            message_row("hidden", "hidden"),
            message_row("unknown", "future-state"),
        ],
        "summaries": [
            summary_row("visible-summary", "visible"),
            summary_row("inactive-summary", "inactive"),
            summary_row("hidden-summary", "hidden"),
            summary_row("unknown-summary", "future-state"),
        ],
    }))))
    .unwrap();
    assert_eq!(messages_text(&persisted), ["visible"]);
    assert_eq!(summaries_content(&persisted), ["visible-summary"]);
}

#[test]
fn direct_session_assembly_rejects_an_incomplete_provider_delta() {
    let records = vec![TranscriptRecord::Session(SessionRecord {
        id: "session".to_string(),
        title: None,
        project: None,
        started_at: None,
        ended_at: None,
        git_branch: None,
        version: None,
        message_count: 1,
        count_mode: SessionCountMode::Delta,
        jsonl_path: "/session.jsonl".to_string(),
        source: "test".to_string(),
    })];
    let error = assemble_session_detail(SessionDetailSource::Records(&records)).unwrap_err();
    assert!(
        error.contains("fresh full parse"),
        "unexpected error: {error}"
    );
}

// provider-session-detail.test.mjs

fn write_codex_fixture(lines: &[Value]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let body = lines
        .iter()
        .map(|line| serde_json::to_string(line).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, body + "\n").unwrap();
    (dir, path)
}

fn unit_for(path: &std::path::Path, session_id: &str) -> IndexUnit {
    IndexUnit {
        key: path.to_string_lossy().into_owned(),
        session_id: session_id.to_string(),
        ..Default::default()
    }
}

fn drain(items: Vec<StreamItem>) -> Vec<TranscriptRecord> {
    items
        .into_iter()
        .filter_map(|item| match item {
            StreamItem::Record(record) => Some(record),
            // The TS generator RETURNs the cursor; the Rust stream appends it
            // as a final item that is not a transcript record.
            StreamItem::Cursor(_) => None,
            StreamItem::Error(error) => panic!("provider error: {error}"),
        })
        .collect()
}

fn open_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_SQL).unwrap();
    conn
}

fn db_rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Value> {
    run_rows(conn, sql, &[]).unwrap()
}

#[test]
fn a_provider_record_stream_assembles_directly_into_session_detail() {
    let thread_id = "019e8951-3e7d-7343-a3e3-05bff48a317d";
    let (_dir, path) = write_codex_fixture(&[
        json!({
            "type": "session_meta", "timestamp": "2026-06-10T10:00:00Z",
            "payload": { "id": thread_id, "cwd": "/proj", "timestamp": "2026-06-10T10:00:00Z" },
        }),
        json!({
            "type": "event_msg", "timestamp": "2026-06-10T10:00:01Z",
            "payload": { "type": "user_message", "message": "inspect the repository" },
        }),
        json!({
            "type": "event_msg", "timestamp": "2026-06-10T10:00:02Z",
            "payload": { "type": "agent_message", "message": "I will inspect it." },
        }),
        json!({
            "type": "response_item", "timestamp": "2026-06-10T10:00:03Z",
            "payload": { "type": "function_call", "call_id": "call_1", "name": "shell",
                         "arguments": "{\"cmd\":\"ls\"}" },
        }),
        json!({
            "type": "response_item", "timestamp": "2026-06-10T10:00:04Z",
            "payload": { "type": "function_call_output", "call_id": "call_1", "output": "package.json" },
        }),
    ]);
    let unit = unit_for(&path, "");
    let records = drain(parse(&unit, None));
    let detail = assemble_session_detail(SessionDetailSource::Records(&records)).unwrap();

    assert_eq!(
        messages_text(&detail),
        ["inspect the repository", "I will inspect it."]
    );
    let call = &detail.messages[1].tool_calls.as_ref().unwrap()[0];
    assert_eq!(call.name, "shell");
    assert_eq!(call.result.as_ref().unwrap().content, "package.json");

    let conn = open_db();
    persist(&conn, &unit, parse(&unit, None).into_iter()).unwrap();
    let rows = SessionDetailRows {
        session: db_rows(&conn, "SELECT * FROM sessions")
            .into_iter()
            .next()
            .and_then(|row| serde_json::from_value(row).ok()),
        messages: db_rows(&conn, "SELECT * FROM messages ORDER BY timestamp, uuid")
            .into_iter()
            .map(|row| serde_json::from_value(row).unwrap())
            .collect(),
        tool_calls: db_rows(&conn, "SELECT * FROM tool_calls")
            .into_iter()
            .map(|row| serde_json::from_value(row).unwrap())
            .collect(),
        tool_results: db_rows(&conn, "SELECT * FROM tool_results")
            .into_iter()
            .map(|row| serde_json::from_value(row).unwrap())
            .collect(),
        ..Default::default()
    };
    let persisted = assemble_session_detail(SessionDetailSource::Rows(rows)).unwrap();
    assert_eq!(persisted, detail);
}

#[test]
fn codex_token_count_without_usage_remains_persistable() {
    let thread_id = "019e8951-3e7d-7343-a3e3-05bff48a3180";
    let (_dir, path) = write_codex_fixture(&[
        json!({
            "type": "session_meta", "timestamp": "2026-07-14T12:21:21.000Z",
            "payload": { "id": thread_id, "cwd": "/tmp/demo", "timestamp": "2026-07-14T12:21:21.000Z" },
        }),
        json!({
            "type": "event_msg", "timestamp": "2026-07-14T12:21:30.000Z",
            "payload": { "type": "agent_message", "message": "hello" },
        }),
        json!({
            "type": "event_msg", "timestamp": "2026-07-14T12:21:31.000Z",
            "payload": { "type": "token_count", "info": null },
        }),
    ]);
    let unit = unit_for(&path, &format!("codex:{thread_id}"));
    let conn = open_db();
    persist(&conn, &unit, parse(&unit, None).into_iter()).unwrap();
    let usage: (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT input_tokens, output_tokens FROM messages WHERE text = 'hello'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(usage, (None, None));
}

#[test]
fn provider_classified_hidden_context_never_reaches_session_detail() {
    let thread_id = "019e8951-3e7d-7343-a3e3-05bff48a317e";
    let (_dir, path) = write_codex_fixture(&[
        json!({
            "type": "session_meta", "timestamp": "2026-06-10T10:00:00Z",
            "payload": { "id": thread_id, "cwd": "/proj", "timestamp": "2026-06-10T10:00:00Z" },
        }),
        json!({
            "type": "response_item", "timestamp": "2026-06-10T10:00:01Z",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text",
                              "text": "<environment_context>\n  <cwd>/proj</cwd>\n</environment_context>" }],
            },
        }),
        json!({
            "type": "response_item", "timestamp": "2026-06-10T10:00:02Z",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text",
                              "text": "<codex_internal_context source=\"goal\">\nsecret state\n</codex_internal_context>" }],
            },
        }),
        json!({
            "type": "event_msg", "timestamp": "2026-06-10T10:00:03Z",
            "payload": { "type": "user_message", "message": "show the actual request" },
        }),
    ]);
    let records = drain(parse(&unit_for(&path, ""), None));
    let detail = assemble_session_detail(SessionDetailSource::Records(&records)).unwrap();

    assert_eq!(messages_text(&detail), ["show the actual request"]);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record,
                TranscriptRecord::Message(message)
                    if message.visibility == MessageVisibility::Hidden
            ))
            .count(),
        2
    );
    let session = records
        .iter()
        .find_map(|record| match record {
            TranscriptRecord::Session(session) => Some(session),
            _ => None,
        })
        .unwrap();
    assert_eq!(session.message_count, 1);
}

#[test]
fn provider_normalization_removes_only_structural_image_wrappers() {
    let thread_id = "019e8951-3e7d-7343-a3e3-05bff48a317f";
    let (_dir, path) = write_codex_fixture(&[
        json!({
            "type": "session_meta", "timestamp": "2026-06-10T10:00:00Z",
            "payload": { "id": thread_id, "cwd": "/proj", "timestamp": "2026-06-10T10:00:00Z" },
        }),
        json!({
            "type": "event_msg", "timestamp": "2026-06-10T10:00:01Z",
            "payload": { "type": "user_message", "message": "look at this screenshot" },
        }),
        json!({
            "type": "response_item", "timestamp": "2026-06-10T10:00:01Z",
            "payload": {
                "type": "message", "role": "user",
                "content": [
                    { "type": "input_text", "text": "look at this screenshot" },
                    { "type": "input_text", "text": "<image>" },
                    { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
                    { "type": "input_text", "text": "</image>" },
                ],
            },
        }),
    ]);
    let records = drain(parse(&unit_for(&path, ""), None));
    let detail = assemble_session_detail(SessionDetailSource::Records(&records)).unwrap();

    assert_eq!(messages_text(&detail), ["look at this screenshot"]);
}

#[test]
fn canonical_visibility_survives_persistence_before_row_assembly() {
    let thread_id = "019e8951-3e7d-7343-a3e3-05bff48a3180";
    let (_dir, path) = write_codex_fixture(&[
        json!({
            "type": "session_meta", "timestamp": "2026-06-10T10:00:00Z",
            "payload": { "id": thread_id, "cwd": "/proj", "timestamp": "2026-06-10T10:00:00Z" },
        }),
        json!({
            "type": "response_item", "timestamp": "2026-06-10T10:00:01Z",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": "<environment_context>hidden</environment_context>" }],
            },
        }),
        json!({
            "type": "event_msg", "timestamp": "2026-06-10T10:00:02Z",
            "payload": { "type": "user_message", "message": "visible request" },
        }),
    ]);
    let conn = open_db();
    let unit = unit_for(&path, "");
    persist(&conn, &unit, parse(&unit, None).into_iter()).unwrap();
    let messages = db_rows(&conn, "SELECT * FROM messages ORDER BY timestamp, uuid");
    assert_eq!(messages[0].get("visibility"), Some(&json!("hidden")));
    let rows = SessionDetailRows {
        messages: messages
            .into_iter()
            .map(|row| serde_json::from_value(row).unwrap())
            .collect(),
        ..Default::default()
    };
    let assembled = assemble_session_detail(SessionDetailSource::Rows(rows)).unwrap();

    assert_eq!(messages_text(&assembled), ["visible request"]);
}

#[test]
fn provider_normalization_classifies_skill_instructions_before_assembly() {
    let thread_id = "019e8951-3e7d-7343-a3e3-05bff48a3181";
    let (_dir, path) = write_codex_fixture(&[
        json!({
            "type": "session_meta", "timestamp": "2026-06-10T10:00:00Z",
            "payload": { "id": thread_id, "cwd": "/proj", "timestamp": "2026-06-10T10:00:00Z" },
        }),
        json!({
            "type": "response_item", "timestamp": "2026-06-10T10:00:01Z",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text",
                              "text": "Base directory for this skill: /tmp/skill\n# Instructions" }],
            },
        }),
    ]);
    let records = drain(parse(&unit_for(&path, ""), None));
    let message = records
        .iter()
        .find_map(|record| match record {
            TranscriptRecord::Message(message) => Some(message),
            _ => None,
        })
        .unwrap();

    assert_eq!(message.content_type.as_deref(), Some("skill_instructions"));
    assert!(message.is_meta);
    assert_eq!(message.visibility, MessageVisibility::Visible);
}
