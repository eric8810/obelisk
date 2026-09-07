// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Rust port of tests/deepseek-tree.test.mjs — the root-tree two-path
//! architecture contract (ADR-0011). Real artifacts live in
//! tests/fixtures/deepseek; synthetic fixtures cover the state transitions.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::Connection;
use serde_json::{json, Value};

use super::deepseek::{parse, DeepseekProvider, NAME};
use crate::persist::persist;
use crate::providers::types::{
    DiscoverContext, IndexUnit, IndexedSession, InventoryIssue, MessageRecord, ProviderAdapter,
    RawLookup, SessionCountMode, SessionRecord, StreamItem, SubagentRecord, ToolCallRecord,
    ToolResultRecord, TranscriptRecord,
};

// ---- fixture construction ----

fn header() -> Value {
    json!({
        "type": "session", "version": 0, "id": "root-session-1",
        "createdAt": 1753005600000i64, "cwd": "/tmp/dsh-project",
        "delegationDepth": 0, "agentPreset": "standard",
    })
}

fn child_header() -> Value {
    json!({
        "type": "session", "version": 0, "id": "child-session-1",
        "createdAt": 1753005604200i64, "cwd": "/tmp/dsh-project",
        "parentSession": "root-session-1", "origin": "subagent", "delegationDepth": 1,
    })
}

