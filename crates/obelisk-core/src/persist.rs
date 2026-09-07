// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Shared persist layer (port of packages/core/src/persist.ts, ADR-0001).
//!
//! Provider-agnostic: it consumes the TranscriptRecord stream from any
//! adapter and writes rows. Write semantics are the canonical ones:
//! messages upsert via ON CONFLICT; sessions merge with the existing row
//! (started_at MIN, ended_at MAX, message_count reset-or-accumulate,
//! fill-if-null for the rest); turn-duration is a targeted UPDATE;
//! delete-session cascades. The stream's final cursor is persisted verbatim
//! into index_state.

use rusqlite::Connection;

use crate::providers::types::{IndexUnit, SessionCountMode, StreamItem, TranscriptRecord};

/// A provider unit failed mid-stream (the TS generator threw).
#[derive(Debug)]
pub struct ProviderUnitError(pub String);

impl std::fmt::Display for ProviderUnitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ProviderUnitError {}

fn min_str(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (None, b) => b.map(str::to_string),
        (a, None) => a.map(str::to_string),
        (Some(a), Some(b)) => Some(if a < b { a } else { b }.to_string()),
    }
}

fn max_str(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (None, b) => b.map(str::to_string),
        (a, None) => a.map(str::to_string),
        (Some(a), Some(b)) => Some(if a > b { a } else { b }.to_string()),
    }
}

/// One persisted row of `sessions`, used for the merge-on-conflict logic.
#[derive(Debug, Clone, Default)]
pub struct ExistingSession {
    pub title: Option<String>,
    pub project: Option<String>,
    pub project_path: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub message_count: i64,
}

fn existing_session(conn: &Connection, id: &str) -> Option<ExistingSession> {
    conn.query_row(
        "SELECT title, project, project_path, started_at, ended_at, git_branch, version, message_count FROM sessions WHERE id = ?1",
        [id],
        |row| {
            Ok(ExistingSession {
                title: row.get(0)?,
                project: row.get(1)?,
                project_path: row.get(2)?,
                started_at: row.get(3)?,
                ended_at: row.get(4)?,
                git_branch: row.get(5)?,
                version: row.get(6)?,
                message_count: row.get(7)?,
            })
        },
    )
    .ok()
}

/// Cascade-delete every row belonging to a session/thread (guardian retraction).
pub fn delete_session(conn: &Connection, session_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM tool_results WHERE session_id=?1 OR message_uuid IN (SELECT uuid FROM messages WHERE session_id=?1 OR agent_id=?1)",
        [session_id],
    )?;
    conn.execute(
        "DELETE FROM tool_calls WHERE session_id=?1 OR message_uuid IN (SELECT uuid FROM messages WHERE session_id=?1 OR agent_id=?1)",
        [session_id],
    )?;
    conn.execute(
        "DELETE FROM messages WHERE session_id=?1 OR agent_id=?1",
        [session_id],
    )?;
    conn.execute(
        "DELETE FROM subagents WHERE agent_id=?1 OR session_id=?1",
        [session_id],
    )?;
    conn.execute(
        "DELETE FROM workflow_agents WHERE session_id=?1",
        [session_id],
    )?;
    conn.execute("DELETE FROM workflows WHERE session_id=?1", [session_id])?;
    conn.execute("DELETE FROM summaries WHERE session_id=?1", [session_id])?;
    conn.execute("DELETE FROM sessions WHERE id=?1", [session_id])?;
    Ok(())
}

