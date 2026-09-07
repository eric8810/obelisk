// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Core provider contract (port of packages/core/src/providers/types.ts,
//! see docs/adr/0001 and ADR-0013).
//!
//! Provider adapters are pure: they discover their own work and parse it
//! into records. The consumer side (persist, session-detail) is
//! provider-agnostic and consumes the canonical transcript only.

use std::collections::HashMap;

/// Opaque per-unit resume/watermark token (`index_state.cursor`). Only the
/// adapter that produced it interprets it.
pub type Cursor = Option<String>;

/// One unit of work an adapter has discovered. Not necessarily a file.
#[derive(Debug, Clone, Default)]
pub struct IndexUnit {
    /// Stable identity used as the index_state cursor key.
    pub key: String,
    /// Session id this unit indexes into.
    pub session_id: String,
    /// Project slug (dash-encoded path), when the source exposes one.
    pub project: Option<String>,
    /// Set for subagent transcripts, whose messages carry an agent id.
    pub is_subagent: bool,
    pub agent_id: Option<String>,
    /// Adapter-private payload, opaque to the orchestration.
    pub meta: Option<serde_json::Value>,
    /// Previously indexed sessions atomically retracted before this unit is written.
    pub retract_session_ids: Vec<String>,
}

/// Read-only source provenance exposed to one provider during discovery.
#[derive(Debug, Clone)]
pub struct IndexedSession {
    pub session_id: String,
    pub jsonl_path: String,
}

/// One source location that prevented a provider from certifying its inventory.
#[derive(Debug, Clone)]
pub struct InventoryIssue {
    pub path: String,
    pub error: String,
}

/// Context the orchestration provides to discovery.
pub struct DiscoverContext<'a> {
    /// Look up the cursor persisted for a unit key on a previous run.
    pub last_cursor: &'a dyn Fn(&str) -> Cursor,
    /// When set (daemon changed-path mode), restrict discovery to these paths.
    pub changed_paths: Option<&'a [String]>,
    /// Sessions already indexed for this provider.
    pub indexed_sessions: Option<&'a dyn Fn() -> Vec<IndexedSession>>,
    /// Report that the source inventory could not be enumerated completely.
    pub report_incomplete_inventory: Option<&'a mut dyn FnMut(InventoryIssue)>,
}

impl<'a> DiscoverContext<'a> {
    pub fn report_incomplete(&mut self, issue: InventoryIssue) {
        if let Some(report) = self.report_incomplete_inventory.as_deref_mut() {
            report(issue);
        }
    }

    pub fn indexed_sessions(&self) -> Vec<IndexedSession> {
        self.indexed_sessions.map(|f| f()).unwrap_or_default()
    }
}

/// Message visibility: provider-attested evidence state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageVisibility {
    Visible,
    Inactive,
    Hidden,
}

impl MessageVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageVisibility::Visible => "visible",
            MessageVisibility::Inactive => "inactive",
            MessageVisibility::Hidden => "hidden",
        }
    }
}

/// Canonical record stream language emitted by every provider adapter.
/// Most kinds map 1:1 onto a schema table; update/retraction records encode
/// canonical state transitions.
#[derive(Debug, Clone)]
pub enum TranscriptRecord {
    Message(MessageRecord),
    ToolCall(ToolCallRecord),
    ToolResult(ToolResultRecord),
    Summary(SummaryRecord),
    Subagent(SubagentRecord),
    Workflow(WorkflowRecord),
    WorkflowAgent(WorkflowAgentRecord),
    /// Update op: sets `messages.turn_duration_ms` for a message inserted by
    /// a separate line, possibly on a different run.
    MessageTurnDuration {
        uuid: String,
        turn_duration_ms: Option<i64>,
    },
    /// Retraction op discovered during parsing.
    DeleteSession {
        session_id: String,
    },
    Session(SessionRecord),
}