/// One checksummed zstd frame per append batch (like the real backend).
fn mk_frame(lines: &[Value]) -> Vec<u8> {
    let body = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.write_all(body.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

fn frames_bytes(frames: &[Vec<Value>], zstd: bool) -> Vec<u8> {
    if zstd {
        frames.iter().flat_map(|frame| mk_frame(frame)).collect()
    } else {
        (frames
            .iter()
            .flatten()
            .map(|event| event.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n")
            .into_bytes()
    }
}

fn session_path(dir: &Path, zstd: bool) -> PathBuf {
    dir.join(if zstd {
        "session.jsonl.zstd"
    } else {
        "session.jsonl"
    })
}

fn write_member(dir: &Path, frames: &[Vec<Value>], zstd: bool) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(session_path(dir, zstd), frames_bytes(frames, zstd)).unwrap();
}

/// Write via tmp + rename: a NEW inode, like upstream rotation.
fn write_member_renamed(dir: &Path, frames: &[Vec<Value>], zstd: bool) {
    std::fs::create_dir_all(dir).unwrap();
    let path = session_path(dir, zstd);
    let tmp = path.with_extension("zstd.tmp");
    std::fs::write(&tmp, frames_bytes(frames, zstd)).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
}

// A realistic root+child event set (mirrors the TS fixture exactly).
fn root_frames() -> Vec<Vec<Value>> {
    vec![
        vec![header()],
        vec![
            json!({"type": "request/header", "seq": 0, "time": 1753005600100i64, "data": {"header": {"config": {"provider": "deepseek-official", "model": "deepseek-v4-flash"}}, "reason": "initial"}}),
            json!({"type": "user/message", "seq": 1, "time": 1753005601000i64, "data": {"content": [{"type": "text", "text": "inspect the project"}], "source": {"kind": "user"}, "role": "user", "id": "msg-1"}}),
        ],
        vec![
            json!({"type": "assistant/message", "seq": 2, "time": 1753005602000i64, "data": {
                "turn": 1, "step": 1,
                "message": {"role": "assistant", "content": [
                    {"type": "reasoning", "text": "think step"},
                    {"type": "text", "text": "doing it"},
                    {"type": "tool-call", "id": "call-1", "name": "read", "arguments": "{\"file_path\":\"/tmp/dsh-project/a.ts\"}"},
                ], "source": {"kind": "model", "provider": "deepseek-official", "model": "deepseek-v4-flash"}, "id": "msg-2"},
                "usage": {"inputTokens": 10, "outputTokens": 4, "cacheReadTokens": 3},
            }}),
        ],
        // A packed chunk row between the assistant message and the durable call.
        vec![
            json!({"type": "text-chunks", "seq0": 50, "time0": 1753005602050i64, "data": {"turn": 1, "step": 1, "index": 0, "dt": [3, 3], "texts": ["d", "o", "i"]}}),
        ],
        vec![
            json!({"type": "tool/call", "seq": 4, "time": 1753005602100i64, "data": {"turn": 1, "step": 1, "callId": "call-1", "name": "read", "arguments": "{\"file_path\":\"/tmp/dsh-project/a.ts\"}"}}),
        ],
        vec![
            json!({"type": "tool/result", "seq": 5, "time": 1753005602500i64, "data": {
                "turn": 1, "step": 1,
                "message": {"source": {"kind": "tool", "callId": "call-1"}, "content": [{"type": "tool-result", "toolCallId": "call-1", "content": [{"type": "text", "text": "file body"}]}], "role": "user", "id": "msg-3"},
            }}),
            json!({"type": "user/message", "seq": 6, "time": 1753005603000i64, "data": {"content": [{"type": "text", "text": "<system-reminder>injected</system-reminder>"}], "source": {"kind": "plugin", "plugin": "x"}, "role": "user", "id": "msg-4"}}),
        ],
        vec![
            json!({"type": "assistant/message", "seq": 7, "time": 1753005604000i64, "data": {
                "turn": 1, "step": 2,
                "message": {"role": "assistant", "content": [{"type": "tool-call", "id": "call-2", "name": "subagent", "arguments": "{\"prompt\":\"review the code\"}"}], "source": {"kind": "model", "model": "deepseek-v4-flash"}, "id": "msg-5"},
                "usage": {"inputTokens": 5, "outputTokens": 1},
            }}),
            json!({"type": "tool/call", "seq": 8, "time": 1753005604050i64, "data": {"turn": 1, "step": 2, "callId": "call-2", "name": "subagent", "arguments": "{\"prompt\":\"review the code\"}"}}),
        ],
        vec![
            json!({"type": "tool/result", "seq": 9, "time": 1753005604100i64, "data": {
                "turn": 1, "step": 2,
                "message": {"source": {"kind": "tool", "callId": "call-2"}, "content": [{"type": "tool-result", "toolCallId": "call-2", "content": [{"type": "text", "text": "started subagent child-session-1"}]}], "role": "user", "id": "msg-6"},
            }}),
            json!({"type": "assistant/message", "seq": 10, "time": 1753005605000i64, "data": {
                "turn": 1, "step": 3,
                "message": {"role": "assistant", "content": [{"type": "text", "text": "final answer"}], "source": {"kind": "model", "model": "deepseek-v4-flash"}, "id": "msg-7"},
                "usage": {"inputTokens": 2, "outputTokens": 2},
            }}),
            json!({"type": "session/title", "seq": 11, "time": 1753005605100i64, "data": {"title": "Fixture title", "messageSeqs": [1], "source": {"kind": "fallback"}}}),
        ],
    ]
}

fn child_frames() -> Vec<Vec<Value>> {
    vec![
        vec![child_header()],
        vec![
            json!({"type": "subagent/descriptor", "seq": 0, "time": 1753005604200i64, "data": {"version": 2, "mode": "continuable", "provider": "spawn", "label": "review helper", "agentProvider": "deepseek-official", "agentModel": "deepseek-v4-flash"}}),
            json!({"type": "user/message", "seq": 1, "time": 1753005604300i64, "data": {"content": [{"type": "text", "text": "review the code"}], "source": {"kind": "user"}, "role": "user", "id": "msg-c1"}}),
        ],
        vec![
            json!({"type": "assistant/message", "seq": 2, "time": 1753005605000i64, "data": {
                "turn": 1, "step": 1,
                "message": {"role": "assistant", "content": [{"type": "reasoning", "text": "child think"}, {"type": "text", "text": "child done"}], "source": {"kind": "model", "model": "deepseek-v4-flash"}, "id": "msg-c2"},
                "usage": {"inputTokens": 20, "outputTokens": 5},
            }}),
        ],
    ]
}

fn project_dir(sessions_dir: &Path) -> PathBuf {
    sessions_dir.join("--tmp-dsh-project--")
}

fn write_tree(dir: &Path, root_frame_count: usize, zstd: bool) -> PathBuf {
    let sessions_dir = dir.join("sessions");
    let root_frames = root_frames();
    write_member(
        &project_dir(&sessions_dir).join("root-session-1"),
        &root_frames[..root_frame_count.min(root_frames.len())],
        zstd,
    );
    write_member(
        &project_dir(&sessions_dir).join("child-session-1"),
        &child_frames(),
        zstd,
    );
    sessions_dir
}

// ---- harness helpers ----

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

fn discover_with(
    provider: &DeepseekProvider,
    last_cursor: &dyn Fn(&str) -> Option<String>,
    changed_paths: Option<Vec<String>>,
    indexed: &[(String, String)],
    issues: &mut Vec<InventoryIssue>,
) -> Vec<IndexUnit> {
    let indexed = indexed.to_vec();
    let indexed_fn = move || {
        indexed
            .iter()
            .map(|(session_id, jsonl_path)| IndexedSession {
                session_id: session_id.clone(),
                jsonl_path: jsonl_path.clone(),
            })
            .collect::<Vec<_>>()
    };
    let mut report = |issue: InventoryIssue| issues.push(issue);
    let mut ctx = DiscoverContext {
        last_cursor,
        changed_paths: changed_paths.as_deref(),
        indexed_sessions: Some(&indexed_fn),
        report_incomplete_inventory: Some(&mut report),
    };
    provider.discover(&mut ctx)
}

fn discover_all(provider: &DeepseekProvider) -> Vec<IndexUnit> {
    discover_with(provider, &|_| None, None, &[], &mut Vec::new())
}

fn fresh_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::schema::SCHEMA_SQL).unwrap();
    conn
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// Comparable whole-database dump (mirrors the TS dumpDb shape).
fn dump(conn: &Connection) -> Vec<String> {
    let queries = [
        "SELECT id, title, project, started_at, ended_at, CAST(message_count AS TEXT), source FROM sessions ORDER BY id",
        "SELECT uuid, session_id, type, parent_uuid, timestamp, role, text, content_type, CAST(is_meta AS TEXT), model, CAST(is_sidechain AS TEXT), agent_id, CAST(input_tokens AS TEXT), CAST(output_tokens AS TEXT) FROM messages ORDER BY uuid",
        "SELECT id, message_uuid, session_id, name, presentation, input_json, file_path FROM tool_calls ORDER BY id",
        "SELECT tool_use_id, message_uuid, session_id, content, CAST(is_error AS TEXT) FROM tool_results ORDER BY tool_use_id",
        "SELECT agent_id, session_id, parent_tool_use_id, agent_type, description FROM subagents ORDER BY agent_id",
    ];
    let mut out = Vec::new();
    for sql in queries {
        let mut stmt = conn.prepare(sql).unwrap();
        let columns = stmt.column_count();
        let rows = stmt
            .query_map([], |row| {
                let mut cols = Vec::with_capacity(columns);
                for i in 0..columns {
                    let value: Option<String> = row.get(i).unwrap();
                    cols.push(value.unwrap_or_else(|| "NULL".into()));
                }
                Ok(cols.join("|"))
            })
            .unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }
    out
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

fn session_of(records: &[TranscriptRecord]) -> &SessionRecord {
    records
        .iter()
        .find_map(|record| match record {
            TranscriptRecord::Session(session) => Some(session),
            _ => None,
        })
        .unwrap()
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

fn subagents_of(records: &[TranscriptRecord]) -> Vec<&SubagentRecord> {
    records
        .iter()
        .filter_map(|record| match record {
            TranscriptRecord::Subagent(subagent) => Some(subagent),
            _ => None,
        })
        .collect()
}

/// Production-shaped cursor store: index_state is keyed by unit.key.
#[derive(Default)]
struct CursorStore(HashMap<String, Option<String>>);

/// Identity is scope-derived, so derive the expected ids from a real
/// discovery probe instead of hardcoding a hash.
fn tree_ids() -> (String, String) {
    static IDS: OnceLock<(String, String)> = OnceLock::new();
    IDS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let provider = DeepseekProvider::with_root(write_tree(dir.path(), usize::MAX, true));
        let unit = discover_all(&provider).remove(0);
        let child_id = unit
            .meta
            .as_ref()
            .and_then(|meta| meta.get("members"))
            .and_then(Value::as_array)
            .and_then(|members| {
                members
                    .iter()
                    .find(|m| m.get("isSubagent").and_then(Value::as_bool) == Some(true))
                    .and_then(|m| m.get("agentId"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap();
        (unit.session_id, child_id)
    })
    .clone()
}

#[test]
fn discovers_one_unit_per_root_tree_and_skips_unchanged_tree() {
    let (root_id, _) = tree_ids();
    let dir = tempfile::tempdir().unwrap();
    let provider = DeepseekProvider::with_root(write_tree(dir.path(), usize::MAX, true));

    let units = discover_all(&provider);
    assert_eq!(units.len(), 1); // root + child = ONE unit
    assert_eq!(units[0].session_id, root_id);
    assert_eq!(units[0].project.as_deref(), Some("-tmp-dsh-project"));

    let (_, cursor) = drain(parse(&units[0], None));
    // unchanged tree → skipped on the next discovery
    let again = discover_with(&provider, &|_| cursor.clone(), None, &[], &mut Vec::new());
    assert!(again.is_empty());

    // A change in the CHILD file brings the whole tree unit back.
    let dir2 = tempfile::tempdir().unwrap();
    let sessions_dir2 = write_tree(dir2.path(), usize::MAX, true);
    let provider2 = DeepseekProvider::with_root(sessions_dir2.clone());
    let unit2 = discover_all(&provider2).remove(0);
    let (_, cursor2) = drain(parse(&unit2, None));
    let child_path = project_dir(&sessions_dir2)
        .join("child-session-1")
        .join("session.jsonl.zstd");
    let extra = mk_frame(&[json!({
        "type": "user/message", "seq": 3, "time": 1753005606000i64,
        "data": {"content": [{"type": "text", "text": "more"}], "source": {"kind": "user"}, "role": "user", "id": "msg-c3"}
    })]);
    let mut bytes = std::fs::read(&child_path).unwrap();
    bytes.extend_from_slice(&extra);
    std::fs::write(&child_path, bytes).unwrap();
    let again2 = discover_with(&provider2, &|_| cursor2.clone(), None, &[], &mut Vec::new());
    assert_eq!(again2.len(), 1, "changed child must rediscover the tree");
}

#[test]
fn projects_whole_tree_into_canonical_records_with_correct_linkage() {
    let (root_id, child_id) = tree_ids();
    let dir = tempfile::tempdir().unwrap();
    let provider = DeepseekProvider::with_root(write_tree(dir.path(), usize::MAX, true));
    let unit = discover_all(&provider).remove(0);
    let (records, _) = drain(parse(&unit, None));
    let messages = messages_of(&records);

    let texts: Vec<(Option<&str>, bool, bool)> = messages
        .iter()
        .filter(|m| m.role.as_deref() == Some("user"))
        .map(|m| (m.text.as_deref(), m.is_meta, m.is_sidechain))
        .collect();
    assert_eq!(
        texts,
        vec![
            (Some("inspect the project"), false, false),
            (
                Some("<system-reminder>injected</system-reminder>"),
                true,
                false
            ),
            (Some("review the code"), false, true),
        ]
    );
    // child messages fold into the root session as sidechain rows
    for message in messages.iter().filter(|m| m.is_sidechain) {
        assert_eq!(message.session_id, root_id);
        assert_eq!(message.agent_id.as_deref(), Some(child_id.as_str()));
    }

    // usage counted once, cacheRead included
    let doing = messages
        .iter()
        .find(|m| m.text.as_deref() == Some("doing it"))
        .unwrap();
    assert_eq!(doing.input_tokens, Some(13));
    assert_eq!(doing.output_tokens, Some(4));
    let thinking = messages
        .iter()
        .find(|m| {
            m.content_type.as_deref() == Some("thinking") && m.text.as_deref() == Some("think step")
        })
        .unwrap();
    assert_eq!(thinking.input_tokens, None);

    // tool linkage: lowercase file tools get file_path; anchors exist
    let calls = tool_calls_of(&records);
    let call_shape: Vec<(&str, Option<&str>, &str)> = calls
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.file_path.as_deref(),
                c.presentation.as_str(),
            )
        })
        .collect();
    assert_eq!(
        call_shape,
        vec![
            ("read", Some("/tmp/dsh-project/a.ts"), "default"),
            ("subagent", None, "default"),
        ]
    );
    let uuids: HashSet<&str> = messages.iter().map(|m| m.uuid.as_str()).collect();
    for record in &records {
        match record {
            TranscriptRecord::ToolCall(call) => {
                assert!(
                    uuids.contains(call.message_uuid.as_str()),
                    "dangling {}",
                    call.message_uuid
                );
            }
            TranscriptRecord::ToolResult(result) => {
                let anchor = result.message_uuid.as_deref().unwrap();
                assert!(uuids.contains(anchor), "dangling {anchor}");
            }
            TranscriptRecord::Message(message) => {
                assert_ne!(message.parent_uuid.as_deref(), Some(message.uuid.as_str()));
            }
            _ => {}
        }
    }

    // the subagent row merges both sides of the delegation into ONE record
    let subs = subagents_of(&records);
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].agent_id, child_id);
    assert_eq!(
        subs[0].parent_tool_use_id.as_deref(),
        Some(format!("{root_id}:call-2").as_str())
    );
    assert_eq!(subs[0].agent_type.as_deref(), Some("deepseek-official"));
    assert_eq!(subs[0].description.as_deref(), Some("review helper"));

    let session = session_of(&records);
    assert_eq!(session.id, root_id);
    assert_eq!(session.title.as_deref(), Some("Fixture title"));
    assert_eq!(session.count_mode, SessionCountMode::Total);
    assert_eq!(session.version.as_deref(), Some("0"));
}

#[test]
fn two_phase_incremental_parse_converges_at_every_frame_boundary() {
    let total_frames = root_frames().len();
    for split in 0..=total_frames {
        let dir_full = tempfile::tempdir().unwrap();
        let provider_full =
            DeepseekProvider::with_root(write_tree(dir_full.path(), usize::MAX, true));
        let db_full = fresh_db();
        let unit_full = discover_all(&provider_full).remove(0);
        persist(&db_full, &unit_full, parse(&unit_full, None).into_iter()).unwrap();

        let dir_split = tempfile::tempdir().unwrap();
        let sessions_dir = write_tree(dir_split.path(), split, true);
        let provider = DeepseekProvider::with_root(sessions_dir);
        let db = fresh_db();
        // split 0 leaves the root headerless: the project suppresses this
        // round (fail closed), so phase 1 may legitimately emit nothing.
        let phase1 = discover_all(&provider);
        assert!(
            phase1.len() == 1 || split == 0,
            "split {split}: phase 1 discovers the tree"
        );
        let cursor = if phase1.len() == 1 {
            persist(&db, &phase1[0], parse(&phase1[0], None).into_iter()).unwrap()
        } else {
            None
        };
        write_tree(dir_split.path(), usize::MAX, true); // append remaining frames
        let phase2 = discover_with(&provider, &|_| cursor.clone(), None, &[], &mut Vec::new());
        if phase2.len() == 1 {
            persist(&db, &phase2[0], parse(&phase2[0], cursor).into_iter()).unwrap();
        } else {
            assert_eq!(
                split, total_frames,
                "split {split}: only the no-change case may skip phase 2"
            );
        }
        assert_eq!(
            dump(&db),
            dump(&db_full),
            "split {split}: database state must equal the full parse"
        );
    }
}

#[test]
fn snapshot_fallback_retracts_stale_rows_on_truncation_and_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = write_tree(dir.path(), usize::MAX, true);
    let provider = DeepseekProvider::with_root(sessions_dir.clone());
    let db = fresh_db();
    let unit = discover_all(&provider).remove(0);
    let cursor = persist(&db, &unit, parse(&unit, None).into_iter()).unwrap();
    let before_messages = count(&db, "SELECT COUNT(*) FROM messages");
    assert!(before_messages > 0);

    // Truncation: drop the last three root frames.
    let root_dir = project_dir(&sessions_dir).join("root-session-1");
    let frames = root_frames();
    write_member(&root_dir, &frames[..frames.len() - 3], true);
    let unit2 = discover_with(&provider, &|_| cursor.clone(), None, &[], &mut Vec::new()).remove(0);
    let cursor2 = persist(&db, &unit2, parse(&unit2, cursor).into_iter()).unwrap();
    assert!(
        count(&db, "SELECT COUNT(*) FROM messages") < before_messages,
        "truncated rows retracted"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE text='final answer'"
        ),
        0
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM subagents WHERE parent_tool_use_id IS NOT NULL"
        ),
        0,
        "spawn link from removed frames is gone"
    );
    assert!(
        count(&db, "SELECT COUNT(*) FROM messages WHERE is_sidechain=1") > 0,
        "child sidechain data intact"
    );

    // Replacement with a new inode and MORE frames: full reparse, no splice.
    let mut grown: Vec<Vec<Value>> = frames[..4].to_vec();
    grown.push(vec![json!({
        "type": "user/message", "seq": 90, "time": 1753005700000i64,
        "data": {"content": [{"type": "text", "text": "BRAND_NEW"}], "source": {"kind": "user"}, "role": "user", "id": "msg-90"}
    })]);
    write_member_renamed(&root_dir, &grown, true);
    let unit3 =
        discover_with(&provider, &|_| cursor2.clone(), None, &[], &mut Vec::new()).remove(0);
    persist(&db, &unit3, parse(&unit3, cursor2).into_iter()).unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM messages WHERE text='BRAND_NEW'"),
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM tool_results WHERE content LIKE '%file body%'"
        ),
        0,
        "old content does not survive a replacement"
    );
}

