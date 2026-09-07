// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Timeline item model (port of session-timeline-items.mjs): each indexed
//! message becomes one or more timeline items; the kind drives rendering.

use serde_json::Value;

use crate::data::TimelineToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineKind {
    Meta,
    Workflow,
    WorkflowTools,
    Skill,
    Thinking,
    Message,
}

#[derive(Debug, Clone)]
pub struct TimelineItem {
    pub kind: TimelineKind,
    /// Stable per-item identity, kept for the branch-disclosure and
    /// follow-tail work later in M2.3.
    #[allow(dead_code)]
    pub key: String,
    pub message_uuid: String,
    pub message: TimelineMessage,
    /// Workflow tools carry the non-workflow tool calls separately.
    pub tool_calls: Vec<TimelineToolCall>,
}

/// Denormalized message row for the timeline. Several fields are carried for
/// the remaining M2.3 views (branch disclosure, file references) and the M2.4
/// usage-stats views; they are read at assembly time, not per-frame.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TimelineMessage {
    pub uuid: String,
    pub session_id: String,
    pub r#type: String,
    pub timestamp: Option<String>,
    pub role: Option<String>,
    pub text: Option<String>,
    pub content_type: Option<String>,
    pub is_meta: bool,
    pub visibility: String,
    pub model: Option<String>,
    pub agent_id: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cwd: Option<String>,
    pub source: String,
    pub tool_calls: Vec<TimelineToolCall>,
    /// The workflow run attached to a Workflow tool call (kind=Workflow).
    pub workflow: Option<Value>,
}

fn message_items(message: &TimelineMessage, index: usize) -> Vec<TimelineItem> {
    let message_uuid = if message.uuid.is_empty() {
        format!("message-{index}")
    } else {
        message.uuid.clone()
    };
    let item = |kind: TimelineKind, tool_calls: Vec<TimelineToolCall>, _workflow: Option<Value>| {
        let key = match kind {
            TimelineKind::WorkflowTools => format!("workflow-tools:{message_uuid}"),
            other => format!("{other:?}:{message_uuid}"),
        };
        // The workflow run rides on the message's Workflow tool call.
        let mut message = message.clone();
        if message.workflow.is_none() {
            message.workflow = message
                .tool_calls
                .iter()
                .find(|call| call.name == "Workflow")
                .and_then(|call| call.workflow.clone());
        }
        TimelineItem {
            kind,
            key,
            message_uuid: message_uuid.clone(),
            message,
            tool_calls,
        }
    };

    if message.is_meta {
        return vec![item(TimelineKind::Meta, Vec::new(), None)];
    }
    let workflow_call = if message.r#type != "user" {
        message
            .tool_calls
            .iter()
            .find(|call| call.name == "Workflow" && call.workflow.is_some())
            .cloned()
    } else {
        None
    };
    if let Some(workflow_call) = workflow_call {
        let workflow = workflow_call.workflow.clone();
        let mut items = vec![item(TimelineKind::Workflow, Vec::new(), workflow)];
        let tool_calls: Vec<TimelineToolCall> = message
            .tool_calls
            .iter()
            .filter(|call| call.id != workflow_call.id)
            .cloned()
            .collect();
        if !tool_calls.is_empty() {
            items.push(item(TimelineKind::WorkflowTools, tool_calls, None));
        }
        return items;
    }
    if message.r#type == "assistant"
        && message.tool_calls.len() == 1
        && message.tool_calls[0].name == "Skill"
        && message.text.as_deref().unwrap_or("").is_empty()
    {
        return vec![item(TimelineKind::Skill, Vec::new(), None)];
    }
    if message.r#type == "assistant" && message.content_type.as_deref() == Some("thinking") {
        return vec![item(TimelineKind::Thinking, Vec::new(), None)];
    }
    vec![item(TimelineKind::Message, Vec::new(), None)]
}

/// Port of reconcileTimelineItems (the reconcile-vs-existing identity check
/// exists for the Vue incremental DOM path; GPUI re-renders from data, so
/// plain assembly is equivalent).
pub fn timeline_items(messages: &[TimelineMessage]) -> Vec<TimelineItem> {
    messages
        .iter()
        .flat_map(|message| message_items(message, messages.len()))
        .collect()
}
