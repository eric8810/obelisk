// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Read-side data loading for the desktop app. The app links obelisk-core
//! directly (no IPC) and reads the shared index; write ownership stays with
//! the resident Rust daemon (ADR-0013 Stage 3).

use std::path::Path;

use obelisk_core::db;

#[derive(Debug, Clone)]
pub struct ProjectSummary {
    pub slug: String,
    pub session_count: usize,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub project: String,
    pub source: String,
    pub started_at: String,
    pub ended_at: String,
    pub message_count: i64,
}

#[derive(Debug, Clone, Default)]
pub struct AppData {
    pub projects: Vec<ProjectSummary>,
    pub sessions: Vec<SessionSummary>,
    /// Memory registry counts for the sidebar badges.
    pub memory_active: usize,
    pub memory_archived: usize,
    /// Whether an index exists and has completed at least one build —
    /// drives the "building vs. genuinely empty" empty-state message.
    pub index_ready: bool,
}

impl AppData {
    /// Load projects + all session summaries from the shared index. Fails
    /// soft to empty data (fresh machine, index not built yet).
    pub fn load(home: &Path, _cwd: &Path) -> Self {
        let conn = match db::open_read_db(home) {
            Ok(conn) => conn,
            Err(_) => return Self::default(),
        };
        let projects = read_projects(&conn);
        let sessions = read_sessions(&conn);
        let (memory_active, memory_archived) = memory_counts(&conn);
        let index_ready = conn
            .query_row(
                "SELECT COUNT(*) FROM index_state WHERE jsonl_path = '__last_build__'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .unwrap_or(false);
        Self {
            projects,
            sessions,
            memory_active,
            memory_archived,
            index_ready,
        }
    }

    /// Sessions for one project, or all sessions when None.
    pub fn sessions_for(&self, project: Option<&str>) -> Vec<SessionSummary> {
        match project {
            None => self.sessions.clone(),
            Some(project) => self
                .sessions
                .iter()
                .filter(|s| s.project == project)
                .cloned()
                .collect(),
        }
    }
}

/// Active/archived memory counts for the sidebar badges (one indexed read;
/// the memory list itself stays lazily loaded per view switch).
fn memory_counts(conn: &rusqlite::Connection) -> (usize, usize) {
    let count = |archived: bool| {
        conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE archived = ?",
            [archived],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    };
    (count(false), count(true))
}

// ---- Memories (Vue db:getMemories / db:readMemoryFile parity) ----

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id: String,
    pub session_id: String,
    pub project: String,
    pub path: String,
    pub summary: String,
    pub created_at: String,
    pub deleted_at: Option<String>,
    pub deleted_reason: Option<String>,
}

impl MemoryEntry {
    pub fn archived(&self) -> bool {
        self.deleted_at.is_some()
    }
}

pub fn load_memories(home: &Path) -> Vec<MemoryEntry> {
    let Ok(conn) = db::open_read_db(home) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, session_id, project, path, COALESCE(summary, ''),
                COALESCE(created_at, ''), deleted_at, deleted_reason
         FROM memories ORDER BY created_at DESC",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok(MemoryEntry {
            id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            project: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            path: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            summary: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            created_at: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            deleted_at: row.get(6)?,
            deleted_reason: row.get(7)?,
        })
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// Read one memory's markdown file. The path comes from the indexer-written
/// memories table (not renderer input), so it is read as-is like the TS side.
pub fn read_memory_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

// ---- Usage stats (Vue db:getUsageStats parity) ----

#[derive(Debug, Clone, Default)]
pub struct UsageDay {
    pub day: String,
    pub tokens: i64,
}

#[derive(Debug, Clone, Default)]
pub struct LongestTurn {
    pub turn_duration_ms: i64,
    /// Message identity, kept for the jump-to-message link (polish pass).
    #[allow(dead_code)]
    pub uuid: String,
    pub session_id: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    pub daily: Vec<UsageDay>,
    pub total_tokens: i64,
    pub peak_day: Option<UsageDay>,
    pub longest_turn: Option<LongestTurn>,
}