#[test]
fn identity_change_in_root_header_retracts_old_session() {
    let (root_id, _) = tree_ids();
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = write_tree(dir.path(), usize::MAX, true);
    let provider = DeepseekProvider::with_root(sessions_dir.clone());
    let db = fresh_db();
    let unit = discover_all(&provider).remove(0);
    let cursor = persist(&db, &unit, parse(&unit, None).into_iter()).unwrap();

    // Rewrite BOTH headers with a different cwd (new scope → new identity).
    let mut frames = root_frames();
    frames[0] = vec![
        json!({"type": "session", "version": 0, "id": "root-session-1",
        "createdAt": 1753005600000i64, "cwd": "/tmp/other-project", "delegationDepth": 0}),
    ];
    let mut child = child_frames();
    child[0] = vec![
        json!({"type": "session", "version": 0, "id": "child-session-1",
        "createdAt": 1753005604200i64, "cwd": "/tmp/other-project",
        "parentSession": "root-session-1", "origin": "subagent", "delegationDepth": 1}),
    ];
    write_member_renamed(
        &project_dir(&sessions_dir).join("root-session-1"),
        &frames,
        true,
    );
    write_member_renamed(
        &project_dir(&sessions_dir).join("child-session-1"),
        &child,
        true,
    );

    let unit2 = discover_with(&provider, &|_| cursor.clone(), None, &[], &mut Vec::new()).remove(0);
    persist(&db, &unit2, parse(&unit2, cursor).into_iter()).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 1);
    assert_ne!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM sessions WHERE id='{}'",
                root_id.replace('\'', "''")
            )
        ),
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM sessions WHERE project='-tmp-other-project'"
        ),
        1
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM messages WHERE session_id='{}'",
                root_id.replace('\'', "''")
            )
        ),
        0,
        "old-identity rows retracted"
    );
}