fn write_record(conn: &Connection, record: &TranscriptRecord) -> rusqlite::Result<()> {
    match record {
        TranscriptRecord::Message(r) => {
            let mut stmt = conn.prepare_cached(
                "INSERT INTO messages (uuid,session_id,type,parent_uuid,timestamp,role,text,content_type,is_meta,visibility,model,is_sidechain,agent_id,input_tokens,output_tokens,cwd,skill,source)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(uuid) DO UPDATE SET
                   session_id=excluded.session_id, type=excluded.type, parent_uuid=excluded.parent_uuid,
                   timestamp=excluded.timestamp, role=excluded.role, text=excluded.text,
                   content_type=excluded.content_type, is_meta=excluded.is_meta,
                   visibility=excluded.visibility, model=excluded.model,
                   is_sidechain=excluded.is_sidechain, agent_id=excluded.agent_id,
                   input_tokens=excluded.input_tokens, output_tokens=excluded.output_tokens,
                   cwd=excluded.cwd, skill=excluded.skill, source=excluded.source",
            )?;
            stmt.execute(rusqlite::params![
                r.uuid,
                r.session_id,
                r.r#type,
                r.parent_uuid,
                r.timestamp,
                r.role,
                r.text,
                r.content_type,
                r.is_meta as i64,
                r.visibility.as_str(),
                r.model,
                r.is_sidechain as i64,
                r.agent_id,
                r.input_tokens,
                r.output_tokens,
                r.cwd,
                r.skill,
                r.source,
            ])?;
        }
        TranscriptRecord::ToolCall(r) => {
            let mut stmt = conn.prepare_cached(
                "INSERT OR REPLACE INTO tool_calls (id,message_uuid,session_id,name,presentation,input_json,file_path) VALUES (?,?,?,?,?,?,?)",
            )?;
            stmt.execute(rusqlite::params![
                r.id,
                r.message_uuid,
                r.session_id,
                r.name,
                r.presentation.as_str(),
                r.input_json,
                r.file_path,
            ])?;
        }
        TranscriptRecord::ToolResult(r) => {
            let mut stmt = conn.prepare_cached(
                "INSERT OR REPLACE INTO tool_results (tool_use_id,message_uuid,session_id,content,file_path,is_error) VALUES (?,?,?,?,?,?)",
            )?;
            stmt.execute(rusqlite::params![
                r.tool_use_id,
                r.message_uuid,
                r.session_id,
                r.content,
                r.file_path,
                r.is_error as i64,
            ])?;
        }
        TranscriptRecord::Summary(r) => {
            let mut stmt = conn.prepare_cached(
                "INSERT OR REPLACE INTO summaries (id,session_id,timestamp,source,content,visibility,input_tokens,output_tokens) VALUES (?,?,?,?,?,?,?,?)",
            )?;
            stmt.execute(rusqlite::params![
                r.id,
                r.session_id,
                r.timestamp,
                r.source,
                r.content,
                r.visibility.map(|v| v.as_str()).unwrap_or("visible"),
                r.input_tokens,
                r.output_tokens,
            ])?;
        }
        TranscriptRecord::Subagent(r) => {
            let mut stmt = conn.prepare_cached(
                "INSERT INTO subagents (agent_id,session_id,parent_tool_use_id,agent_type,description,duration_ms,total_tokens)
                 VALUES (?,?,?,?,?,?,?)
                 ON CONFLICT(agent_id) DO UPDATE SET
                   session_id=excluded.session_id,
                   parent_tool_use_id=COALESCE(excluded.parent_tool_use_id, subagents.parent_tool_use_id),
                   agent_type=COALESCE(excluded.agent_type, subagents.agent_type),
                   description=COALESCE(excluded.description, subagents.description),
                   duration_ms=COALESCE(excluded.duration_ms, subagents.duration_ms),
                   total_tokens=COALESCE(excluded.total_tokens, subagents.total_tokens)",
            )?;
            stmt.execute(rusqlite::params![
                r.agent_id,
                r.session_id,
                r.parent_tool_use_id,
                r.agent_type,
                r.description,
                r.duration_ms,
                r.total_tokens,
            ])?;
        }
        TranscriptRecord::Workflow(r) => {
            let mut stmt = conn.prepare_cached(
                "INSERT OR REPLACE INTO workflows
                   (run_id,session_id,parent_tool_use_id,task_id,script,result_json,timestamp,agent_count,duration_ms,total_tokens,status,workflow_name)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            )?;
            stmt.execute(rusqlite::params![
                r.run_id,
                r.session_id,
                r.parent_tool_use_id,
                r.task_id,
                r.script,
                r.result_json,
                r.timestamp,
                r.agent_count,
                r.duration_ms,
                r.total_tokens,
                r.status,
                r.workflow_name,
            ])?;
        }
        TranscriptRecord::WorkflowAgent(r) => {
            let mut stmt = conn.prepare_cached(
                "INSERT INTO workflow_agents
                   (agent_id,run_id,session_id,agent_type,description,phase,label,model,state,duration_ms,tokens,tool_calls)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(agent_id) DO UPDATE SET
                   run_id=excluded.run_id, session_id=excluded.session_id,
                   agent_type=COALESCE(excluded.agent_type, workflow_agents.agent_type),
                   description=COALESCE(excluded.description, workflow_agents.description),
                   phase=COALESCE(excluded.phase, workflow_agents.phase),
                   label=COALESCE(excluded.label, workflow_agents.label),
                   model=COALESCE(excluded.model, workflow_agents.model),
                   state=COALESCE(excluded.state, workflow_agents.state),
                   duration_ms=COALESCE(excluded.duration_ms, workflow_agents.duration_ms),
                   tokens=COALESCE(excluded.tokens, workflow_agents.tokens),
                   tool_calls=COALESCE(excluded.tool_calls, workflow_agents.tool_calls)",
            )?;
            stmt.execute(rusqlite::params![
                r.agent_id,
                r.run_id,
                r.session_id,
                r.agent_type,
                r.description,
                r.phase,
                r.label,
                r.model,
                r.state,
                r.duration_ms,
                r.tokens,
                r.tool_calls,
            ])?;
        }
        TranscriptRecord::MessageTurnDuration {
            uuid,
            turn_duration_ms,
        } => {
            let mut stmt =
                conn.prepare_cached("UPDATE messages SET turn_duration_ms=?1 WHERE uuid=?2")?;
            stmt.execute(rusqlite::params![turn_duration_ms, uuid])?;
        }
        TranscriptRecord::Session(r) => {
            let prev = existing_session(conn, &r.id);
            // 'delta' accumulates onto the existing count (line-incremental
            // adapters); 'total' replaces it (full-reparse adapters).
            let message_count = match (r.count_mode, &prev) {
                (SessionCountMode::Delta, Some(prev)) => {
                    prev.message_count.max(0) + r.message_count
                }
                _ => r.message_count,
            };
            let mut stmt = conn.prepare_cached(
                "INSERT OR REPLACE INTO sessions (id,title,project,project_path,started_at,ended_at,git_branch,version,message_count,jsonl_path,source) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            )?;
            stmt.execute(rusqlite::params![
                r.id,
                r.title
                    .clone()
                    .or_else(|| prev.as_ref().and_then(|p| p.title.clone())),
                r.project
                    .clone()
                    .or_else(|| prev.as_ref().and_then(|p| p.project.clone())),
                // authoritative project_path is set by refreshSessionProjectPaths
                prev.as_ref().and_then(|p| p.project_path.clone()),
                min_str(
                    prev.as_ref().and_then(|p| p.started_at.as_deref()),
                    r.started_at.as_deref(),
                ),
                max_str(
                    prev.as_ref().and_then(|p| p.ended_at.as_deref()),
                    r.ended_at.as_deref(),
                ),
                r.git_branch
                    .clone()
                    .or_else(|| prev.as_ref().and_then(|p| p.git_branch.clone())),
                r.version
                    .clone()
                    .or_else(|| prev.as_ref().and_then(|p| p.version.clone())),
                message_count,
                r.jsonl_path,
                r.source,
            ])?;
        }
        TranscriptRecord::DeleteSession { session_id } => {
            delete_session(conn, session_id)?;
        }
    }
    Ok(())
}

/// Consume one unit's record stream into the database and return the new
/// cursor (also written to index_state).
pub fn persist(
    conn: &Connection,
    unit: &IndexUnit,
    stream: impl Iterator<Item = StreamItem>,
) -> rusqlite::Result<Option<String>> {
    for session_id in &unit.retract_session_ids {
        delete_session(conn, session_id)?;
    }
    let mut cursor: Option<String> = None;
    for item in stream {
        match item {
            StreamItem::Record(record) => write_record(conn, &record)?,
            StreamItem::Cursor(value) => cursor = Some(value),
            StreamItem::Error(error) => {
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    ProviderUnitError(error),
                )))
            }
        }
    }
    if let Some(cursor) = &cursor {
        let mut parts = cursor.split(':');
        let mtime = parts
            .next()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0);
        let lines = parts
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let mut stmt = conn.prepare_cached(
            "INSERT OR REPLACE INTO index_state (jsonl_path,mtime,lines_processed,cursor) VALUES (?,?,?,?)",
        )?;
        stmt.execute(rusqlite::params![unit.key, mtime, lines, cursor])?;
    }
    Ok(cursor)
}
