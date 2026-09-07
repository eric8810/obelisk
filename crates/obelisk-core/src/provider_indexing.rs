// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Provider plan construction and execution (port of
//! packages/core/src/provider-indexing.ts).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use crate::parsing::sorted_read_dir;
use crate::persist::persist;
use crate::providers::types::{
    Cursor, DiscoverContext, IndexUnit, IndexedSession, InventoryIssue, ProviderAdapter,
    ProviderRegistry,
};
use crate::tx::WriteError;

#[derive(Debug, Clone)]
pub struct ProviderSessionProvenance {
    pub source: String,
    pub session_id: String,
    pub jsonl_path: String,
}

#[derive(Clone)]
pub struct ProviderIndexItem {
    pub provider: Arc<dyn ProviderAdapter>,
    pub unit: IndexUnit,
    pub cursor: Cursor,
}

impl std::fmt::Debug for ProviderIndexItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderIndexItem")
            .field("provider", &self.provider.name())
            .field("key", &self.unit.key)
            .field("cursor", &self.cursor)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ProviderInventoryIssue {
    pub provider: String,
    pub path: String,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct ProviderIndexPlan {
    pub items: Vec<ProviderIndexItem>,
    pub pending_markers: HashMap<String, String>,
    pub replay_keys: HashMap<String, Vec<String>>,
    pub incomplete_providers: HashSet<String>,
    pub inventory_issues: Vec<ProviderInventoryIssue>,
}

/// Lightweight reference to a plan item (used by result/markers without
/// cloning units).
#[derive(Debug, Clone)]
pub struct ItemRef {
    pub provider: String,
    pub key: String,
}

#[derive(Debug, Default)]
pub struct ProviderIndexResult {
    pub committed: Vec<ItemRef>,
    pub failed_providers: HashSet<String>,
    pub failed_items: Vec<ItemRef>,
    pub complete: bool,
    /// (provider, unit key, error message) of the item that stopped the run.
    pub stopped: Option<(String, String, String)>,
}

pub fn stored_provider_cursor(conn: &Connection, key: &str) -> Cursor {
    use rusqlite::OptionalExtension;
    let row: Option<(f64, i64, Option<String>)> = conn
        .query_row(
            "SELECT mtime, lines_processed, cursor FROM index_state WHERE jsonl_path = ?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap_or(None);
    match row {
        None => None,
        Some((_, _, Some(cursor))) => Some(cursor),
        Some((mtime, lines, None)) => Some(format!("{mtime}:{lines}")),
    }
}

pub fn provider_session_unit_key(
    provider: Option<&Arc<dyn ProviderAdapter>>,
    session: &IndexedSession,
) -> String {
    provider
        .and_then(|p| p.session_unit_key(session))
        .unwrap_or_else(|| session.jsonl_path.clone())
}

pub fn stored_session_cursor(
    conn: &Connection,
    registry: &ProviderRegistry,
    session: Option<&serde_json::Value>,
) -> Cursor {
    let session = session?;
    let jsonl_path = session.get("jsonl_path").and_then(|v| v.as_str())?;
    let source = session
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("claude");
    let unit_key = provider_session_unit_key(
        registry.get(source).as_ref(),
        &IndexedSession {
            session_id: session
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            jsonl_path: jsonl_path.to_string(),
        },
    );
    stored_provider_cursor(conn, &unit_key)
}

pub fn read_provider_session_provenance(conn: &Connection) -> Vec<ProviderSessionProvenance> {
    let mut stmt = conn
        .prepare(
            "SELECT id, jsonl_path, COALESCE(source, 'claude') AS source
             FROM sessions
             WHERE jsonl_path IS NOT NULL AND jsonl_path != ''",
        )
        .expect("sessions provenance query compiles");
    let rows = stmt
        .query_map([], |row| {
            Ok(ProviderSessionProvenance {
                session_id: row.get::<_, String>(0)?,
                jsonl_path: row.get::<_, String>(1)?,
                source: row.get::<_, String>(2)?,
            })
        })
        .expect("sessions provenance query runs");
    rows.filter_map(Result::ok).collect()
}

// ---- hot-file hints for the watcher (ADR-0009) ----

pub const WATCH_HINT_LIMIT: usize = 64;
const HINT_DIRECTORY_FILE_LIMIT: usize = 4;
const HINT_DIRECTORY_CANDIDATE_LIMIT: usize = 16;

fn expand_hint_directory(dir: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    fn walk(current: &Path, depth: usize, candidates: &mut Vec<String>) {
        if candidates.len() >= HINT_DIRECTORY_CANDIDATE_LIMIT || depth > 4 {
            return;
        }
        let entries = match sorted_read_dir(current) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for (name, path) in entries {
            if candidates.len() >= HINT_DIRECTORY_CANDIDATE_LIMIT {
                return;
            }
            if path.is_dir() {
                walk(&path, depth + 1, candidates);
            } else if name.ends_with(".jsonl") {
                candidates.push(path.to_string_lossy().into_owned());
            }
        }
    }
    walk(Path::new(dir), 0, &mut candidates);
    // Rank by file mtime: the actively appended wire is the most recently
    // written; plain readdir order would systematically pick stale files.
    let mut ranked: Vec<(String, f64)> = candidates
        .into_iter()
        .map(|file| {
            let mtime = crate::parsing::file_mtime_ms(Path::new(&file)).unwrap_or(0.0);
            (file, mtime)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
        .into_iter()
        .take(HINT_DIRECTORY_FILE_LIMIT)
        .map(|(file, _)| file)
        .collect()
}

pub fn read_recent_transcript_hints(conn: &Connection, limit: usize) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT jsonl_path FROM index_state WHERE jsonl_path NOT LIKE '\\_\\_%' ESCAPE '\\' ORDER BY mtime DESC LIMIT ?1",
        )
        .expect("hint query compiles");
    let rows = stmt
        .query_map([limit as i64 * 4], |row| row.get::<_, String>(0))
        .expect("hint query runs");
    let mut hints: Vec<String> = Vec::new();
    for key in rows.filter_map(Result::ok) {
        if hints.len() >= limit {
            break;
        }
        let path = PathBuf::from(&key);
        let is_directory = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.is_dir(),
            Err(_) => continue, // deleted between build and hint collection
        };
        if !is_directory {
            hints.push(key);
            continue;
        }
        for file in expand_hint_directory(&key) {
            if hints.len() >= limit {
                break;
            }
            hints.push(file);
        }
    }
    hints
}

// ---- plan construction ----

pub struct PlanOptions<'a> {
    pub force: bool,
    pub changed_paths: Option<&'a [String]>,
    pub prior_sessions: Option<&'a [ProviderSessionProvenance]>,
}

