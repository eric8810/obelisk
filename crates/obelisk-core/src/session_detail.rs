// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Session-detail presentation assembly (port of
//! packages/core/src/session-detail.ts, see ADR-0001).
//!
//! One canonical seam turns a provider record stream (a fresh full parse with
//! cursor = null) — or the same records after a persistence round-trip — into
//! the presentation model consumed by the timeline renderer. Provider-specific
//! wire semantics must already be resolved before this seam.
//!
//! Input model: the TS union (`Iterable<TranscriptRecord> | SessionDetailRows`)
//! maps to [`SessionDetailSource`]. The rows half is typed row structs with
//! lenient serde deserializers (JS truthiness / `typeof` coercions) built from
//! query.rs-style `serde_json::Value` rows, mirroring the TS duck typing; both
//! flows funnel through the same canonical record language.
//!
//! Output model: typed structs with serde `Serialize`. The TS output shape is
//! closed (its index signatures exist only on the input rows), so typed structs
//! beat `serde_json::Value`. Divergences from the TS JSON shape, deliberate for
//! the typed Rust renderer: `is_meta`/`is_error` are `bool` (TS emits 0/1) and
//! `ToolResultRecord.message_uuid` stays `Option<String>` where TS coerces to
//! `''` (assembly treats both as "no message").

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::providers::types::{
    MessageRecord, MessageVisibility, SessionCountMode, SessionRecord, SubagentRecord,
    SummaryRecord, ToolCallPresentation, ToolCallRecord, ToolResultRecord, TranscriptRecord,
    WorkflowAgentRecord, WorkflowRecord,
};

const DELTA_REJECTED: &str =
    "Direct session detail assembly requires a fresh full parse (cursor = null), not a provider delta";

// ---------------------------------------------------------------------------
// Output model
// ---------------------------------------------------------------------------

/// The assembled session detail snapshot (TS `SessionDetailSnapshot`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetailSnapshot {
    pub session: Option<SessionDetailSession>,
    pub messages: Vec<AssembledMessage>,
    pub workflows: Vec<SessionDetailWorkflow>,
    pub summaries: Vec<SessionDetailSummary>,
}

/// Session header (TS `SessionDetailSession` = SessionRecord minus kind/countMode).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetailSession {
    pub id: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub message_count: i64,
    pub jsonl_path: String,
    pub source: String,
}

/// One timeline message with folded tool calls (TS `AssembledMessage`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssembledMessage {
    pub uuid: String,
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    pub timestamp: Option<String>,
    pub text: Option<String>,
    pub content_type: Option<String>,
    pub is_meta: bool,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub turn_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AssembledToolCall>>,
    #[serde(rename = "_thinking", skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(rename = "_skillMd", skip_serializing_if = "Option::is_none")]
    pub skill_md: Option<String>,
}

impl AssembledMessage {
    fn is_assistant(&self) -> bool {
        self.r#type.as_deref() == Some("assistant")
    }

    fn is_assistant_thinking(&self) -> bool {
        self.is_assistant() && self.content_type.as_deref() == Some("thinking")
    }

    fn is_assistant_tool_use(&self) -> bool {
        self.is_assistant() && self.content_type.as_deref() == Some("tool_use")
    }
}

/// A tool call folded into its owning assistant message (TS `AssembledToolCall`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssembledToolCall {
    pub id: String,
    pub name: String,
    pub presentation: ToolCallPresentation,
    pub input_json: Option<String>,
    pub result: Option<SessionDetailToolResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SessionDetailSubagentRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<SessionDetailWorkflow>,
}

/// Attached tool result (TS `SessionDetailToolResult` = ToolResultRecord minus kind).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetailToolResult {
    pub tool_use_id: String,
    pub message_uuid: Option<String>,
    pub session_id: String,
    pub content: String,
    pub file_path: Option<String>,
    pub is_error: bool,
}

/// Subagent linkage embedded on the spawning tool call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetailSubagentRef {
    pub agent_id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
}