#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub uuid: String,
    pub session_id: String,
    pub r#type: String,
    pub parent_uuid: Option<String>,
    pub timestamp: Option<String>,
    pub role: Option<String>,
    pub text: Option<String>,
    pub content_type: Option<String>,
    pub is_meta: bool,
    pub visibility: MessageVisibility,
    pub model: Option<String>,
    pub is_sidechain: bool,
    pub agent_id: Option<String>,
    /// Provider-normalized total input, including cached input.
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cwd: Option<String>,
    pub skill: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub id: String,
    pub message_uuid: String,
    pub session_id: String,
    pub name: String,
    pub presentation: ToolCallPresentation,
    pub input_json: String,
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallPresentation {
    Default,
    Skill,
}

impl ToolCallPresentation {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolCallPresentation::Default => "default",
            ToolCallPresentation::Skill => "skill",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResultRecord {
    pub tool_use_id: String,
    pub message_uuid: Option<String>,
    pub session_id: String,
    pub content: String,
    pub file_path: Option<String>,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct SummaryRecord {
    pub id: String,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub source: String,
    pub content: String,
    /// Provider-attested evidence state; None means `visible`.
    pub visibility: Option<MessageVisibility>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

/// One codex subagent. Non-key fields are optional: a row can be contributed
/// by more than one point in the parse, and persist merges column-wise.
#[derive(Debug, Clone)]
pub struct SubagentRecord {
    pub agent_id: String,
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub duration_ms: Option<i64>,
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct WorkflowRecord {
    pub run_id: String,
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub task_id: Option<String>,
    pub script: Option<String>,
    pub result_json: Option<String>,
    pub timestamp: Option<String>,
    pub agent_count: i64,
    pub duration_ms: Option<i64>,
    pub total_tokens: Option<i64>,
    pub status: Option<String>,
    pub workflow_name: Option<String>,
}

/// One workflow agent; every optional field a unit does not know is omitted
/// and persist merges column-wise (COALESCE) so all contributors land on the
/// same unified `agent_id` row.
#[derive(Debug, Clone)]
pub struct WorkflowAgentRecord {
    pub agent_id: String,
    pub run_id: String,
    pub session_id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub phase: Option<String>,
    pub label: Option<String>,
    pub model: Option<String>,
    pub state: Option<String>,
    pub duration_ms: Option<i64>,
    pub tokens: Option<i64>,
    pub tool_calls: Option<i64>,
}

/// Session-level aggregate, emitted once after the unit's records. See the TS
/// contract: `started_at`/`ended_at`/`message_count` reflect this chunk;
/// persist merges with the existing row.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub message_count: i64,
    /// `Delta` accumulates onto the existing count (line-incremental adapters);
    /// `Total` replaces it (full-reparse adapters).
    pub count_mode: SessionCountMode,
    pub jsonl_path: String,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCountMode {
    Total,
    Delta,
}

/// The TS `parse` is a generator yielding records and RETURNing the new
/// cursor. In Rust the stream is an iterator whose final item carries the
/// cursor, so persist stays memory-bounded exactly like the TS generator.
/// A provider may also fail mid-stream (TS: the generator throws); the
/// error variant aborts the unit and the indexer skips it with a warning.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum StreamItem {
    Record(TranscriptRecord),
    Cursor(String),
    Error(String),
}

pub type ParseStream<'a> = Box<dyn Iterator<Item = StreamItem> + Send + 'a>;

/// Serializable source metadata consumed by settings and renderer surfaces.
#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub vendor: &'static str,
    pub default_root: String,
    pub color: &'static str,
    /// The automatic root is ambiguous; callers must preserve omission until
    /// the user chooses one.
    pub requires_explicit_root: bool,
    /// User-facing explanation for an unavailable automatic root.
    pub root_resolution_reason: Option<String>,
}

pub struct RawLookup<'a> {
    pub source: &'a str,
    pub message_uuid: &'a str,
    pub session: Option<&'a serde_json::Value>,
    pub agent_id: Option<&'a str>,
    /// Cursor committed for this session's source unit, when available.
    pub cursor: Option<&'a str>,
    pub subagent: Option<&'a serde_json::Value>,
    pub workflow_agent: Option<&'a serde_json::Value>,
}

pub struct RawRecord {
    pub text: String,
    pub total_length: Option<usize>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    pub has_more: Option<bool>,
    /// Provider-projected full message body for renderer expansion.
    pub message_text: Option<String>,
}

/// A watchable provider source: a directory `tree` (recursive subscription)
/// or an exact `file` (metadata polling).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchTarget {
    pub kind: WatchTargetKind,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchTargetKind {
    Tree,
    File,
}

/// Complete adapter interface used by every indexing and presentation caller.
pub trait ProviderAdapter: Send + Sync {
    /// Stable source tag stored on rows, e.g. "claude" / "codex".
    fn name(&self) -> &'static str;
    fn descriptor(&self) -> ProviderDescriptor;
    /// Optional index semantics marker; absence forces one replay.
    fn index_version_marker(&self) -> Option<&'static str> {
        None
    }
    /// Recover the IndexUnit key for a persisted session when it differs from
    /// the canonical source path. File-backed providers normally omit this.
    fn session_unit_key(&self, _session: &IndexedSession) -> Option<String> {
        None
    }
    fn watch_targets(&self, configured_root: &str) -> Vec<WatchTarget>;
    fn discover<'a>(&'a self, ctx: &mut DiscoverContext<'a>) -> Vec<IndexUnit>;
    fn parse<'a>(&'a self, unit: &'a IndexUnit, cursor: Cursor) -> ParseStream<'a>;
    fn raw(&self, input: &RawLookup) -> Option<RawRecord>;
}

