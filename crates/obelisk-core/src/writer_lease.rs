// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Cross-process single-writer lease (port of packages/core/src/writer-lease.ts).
//!
//! The lock lives in a dedicated SQLite database so every process on every
//! platform shares identical locking semantics. BEGIN IMMEDIATE with
//! busy_timeout=0 either takes the write lock immediately or fails busy.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

pub fn writer_lock_path_for(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("writer.lock.sqlite")
}

fn is_busy(error: &rusqlite::Error) -> bool {
    if let rusqlite::Error::SqliteFailure(ffi_err, message) = error {
        if ffi_err.code == rusqlite::ffi::ErrorCode::DatabaseBusy {
            return true;
        }
        if let Some(message) = message {
            let lowered = message.to_lowercase();
            if lowered.contains("database is locked") || lowered.contains("database is busy") {
                return true;
            }
        }
    }
    false
}

/// An acquired writer lease. Drop (or `release`) returns the write lock.
pub struct WriterLease {
    conn: Option<Connection>,
}

impl WriterLease {
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(conn) = self.conn.take() {
            // A failed ROLLBACK still releases the lock when the connection
            // closes; never mask the primary error with a cleanup error.
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

#[derive(Debug, Clone)]
pub struct AcquireOptions {
    pub wait_ms: u64,
    pub retry_delay_ms: u64,
}

impl Default for AcquireOptions {
    fn default() -> Self {
        Self {
            wait_ms: 0,
            retry_delay_ms: 25,
        }
    }
}

/// Try to acquire the cross-process writer lease. `None` means another
/// writer holds it (after any requested wait budget).
pub fn acquire_writer_lease(lock_path: &Path, options: AcquireOptions) -> Option<WriterLease> {
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let started_at = Instant::now();
    let max_attempts = if options.wait_ms > 0 {
        ((options.wait_ms / options.retry_delay_ms.max(1)) + 1) as usize
    } else {
        1
    };
    for attempt in 0..max_attempts {
        match Connection::open(lock_path) {
            Ok(conn) => {
                conn.execute_batch("PRAGMA busy_timeout=0").ok();
                match conn.execute_batch("BEGIN IMMEDIATE") {
                    Ok(()) => return Some(WriterLease { conn: Some(conn) }),
                    Err(error) => {
                        drop(conn);
                        if !is_busy(&error) {
                            // The lease database itself is broken; treat as
                            // unable to acquire rather than crashing queries.
                            return None;
                        }
                        let remaining = options
                            .wait_ms
                            .saturating_sub(started_at.elapsed().as_millis() as u64);
                        if remaining == 0 || attempt + 1 >= max_attempts {
                            return None;
                        }
                        std::thread::sleep(Duration::from_millis(
                            options.retry_delay_ms.min(remaining),
                        ));
                    }
                }
            }
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_exclusive_and_releasable() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("writer.lock.sqlite");
        let first = acquire_writer_lease(&lock, AcquireOptions::default()).unwrap();
        assert!(acquire_writer_lease(
            &lock,
            AcquireOptions {
                wait_ms: 0,
                retry_delay_ms: 1
            }
        )
        .is_none());
        first.release();
        let second = acquire_writer_lease(&lock, AcquireOptions::default()).unwrap();
        drop(second);
        let third = acquire_writer_lease(&lock, AcquireOptions::default()).unwrap();
        drop(third);
    }

    #[test]
    fn lease_waits_within_budget() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("writer.lock.sqlite");
        let first = acquire_writer_lease(&lock, AcquireOptions::default()).unwrap();
        let started = Instant::now();
        let second = acquire_writer_lease(
            &lock,
            AcquireOptions {
                wait_ms: 120,
                retry_delay_ms: 20,
            },
        );
        assert!(second.is_none());
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(first);
    }
}