/// One workflow run as presented on the timeline (TS `SessionDetailWorkflow`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetailWorkflow {
    pub run_id: String,
    pub parent_tool_use_id: Option<String>,
    pub workflow_name: Option<String>,
    pub status: Option<String>,
    pub duration_ms: Option<i64>,
    pub total_tokens: Option<i64>,
    pub agent_count: i64,
    pub agents: Vec<SessionDetailWorkflowAgent>,
}

/// One workflow agent row embedded in its run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetailWorkflowAgent {
    pub agent_id: String,
    pub phase: Option<String>,
    pub label: Option<String>,
    pub state: Option<String>,
    pub tokens: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// One visible summary (TS `SessionDetailSummary` = SummaryRecord minus kind).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetailSummary {
    pub id: String,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub source: String,
    pub content: String,
    pub visibility: Option<MessageVisibility>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

impl Serialize for ToolCallPresentation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for MessageVisibility {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Rows input model (TS SessionDetailRows and its duck-typed row interfaces)
// ---------------------------------------------------------------------------

/// Persisted-row input (TS `SessionDetailRows`), deserialized leniently from
/// `serde_json::Value` rows so partial JSON objects behave like the TS
/// `typeof`-checked duck typing. Deserialize any query.rs row `Value` with
/// `serde_json::from_value`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionDetailRows {
    pub session: Option<SessionDetailSessionRow>,
    pub messages: Vec<SessionMessageRow>,
    #[serde(alias = "toolCalls")]
    pub tool_calls: Vec<SessionToolCallRow>,
    #[serde(alias = "toolResults")]
    pub tool_results: Vec<SessionToolResultRow>,
    pub subagents: Vec<SessionSubagentRow>,
    pub workflows: Vec<SessionWorkflowRow>,
    pub summaries: Vec<SessionSummaryRow>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionDetailSessionRow {
    #[serde(deserialize_with = "de_string")]
    pub id: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub title: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub project: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub started_at: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub ended_at: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub git_branch: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub version: Option<String>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub message_count: Option<i64>,
    #[serde(deserialize_with = "de_opt_str")]
    pub jsonl_path: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionMessageRow {
    #[serde(deserialize_with = "de_string")]
    pub uuid: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub session_id: Option<String>,
    #[serde(rename = "type", deserialize_with = "de_opt_str")]
    pub r#type: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub role: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub parent_uuid: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub timestamp: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub text: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub content_type: Option<String>,
    #[serde(deserialize_with = "de_truthy")]
    pub is_meta: bool,
    #[serde(deserialize_with = "de_opt_str")]
    pub visibility: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub model: Option<String>,
    #[serde(deserialize_with = "de_truthy")]
    pub is_sidechain: bool,
    #[serde(deserialize_with = "de_opt_str")]
    pub agent_id: Option<String>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub input_tokens: Option<i64>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub output_tokens: Option<i64>,
    #[serde(deserialize_with = "de_opt_str")]
    pub cwd: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub skill: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub source: Option<String>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub turn_duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionToolCallRow {
    #[serde(deserialize_with = "de_string")]
    pub id: String,
    #[serde(deserialize_with = "de_string")]
    pub message_uuid: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "de_string")]
    pub name: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub presentation: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub input_json: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionToolResultRow {
    #[serde(deserialize_with = "de_string")]
    pub tool_use_id: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub message_uuid: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub content: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub file_path: Option<String>,
    #[serde(deserialize_with = "de_truthy")]
    pub is_error: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionSubagentRow {
    #[serde(deserialize_with = "de_string")]
    pub agent_id: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub parent_tool_use_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub agent_type: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub description: Option<String>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub duration_ms: Option<i64>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionWorkflowRow {
    #[serde(deserialize_with = "de_string")]
    pub run_id: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub parent_tool_use_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub task_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub script: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub result_json: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub timestamp: Option<String>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub agent_count: Option<i64>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub duration_ms: Option<i64>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub total_tokens: Option<i64>,
    #[serde(deserialize_with = "de_opt_str")]
    pub status: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub workflow_name: Option<String>,
    pub agents: Option<Vec<SessionWorkflowAgentRow>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionWorkflowAgentRow {
    #[serde(deserialize_with = "de_string")]
    pub agent_id: String,
    #[serde(deserialize_with = "de_opt_str")]
    pub run_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub agent_type: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub description: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub phase: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub label: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub model: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub state: Option<String>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub duration_ms: Option<i64>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub tokens: Option<i64>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub tool_calls: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SessionSummaryRow {
    #[serde(deserialize_with = "de_opt_string_or_number")]
    pub id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub session_id: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub timestamp: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub source: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub content: Option<String>,
    #[serde(deserialize_with = "de_opt_str")]
    pub visibility: Option<String>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub input_tokens: Option<i64>,
    #[serde(deserialize_with = "de_opt_i64")]
    pub output_tokens: Option<i64>,
}

// Lenient deserializers mirroring the TS `typeof` coercions on row values.

/// `typeof value === 'string' ? value : null`
fn de_opt_str<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(deserializer)?
        .as_str()
        .map(str::to_string))
}

/// Required string key: any non-string (or missing) becomes `""`.
fn de_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Value::deserialize(deserializer)?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// `typeof value === 'number' ? value : null` (integral numbers only).
fn de_opt_i64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|f| f.fract() == 0.0)
                .map(|f| f as i64)
        }),
        _ => None,
    })
}