#[test]
fn discovery_emits_tombstone_for_disappeared_session() {
    let (root_id, _) = tree_ids();
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = write_tree(dir.path(), usize::MAX, true);
    let provider = DeepseekProvider::with_root(sessions_dir.clone());
    let db = fresh_db();
    let unit = discover_all(&provider).remove(0);
    let cursor = persist(&db, &unit, parse(&unit, None).into_iter()).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 1);

    std::fs::remove_dir_all(project_dir(&sessions_dir)).unwrap();
    let units = discover_with(
        &provider,
        &|_| cursor.clone(),
        None,
        &[(root_id.clone(), unit.key.clone())],
        &mut Vec::new(),
    );
    let tombstone = units
        .iter()
        .find(|u| u.retract_session_ids.iter().any(|id| id == &root_id))
        .expect("tombstone unit emitted");
    persist(&db, tombstone, parse(tombstone, cursor).into_iter()).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), 0);
}

#[test]
fn parses_real_sanitized_dsh_artifacts_end_to_end() {
    let fixture_root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/deepseek/sessions");
    let provider = DeepseekProvider::with_root(fixture_root);
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1); // the real root+child pair is one tree
    let unit = &units[0];
    let (records, _) = drain(parse(unit, None));
    let messages = messages_of(&records);
    assert!(!messages.is_empty());
    assert!(!tool_calls_of(&records).is_empty());
    assert!(!tool_results_of(&records).is_empty());
    assert_eq!(subagents_of(&records).len(), 1);
    let uuids: HashSet<&str> = messages.iter().map(|m| m.uuid.as_str()).collect();
    for record in &records {
        match record {
            TranscriptRecord::ToolCall(call) => {
                assert!(
                    uuids.contains(call.message_uuid.as_str()),
                    "dangling {}",
                    call.message_uuid
                );
            }
            TranscriptRecord::ToolResult(result) => {
                let anchor = result.message_uuid.as_deref().unwrap();
                assert!(uuids.contains(anchor), "dangling {anchor}");
            }
            TranscriptRecord::Message(message) => {
                assert_ne!(message.parent_uuid.as_deref(), Some(message.uuid.as_str()));
            }
            _ => {}
        }
    }
    let db = fresh_db();
    persist(&db, unit, parse(unit, None).into_iter()).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 1);
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM messages") as usize,
        messages.len()
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM tool_calls") as usize,
        tool_calls_of(&records).len()
    );
}