pub fn load_usage_stats(home: &Path) -> UsageStats {
    let Ok(conn) = db::open_read_db(home) else {
        return UsageStats::default();
    };
    // Visibility controls evidence display, not accounting: abandoned model
    // calls still consumed tokens, so usage aggregates include them.
    let mut stats = UsageStats::default();
    let Ok(mut stmt) = conn.prepare(
        "WITH usage_events AS (
             SELECT timestamp, input_tokens, output_tokens FROM messages
             WHERE input_tokens IS NOT NULL OR output_tokens IS NOT NULL
             UNION ALL
             SELECT su.timestamp, su.input_tokens, su.output_tokens FROM summaries su
             WHERE su.input_tokens IS NOT NULL OR su.output_tokens IS NOT NULL
         )
         SELECT DATE(timestamp) AS day,
                SUM(COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)) AS tokens
         FROM usage_events GROUP BY DATE(timestamp) ORDER BY day",
    ) else {
        return stats;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok(UsageDay {
            day: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            tokens: row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
        })
    }) else {
        return stats;
    };
    for day in rows.flatten() {
        stats.total_tokens += day.tokens;
        if stats
            .peak_day
            .as_ref()
            .is_none_or(|peak| day.tokens > peak.tokens)
        {
            stats.peak_day = Some(day.clone());
        }
        stats.daily.push(day);
    }
    if let Ok(mut stmt) = conn.prepare(
        "SELECT turn_duration_ms, uuid, session_id, COALESCE(timestamp, '')
         FROM messages WHERE turn_duration_ms IS NOT NULL
         ORDER BY turn_duration_ms DESC LIMIT 1",
    ) {
        if let Ok(turn) = stmt.query_row([], |row| {
            Ok(LongestTurn {
                turn_duration_ms: row.get(0)?,
                uuid: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                session_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                timestamp: row.get(3)?,
            })
        }) {
            stats.longest_turn = Some(turn);
        }
    }
    stats
}

// ---- Overview stats (Vue db:getStats parity) ----

#[derive(Debug, Clone, Default)]
pub struct OverviewStats {
    pub sessions: i64,
    pub memories: i64,
    pub memories_archived: i64,
}

pub fn load_stats(home: &Path) -> OverviewStats {
    let Ok(conn) = db::open_read_db(home) else {
        return OverviewStats::default();
    };
    OverviewStats {
        sessions: conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap_or(0),
        memories: conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0),
        memories_archived: conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE deleted_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0),
    }
}

// ---- Recap (Vue recap:list / recap:read parity) ----

/// List weekly-recap JSON files from `~/.obelisk/recap`, newest first.
pub fn list_recaps(home: &Path) -> Vec<String> {
    let dir = home.join(".obelisk").join("recap");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    names.reverse();
    names
}

/// Read one recap file. The name is basename-checked like the TS side, so a
/// crafted name cannot escape the recap directory.
pub fn read_recap(home: &Path, filename: &str) -> Option<serde_json::Value> {
    let dir = home.join(".obelisk").join("recap");
    let base = std::path::Path::new(filename);
    if base.file_name()? != base.as_os_str() {
        return None;
    }
    let path = dir.join(base);
    if !path.starts_with(&dir) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist one provider root override (`providerRoots[id]` in settings.json).
/// Plain-file write like the editor scheme: it never touches the SQLite
/// index, so it stays legal regardless of which process owns DB writes.
pub fn save_provider_root(home: &Path, provider_id: &str, root: &str) -> Result<(), String> {
    let settings_path = obelisk_core::provider_settings::settings_path(home);
    let mut value = match obelisk_core::provider_settings::read_persisted_provider_settings(home) {
        obelisk_core::provider_settings::SettingsRead::Ok(value) => value,
        obelisk_core::provider_settings::SettingsRead::Failed(error) => {
            return Err(error);
        }
    };
    if !value.is_object() {
        return Err("settings.json is not a JSON object".to_string());
    }
    let roots = value
        .as_object_mut()
        .unwrap()
        .entry("providerRoots")
        .or_insert_with(|| serde_json::json!({}));
    if !roots.is_object() {
        return Err("providerRoots in settings.json is not an object".to_string());
    }
    roots
        .as_object_mut()
        .unwrap()
        .insert(provider_id.to_string(), serde_json::json!(root));
    let serialized = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    let dir = settings_path
        .parent()
        .ok_or("settings path has no parent")?
        .to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let temporary = settings_path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, &settings_path).map_err(|e| e.to_string())?;
    Ok(())
}

// ---- Settings (Vue settings:get / settings:set parity; the settings file
// is plain JSON — writing it does not touch the SQLite index, so it stays
// legal regardless of which process owns the DB writes) ----

#[derive(Debug, Clone)]
pub struct SettingsSnapshot {
    pub raw: serde_json::Value,
}

