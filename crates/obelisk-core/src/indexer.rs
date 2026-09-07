// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Passive-pull indexing orchestration (port of packages/core/src/indexer.ts).

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::db;
use crate::index_finalize::{
    backfill_unresolved_session_project_paths_once, ensure_fts_ready, refresh_session_project_paths,
};
use crate::provider_indexing::{
    create_provider_index_plan, index_provider_plan, read_recent_transcript_hints,
    write_provider_index_markers, IndexAction, InlineTxRunner, PlanOptions, RunHooks, TxRunner,
    WATCH_HINT_LIMIT,
};
use crate::provider_settings::{
    create_configured_builtin_provider_runtime, read_persisted_provider_settings, SettingsRead,
};
use crate::providers::types::ProviderRegistry;
use crate::schema::core_schema_needs_migration;
use crate::tx::{
    has_unusable_transaction, is_begin_busy_failure, run_retryable_write_transaction,
    run_write_transaction, WriteError, WriteRetryOptions, WriteTxOptions,
};
use crate::writer_lease::{acquire_writer_lease, writer_lock_path_for, AcquireOptions};

const BUILD_DEBOUNCE_MS: f64 = 30000.0;
const APP_HEARTBEAT_FRESH_MS: f64 = 60000.0;

pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SkippedFile {
    pub provider: String,
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BuildResult {
    pub skip: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete_providers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inventory_issues: Vec<InventoryIssueReport>,
    pub skipped: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_files: Vec<SkippedFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_hints: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct InventoryIssueReport {
    pub provider: String,
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy)]
struct Ownership {
    skip: bool,
    reason: Option<&'static str>,
}

fn build_ownership_skip(
    conn: &Connection,
    now: f64,
    ignore_recent: bool,
    ignore_daemon: bool,
) -> Ownership {
    if !ignore_daemon {
        let heartbeat: Option<f64> = conn
            .query_row(
                "SELECT mtime FROM index_state WHERE jsonl_path='__app_heartbeat__'",
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(heartbeat) = heartbeat {
            if now - heartbeat < APP_HEARTBEAT_FRESH_MS {
                return Ownership {
                    skip: true,
                    reason: Some("daemon_active"),
                };
            }
        }
    }
    if !ignore_recent {
        let last: Option<f64> = conn
            .query_row(
                "SELECT mtime FROM index_state WHERE jsonl_path='__last_build__'",
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(last) = last {
            if now - last < BUILD_DEBOUNCE_MS {
                return Ownership {
                    skip: true,
                    reason: Some("recent_build"),
                };
            }
        }
    }
    Ownership {
        skip: false,
        reason: None,
    }
}

fn is_missing_index_state_table(error: &rusqlite::Error) -> bool {
    let message = error.to_string();
    message.contains("no such table") && message.contains("index_state")
}

/// Read-side ownership check. A missing table means the write path must
/// initialize a new/legacy index; any other read failure leaves daemon
/// ownership unknown, so fail closed by rethrowing.
fn inspect_build_ownership(
    home: &Path,
    force: bool,
    ignore_recent_build: bool,
    ignore_daemon_ownership: bool,
    now: f64,
) -> Result<Ownership, rusqlite::Error> {
    if !db::db_path(home).exists() {
        return Ok(Ownership {
            skip: false,
            reason: None,
        });
    }
    let conn = db::open_read_db(home)?;
    let result = (|| -> Result<Ownership, rusqlite::Error> {
        let ownership = build_ownership_skip(
            &conn,
            now,
            ignore_recent_build || force,
            ignore_daemon_ownership,
        );
        if ownership.skip && ownership.reason != Some("daemon_active") {
            return Ok(ownership);
        }
        if !ownership.skip && core_schema_needs_migration(&conn) {
            return Ok(Ownership {
                skip: false,
                reason: None,
            });
        }
        Ok(ownership)
    })();
    drop(conn);
    match result {
        Ok(ownership) => Ok(ownership),
        Err(error) => {
            if is_missing_index_state_table(&error) {
                Ok(Ownership {
                    skip: false,
                    reason: None,
                })
            } else {
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SchemaState {
    pub ready: bool,
    pub reason: Option<&'static str>,
}

/// Queries and daemon-arbitration checks must never migrate/configure the
/// index. If the schema is stale, a writer lease lets this caller upgrade it;
/// an active daemon owns that upgrade instead.
pub fn ensure_readable_schema(home: &Path) -> Result<SchemaState, rusqlite::Error> {
    let inspect = || -> Result<SchemaState, rusqlite::Error> {
        if !db::db_path(home).exists() {
            return Ok(SchemaState {
                ready: false,
                reason: None,
            });
        }
        let conn = db::open_read_db(home)?;
        let state = (|| -> Result<SchemaState, rusqlite::Error> {
            if !core_schema_needs_migration(&conn) {
                return Ok(SchemaState {
                    ready: true,
                    reason: None,
                });
            }
            match build_ownership_skip(&conn, now_ms(), true, false) {
                Ownership {
                    skip: true,
                    reason: Some("daemon_active"),
                } => Ok(SchemaState {
                    ready: false,
                    reason: Some("daemon_active"),
                }),
                Ownership {
                    skip: true,
                    reason: Some("recent_build"),
                } => Ok(SchemaState {
                    ready: false,
                    reason: None,
                }),
                _ => Ok(SchemaState {
                    ready: false,
                    reason: None,
                }),
            }
        })();
        drop(conn);
        match state {
            Err(error) if !is_missing_index_state_table(&error) => Err(error),
            other => other,
        }
    };
    let state = inspect()?;
    if state.ready || state.reason.is_some() {
        return Ok(state);
    }
    let lease = acquire_writer_lease(
        &writer_lock_path_for(&db::db_path(home)),
        AcquireOptions {
            wait_ms: 1000,
            retry_delay_ms: 25,
        },
    );
    let Some(lease) = lease else {
        return Ok(SchemaState {
            ready: false,
            reason: Some("writer_busy"),
        });
    };
    let outcome = (|| -> Result<SchemaState, rusqlite::Error> {
        let state = inspect()?;
        if state.ready || state.reason.is_some() {
            return Ok(state);
        }
        drop(db::open_db(home)?);
        Ok(SchemaState {
            ready: true,
            reason: None,
        })
    })();
    lease.release();
    outcome
}

/// A workflow unit links to its parent Workflow tool call by matching the
/// unique run id in the tool_result text — but the run json can reach the
/// index before that tool_result lands in the main transcript. Once every
/// unit is persisted the tool_results table holds the result text, so the
/// match can be completed in SQL. Runs at every finalize.
fn heal_workflow_parent_links(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "UPDATE workflows
         SET parent_tool_use_id = (
           SELECT tr.tool_use_id
           FROM tool_results tr
           JOIN tool_calls tc ON tc.id = tr.tool_use_id AND tc.session_id = tr.session_id
           WHERE tr.session_id = workflows.session_id
             AND tc.name = 'Workflow'
             AND instr(tr.content, workflows.run_id) > 0
           ORDER BY tr.rowid
           LIMIT 1
         )
         WHERE parent_tool_use_id IS NULL
           AND EXISTS (
             SELECT 1
             FROM tool_results tr
             JOIN tool_calls tc ON tc.id = tr.tool_use_id AND tc.session_id = tr.session_id
             WHERE tr.session_id = workflows.session_id
               AND tc.name = 'Workflow'
               AND instr(tr.content, workflows.run_id) > 0
           )",
    )?;
    Ok(())
}

#[derive(Clone, Default)]
pub struct BuildIndexOptions {
    pub force: bool,
    /// Bypass the recent-build debounce without selecting the force
    /// full-republish path (invocation-nonce freshness recovery).
    pub ignore_recent_build: bool,
    /// Bypass the daemon-ownership policy check; narrow carve-out for the
    /// invocation-nonce freshness build. The writer lease remains the sole
    /// write arbitrator (ADR 0006 amendment).
    pub ignore_daemon_ownership: bool,
    pub provider_registry: Option<Arc<ProviderRegistry>>,
}

struct RetryableTxRunner<'a> {
    conn: &'a Connection,
}

impl TxRunner for RetryableTxRunner<'_> {
    fn run(
        &mut self,
        label: &str,
        work: &mut dyn FnMut() -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
    ) -> Result<(), WriteError> {
        run_retryable_write_transaction(
            self.conn,
            work,
            WriteTxOptions {
                label: Some(label.to_string()),
            },
            &WriteRetryOptions::default(),
        )
        .map(|_: ()| ())
    }
}

/// The passive-pull build entry point. Mirrors TS `buildIndex`: ownership
/// check → writer lease → provider plan → per-unit transactions (or one
/// force snapshot) → finalize. All skip paths return honest reasons.
pub fn build_index(home: &Path, options: BuildIndexOptions) -> BuildResult {
    build_index_at(home, options, now_ms())
}

/// Write the resident-daemon liveness marker (TS `writeHeartbeat` port,
/// ADR-0013 M3.1). The row makes CLI-side builds skip with `daemon_active`
/// while it stays inside the freshness window above; the write itself goes
/// through the writer lease so it never collides with another writer.
pub fn write_daemon_heartbeat(home: &Path) -> bool {
    let db_path = db::db_path(home);
    if !db_path.exists() {
        return false;
    }
    let Some(lease) = acquire_writer_lease(
        &writer_lock_path_for(&db_path),
        AcquireOptions {
            wait_ms: 1000,
            retry_delay_ms: 25,
        },
    ) else {
        return false;
    };
    let wrote = db::open_db(home).and_then(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__app_heartbeat__', ?, 0)",
            [now_ms()],
        )
    });
    lease.release();
    wrote.is_ok()
}

pub fn build_index_at(home: &Path, options: BuildIndexOptions, now: f64) -> BuildResult {
    let ownership = match inspect_build_ownership(
        home,
        options.force,
        options.ignore_recent_build,
        options.ignore_daemon_ownership,
        now,
    ) {
        Ok(ownership) => ownership,
        Err(_) => {
            // Ownership unknown (read failure other than a missing table):
            // fail closed by letting the write path decide below.
            Ownership {
                skip: false,
                reason: None,
            }
        }
    };
    if ownership.skip {
        return BuildResult {
            skip: true,
            reason: ownership.reason.map(str::to_string),
            ..Default::default()
        };
    }
    let Some(lease) = acquire_writer_lease(
        &writer_lock_path_for(&db::db_path(home)),
        AcquireOptions::default(),
    ) else {
        return BuildResult {
            skip: true,
            reason: Some("writer_busy".to_string()),
            ..Default::default()
        };
    };
    let result = build_index_with_lease(home, &options, now);
    lease.release();
    result
}

fn build_index_with_lease(home: &Path, options: &BuildIndexOptions, now: f64) -> BuildResult {
    // Ownership may change between the first read and lease acquisition.
    let ownership = inspect_build_ownership(
        home,
        options.force,
        options.ignore_recent_build,
        options.ignore_daemon_ownership,
        now,
    );
    let ownership = match ownership {
        Ok(ownership) => ownership,
        Err(_) => Ownership {
            skip: false,
            reason: None,
        },
    };
    if ownership.skip {
        return BuildResult {
            skip: true,
            reason: ownership.reason.map(str::to_string),
            ..Default::default()
        };
    }
    let registry_storage;
    let registry: Arc<ProviderRegistry> = match &options.provider_registry {
        Some(registry) => registry.clone(),
        None => {
            let settings = read_persisted_provider_settings(home);
            let SettingsRead::Ok(persisted) = settings else {
                let SettingsRead::Failed(error) = settings else {
                    unreachable!()
                };
                return BuildResult {
                    skip: true,
                    reason: Some("settings_unavailable".to_string()),
                    error: Some(error),
                    ..Default::default()
                };
            };
            let runtime = create_configured_builtin_provider_runtime(
                home,
                &std::env::current_dir().unwrap_or_default(),
                &persisted,
                &Default::default(),
            );
            registry_storage = Arc::new(runtime.registry);
            registry_storage
        }
    };

    let conn = match db::open_db(home) {
        Ok(conn) => conn,
        Err(error) => {
            return BuildResult {
                skip: false,
                complete: Some(false),
                reason: Some("provider_failure".to_string()),
                error: Some(error.to_string()),
                ..Default::default()
            }
        }
    };
    let mut skipped_files: Vec<SkippedFile> = Vec::new();
    let outcome = build_with_connection(&conn, home, options, &registry, now, &mut skipped_files);
    drop(conn);
    match outcome {
        Ok(result) => result,
        Err(error) => BuildResult {
            skip: false,
            complete: Some(false),
            reason: Some("provider_failure".to_string()),
            error: Some(error.to_string()),
            skipped: skipped_files.len(),
            skipped_files,
            ..Default::default()
        },
    }
}

fn build_with_connection(
    conn: &Connection,
    _home: &Path,
    options: &BuildIndexOptions,
    registry: &Arc<ProviderRegistry>,
    now: f64,
    skipped_files: &mut Vec<SkippedFile>,
) -> Result<BuildResult, String> {
    let plan = create_provider_index_plan(
        conn,
        registry,
        PlanOptions {
            force: options.force,
            changed_paths: None,
            prior_sessions: None,
        },
    );
    let mut incomplete_providers: Vec<String> = plan.incomplete_providers.iter().cloned().collect();
    incomplete_providers.sort();
    let inventory_issues: Vec<InventoryIssueReport> = plan
        .inventory_issues
        .iter()
        .map(|issue| InventoryIssueReport {
            provider: issue.provider.clone(),
            path: issue.path.clone(),
            error: issue.error.clone(),
        })
        .collect();

    if options.force && !incomplete_providers.is_empty() {
        return Ok(BuildResult {
            skip: false,
            complete: Some(false),
            reason: Some("incomplete_snapshot".to_string()),
            incomplete_providers,
            inventory_issues,
            skipped: 0,
            skipped_files: std::mem::take(skipped_files),
            ..Default::default()
        });
    }

    if options.force {
        // A force build publishes one complete source snapshot or nothing.
        // Bulk-replacement optimization: the per-row FTS maintenance
        // triggers are dropped for the delete+insert pass and both FTS
        // indexes are rebuilt in one statement afterwards (the same
        // mechanism the repair path uses; row content is identical, so the
        // published dump is unchanged).
        let force_result = run_write_transaction(
            conn,
            || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                crate::index_finalize::drop_message_fts_triggers(conn)?;
                conn.execute_batch("DELETE FROM index_state")?;
                for table in [
                    "messages",
                    "tool_calls",
                    "tool_results",
                    "sessions",
                    "summaries",
                    "subagents",
                    "workflows",
                    "workflow_agents",
                ] {
                    conn.execute_batch(&format!("DELETE FROM {table}"))?;
                }
                let mut inline = InlineTxRunner;
                let mut hooks = RunHooks {
                    on_persisted: None,
                    on_committed: None,
                    on_error: &mut |_error, _item| IndexAction::Skip,
                };
                let provider_result = index_provider_plan(conn, &plan, &mut inline, &mut hooks);
                refresh_session_project_paths(conn, None)?;
                backfill_unresolved_session_project_paths_once(conn, now)?;
                heal_workflow_parent_links(conn)?;
                ensure_fts_ready(conn, true, now)?;
                // Re-create the maintenance triggers after the one-shot
                // rebuild (the DDL is idempotent).
                conn.execute_batch(crate::schema::SCHEMA_SQL)?;
                conn.execute(
                    "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__last_build__', ?, 0)",
                    [now],
                )?;
                write_provider_index_markers(conn, &plan, &provider_result, now)?;
                if let Some((provider, key, error)) = provider_result.stopped {
                    return Err(format!("failed to index {provider} unit {key}: {error}").into());
                }
                if !provider_result.failed_items.is_empty() {
                    let failed = &provider_result.failed_items[0];
                    return Err(format!(
                        "failed to index {} unit {}: see provider errors",
                        failed.provider, failed.key
                    )
                    .into());
                }
                Ok(())
            },
            WriteTxOptions {
                label: Some("force-rebuild".to_string()),
            },
        );
        if let Err(error) = force_result {
            if is_begin_busy_failure(&error) {
                return Ok(BuildResult {
                    skip: true,
                    complete: Some(false),
                    reason: Some("database_busy".to_string()),
                    incomplete_providers,
                    inventory_issues,
                    skipped: 0,
                    skipped_files: std::mem::take(skipped_files),
                    ..Default::default()
                });
            }
            let message = error.to_string();
            skipped_files.push(SkippedFile {
                provider: "unknown".to_string(),
                path: "force".to_string(),
                error: message.clone(),
            });
            return Ok(BuildResult {
                skip: false,
                complete: Some(false),
                reason: Some("provider_failure".to_string()),
                incomplete_providers,
                inventory_issues,
                skipped: skipped_files.len(),
                skipped_files: std::mem::take(skipped_files),
                ..Default::default()
            });
        }
        return Ok(BuildResult {
            skip: false,
            complete: Some(true),
            incomplete_providers,
            inventory_issues,
            skipped: 0,
            skipped_files: std::mem::take(skipped_files),
            watch_hints: Some(read_recent_transcript_hints(conn, WATCH_HINT_LIMIT)),
            ..Default::default()
        });
    }

    // Incremental path: per-unit transactions with retry policy.
    let mut tx_runner = RetryableTxRunner { conn };
    let mut on_persisted = |conn_unit: &crate::provider_indexing::ProviderIndexItem, _| {
        let mut ids = std::collections::HashSet::new();
        ids.insert(conn_unit.unit.session_id.clone());
        for id in &conn_unit.unit.retract_session_ids {
            ids.insert(id.clone());
        }
        let _ = refresh_session_project_paths(conn, Some(&ids));
    };
    let mut on_error =
        |error: &WriteError, item: &crate::provider_indexing::ProviderIndexItem| -> IndexAction {
            if is_begin_busy_failure(error) {
                return IndexAction::Stop;
            }
            if has_unusable_transaction(error) {
                // Rethrow-equivalent: mark as a hard failure via Stop with the
                // error surfaced in skipped_files; the TS version throws.
                skipped_files.push(SkippedFile {
                    provider: item.provider.name().to_string(),
                    path: item.unit.key.clone(),
                    error: error.to_string(),
                });
                return IndexAction::Stop;
            }
            let message = error.to_string();
            skipped_files.push(SkippedFile {
                provider: item.provider.name().to_string(),
                path: item.unit.key.clone(),
                error: message,
            });
            IndexAction::Skip
        };
    let mut hooks = RunHooks {
        on_persisted: Some(&mut on_persisted),
        on_committed: None,
        on_error: &mut on_error,
    };
    let provider_result = index_provider_plan(conn, &plan, &mut tx_runner, &mut hooks);
    if provider_result.stopped.is_some() {
        return Ok(BuildResult {
            skip: true,
            complete: Some(false),
            reason: Some("database_busy".to_string()),
            incomplete_providers,
            inventory_issues,
            skipped: skipped_files.len(),
            skipped_files: std::mem::take(skipped_files),
            ..Default::default()
        });
    }
    // Finalize is one transaction and is NOT swallowed: a finalize failure
    // fails the build (a half-finalized index would be inconsistent).
    let finalize = run_retryable_write_transaction(
        conn,
        || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            backfill_unresolved_session_project_paths_once(conn, now)?;
            heal_workflow_parent_links(conn)?;
            ensure_fts_ready(conn, false, now)?;
            conn.execute(
                "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__last_build__', ?, 0)",
                [now],
            )?;
            write_provider_index_markers(conn, &plan, &provider_result, now)?;
            Ok(())
        },
        WriteTxOptions {
            label: Some("finalize".to_string()),
        },
        &WriteRetryOptions::default(),
    );
    if let Err(error) = finalize {
        if is_begin_busy_failure(&error) {
            return Ok(BuildResult {
                skip: true,
                complete: Some(false),
                reason: Some("database_busy".to_string()),
                incomplete_providers,
                inventory_issues,
                skipped: skipped_files.len(),
                skipped_files: std::mem::take(skipped_files),
                ..Default::default()
            });
        }
        return Err(error.to_string());
    }
    Ok(BuildResult {
        skip: false,
        complete: Some(provider_result.complete),
        incomplete_providers,
        inventory_issues,
        skipped: skipped_files.len(),
        skipped_files: std::mem::take(skipped_files),
        watch_hints: Some(read_recent_transcript_hints(conn, WATCH_HINT_LIMIT)),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_on_empty_home_creates_index() {
        let home = tempfile::tempdir().unwrap();
        let result = build_index(
            home.path(),
            BuildIndexOptions {
                force: true,
                ..Default::default()
            },
        );
        assert!(!result.skip);
        assert_eq!(
            result.complete,
            Some(true),
            "empty home force build completes: {result:?}"
        );
        let conn = db::open_read_db(home.path()).unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sessions, 0);
        let markers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM index_state WHERE jsonl_path LIKE '\\_\\_%' ESCAPE '\\'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Empty home: system markers (__last_build__/__fts_triggers_ready__/
        // __project_path_backfill_v1__) plus one index-version marker per
        // builtin provider that defines one.
        assert!(
            (4..=8).contains(&markers),
            "unexpected marker count {markers}"
        );
        for marker in [
            "__last_build__",
            "__fts_triggers_ready__",
            "__project_path_backfill_v1__",
        ] {
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM index_state WHERE jsonl_path = ?1",
                    [marker],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "{marker} must be written");
        }
    }

    #[test]
    fn recent_build_is_debounced_but_force_bypasses() {
        let home = tempfile::tempdir().unwrap();
        // Seed the index with a recent __last_build__ marker.
        {
            let conn = db::open_db(home.path()).unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__last_build__', ?, 0)",
                [now_ms()],
            )
            .unwrap();
        }
        let now = now_ms();
        let incremental = build_index_at(home.path(), BuildIndexOptions::default(), now);
        assert!(incremental.skip);
        assert_eq!(incremental.reason.as_deref(), Some("recent_build"));

        let forced = build_index_at(
            home.path(),
            BuildIndexOptions {
                force: true,
                ..Default::default()
            },
            now,
        );
        assert!(!forced.skip, "force bypasses the debounce");
    }

    #[test]
    fn write_daemon_heartbeat_persists_and_owns() {
        let home = tempfile::tempdir().unwrap();
        // No index db yet: the heartbeat is silently skipped (TS parity).
        assert!(!write_daemon_heartbeat(home.path()));

        // Build once so the db and schema exist, then the marker lands and
        // immediately owns builds for the freshness window.
        build_index(home.path(), BuildIndexOptions::default());
        assert!(write_daemon_heartbeat(home.path()));
        let fresh = build_index_at(
            home.path(),
            BuildIndexOptions::default(),
            now_ms() + 10_000.0,
        );
        assert!(fresh.skip);
        assert_eq!(fresh.reason.as_deref(), Some("daemon_active"));
        let expired = build_index_at(
            home.path(),
            BuildIndexOptions::default(),
            now_ms() + APP_HEARTBEAT_FRESH_MS + 5_000.0,
        );
        assert!(!expired.skip);
    }

    #[test]
    fn write_daemon_heartbeat_defers_to_a_busy_lease() {
        let home = tempfile::tempdir().unwrap();
        build_index(home.path(), BuildIndexOptions::default());
        // Hold the lease from another connection: the heartbeat (1s wait)
        // must back off without taking the write path.
        let lock = acquire_writer_lease(
            &writer_lock_path_for(&db::db_path(home.path())),
            AcquireOptions::default(),
        )
        .unwrap();
        assert!(!write_daemon_heartbeat(home.path()));
        lock.release();
        assert!(write_daemon_heartbeat(home.path()));
    }

    #[test]
    fn daemon_heartbeat_owns_the_build() {
        let home = tempfile::tempdir().unwrap();
        {
            let conn = db::open_db(home.path()).unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__app_heartbeat__', ?, 0)",
                [now_ms()],
            )
            .unwrap();
        }
        let result = build_index(home.path(), BuildIndexOptions::default());
        assert!(result.skip);
        assert_eq!(result.reason.as_deref(), Some("daemon_active"));

        // The narrow carve-out bypasses the ownership check.
        let carved = build_index(
            home.path(),
            BuildIndexOptions {
                ignore_daemon_ownership: true,
                ..Default::default()
            },
        );
        assert!(!carved.skip, "carve-out bypasses daemon ownership");
    }
}
