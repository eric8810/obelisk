// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! SQLite write plumbing (port of packages/core/src/tx.ts + write-coordinator.ts).
//!
//! One binding (rusqlite) serves every binary, so the TS binding-agnostic
//! adapters are unnecessary; the transaction primitive, its diagnostics, and
//! the retry policy above it survive unchanged (ADR-0006 semantics).

use std::time::Instant;

/// rusqlite's autocommit flag inverted is TS's `inTransaction()`.
pub fn in_transaction(conn: &rusqlite::Connection) -> bool {
    !conn.is_autocommit()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Begin,
    Work,
    Commit,
    Rollback,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Begin => "begin",
            Phase::Work => "work",
            Phase::Commit => "commit",
            Phase::Rollback => "rollback",
        }
    }
}

/// Diagnostics attached to every write-transaction failure. The retry layer
/// reads exactly these fields (TS `error.obelisk`).
#[derive(Debug, Clone)]
pub struct WriteTxDiagnostics {
    pub phase: &'static str,
    pub code: Option<String>,
    pub label: Option<String>,
    pub rollback_succeeded: Option<bool>,
    pub rollback_error: Option<String>,
    pub transaction_active: Option<bool>,
    pub attempts: usize,
}

/// A failed write transaction, carrying its diagnostics.
#[derive(Debug)]
pub struct WriteError {
    pub source: Box<dyn std::error::Error + Send + Sync>,
    pub diagnostics: WriteTxDiagnostics,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn sqlite_busy_code(error: &rusqlite::Error) -> Option<&'static str> {
    if let rusqlite::Error::SqliteFailure(ffi_err, message) = error {
        if ffi_err.code == rusqlite::ffi::ErrorCode::DatabaseBusy {
            return Some("SQLITE_BUSY");
        }
        if let Some(message) = message {
            let lowered = message.to_lowercase();
            if lowered.contains("database is locked") || lowered.contains("database is busy") {
                return Some("SQLITE_BUSY");
            }
        }
    }
    None
}

fn error_code(error: &rusqlite::Error) -> Option<String> {
    match error {
        rusqlite::Error::SqliteFailure(ffi_err, _) => Some(format!("{:?}", ffi_err.code)),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
pub struct WriteTxOptions {
    /// Diagnostic label for this transaction (e.g. a file path or "finalize").
    pub label: Option<String>,
}

/// Run `work` exactly once inside a transaction and return its value. Retry
/// and scheduling policy belongs to the build coordinator. Cleanup never
/// masks the primary exception. `work` may fail with any error; SQLite
/// errors are recognized for busy/code diagnostics exactly like the TS
/// duck-typed `error.code` path.
pub fn run_write_transaction<T>(
    conn: &rusqlite::Connection,
    work: impl FnOnce() -> Result<T, Box<dyn std::error::Error + Send + Sync>>,
    options: WriteTxOptions,
) -> Result<T, WriteError> {
    let mut phase = Phase::Begin;
    let result = (|| -> Result<(T,), Box<dyn std::error::Error + Send + Sync>> {
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
        phase = Phase::Work;
        let value = work()?;
        phase = Phase::Commit;
        conn.execute_batch("COMMIT")
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
        Ok((value,))
    })();
    match result {
        Ok((value,)) => Ok(value),
        Err(error) => {
            let mut rollback_succeeded: Option<bool> = None;
            let mut rollback_error: Option<String> = None;
            let active_before_rollback = Some(in_transaction(conn));
            if active_before_rollback != Some(false) {
                match conn.execute_batch("ROLLBACK") {
                    Ok(()) => rollback_succeeded = Some(true),
                    Err(failure) => {
                        rollback_succeeded = Some(false);
                        rollback_error = Some(failure.to_string());
                    }
                }
            }
            let sqlite_error = error.downcast_ref::<rusqlite::Error>();
            let busy = sqlite_error.and_then(sqlite_busy_code).map(str::to_string);
            let code = busy.clone().or_else(|| {
                sqlite_error
                    .and_then(error_code)
                    .or_else(|| Some(error.to_string()))
            });
            let diagnostics = WriteTxDiagnostics {
                phase: phase.as_str(),
                code,
                label: options.label,
                rollback_succeeded,
                rollback_error,
                transaction_active: Some(in_transaction(conn)),
                attempts: 1,
            };
            Err(WriteError {
                source: error,
                diagnostics,
            })
        }
    }
}

/// Connection-level pragmas used by every Obelisk writer/reader.
pub fn configure_connection(
    conn: &rusqlite::Connection,
    busy_timeout_ms: u32,
) -> rusqlite::Result<()> {
    conn.execute_batch(&format!("PRAGMA busy_timeout={busy_timeout_ms}"))?;
    conn.execute_batch("PRAGMA journal_mode=WAL")?;
    conn.execute_batch("PRAGMA synchronous=NORMAL")?;
    Ok(())
}

// ---- bounded retry policy (write-coordinator.ts) ----

#[derive(Debug, Clone)]
pub struct WriteRetryOptions {
    pub max_attempts: usize,
    pub budget_ms: u128,
    pub retry_delay_ms: u64,
    /// BEGIN-busy means the work never ran, so retrying is safe for idempotent
    /// work. Opt-in: index builds defer BEGIN contention to their scheduler.
    pub retry_on_begin_busy: bool,
}

impl Default for WriteRetryOptions {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            budget_ms: 1000,
            retry_delay_ms: 25,
            retry_on_begin_busy: false,
        }
    }
}