pub fn create_provider_index_plan(
    conn: &Connection,
    registry: &ProviderRegistry,
    options: PlanOptions,
) -> ProviderIndexPlan {
    let force = options.force;
    let mut items: Vec<ProviderIndexItem> = Vec::new();
    let mut pending_markers: HashMap<String, String> = HashMap::new();
    let mut replay_keys: HashMap<String, Vec<String>> = HashMap::new();
    let mut incomplete_providers: HashSet<String> = HashSet::new();
    let mut inventory_issues: Vec<ProviderInventoryIssue> = Vec::new();
    let db_provenance: Vec<ProviderSessionProvenance> = options
        .prior_sessions
        .map(|s| s.to_vec())
        .unwrap_or_else(|| read_provider_session_provenance(conn));
    for provider in registry.list() {
        let indexed_sessions: Vec<IndexedSession> = db_provenance
            .iter()
            .filter(|session| session.source == provider.name())
            .map(|session| IndexedSession {
                session_id: session.session_id.clone(),
                jsonl_path: session.jsonl_path.clone(),
            })
            .collect();
        let marker = provider.index_version_marker();
        let marker_missing = marker
            .map(|marker| {
                conn.query_row(
                    "SELECT jsonl_path FROM index_state WHERE jsonl_path = ?1",
                    [marker],
                    |_| Ok(()),
                )
                .is_err()
            })
            .unwrap_or(false);
        let full_reindex = force || (marker_missing && !indexed_sessions.is_empty());
        if marker_missing && !indexed_sessions.is_empty() {
            let mut keys: Vec<String> = indexed_sessions
                .iter()
                .map(|session| provider_session_unit_key(Some(&provider), session))
                .collect();
            keys.sort();
            keys.dedup();
            replay_keys.insert(provider.name().to_string(), keys);
        }
        let mut inventory_complete = true;
        let mut reported_issue: Option<InventoryIssue> = None;
        let indexed_sessions_fn;
        let mut ctx = DiscoverContext {
            last_cursor: &|key: &str| {
                if full_reindex {
                    None
                } else {
                    stored_provider_cursor(conn, key)
                }
            },
            changed_paths: if full_reindex {
                None
            } else {
                options.changed_paths
            },
            indexed_sessions: {
                indexed_sessions_fn = || indexed_sessions.clone();
                Some(&indexed_sessions_fn)
            },
            report_incomplete_inventory: Some(&mut |issue: InventoryIssue| {
                inventory_complete = false;
                if reported_issue.is_none() {
                    reported_issue = Some(issue);
                }
            }),
        };
        let units = provider.discover(&mut ctx);
        if !inventory_complete {
            incomplete_providers.insert(provider.name().to_string());
            let issue = reported_issue.unwrap_or(InventoryIssue {
                path: provider.descriptor().default_root.clone(),
                error: "Source inventory is incomplete".to_string(),
            });
            inventory_issues.push(ProviderInventoryIssue {
                provider: provider.name().to_string(),
                path: issue.path,
                error: issue.error,
            });
        }
        if let Some(marker) = marker {
            if (force || marker_missing) && (inventory_complete || !indexed_sessions.is_empty()) {
                pending_markers.insert(provider.name().to_string(), marker.to_string());
            }
        }
        for unit in units {
            let cursor = if full_reindex {
                None
            } else {
                stored_provider_cursor(conn, &unit.key)
            };
            items.push(ProviderIndexItem {
                provider: provider.clone(),
                unit,
                cursor,
            });
        }
    }
    ProviderIndexPlan {
        items,
        pending_markers,
        replay_keys,
        incomplete_providers,
        inventory_issues,
    }
}

