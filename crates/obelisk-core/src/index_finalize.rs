// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Shared database-finalize policies (port of packages/core/src/index-finalize.ts).
//! Callers keep ownership of discovery, retries, watcher state, and
//! transactions; this module owns the invariants that must not diverge
//! between the passive CLI indexer and the app daemon.

use rusqlite::Connection;

use crate::parsing::infer_project_path;

pub const FTS_TRIGGERS_READY_MARKER: &str = "__fts_triggers_ready__";
pub const PROJECT_PATH_BACKFILL_MARKER: &str = "__project_path_backfill_v1__";
const MESSAGE_FTS_TRIGGERS: [&str; 3] = ["messages_fts_ai", "messages_fts_ad", "messages_fts_au"];

/// Remove message FTS triggers before a caller-owned bulk replacement.
pub fn drop_message_fts_triggers(conn: &Connection) -> rusqlite::Result<()> {
    for trigger in MESSAGE_FTS_TRIGGERS {
        conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {trigger}"))?;
    }
    Ok(())
}

/// Rebuild both external-content FTS indexes only when the index has not yet
/// recorded trigger readiness, or when a force snapshot explicitly requests a
/// complete repair. Returns true when a rebuild ran.
pub fn ensure_fts_ready(conn: &Connection, force: bool, now_ms: f64) -> rusqlite::Result<bool> {
    let ready = conn
        .query_row(
            "SELECT jsonl_path FROM index_state WHERE jsonl_path = ?1",
            [FTS_TRIGGERS_READY_MARKER],
            |_| Ok(()),
        )
        .is_ok();
    if ready && !force {
        return Ok(false);
    }
    conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('rebuild')")?;
    conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')")?;
    conn.execute(
        "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES (?, ?, 0)",
        rusqlite::params![FTS_TRIGGERS_READY_MARKER, now_ms],
    )?;
    Ok(true)
}

/// Refresh project paths for every session (None) or exactly one affected set.
pub fn refresh_session_project_paths(
    conn: &Connection,
    session_ids: Option<&std::collections::HashSet<String>>,
) -> rusqlite::Result<()> {
    let mut session_rows: Vec<(String, Option<String>)> = Vec::new();
    match session_ids {
        None => {
            let mut stmt = conn.prepare("SELECT id, project FROM sessions")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            for row in rows {
                session_rows.push(row?);
            }
        }
        Some(ids) => {
            let mut stmt = conn.prepare("SELECT id, project FROM sessions WHERE id = ?1")?;
            for id in ids {
                if let Ok(row) = stmt.query_row([id.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                }) {
                    session_rows.push(row);
                }
            }
        }
    }
    let mut cwd_stmt = conn.prepare(
        "SELECT cwd FROM messages
         WHERE session_id = ?1 AND cwd IS NOT NULL AND cwd != ''
         ORDER BY timestamp IS NULL, timestamp",
    )?;
    for (session_id, project) in &session_rows {
        let rows =
            cwd_stmt.query_map([session_id.as_str()], |row| row.get::<_, Option<String>>(0))?;
        let cwds: Vec<Option<String>> = rows.filter_map(Result::ok).collect();
        let cwd_refs: Vec<Option<&str>> = cwds.iter().map(|c| c.as_deref()).collect();
        let project_path = infer_project_path(project.as_deref(), &cwd_refs);
        if let Some(project_path) = project_path {
            conn.execute(
                "UPDATE sessions SET project_path = ?1 WHERE id = ?2",
                rusqlite::params![project_path, session_id],
            )?;
        }
    }
    Ok(())
}

/// Repair legacy unresolved project paths once; new/changed sessions use
/// their unit transaction. Returns true when the backfill ran.
pub fn backfill_unresolved_session_project_paths_once(
    conn: &Connection,
    now_ms: f64,
) -> rusqlite::Result<bool> {
    let done = conn
        .query_row(
            "SELECT jsonl_path FROM index_state WHERE jsonl_path = ?1",
            [PROJECT_PATH_BACKFILL_MARKER],
            |_| Ok(()),
        )
        .is_ok();
    if done {
        return Ok(false);
    }
    let mut unresolved = std::collections::HashSet::new();
    {
        let mut stmt = conn
            .prepare("SELECT id FROM sessions WHERE project_path IS NULL OR project_path = ''")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            unresolved.insert(row?);
        }
    }
    refresh_session_project_paths(conn, Some(&unresolved))?;
    conn.execute(
        "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES (?, ?, 0)",
        rusqlite::params![PROJECT_PATH_BACKFILL_MARKER, now_ms],
    )?;
    Ok(true)
}
