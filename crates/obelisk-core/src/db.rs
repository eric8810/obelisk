// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Database lifecycle (port of packages/core/src/db.ts). One rusqlite binding
//! serves both binaries; the TS injected persist seam is retired (ADR-0013).

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::schema::{migrate_core_schema_columns, SCHEMA_SQL};
use crate::tx::configure_connection;

pub fn obelisk_dir(home: &Path) -> PathBuf {
    home.join(".obelisk")
}

pub fn db_path(home: &Path) -> PathBuf {
    obelisk_dir(home).join("obelisk.sqlite")
}

pub fn legacy_db_path(home: &Path) -> PathBuf {
    home.join(".claude").join("obelisk.sqlite")
}

fn migrate_legacy_db_if_needed(home: &Path) {
    let db_path = db_path(home);
    if db_path.exists() {
        return;
    }
    let legacy = legacy_db_path(home);
    if !legacy.exists() {
        return;
    }
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::copy(&legacy, &db_path);
    }
}

/// Writer path: migrate legacy DB, create the schema, apply additive columns.
pub fn open_db(home: &Path) -> rusqlite::Result<Connection> {
    migrate_legacy_db_if_needed(home);
    let path = db_path(home);
    if let Some(parent) = path.parent() {
        if let Err(_error) = std::fs::create_dir_all(parent) {
            return Err(rusqlite::Error::InvalidPath(parent.to_path_buf()));
        }
    }
    let conn = Connection::open(&path)?;
    configure_connection(&conn, 250)?;
    migrate_core_schema_columns(&conn)?;
    conn.execute_batch(SCHEMA_SQL)?;
    migrate_core_schema_columns(&conn)?;
    Ok(conn)
}