#[test]
fn raw_resolves_uuids_scope_and_sidechain_aware() {
    let dir = tempfile::tempdir().unwrap();
    let provider = DeepseekProvider::with_root(write_tree(dir.path(), usize::MAX, true));
    let unit = discover_all(&provider).remove(0);
    let (records, _) = drain(parse(&unit, None));

    let user_msg = records
        .iter()
        .find_map(|r| match r {
            TranscriptRecord::Message(m) if m.text.as_deref() == Some("inspect the project") => {
                Some(m)
            }
            _ => None,
        })
        .unwrap();
    let session = json!({"jsonl_path": unit.key});
    fn raw_lookup<'a>(
        uuid: &'a str,
        agent_id: Option<&'a str>,
        session: &'a Value,
    ) -> RawLookup<'a> {
        RawLookup {
            source: NAME,
            message_uuid: uuid,
            session: Some(session),
            agent_id,
            cursor: None,
            subagent: None,
            workflow_agent: None,
        }
    }
    let raw = provider
        .raw(&raw_lookup(&user_msg.uuid, None, &session))
        .unwrap();
    assert!(raw.text.contains("inspect the project"));

    // Sidechain message: agentId steers the lookup to the child's file even
    // though the session row points at the root file.
    let child_msg = records
        .iter()
        .find_map(|r| match r {
            TranscriptRecord::Message(m) if m.text.as_deref() == Some("review the code") => Some(m),
            _ => None,
        })
        .unwrap();
    let child_raw = provider
        .raw(&raw_lookup(
            &child_msg.uuid,
            child_msg.agent_id.as_deref(),
            &session,
        ))
        .unwrap();
    assert!(child_raw.text.contains("review the code"));

    assert!(provider
        .raw(&raw_lookup("deepseek:bogus", None, &session))
        .is_none());
}