/// JS truthiness: `value ? 1 : 0`.
fn de_truthy<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(js_truthy(&Value::deserialize(deserializer)?))
}

/// `String(value)` for string|number summary ids.
fn de_opt_string_or_number<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::String(value) => Some(value),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(boolean) => *boolean,
        Value::Number(number) => number
            .as_f64()
            .map(|f| f != 0.0 && !f.is_nan())
            .unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// Input for [`assemble_session_detail`]: either a provider's complete
/// canonical transcript stream, or the same records after a persistence
/// round-trip (TS union `Iterable<TranscriptRecord> | SessionDetailRows`).
#[allow(clippy::large_enum_variant)]
pub enum SessionDetailSource<'a> {
    Records(&'a [TranscriptRecord]),
    Rows(SessionDetailRows),
}

/// Assemble the session detail from records or persisted rows. This is the
/// only presentation seam. Errors when a record stream carries a provider
/// delta session aggregate (TS throws).
pub fn assemble_session_detail(
    source: SessionDetailSource<'_>,
) -> Result<SessionDetailSnapshot, String> {
    match source {
        SessionDetailSource::Records(records) => assemble_transcript_records(records),
        SessionDetailSource::Rows(rows) => {
            let records = records_from_rows(&rows);
            assemble_transcript_records(&records)
        }
    }
}

/// One canonical message before folding (TS `SessionDetailMessage`).
#[derive(Debug, Clone)]
struct CanonicalMessage {
    uuid: String,
    r#type: Option<String>,
    timestamp: Option<String>,
    text: Option<String>,
    content_type: Option<String>,
    is_meta: bool,
    session_id: Option<String>,
    cwd: Option<String>,
    turn_duration_ms: Option<i64>,
}

fn canonical_visibility(value: Option<&str>) -> MessageVisibility {
    match value {
        None | Some("visible") => MessageVisibility::Visible,
        Some("inactive") => MessageVisibility::Inactive,
        _ => MessageVisibility::Hidden,
    }
}

