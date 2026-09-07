// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Shared Obelisk Core schema (frozen SQLite contract; see ADR-0013).
//!
//! The DDL lives in `schema.sql`, embedded verbatim from the TS core so the
//! two implementations cannot drift during the transition. Additive column
//! migrations (the TS `schema-migrations.ts` chain) run after the DDL.

pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Additive column migrations shared by every writer (TS `COLUMN_MIGRATIONS`,
/// in the same order). Each entry is `(table, column, column definition)`.
pub const COLUMN_MIGRATIONS: &[(&str, &str, &str)] = &[
    ("sessions", "source", "TEXT DEFAULT 'claude'"),
    ("messages", "content_type", "TEXT"),
    ("messages", "is_meta", "INTEGER DEFAULT 0"),
    ("messages", "visibility", "TEXT DEFAULT 'visible'"),
    ("messages", "source", "TEXT DEFAULT 'claude'"),
    ("tool_calls", "presentation", "TEXT DEFAULT 'default'"),
    ("workflows", "parent_tool_use_id", "TEXT"),
    ("index_state", "cursor", "TEXT"),
    ("summaries", "visibility", "TEXT DEFAULT 'visible'"),
    ("summaries", "input_tokens", "INTEGER"),
    ("summaries", "output_tokens", "INTEGER"),
    ("memories", "anchors", "TEXT"),
    ("memories", "deleted_at", "TEXT"),
    ("memories", "deleted_reason", "TEXT"),
];

/// True when any expected table/column of the migration chain is missing.
/// Read paths use this to fail closed on an unreadable schema.
pub fn core_schema_needs_migration(conn: &rusqlite::Connection) -> bool {
    use rusqlite::OptionalExtension;

    let mut columns_by_table: std::collections::HashMap<&str, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (table, column, _) in COLUMN_MIGRATIONS {
        let exists: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .optional()
            .expect("sqlite_master read cannot fail on a healthy database");
        if exists.is_none() {
            return true;
        }
        let columns = columns_by_table
            .entry(table)
            .or_insert_with(|| table_columns(conn, table));
        if !columns.contains(*column) {
            return true;
        }
    }
    false
}

/// Apply additive `ALTER TABLE ... ADD COLUMN` migrations for existing tables.
pub fn migrate_core_schema_columns(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;

    let mut columns_by_table: std::collections::HashMap<&str, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (table, column, definition) in COLUMN_MIGRATIONS {
        let exists: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            continue;
        }
        let columns = columns_by_table
            .entry(table)
            .or_insert_with(|| table_columns(conn, table));
        if columns.contains(*column) {
            continue;
        }
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
        columns.insert(column.to_string());
    }
    Ok(())
}

fn table_columns(conn: &rusqlite::Connection, table: &str) -> std::collections::HashSet<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("PRAGMA table_info cannot fail on a healthy database");
    let mut columns = std::collections::HashSet::new();
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("PRAGMA table_info cannot fail on a healthy database");
    for column in rows {
        columns.insert(column.expect("PRAGMA table_info row cannot fail"));
    }
    columns
}

#[cfg(test)]
mod tests {
    use super::*;

    // The TS side of the byte-identical cross-check retired with the TS
    // implementation (ADR-0013 Stage 3): crates/obelisk-core/src/schema.sql
    // is now the single source of truth, and the golden dump
    // (tests/golden/expected-dump.json) pins the resulting row shape.

    fn memory_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn
    }

    #[test]
    fn schema_is_self_contained_and_idempotent() {
        let conn = memory_db();
        // Applying the same DDL twice is a no-op (IF NOT EXISTS everywhere).
        conn.execute_batch(SCHEMA_SQL).unwrap();
        assert!(!core_schema_needs_migration(&conn));
    }

    #[test]
    fn additive_migrations_upgrade_a_legacy_table() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // A pre-migration legacy schema: every migration-chain table exists,
        // each missing the additive columns the modern schema added later.
        conn.execute_batch(
            "CREATE TABLE sessions (
               id TEXT PRIMARY KEY, title TEXT, project TEXT, project_path TEXT,
               started_at TEXT, ended_at TEXT, git_branch TEXT, version TEXT,
               message_count INTEGER DEFAULT 0, jsonl_path TEXT);
             CREATE TABLE messages (
               uuid TEXT PRIMARY KEY, session_id TEXT, type TEXT, parent_uuid TEXT,
               timestamp TEXT, role TEXT, text TEXT, model TEXT,
               is_sidechain INTEGER DEFAULT 0, agent_id TEXT,
               input_tokens INTEGER, output_tokens INTEGER,
               cwd TEXT, skill TEXT, turn_duration_ms INTEGER);
             CREATE TABLE tool_calls (
               id TEXT PRIMARY KEY, message_uuid TEXT, session_id TEXT,
               name TEXT, input_json TEXT, file_path TEXT);
             CREATE TABLE workflows (
               run_id TEXT PRIMARY KEY, session_id TEXT, task_id TEXT,
               script TEXT, result_json TEXT, timestamp TEXT, agent_count INTEGER DEFAULT 0,
               duration_ms INTEGER, total_tokens INTEGER, status TEXT, workflow_name TEXT);
             CREATE TABLE index_state (
               jsonl_path TEXT PRIMARY KEY, mtime REAL, lines_processed INTEGER);
             CREATE TABLE summaries (
               id TEXT PRIMARY KEY, session_id TEXT, timestamp TEXT,
               source TEXT, content TEXT);
             CREATE TABLE memories (
               id TEXT PRIMARY KEY, session_id TEXT, project TEXT,
               message_start TEXT, message_end TEXT,
               path TEXT, summary TEXT, created_at TEXT);",
        )
        .unwrap();
        assert!(core_schema_needs_migration(&conn));
        migrate_core_schema_columns(&conn).unwrap();
        assert!(!core_schema_needs_migration(&conn));
    }
}