#[test]
fn plaintext_logs_index_the_same_content_as_zstd_framed_logs() {
    let dir_plain = tempfile::tempdir().unwrap();
    let dir_zstd = tempfile::tempdir().unwrap();
    let provider_plain =
        DeepseekProvider::with_root(write_tree(dir_plain.path(), usize::MAX, false));
    let provider_zstd = DeepseekProvider::with_root(write_tree(dir_zstd.path(), usize::MAX, true));
    let dump_of = |provider: &DeepseekProvider| -> Vec<String> {
        let db = fresh_db();
        let unit = discover_all(provider).remove(0);
        persist(&db, &unit, parse(&unit, None).into_iter()).unwrap();
        dump(&db)
    };
    assert_eq!(dump_of(&provider_plain), dump_of(&provider_zstd));
}

#[test]
fn resolves_sessions_root_from_dsh_home_with_blank_counting_as_unset() {
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("DSH_HOME", "/tmp/custom-dsh-home");
    assert_eq!(
        DeepseekProvider::new().descriptor().default_root,
        "/tmp/custom-dsh-home/sessions"
    );
    std::env::set_var("DSH_HOME", "   ");
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        assert_eq!(
            DeepseekProvider::new().descriptor().default_root,
            format!("{home}/.dsh/sessions")
        );
    }
    std::env::remove_var("DSH_HOME");
    assert_eq!(
        DeepseekProvider::new().descriptor().default_root,
        format!("{home}/.dsh/sessions")
    );
}

#[test]
fn one_shot_subagent_descriptor_falls_back_to_provider() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    write_member(
        &sessions_dir
            .join("--tmp-dsh-project--")
            .join("root-session-1"),
        &[vec![header()]],
        false,
    );
    write_member(
        &sessions_dir
            .join("--tmp-dsh-project--")
            .join("one-shot-child"),
        &[
            vec![
                json!({"type": "session", "version": 0, "id": "one-shot-child",
                "createdAt": 1753005604200i64, "cwd": "/tmp/dsh-project",
                "parentSession": "root-session-1", "origin": "subagent", "delegationDepth": 1}),
            ],
            vec![
                json!({"type": "subagent/descriptor", "seq": 0, "time": 1753005604200i64,
                "data": {"version": 2, "mode": "one-shot", "provider": "code", "label": "fix the bug"}}),
            ],
            vec![
                json!({"type": "user/message", "seq": 1, "time": 1753005604300i64,
                "data": {"content": [{"type": "text", "text": "do it"}], "source": {"kind": "user"}, "role": "user", "id": "m-1"}}),
            ],
        ],
        false,
    );
    let provider = DeepseekProvider::with_root(sessions_dir);
    let unit = discover_all(&provider).remove(0);
    let (records, _) = drain(parse(&unit, None));
    let subs = subagents_of(&records);
    let sub = subs.iter().find(|s| s.agent_type.is_some()).unwrap();
    assert_eq!(sub.agent_type.as_deref(), Some("code"));
    assert_eq!(sub.description.as_deref(), Some("fix the bug"));
}