/// Project one canonical provider record stream into the session detail.
fn assemble_transcript_records(
    records: &[TranscriptRecord],
) -> Result<SessionDetailSnapshot, String> {
    let mut session: Option<SessionDetailSession> = None;
    let mut messages: Vec<CanonicalMessage> = Vec::new();
    let mut messages_by_uuid: HashMap<String, usize> = HashMap::new();
    let mut main_message_uuids: HashSet<String> = HashSet::new();
    let mut tool_calls: Vec<&ToolCallRecord> = Vec::new();
    let mut tool_results: Vec<&ToolResultRecord> = Vec::new();
    let mut subagents: Vec<&SubagentRecord> = Vec::new();
    let mut workflows: Vec<&WorkflowRecord> = Vec::new();
    // Insertion-ordered merge map (TS Map preserves first-insertion order).
    let mut workflow_agents: Vec<WorkflowAgentRecord> = Vec::new();
    let mut workflow_agent_index: HashMap<String, usize> = HashMap::new();
    let mut summaries: Vec<SessionDetailSummary> = Vec::new();

    for record in records {
        match record {
            TranscriptRecord::Session(r) => {
                if r.count_mode == SessionCountMode::Delta {
                    return Err(DELTA_REJECTED.to_string());
                }
                session = Some(SessionDetailSession {
                    id: r.id.clone(),
                    title: r.title.clone(),
                    project: r.project.clone(),
                    started_at: r.started_at.clone(),
                    ended_at: r.ended_at.clone(),
                    git_branch: r.git_branch.clone(),
                    version: r.version.clone(),
                    message_count: r.message_count,
                    jsonl_path: r.jsonl_path.clone(),
                    source: r.source.clone(),
                });
            }
            TranscriptRecord::Message(r) => {
                if r.visibility != MessageVisibility::Visible {
                    continue;
                }
                let message = CanonicalMessage {
                    uuid: r.uuid.clone(),
                    r#type: if r.r#type.is_empty() {
                        r.role.clone()
                    } else {
                        Some(r.r#type.clone())
                    },
                    timestamp: r.timestamp.clone(),
                    text: r.text.clone(),
                    content_type: r.content_type.clone(),
                    is_meta: r.is_meta,
                    // Both kept per message, not per session: the working
                    // directory can change mid-session, and the session id
                    // scopes which roots a file reference may resolve in.
                    session_id: Some(r.session_id.clone()),
                    cwd: r.cwd.clone(),
                    turn_duration_ms: None,
                };
                messages_by_uuid.insert(message.uuid.clone(), messages.len());
                messages.push(message);
                if r.agent_id.is_none() {
                    main_message_uuids.insert(r.uuid.clone());
                }
            }
            TranscriptRecord::ToolCall(r) => tool_calls.push(r),
            TranscriptRecord::ToolResult(r) => tool_results.push(r),
            TranscriptRecord::Subagent(r) => subagents.push(r),
            TranscriptRecord::Workflow(r) => workflows.push(r),
            TranscriptRecord::WorkflowAgent(r) => {
                merge_workflow_agent(&mut workflow_agents, &mut workflow_agent_index, r);
            }
            TranscriptRecord::Summary(r) => {
                if r.visibility.unwrap_or(MessageVisibility::Visible) != MessageVisibility::Visible
                {
                    continue;
                }
                summaries.push(SessionDetailSummary {
                    id: r.id.clone(),
                    session_id: r.session_id.clone(),
                    timestamp: r.timestamp.clone(),
                    source: r.source.clone(),
                    content: r.content.clone(),
                    visibility: r.visibility,
                    input_tokens: r.input_tokens,
                    output_tokens: r.output_tokens,
                });
            }
            TranscriptRecord::MessageTurnDuration {
                uuid,
                turn_duration_ms,
            } => {
                if let Some(&index) = messages_by_uuid.get(uuid) {
                    messages[index].turn_duration_ms = *turn_duration_ms;
                }
            }
            TranscriptRecord::DeleteSession { .. } => {}
        }
    }

    let assembled_workflows: Vec<SessionDetailWorkflow> = workflows
        .iter()
        .map(|workflow| SessionDetailWorkflow {
            run_id: workflow.run_id.clone(),
            parent_tool_use_id: workflow.parent_tool_use_id.clone(),
            workflow_name: workflow.workflow_name.clone(),
            status: workflow.status.clone(),
            duration_ms: workflow.duration_ms,
            total_tokens: workflow.total_tokens,
            // WorkflowRecord.agent_count is always set; the TS `?? agents.len()`
            // fallback is dead code.
            agent_count: workflow.agent_count,
            agents: workflow_agents
                .iter()
                .filter(|agent| agent.run_id == workflow.run_id)
                .map(|agent| SessionDetailWorkflowAgent {
                    agent_id: agent.agent_id.clone(),
                    phase: agent.phase.clone(),
                    label: agent.label.clone(),
                    state: agent.state.clone(),
                    tokens: agent.tokens,
                    duration_ms: agent.duration_ms,
                })
                .collect(),
        })
        .collect();

    let mut detail_messages = if session.is_some() {
        messages
            .into_iter()
            .filter(|message| main_message_uuids.contains(&message.uuid))
            .collect::<Vec<_>>()
    } else {
        messages
    };
    detail_messages.sort_by(|left, right| {
        left.timestamp
            .as_deref()
            .unwrap_or("")
            .cmp(right.timestamp.as_deref().unwrap_or(""))
            .then_with(|| left.uuid.cmp(&right.uuid))
    });

    Ok(SessionDetailSnapshot {
        session,
        messages: assemble_messages(
            &detail_messages,
            &tool_calls,
            &tool_results,
            &subagents,
            &assembled_workflows,
        ),
        workflows: assembled_workflows,
        summaries,
    })
}