impl SettingsSnapshot {
    pub fn provider_roots(&self) -> Vec<(String, String)> {
        self.raw
            .get("providerRoots")
            .and_then(|roots| roots.as_object())
            .map(|roots| {
                roots
                    .iter()
                    .filter_map(|(id, path)| {
                        path.as_str().map(|path| (id.clone(), path.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn editor_scheme(&self) -> String {
        self.raw
            .get("editorScheme")
            .and_then(|scheme| scheme.as_str())
            .unwrap_or(crate::file_reference::DEFAULT_EDITOR_SCHEME)
            .to_string()
    }
}

pub fn load_settings(home: &Path) -> SettingsSnapshot {
    let raw = match obelisk_core::provider_settings::read_persisted_provider_settings(home) {
        obelisk_core::provider_settings::SettingsRead::Ok(value) => value,
        _ => serde_json::json!({}),
    };
    SettingsSnapshot { raw }
}

/// Persist the settings file atomically (tmp + rename, Vue
/// `savePersistedSettings` pattern). Only `editorScheme` is editable in the
/// Stage-2 settings page.
pub fn save_editor_scheme(home: &Path, scheme: &str) -> Result<(), String> {
    let settings_path = obelisk_core::provider_settings::settings_path(home);
    let mut value = match obelisk_core::provider_settings::read_persisted_provider_settings(home) {
        obelisk_core::provider_settings::SettingsRead::Ok(value) => value,
        obelisk_core::provider_settings::SettingsRead::Failed(error) => {
            return Err(error);
        }
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("editorScheme".to_string(), serde_json::json!(scheme));
    }
    let serialized = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    let dir = settings_path
        .parent()
        .ok_or("settings path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let temporary = settings_path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &settings_path).map_err(|error| error.to_string())
}

// ---- Full-text search (the agent-facing FTS contract, exposed in-app so
// the search box aligns with `obelisk --search`; read-only) ----

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message_uuid: String,
    pub session_id: String,
    pub snippet: String,
    pub session_title: String,
}

pub fn search_messages(home: &Path, cwd: &Path, query: &str) -> Vec<SearchHit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(conn) = obelisk_core::db::open_read_db(home) else {
        return Vec::new();
    };
    let obelisk_core::provider_settings::SettingsRead::Ok(persisted) =
        obelisk_core::provider_settings::read_persisted_provider_settings(home)
    else {
        return Vec::new();
    };
    let runtime = obelisk_core::provider_settings::create_configured_builtin_provider_runtime(
        home,
        cwd,
        &persisted,
        &Default::default(),
    );
    let api = obelisk_core::query::QueryApi {
        conn,
        registry: std::sync::Arc::new(runtime.registry),
        invoking_session_id: None,
        cwd: cwd.to_path_buf(),
    };
    let rows = api.search(
        &serde_json::json!(query),
        &serde_json::json!({ "limit": 50 }),
    );
    rows.iter()
        .filter_map(|row| {
            let session = row.get("session")?;
            Some(SearchHit {
                message_uuid: row
                    .get("uuid")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                session_id: session
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                snippet: row
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(160)
                    .collect(),
                session_title: session
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(untitled)")
                    .to_string(),
            })
        })
        .collect()
}

fn read_projects(conn: &rusqlite::Connection) -> Vec<ProjectSummary> {
    let mut stmt = match conn.prepare(
        "SELECT project, COUNT(*) AS c
         FROM sessions
         WHERE project IS NOT NULL
         GROUP BY project
         ORDER BY MAX(COALESCE(ended_at, started_at)) DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt
        .query_map([], |row| {
            Ok(ProjectSummary {
                slug: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                session_count: row.get::<_, i64>(1)?.max(0) as usize,
            })
        })
        .unwrap_or_else(|_| panic!("projects query is valid"));
    rows.filter_map(Result::ok).collect()
}

fn read_sessions(conn: &rusqlite::Connection) -> Vec<SessionSummary> {
    let mut stmt = match conn.prepare(
        "SELECT id, COALESCE(title, ''), COALESCE(project, ''), COALESCE(source, 'claude'),
                COALESCE(started_at, ''), COALESCE(ended_at, ''),
                COALESCE(message_count, 0)
         FROM sessions
         ORDER BY COALESCE(ended_at, started_at) DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                project: row.get(2)?,
                source: row.get(3)?,
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                // COALESCE(message_count, 0) is column 6.
                message_count: row.get(6)?,
            })
        })
        .unwrap_or_else(|_| panic!("sessions query is valid"));
    rows.filter_map(Result::ok).collect()
}

// ---- session detail (timeline source) ----

#[derive(Debug, Clone)]
pub struct TimelineToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct TimelineToolCall {
    pub id: String,
    pub name: String,
    pub input_json: String,
    /// Kept for file-reference rendering (remaining M2.3 work).
    #[allow(dead_code)]
    pub file_path: Option<String>,
    pub result: Option<TimelineToolResult>,
    /// The workflow run record for Workflow tool calls.
    pub workflow: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct SessionDetail {
    pub session_title: String,
    pub session_source: String,
    pub messages: Vec<crate::timeline::TimelineMessage>,
}

/// Load one session's timeline source: messages ordered by timestamp with
/// their tool calls joined to results and workflow runs. Reads only.
pub fn load_session_detail(conn: &rusqlite::Connection, session_id: &str) -> Option<SessionDetail> {
    let (session_title, session_source): (String, String) = conn
        .query_row(
            "SELECT COALESCE(title, '(untitled)'), COALESCE(source, 'claude') FROM sessions WHERE id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;

    // Messages in timeline order (visible first, then inactive/hidden for
    // disclosure; the timeline view groups them).
    let mut messages: Vec<crate::timeline::TimelineMessage> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT uuid, session_id, type, timestamp, role, text, content_type,
                        COALESCE(is_meta, 0), COALESCE(visibility, 'visible'), model, agent_id,
                        input_tokens, output_tokens, cwd, COALESCE(source, 'claude')
                 FROM messages
                 WHERE session_id = ?1
                 ORDER BY (visibility = 'visible') DESC, timestamp",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(crate::timeline::TimelineMessage {
                    uuid: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r#type: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    timestamp: row.get(3)?,
                    role: row.get(4)?,
                    text: row.get(5)?,
                    content_type: row.get(6)?,
                    is_meta: row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                    visibility: row
                        .get::<_, Option<String>>(8)?
                        .unwrap_or_else(|| "visible".to_string()),
                    model: row.get(9)?,
                    agent_id: row.get(10)?,
                    input_tokens: row.get(11)?,
                    output_tokens: row.get(12)?,
                    cwd: row.get(13)?,
                    source: row
                        .get::<_, Option<String>>(14)?
                        .unwrap_or_else(|| "claude".to_string()),
                    tool_calls: Vec::new(),
                    workflow: None,
                })
            })
            .ok()?;
        for message in rows.flatten() {
            messages.push(message);
        }
    }
    if messages.is_empty() {
        return None;
    }

    // Tool calls keyed by message uuid.
    let mut calls_by_message: std::collections::HashMap<String, Vec<TimelineToolCall>> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, message_uuid, name, input_json, file_path
                 FROM tool_calls
                 WHERE session_id = ?1
                 ORDER BY rowid",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                let call = TimelineToolCall {
                    id: row.get(0)?,
                    name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    input_json: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    file_path: row.get(4)?,
                    result: None,
                    workflow: None,
                };
                let message_uuid: Option<String> = row.get(1)?;
                Ok((message_uuid, call))
            })
            .ok()?;
        for row in rows.flatten() {
            let (message_uuid, call) = row;
            if let Some(message_uuid) = message_uuid {
                calls_by_message.entry(message_uuid).or_default().push(call);
            }
        }
    }

    // Tool results keyed by tool id.
    let mut results_by_tool: std::collections::HashMap<String, TimelineToolResult> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT tool_use_id, content, COALESCE(is_error, 0)
                 FROM tool_results
                 WHERE session_id = ?1",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    TimelineToolResult {
                        content: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        is_error: row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
                    },
                ))
            })
            .ok()?;
        for row in rows.flatten() {
            results_by_tool.insert(row.0, row.1);
        }
    }

    // Workflow runs keyed by run_id (result text contains the run id).
    let mut workflows_by_run: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT run_id, session_id, parent_tool_use_id, workflow_name, status,
                        timestamp, agent_count, duration_ms, total_tokens
                 FROM workflows
                 WHERE session_id = ?1",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(serde_json::json!({
                    "run_id": row.get::<_, String>(0)?,
                    "session_id": row.get::<_, Option<String>>(1)?,
                    "parent_tool_use_id": row.get::<_, Option<String>>(2)?,
                    "workflow_name": row.get::<_, Option<String>>(3)?,
                    "status": row.get::<_, Option<String>>(4)?,
                    "timestamp": row.get::<_, Option<String>>(5)?,
                    "agent_count": row.get::<_, Option<i64>>(6)?,
                    "duration_ms": row.get::<_, Option<i64>>(7)?,
                    "total_tokens": row.get::<_, Option<i64>>(8)?,
                }))
            })
            .ok()?;
        for row in rows.flatten() {
            if let Some(run_id) = row.get("run_id").and_then(|v| v.as_str()) {
                workflows_by_run.insert(run_id.to_string(), row);
            }
        }
    }

    for message in &mut messages {
        if let Some(calls) = calls_by_message.remove(&message.uuid) {
            let mut attached = Vec::new();
            for mut call in calls {
                if let Some(result) = results_by_tool.remove(&call.id) {
                    call.result = Some(result);
                }
                // Link the workflow run when the result text names the run id.
                if call.name == "Workflow" {
                    if let Some(result) = &call.result {
                        for (run_id, workflow) in &workflows_by_run {
                            if result.content.contains(run_id) {
                                call.workflow = Some(workflow.clone());
                                break;
                            }
                        }
                    }
                }
                attached.push(call);
            }
            message.tool_calls = attached;
        }
    }

    Some(SessionDetail {
        session_title,
        session_source,
        messages,
    })
}