/// The provider registry (port of providers/registry.ts).
pub struct ProviderRegistry {
    by_id: HashMap<String, std::sync::Arc<dyn ProviderAdapter>>,
    order: Vec<String>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<std::sync::Arc<dyn ProviderAdapter>>) -> Result<Self, String> {
        let mut by_id = HashMap::new();
        let mut order = Vec::new();
        for provider in providers {
            let id = provider.descriptor().id;
            if provider.name() != id {
                return Err(format!(
                    "Provider name \"{}\" must match descriptor id \"{}\"",
                    provider.name(),
                    id
                ));
            }
            if by_id.contains_key(id) {
                return Err(format!("Duplicate provider id: {id}"));
            }
            order.push(id.to_string());
            by_id.insert(id.to_string(), provider);
        }
        Ok(Self { by_id, order })
    }

    pub fn catalog(&self) -> Vec<ProviderDescriptor> {
        self.list().iter().map(|p| p.descriptor()).collect()
    }

    pub fn get(&self, source: &str) -> Option<std::sync::Arc<dyn ProviderAdapter>> {
        self.by_id.get(source).cloned()
    }

    pub fn list(&self) -> Vec<std::sync::Arc<dyn ProviderAdapter>> {
        self.order
            .iter()
            .filter_map(|id| self.by_id.get(id).cloned())
            .collect()
    }

    pub fn watch_targets(&self, configured_roots: &HashMap<String, String>) -> Vec<WatchTarget> {
        let mut seen = std::collections::HashSet::new();
        let mut targets = Vec::new();
        for provider in self.list() {
            let root = configured_roots
                .get(provider.name())
                .cloned()
                .unwrap_or_else(|| provider.descriptor().default_root.clone());
            for target in provider.watch_targets(&root) {
                let key = format!("{:?}:{}", target.kind, target.path);
                if seen.insert(key) {
                    targets.push(target);
                }
            }
        }
        targets
    }

    pub fn raw(&self, input: &RawLookup) -> Option<RawRecord> {
        self.by_id.get(input.source).and_then(|p| p.raw(input))
    }
}