/// Column-wise last-write-wins merge skipping nulls (TS spread over non-null
/// entries), preserving first-insertion order for deterministic agent lists.
fn merge_workflow_agent(
    agents: &mut Vec<WorkflowAgentRecord>,
    index: &mut HashMap<String, usize>,
    record: &WorkflowAgentRecord,
) {
    if let Some(&position) = index.get(&record.agent_id) {
        let merged = &mut agents[position];
        merged.run_id = record.run_id.clone();
        merged.session_id = record.session_id.clone();
        if record.agent_type.is_some() {
            merged.agent_type = record.agent_type.clone();
        }
        if record.description.is_some() {
            merged.description = record.description.clone();
        }
        if record.phase.is_some() {
            merged.phase = record.phase.clone();
        }
        if record.label.is_some() {
            merged.label = record.label.clone();
        }
        if record.model.is_some() {
            merged.model = record.model.clone();
        }
        if record.state.is_some() {
            merged.state = record.state.clone();
        }
        if record.duration_ms.is_some() {
            merged.duration_ms = record.duration_ms;
        }
        if record.tokens.is_some() {
            merged.tokens = record.tokens;
        }
        if record.tool_calls.is_some() {
            merged.tool_calls = record.tool_calls;
        }
    } else {
        index.insert(record.agent_id.clone(), agents.len());
        agents.push(record.clone());
    }
}