// ---- plan execution ----

/// What a transaction runner does with a unit's work: run it inside a
/// transaction (incremental path) or directly inside a caller-owned one
/// (force path).
pub trait TxRunner {
    fn run(
        &mut self,
        label: &str,
        work: &mut dyn FnMut() -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
    ) -> Result<(), WriteError>;
}

/// No-op runner for the strict/force path (work runs inside the caller's
/// already-open transaction).
pub struct InlineTxRunner;

impl TxRunner for InlineTxRunner {
    fn run(
        &mut self,
        _label: &str,
        work: &mut dyn FnMut() -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
    ) -> Result<(), WriteError> {
        work().map_err(|error| crate::tx::WriteError {
            diagnostics: crate::tx::WriteTxDiagnostics {
                phase: "work",
                code: None,
                label: None,
                rollback_succeeded: None,
                rollback_error: None,
                transaction_active: None,
                attempts: 1,
            },
            source: error,
        })
    }
}

pub enum IndexAction {
    Skip,
    Stop,
}

pub type PersistedHook<'a> = Option<&'a mut dyn FnMut(&ProviderIndexItem, Cursor)>;

pub struct RunHooks<'a> {
    /// Runs after persist but inside the same transaction, before commit.
    pub on_persisted: PersistedHook<'a>,
    pub on_committed: PersistedHook<'a>,
    /// (error, item) → Skip (log + continue) or Stop (database_busy).
    pub on_error: &'a mut dyn FnMut(&WriteError, &ProviderIndexItem) -> IndexAction,
}

pub fn index_provider_plan(
    conn: &Connection,
    plan: &ProviderIndexPlan,
    tx: &mut dyn TxRunner,
    hooks: &mut RunHooks,
) -> ProviderIndexResult {
    let mut result = ProviderIndexResult::default();
    for item in &plan.items {
        let label = format!("provider:{}:{}", item.provider.name(), item.unit.key);
        let timing = std::env::var_os("OBELISK_TIMING").is_some();
        let started = std::time::Instant::now();
        let mut next_cursor: Option<Cursor> = None;
        let outcome = {
            let mut work = || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let stream = item.provider.parse(&item.unit, item.cursor.clone());
                let cursor = persist(conn, &item.unit, stream)?;
                if let Some(on_persisted) = hooks.on_persisted.as_deref_mut() {
                    on_persisted(item, cursor.clone());
                }
                next_cursor = Some(cursor);
                Ok(())
            };
            tx.run(&label, &mut work)
        };
        if timing {
            eprintln!(
                "[timing] {} {:?} {}",
                label,
                started.elapsed(),
                next_cursor.as_ref().map(|c| c.is_some()).unwrap_or(false)
            );
        }
        match outcome {
            Ok(()) => {
                result.committed.push(ItemRef {
                    provider: item.provider.name().to_string(),
                    key: item.unit.key.clone(),
                });
                if let Some(on_committed) = hooks.on_committed.as_deref_mut() {
                    on_committed(item, next_cursor.unwrap_or(None));
                }
            }
            Err(error) => {
                result
                    .failed_providers
                    .insert(item.provider.name().to_string());
                result.failed_items.push(ItemRef {
                    provider: item.provider.name().to_string(),
                    key: item.unit.key.clone(),
                });
                if let IndexAction::Stop = (hooks.on_error)(&error, item) {
                    result.stopped = Some((
                        item.provider.name().to_string(),
                        item.unit.key.clone(),
                        error.to_string(),
                    ));
                    result.complete = false;
                    return result;
                }
            }
        }
    }
    result.complete = result.failed_items.is_empty() && plan.incomplete_providers.is_empty();
    result
}

pub fn write_provider_index_markers(
    conn: &Connection,
    plan: &ProviderIndexPlan,
    result: &ProviderIndexResult,
    now_ms: f64,
) -> rusqlite::Result<()> {
    if result.stopped.is_some() {
        return Ok(());
    }
    let committed: HashSet<String> = result
        .committed
        .iter()
        .map(|item| format!("{}\0{}", item.provider, item.key))
        .collect();
    // A marker records that replay was scheduled. Per-unit cursors record
    // which known sources completed it, so an incomplete inventory retries
    // only the missing or failed sources instead of every readable sibling.
    for (provider, keys) in &plan.replay_keys {
        for key in keys {
            if !committed.contains(&format!("{provider}\0{key}")) {
                conn.execute("DELETE FROM index_state WHERE jsonl_path = ?1", [key])?;
            }
        }
    }
    for item in &result.failed_items {
        if plan.pending_markers.contains_key(&item.provider) {
            conn.execute("DELETE FROM index_state WHERE jsonl_path = ?1", [&item.key])?;
        }
    }
    for marker in plan.pending_markers.values() {
        conn.execute(
            "INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES (?, ?, 0)",
            rusqlite::params![marker, now_ms],
        )?;
    }
    Ok(())
}