#[test]
fn malformed_lines_and_packed_chunk_rows_never_abort_the_parse() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let session_dir = sessions_dir.join("--tmp-dsh-project--").join("bad-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut frames = vec![vec![
        json!({"type": "session", "version": 0, "id": "bad-session",
        "createdAt": 1753005600000i64, "cwd": "/tmp/dsh-project", "delegationDepth": 0}),
    ]];
    frames.push(vec![json!({"type": "user/message", "seq": 1, "time": 1753005601000i64,
        "data": {"content": [{"type": "text", "text": "survives"}], "source": {"kind": "user"}, "role": "user", "id": "m-2"}})]);
    let mut text = frames
        .iter()
        .flatten()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    text = format!("{text}\n{{\"type\":\"user/message\",\"seq\":1,BROKEN");
    text = format!(
        "{text}\n{}",
        json!({"type": "text-chunks", "seq0": 10, "time0": 1753005601500i64, "data": {"turn": 1, "step": 1, "index": 0, "dt": [5], "texts": ["a", "b", "c"]}})
    );
    std::fs::write(session_dir.join("session.jsonl"), text + "\n").unwrap();
    let provider = DeepseekProvider::with_root(sessions_dir);
    let unit = discover_all(&provider).remove(0);
    let (records, _) = drain(parse(&unit, None));
    assert!(messages_of(&records)
        .iter()
        .any(|m| m.text.as_deref() == Some("survives")));
}

#[test]
fn changed_path_reconciliation_routes_deleted_child_to_its_tree() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = write_tree(dir.path(), usize::MAX, true);
    let provider = DeepseekProvider::with_root(sessions_dir.clone());
    let db = fresh_db();
    let unit = discover_all(&provider).remove(0);
    let cursor = persist(&db, &unit, parse(&unit, None).into_iter()).unwrap();
    assert!(count(&db, "SELECT COUNT(*) FROM messages WHERE is_sidechain=1") > 0);

    let child_path = project_dir(&sessions_dir)
        .join("child-session-1")
        .join("session.jsonl.zstd");
    std::fs::remove_file(&child_path).unwrap();
    let units = discover_with(
        &provider,
        &|_| cursor.clone(),
        Some(vec![child_path.to_string_lossy().into_owned()]),
        &[],
        &mut Vec::new(),
    );
    assert_eq!(units.len(), 1, "deleted child must route to its tree");
    persist(&db, &units[0], parse(&units[0], cursor).into_iter()).unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM messages WHERE is_sidechain=1"),
        0,
        "stale sidechain rows retracted"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE text='inspect the project'"
        ),
        1,
        "root rows kept"
    );
}

#[test]
fn moved_root_keeps_its_session_because_tombstones_key_on_identity() {
    let (root_id, _) = tree_ids();
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = write_tree(dir.path(), usize::MAX, true);
    let provider = DeepseekProvider::with_root(sessions_dir.clone());
    let db = fresh_db();
    let mut store = CursorStore::default();
    let unit = discover_all(&provider).remove(0);
    let cursor = persist(&db, &unit, parse(&unit, None).into_iter()).unwrap();
    store.0.insert(unit.key.clone(), cursor);

    // Move the whole project dir; the new unit key has NO cursor in prod.
    std::fs::rename(project_dir(&sessions_dir), sessions_dir.join("--moved--")).unwrap();
    let units = discover_with(
        &provider,
        &|key| store.0.get(key).cloned().flatten(),
        None,
        &[(root_id.clone(), unit.key.clone())],
        &mut Vec::new(),
    );
    assert!(
        units.iter().all(|u| u.retract_session_ids.is_empty()),
        "no tombstone for a moved tree"
    );
    assert_eq!(units.len(), 1);
    let new_cursor = store.0.get(&units[0].key).cloned().flatten();
    persist(&db, &units[0], parse(&units[0], new_cursor).into_iter()).unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM sessions"),
        1,
        "session survives the move"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM sessions WHERE id='{}'",
                root_id.replace('\'', "''")
            )
        ),
        1
    );
}

#[test]
fn duplicate_and_divergent_copies_of_one_identity() {
    // Identical copies dedupe to one member (no double counting).
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let frames = vec![
        vec![header()],
        vec![
            json!({"type": "user/message", "seq": 1, "time": 1753005601000i64,
            "data": {"content": [{"type": "text", "text": "one"}], "source": {"kind": "user"}, "role": "user", "id": "m-1"}}),
        ],
    ];
    for name in ["root-session-1", "root-session-1-copy"] {
        write_member(
            &sessions_dir.join("--tmp-dsh-project--").join(name),
            &frames,
            false,
        );
    }
    let provider = DeepseekProvider::with_root(sessions_dir);
    let units = discover_all(&provider);
    assert_eq!(units.len(), 1, "identical copies dedupe to one member");
    let db = fresh_db();
    persist(&db, &units[0], parse(&units[0], None).into_iter()).unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), 1);
    assert_eq!(
        count(&db, "SELECT message_count FROM sessions"),
        1,
        "ADR-0007: count matches rows"
    );

    // Divergent copies fail closed: nothing is published, issue recorded.
    let dir2 = tempfile::tempdir().unwrap();
    let sessions_dir2 = dir2.path().join("sessions");
    for (name, text) in [
        ("root-session-1", "FROM_A"),
        ("root-session-1-copy", "FROM_B_DIFFERENT"),
    ] {
        write_member(
            &sessions_dir2.join("--tmp-dsh-project--").join(name),
            &[
                vec![header()],
                vec![
                    json!({"type": "user/message", "seq": 1, "time": 1753005601000i64,
                    "data": {"content": [{"type": "text", "text": text}], "source": {"kind": "user"}, "role": "user", "id": "m-1"}}),
                ],
            ],
            false,
        );
    }
    let provider2 = DeepseekProvider::with_root(sessions_dir2);
    let mut issues = Vec::new();
    let units2 = discover_with(&provider2, &|_| None, None, &[], &mut issues);
    assert_eq!(units2.len(), 0, "divergent copies publish nothing");
    assert!(issues.iter().any(|issue| issue.error.contains("Divergent")));
}