/// Fold tool calls, results, subagent/workflow linkage, thinking, and skill
/// instructions into the visible message list (TS `assembleMessages`).
fn assemble_messages(
    messages: &[CanonicalMessage],
    tool_calls: &[&ToolCallRecord],
    tool_results: &[&ToolResultRecord],
    subagents: &[&SubagentRecord],
    workflows: &[SessionDetailWorkflow],
) -> Vec<AssembledMessage> {
    let visible_message_uuids: HashSet<&str> = messages
        .iter()
        .map(|message| message.uuid.as_str())
        .collect();
    let visible_tool_calls: Vec<&ToolCallRecord> = tool_calls
        .iter()
        .copied()
        .filter(|call| visible_message_uuids.contains(call.message_uuid.as_str()))
        .collect();
    let visible_tool_results: Vec<&ToolResultRecord> = tool_results
        .iter()
        .copied()
        .filter(|result| {
            result
                .message_uuid
                .as_deref()
                .is_some_and(|uuid| visible_message_uuids.contains(uuid))
        })
        .collect();
    let visible_call_ids: HashSet<&str> = visible_tool_calls
        .iter()
        .map(|call| call.id.as_str())
        .collect();
    // Tool-result messages whose result is attached to a visible call fold
    // into that call and never render standalone.
    let attached_result_message_uuids: HashSet<&str> = visible_tool_results
        .iter()
        .filter(|result| visible_call_ids.contains(result.tool_use_id.as_str()))
        .filter_map(|result| result.message_uuid.as_deref())
        .collect();
    let omit_tool_result_message = |message: &AssembledMessage| {
        message.content_type.as_deref() == Some("tool_result")
            && (attached_result_message_uuids.contains(message.uuid.as_str())
                || message.text.as_deref().unwrap_or("").is_empty())
    };

    let mut results_by_call_id: HashMap<&str, SessionDetailToolResult> = HashMap::new();
    for result in &visible_tool_results {
        results_by_call_id.insert(
            result.tool_use_id.as_str(),
            SessionDetailToolResult {
                tool_use_id: result.tool_use_id.clone(),
                message_uuid: result.message_uuid.clone(),
                session_id: result.session_id.clone(),
                content: result.content.clone(),
                file_path: result.file_path.clone(),
                is_error: result.is_error,
            },
        );
    }

    let mut subagents_by_call_id: HashMap<&str, &SubagentRecord> = HashMap::new();
    for subagent in subagents {
        if let Some(parent) = subagent
            .parent_tool_use_id
            .as_deref()
            .filter(|parent| !parent.is_empty())
        {
            subagents_by_call_id.insert(parent, subagent);
        }
    }

    let mut workflows_by_call_id: HashMap<&str, &SessionDetailWorkflow> = HashMap::new();
    for workflow in workflows {
        if let Some(parent) = workflow
            .parent_tool_use_id
            .as_deref()
            .filter(|parent| !parent.is_empty())
        {
            workflows_by_call_id.insert(parent, workflow);
        }
    }

    let mut calls_by_message_uuid: HashMap<String, Vec<AssembledToolCall>> = HashMap::new();
    for call in &visible_tool_calls {
        let mut assembled = AssembledToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            presentation: call.presentation,
            input_json: Some(call.input_json.clone()),
            result: results_by_call_id.get(call.id.as_str()).cloned(),
            subagent: None,
            workflow: None,
        };
        if let Some(subagent) = subagents_by_call_id.get(call.id.as_str()) {
            assembled.subagent = Some(SessionDetailSubagentRef {
                agent_id: subagent.agent_id.clone(),
                agent_type: subagent.agent_type.clone(),
                description: subagent.description.clone(),
            });
        }
        if let Some(workflow) = workflows_by_call_id.get(call.id.as_str()) {
            assembled.workflow = Some((*workflow).clone());
        }
        calls_by_message_uuid
            .entry(call.message_uuid.clone())
            .or_default()
            .push(assembled);
    }

    let mut raw: Vec<AssembledMessage> = messages
        .iter()
        .map(|message| {
            let mut assembled = AssembledMessage {
                uuid: message.uuid.clone(),
                r#type: message.r#type.clone(),
                timestamp: message.timestamp.clone(),
                text: message.text.clone(),
                content_type: message.content_type.clone(),
                is_meta: message.is_meta,
                session_id: message.session_id.clone(),
                cwd: message.cwd.clone(),
                turn_duration_ms: message.turn_duration_ms,
                tool_calls: None,
                thinking: None,
                skill_md: None,
            };
            if let Some(calls) = calls_by_message_uuid.get(&message.uuid) {
                if !calls.is_empty() {
                    assembled.tool_calls = Some(calls.clone());
                }
            }
            assembled
        })
        .collect();

    let mut output: Vec<AssembledMessage> = Vec::new();
    let mut index = 0usize;
    while index < raw.len() {
        if omit_tool_result_message(&raw[index]) {
            index += 1;
            continue;
        }

        // Consecutive thinking runs collapse; a following non-thinking
        // assistant message inherits them as `_thinking`.
        if raw[index].is_assistant_thinking() {
            let mut thinking_parts = vec![raw[index].text.clone().unwrap_or_default()];
            let mut next_index = index + 1;
            while next_index < raw.len() && raw[next_index].is_assistant_thinking() {
                thinking_parts.push(raw[next_index].text.clone().unwrap_or_default());
                next_index += 1;
            }
            let joined = thinking_parts.join("\n\n");
            if next_index < raw.len()
                && raw[next_index].is_assistant()
                && raw[next_index].content_type.as_deref() != Some("thinking")
            {
                raw[next_index].thinking = Some(joined);
                index = next_index;
                continue;
            }
            let mut message = raw[index].clone();
            message.text = Some(joined);
            message.content_type = Some("thinking".to_string());
            output.push(message);
            index = next_index;
            continue;
        }

        // A tool_use message keeps its own calls (even when empty) and absorbs
        // following tool_use / skill-instruction messages.
        if raw[index].is_assistant_tool_use() {
            let mut merged = raw[index].clone();
            merged.tool_calls.get_or_insert_with(Vec::new);
            let skill_only = merged.tool_calls.as_ref().is_some_and(|calls| {
                calls.len() == 1 && calls[0].presentation == ToolCallPresentation::Skill
            });
            let mut next_index = index + 1;
            while next_index < raw.len() {
                let next = &raw[next_index];
                if omit_tool_result_message(next) {
                    next_index += 1;
                    continue;
                }
                if next.content_type.as_deref() == Some("skill_instructions")
                    && next.text.as_deref().is_some_and(|text| !text.is_empty())
                {
                    merged.skill_md = next.text.clone();
                    next_index += 1;
                    continue;
                }
                if !skill_only && next.is_assistant_tool_use() {
                    if let Some(calls) = &next.tool_calls {
                        merged
                            .tool_calls
                            .get_or_insert_with(Vec::new)
                            .extend(calls.iter().cloned());
                    }
                    let merged_text_empty = merged
                        .text
                        .as_deref()
                        .map(|text| text.is_empty())
                        .unwrap_or(true);
                    if next.text.as_deref().is_some_and(|text| !text.is_empty())
                        && merged_text_empty
                    {
                        merged.text = next.text.clone();
                    }
                    next_index += 1;
                    continue;
                }
                break;
            }
            output.push(merged);
            index = next_index;
            continue;
        }

        // Any other assistant message absorbs following tool_use messages'
        // calls; empty call lists are dropped again.
        let mut assembled = raw[index].clone();
        let mut next_index = index + 1;
        if assembled.is_assistant()
            && assembled.content_type.as_deref() != Some("tool_use")
            && assembled.content_type.as_deref() != Some("thinking")
        {
            assembled.tool_calls.get_or_insert_with(Vec::new);
            while next_index < raw.len() {
                let next = &raw[next_index];
                if omit_tool_result_message(next) {
                    next_index += 1;
                    continue;
                }
                if next.is_assistant_tool_use() {
                    if let Some(calls) = &next.tool_calls {
                        assembled
                            .tool_calls
                            .get_or_insert_with(Vec::new)
                            .extend(calls.iter().cloned());
                    }
                    next_index += 1;
                    continue;
                }
                break;
            }
            if assembled
                .tool_calls
                .as_ref()
                .is_some_and(|calls| calls.is_empty())
            {
                assembled.tool_calls = None;
            }
        }
        output.push(assembled);
        index = next_index;
    }

    output
}