pub fn is_begin_busy_failure(error: &WriteError) -> bool {
    let info = &error.diagnostics;
    info.phase == "begin"
        && info
            .code
            .as_deref()
            .map(|c| c.starts_with("SQLITE_BUSY"))
            .unwrap_or(false)
        && info.transaction_active == Some(false)
}

pub fn has_unusable_transaction(error: &WriteError) -> bool {
    error.diagnostics.transaction_active != Some(false)
}

pub fn is_retryable_write_failure(error: &WriteError) -> bool {
    let info = &error.diagnostics;
    (info.phase == "work" || info.phase == "commit")
        && info
            .code
            .as_deref()
            .map(|c| c.starts_with("SQLITE_BUSY"))
            .unwrap_or(false)
        && info.transaction_active == Some(false)
}

/// Core's bounded retry policy above the transaction primitive. Callers opt
/// in only for idempotent work; an uncertain/live transaction is never retried.
pub fn run_with_write_retry<T>(
    mut operation: impl FnMut() -> Result<T, WriteError>,
    options: &WriteRetryOptions,
) -> Result<T, WriteError> {
    let started_at = Instant::now();
    let mut attempt = 1usize;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(mut error) => {
                error.diagnostics.attempts = attempt;
                let retryable = is_retryable_write_failure(&error)
                    || (options.retry_on_begin_busy && is_begin_busy_failure(&error));
                if !retryable || attempt >= options.max_attempts {
                    return Err(error);
                }
                let remaining = options
                    .budget_ms
                    .saturating_sub(started_at.elapsed().as_millis());
                if remaining == 0 {
                    return Err(error);
                }
                let delay = (options.retry_delay_ms * attempt as u64).min(remaining as u64);
                std::thread::sleep(std::time::Duration::from_millis(delay));
                attempt += 1;
            }
        }
    }
}

pub fn run_retryable_write_transaction<T>(
    conn: &rusqlite::Connection,
    work: impl FnMut() -> Result<T, Box<dyn std::error::Error + Send + Sync>>,
    transaction_options: WriteTxOptions,
    retry_options: &WriteRetryOptions,
) -> Result<T, WriteError> {
    let mut work = work;
    run_with_write_retry(
        || run_write_transaction(conn, &mut work, transaction_options.clone()),
        retry_options,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
        conn
    }

    #[test]
    fn transaction_commits_and_reports_diagnostics() {
        let conn = conn();
        let out = run_write_transaction(
            &conn,
            || {
                Ok(conn
                    .execute("INSERT INTO t (v) VALUES (1)", [])
                    .map(|_| ())?)
            },
            WriteTxOptions {
                label: Some("test".into()),
            },
        );
        assert!(out.is_ok());
        assert!(!in_transaction(&conn));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn work_failure_rolls_back_and_carries_diagnostics() {
        let conn = conn();
        let err = run_write_transaction::<()>(
            &conn,
            || {
                Ok(conn
                    .execute("INSERT INTO bad_table (v) VALUES (1)", [])
                    .map(|_| ())?)
            },
            WriteTxOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.diagnostics.phase, "work");
        assert!(err.diagnostics.rollback_succeeded == Some(true));
        assert_eq!(err.diagnostics.transaction_active, Some(false));
        assert!(!in_transaction(&conn));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn non_sqlite_work_errors_also_rollback() {
        let conn = conn();
        let err = run_write_transaction::<()>(
            &conn,
            || Err::<(), _>("plain layer failure".into()),
            WriteTxOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.diagnostics.phase, "work");
        assert_eq!(err.diagnostics.code.as_deref(), Some("plain layer failure"));
        assert!(!in_transaction(&conn));
    }

    #[test]
    fn begin_busy_is_detected_cross_connection() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("busy.sqlite");
        let outer = rusqlite::Connection::open(&db).unwrap();
        outer
            .execute_batch("CREATE TABLE t (v INTEGER); BEGIN IMMEDIATE")
            .unwrap();
        let inner = rusqlite::Connection::open(&db).unwrap();
        inner.execute_batch("PRAGMA busy_timeout=0").unwrap();
        let err =
            run_write_transaction::<()>(&inner, || Ok(()), WriteTxOptions::default()).unwrap_err();
        assert_eq!(err.diagnostics.phase, "begin");
        assert!(err
            .diagnostics
            .code
            .as_deref()
            .unwrap()
            .starts_with("SQLITE_BUSY"));
        assert_eq!(err.diagnostics.transaction_active, Some(false));
        outer.execute_batch("ROLLBACK").unwrap();
    }
}