/// Reader path: never migrates or configures the index. The caller is
/// responsible for ensuring the database exists first.
pub fn open_read_db(home: &Path) -> rusqlite::Result<Connection> {
    let conn =
        Connection::open_with_flags(db_path(home), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("PRAGMA busy_timeout=250")?;
    Ok(conn)
}

/// The memories table columns expected by the attune layer, derived from the
/// shared schema.sql (not restated) so the two can never drift apart.
pub fn attune_memory_columns() -> &'static Vec<String> {
    static COLUMNS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    COLUMNS.get_or_init(|| {
        let re = regex::Regex::new(r"(?s)CREATE TABLE IF NOT EXISTS memories \(([^;]+)\);")
            .expect("memories table regex compiles");
        re.captures(SCHEMA_SQL)
            .map(|caps| {
                caps[1]
                    .split(',')
                    .map(|part| {
                        part.split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_string()
                    })
                    .filter(|column| !column.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    })
}

pub fn attune_memory_triggers() -> &'static Vec<String> {
    static TRIGGERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    TRIGGERS.get_or_init(|| {
        let re = regex::Regex::new(r"CREATE TRIGGER IF NOT EXISTS (memories_fts_\w+)")
            .expect("memories trigger regex compiles");
        re.captures_iter(SCHEMA_SQL)
            .map(|caps| caps[1].to_string())
            .collect()
    })
}

pub const ATTUNE_NOT_INITIALIZED: &str =
    "Obelisk index is not initialized; run an index build (obelisk --build) before writing memories";
pub const ATTUNE_PREDATES_MEMORY: &str =
    "Obelisk index predates the memory layer; run an index build (obelisk --build) before writing memories";

/// Memory mutations touch only memories/memories_fts, which index builds
/// never delete from — so attune is independent of daemon write ownership
/// and the writer lease (ADR-0006 amendment). It still never migrates the
/// index: it opens the existing database as-is and fails honestly when the
/// memory layer is not there yet.
pub fn open_attune_db(home: &Path) -> Result<Connection, String> {
    let path = db_path(home);
    if !path.exists() {
        return Err(ATTUNE_NOT_INITIALIZED.to_string());
    }
    let conn =
        Connection::open(&path).map_err(|error| format!("{ATTUNE_NOT_INITIALIZED}: {error}"))?;
    // Kept short on purpose: lock waiting is owned by the retry layer in
    // execute_attune, so each BEGIN fails fast and retries within that budget.
    conn.execute_batch("PRAGMA busy_timeout=250")
        .map_err(|error| format!("{ATTUNE_NOT_INITIALIZED}: {error}"))?;
    let info = table_columns(&conn, "memories");
    let expected = attune_memory_columns();
    let pk_columns = primary_key_columns(&conn, "memories");
    let columns_ok = !expected.is_empty()
        && expected.iter().all(|column| info.contains(column))
        && pk_columns.len() == 1
        && pk_columns[0] == "id";
    if !columns_ok {
        return Err(ATTUNE_PREDATES_MEMORY.to_string());
    }
    Ok(conn)
}

fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(&format!("PRAGMA table_info({table})")) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |row| row.get::<_, String>(1));
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn primary_key_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = match conn.prepare(&format!("PRAGMA table_info({table})")) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
    });
    match rows {
        Ok(rows) => rows
            .filter_map(Result::ok)
            .filter(|(_, pk)| *pk > 0)
            .map(|(name, _)| name)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn random_probe_id() -> String {
    use uuid::Uuid;
    format!("__attune_probe_{}", Uuid::new_v4())
}

fn random_probe_token(prefix: &str) -> String {
    use uuid::Uuid;
    format!("{prefix}{}", Uuid::new_v4().simple())
}

/// Verify the recall half of the memory layer (FTS + triggers) actually works
/// before accepting mutations. Wrapped in a SAVEPOINT that is always rolled
/// back, so even a successful probe leaves no persistent trace. Must run
/// inside a write transaction (executeAttune's retryable mutation wrapper).
pub fn probe_attune_memory_layer(conn: &Connection) -> Result<(), String> {
    let probe_id = random_probe_id();
    let insert_token = random_probe_token("pins");
    let update_token = random_probe_token("pupd");

    let matches = |token: &str| -> usize {
        conn.query_row(
            "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH ?1 AND id = ?2",
            rusqlite::params![token, probe_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    };
    let layer_error = || ATTUNE_PREDATES_MEMORY.to_string();

    conn.execute_batch("SAVEPOINT attune_probe")
        .map_err(|_| layer_error())?;
    let outcome = (|| -> Result<(), rusqlite::Error> {
        conn.execute(
            "INSERT INTO memories (id, path, summary, created_at) VALUES (?, ?, ?, ?)",
            rusqlite::params![
                probe_id,
                "/probe",
                format!("{insert_token} marker"),
                "1970-01-01T00:00:00Z"
            ],
        )?;
        if matches(&insert_token) != 1 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        conn.execute(
            "UPDATE memories SET summary=? WHERE id=?",
            rusqlite::params![format!("{update_token} marker"), probe_id],
        )?;
        if matches(&insert_token) != 0 || matches(&update_token) != 1 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        conn.execute(
            "DELETE FROM memories WHERE id=?",
            rusqlite::params![probe_id],
        )?;
        if matches(&update_token) != 0 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => {
            // A cleanup failure MUST propagate — otherwise the outer
            // transaction would commit the probe writes it meant to discard.
            conn.execute_batch("ROLLBACK TO attune_probe")
                .map_err(|_| layer_error())?;
            conn.execute_batch("RELEASE attune_probe")
                .map_err(|_| layer_error())?;
            Ok(())
        }
        Err(_) => {
            // Failure path: the probe fails, so the outer transaction rolls
            // back regardless; cleanup errors here may be swallowed.
            let _ = conn.execute_batch("ROLLBACK TO attune_probe");
            let _ = conn.execute_batch("RELEASE attune_probe");
            Err(layer_error())
        }
    }
}

pub fn rebuild_memory_fts(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_db_creates_schema_and_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        {
            let conn = open_db(home.path()).unwrap();
            let tables: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('messages','sessions','memories','index_state')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(tables, 4);
        }
        // Reopening re-applies idempotent DDL and additive migrations.
        let conn = open_db(home.path()).unwrap();
        drop(conn);
    }

    #[test]
    fn open_read_db_fails_when_missing() {
        let home = tempfile::tempdir().unwrap();
        assert!(open_read_db(home.path()).is_err());
    }

    #[test]
    fn attune_requires_initialized_index() {
        let home = tempfile::tempdir().unwrap();
        let error = open_attune_db(home.path()).unwrap_err();
        assert_eq!(error, ATTUNE_NOT_INITIALIZED);
        open_db(home.path()).unwrap();
        let conn = open_attune_db(home.path()).unwrap();
        // The probe runs inside a transaction in production; wrap for the test.
        conn.execute_batch("BEGIN").unwrap();
        probe_attune_memory_layer(&conn).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "probe must leave no rows");
    }

    #[test]
    fn schema_derivation_lists_memory_columns_and_triggers() {
        let columns = attune_memory_columns();
        assert!(columns.contains(&"id".to_string()));
        assert!(columns.contains(&"anchors".to_string()));
        assert!(columns.contains(&"deleted_reason".to_string()));
        let triggers = attune_memory_triggers();
        assert_eq!(triggers.len(), 3);
        assert!(triggers.contains(&"memories_fts_ai".to_string()));
    }
}