#[test]
fn unreadable_member_suppresses_its_whole_project_fail_closed() {
    let (root_id, _) = tree_ids();
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = write_tree(dir.path(), usize::MAX, true);
    // A second, unrelated tree in ANOTHER project stays live.
    write_member(
        &sessions_dir.join("--other--").join("other-session"),
        &[
            vec![
                json!({"type": "session", "version": 0, "id": "other-session",
                "createdAt": 1753005600000i64, "cwd": "/other", "delegationDepth": 0}),
            ],
            vec![
                json!({"type": "user/message", "seq": 1, "time": 1753005601000i64,
                "data": {"content": [{"type": "text", "text": "other"}], "source": {"kind": "user"}, "role": "user", "id": "m-1"}}),
            ],
        ],
        false,
    );
    let provider = DeepseekProvider::with_root(sessions_dir.clone());

    // Corrupt the child: valid header frame, invalid frame magic later.
    let child_path = project_dir(&sessions_dir)
        .join("child-session-1")
        .join("session.jsonl.zstd");
    let mut bytes = std::fs::read(&child_path).unwrap();
    bytes.extend_from_slice(b"%%%not-a-frame%%%");
    std::fs::write(&child_path, bytes).unwrap();

    let mut issues = Vec::new();
    let units = discover_with(&provider, &|_| None, None, &[], &mut issues);
    assert!(!issues.is_empty(), "inventory issue reported");
    assert!(
        !units.iter().any(|u| u.session_id == root_id),
        "affected tree suppressed"
    );
    assert!(
        units.iter().any(|u| u.session_id.contains("other-session")),
        "unrelated tree still discovered"
    );
}

#[test]
fn offline_source_root_reports_incomplete_inventory_and_no_tombstones() {
    let (root_id, _) = tree_ids();
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = write_tree(dir.path(), usize::MAX, true);
    let provider = DeepseekProvider::with_root(sessions_dir.clone());
    let db = fresh_db();
    let unit = discover_all(&provider).remove(0);
    persist(&db, &unit, parse(&unit, None).into_iter()).unwrap();

    std::fs::remove_dir_all(&sessions_dir).unwrap();
    let mut issues = Vec::new();
    let units = discover_with(
        &provider,
        &|_| None,
        None,
        &[(root_id.clone(), unit.key.clone())],
        &mut issues,
    );
    assert!(!issues.is_empty(), "offline root reported");
    assert!(
        units.iter().all(|u| u.retract_session_ids.is_empty()),
        "no tombstone while inventory is incomplete"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM sessions"),
        1,
        "last-good snapshot preserved"
    );
}

#[test]
fn headerless_and_higher_version_artifacts_fail_closed() {
    // A headerless artifact is reported, never silently skipped.
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let session_dir = sessions_dir.join("--proj--").join("empty-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("session.jsonl"), "").unwrap();
    let provider = DeepseekProvider::with_root(sessions_dir);
    let mut issues = Vec::new();
    let units = discover_with(&provider, &|_| None, None, &[], &mut issues);
    assert_eq!(units.len(), 0);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].path.ends_with("session.jsonl"));

    // An unknown higher header version is skipped and recorded, never
    // parsed as v0.
    let dir2 = tempfile::tempdir().unwrap();
    let sessions_dir2 = dir2.path().join("sessions");
    write_member(
        &sessions_dir2.join("--proj--").join("future-session"),
        &[
            vec![
                json!({"type": "session", "version": 2, "id": "future-session",
                "createdAt": 1753005600000i64, "cwd": "/tmp/p", "delegationDepth": 0}),
            ],
            vec![
                json!({"type": "user/message", "seq": 1, "time": 1753005601000i64,
                "data": {"content": [{"type": "text", "text": "future"}], "source": {"kind": "user"}, "role": "user", "id": "m-1"}}),
            ],
        ],
        false,
    );
    let provider2 = DeepseekProvider::with_root(sessions_dir2);
    let mut issues2 = Vec::new();
    let units2 = discover_with(&provider2, &|_| None, None, &[], &mut issues2);
    assert_eq!(units2.len(), 0);
    assert!(issues2
        .iter()
        .any(|issue| issue.error.contains("Unsupported session format version 2")));
}