/// Adapt persisted rows back into the canonical record language providers
/// emit (TS `sessionDetailRecordsFromRows`).
fn records_from_rows(rows: &SessionDetailRows) -> Vec<TranscriptRecord> {
    let mut records: Vec<TranscriptRecord> = Vec::new();
    if let Some(session) = &rows.session {
        records.push(TranscriptRecord::Session(SessionRecord {
            id: session.id.clone(),
            title: session.title.clone(),
            project: session.project.clone(),
            started_at: session.started_at.clone(),
            ended_at: session.ended_at.clone(),
            git_branch: session.git_branch.clone(),
            version: session.version.clone(),
            message_count: session.message_count.unwrap_or(0),
            count_mode: SessionCountMode::Total,
            jsonl_path: session.jsonl_path.clone().unwrap_or_default(),
            source: session.source.clone().unwrap_or_default(),
        }));
    }
    for message in &rows.messages {
        records.push(TranscriptRecord::Message(MessageRecord {
            uuid: message.uuid.clone(),
            session_id: message.session_id.clone().unwrap_or_default(),
            r#type: message
                .r#type
                .clone()
                .or_else(|| message.role.clone())
                .unwrap_or_default(),
            parent_uuid: message.parent_uuid.clone(),
            timestamp: message.timestamp.clone(),
            role: message.role.clone(),
            text: message.text.clone(),
            content_type: message.content_type.clone(),
            is_meta: message.is_meta,
            visibility: canonical_visibility(message.visibility.as_deref()),
            model: message.model.clone(),
            is_sidechain: message.is_sidechain,
            agent_id: message.agent_id.clone(),
            input_tokens: message.input_tokens,
            output_tokens: message.output_tokens,
            cwd: message.cwd.clone(),
            skill: message.skill.clone(),
            source: message.source.clone().unwrap_or_default(),
        }));
        // The TS row spread carries turn_duration_ms inside the message
        // record; a follow-up update op is the equivalent canonical encoding.
        if let Some(turn_duration_ms) = message.turn_duration_ms {
            records.push(TranscriptRecord::MessageTurnDuration {
                uuid: message.uuid.clone(),
                turn_duration_ms: Some(turn_duration_ms),
            });
        }
    }
    for call in &rows.tool_calls {
        records.push(TranscriptRecord::ToolCall(ToolCallRecord {
            id: call.id.clone(),
            message_uuid: call.message_uuid.clone(),
            session_id: call.session_id.clone().unwrap_or_default(),
            name: call.name.clone(),
            presentation: if call.presentation.as_deref() == Some("skill") {
                ToolCallPresentation::Skill
            } else {
                ToolCallPresentation::Default
            },
            input_json: call.input_json.clone().unwrap_or_default(),
            file_path: call.file_path.clone(),
        }));
    }
    for result in &rows.tool_results {
        records.push(TranscriptRecord::ToolResult(ToolResultRecord {
            tool_use_id: result.tool_use_id.clone(),
            message_uuid: result.message_uuid.clone(),
            session_id: result.session_id.clone().unwrap_or_default(),
            content: result.content.clone().unwrap_or_default(),
            file_path: result.file_path.clone(),
            is_error: result.is_error,
        }));
    }
    for subagent in &rows.subagents {
        records.push(TranscriptRecord::Subagent(SubagentRecord {
            agent_id: subagent.agent_id.clone(),
            session_id: subagent.session_id.clone().unwrap_or_default(),
            parent_tool_use_id: subagent.parent_tool_use_id.clone(),
            agent_type: subagent.agent_type.clone(),
            description: subagent.description.clone(),
            duration_ms: subagent.duration_ms,
            total_tokens: subagent.total_tokens,
        }));
    }
    for workflow in &rows.workflows {
        records.push(TranscriptRecord::Workflow(WorkflowRecord {
            run_id: workflow.run_id.clone(),
            session_id: workflow.session_id.clone().unwrap_or_default(),
            parent_tool_use_id: workflow.parent_tool_use_id.clone(),
            task_id: workflow.task_id.clone(),
            script: workflow.script.clone(),
            result_json: workflow.result_json.clone(),
            timestamp: workflow.timestamp.clone(),
            agent_count: workflow.agent_count.unwrap_or(0),
            duration_ms: workflow.duration_ms,
            total_tokens: workflow.total_tokens,
            status: workflow.status.clone(),
            workflow_name: workflow.workflow_name.clone(),
        }));
        for agent in workflow.agents.iter().flatten() {
            records.push(TranscriptRecord::WorkflowAgent(WorkflowAgentRecord {
                agent_id: agent.agent_id.clone(),
                run_id: agent
                    .run_id
                    .clone()
                    .unwrap_or_else(|| workflow.run_id.clone()),
                session_id: agent
                    .session_id
                    .clone()
                    .or_else(|| workflow.session_id.clone())
                    .unwrap_or_default(),
                agent_type: agent.agent_type.clone(),
                description: agent.description.clone(),
                phase: agent.phase.clone(),
                label: agent.label.clone(),
                model: agent.model.clone(),
                state: agent.state.clone(),
                duration_ms: agent.duration_ms,
                tokens: agent.tokens,
                tool_calls: agent.tool_calls,
            }));
        }
    }
    for summary in &rows.summaries {
        records.push(TranscriptRecord::Summary(SummaryRecord {
            id: summary.id.clone().unwrap_or_default(),
            session_id: summary.session_id.clone().unwrap_or_default(),
            timestamp: summary.timestamp.clone(),
            source: summary.source.clone().unwrap_or_default(),
            content: summary.content.clone().unwrap_or_default(),
            visibility: Some(canonical_visibility(summary.visibility.as_deref())),
            input_tokens: summary.input_tokens,
            output_tokens: summary.output_tokens,
        }));
    }
    records
}
