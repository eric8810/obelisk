// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! M2.5 background indexing daemon for the desktop app.
//!
//! The app is tray-resident, so it watches the providers' transcript trees
//! while it lives: filesystem events (plus polled exact-file targets and the
//! bounded hot set, see obelisk-core `watcher`) are debounced into
//! incremental `build_index` calls, whose watch hints are promoted back into
//! the watcher's hot set (ADR-0009). After each build the UI reloads from
//! the shared index; an open timeline in follow-tail mode stays pinned.
//!
//! Stage-3 write ownership: this daemon owns the index (ADR-0013 M3.1). It
//! writes the `__app_heartbeat__` liveness marker every 30s (freshness 60s)
//! so CLI-side mutations skip with `daemon_active`, and its own builds pass
//! `ignore_daemon_ownership: true` — the marker it owns must not suppress
//! the process that maintains it. The writer lease stays the sole write
//! arbitrator, so a lingering TS-era daemon simply serializes with us.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::StreamExt;
use gpui::AppContext;
use obelisk_core::indexer::{build_index, BuildIndexOptions};
use obelisk_core::provider_settings::{
    create_configured_builtin_provider_runtime, read_persisted_provider_settings, SettingsRead,
};
use obelisk_core::providers::types::WatchTargetKind;
use obelisk_core::watcher::{
    AdaptiveWatcher, AdaptiveWatcherOptions, WatchInvalidation, WatchTarget,
};

use crate::data::AppData;
use crate::ObeliskApp;

/// Trailing debounce: a quiet window of this length after the last event
/// releases a build (TS indexer-service DEFAULT_DEBOUNCE_MS).
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Ceiling on how long sustained activity can postpone a build (#86).
const MAX_WAIT: Duration = Duration::from_millis(1500);
/// Interval for a periodic full reconcile build, bounding worst-case
/// staleness from events the watcher silently drops.
const RECONCILE: Duration = Duration::from_secs(5 * 60);
/// Daemon liveness cadence: refreshed every 30s while the fresh window in
/// obelisk-core is 60s (TS indexer-service DEFAULT_HEARTBEAT_MS).
const HEARTBEAT: Duration = Duration::from_secs(30);

/// Transcript suffixes that native events may promote into the hot set
/// (TS indexer-service TRANSCRIPT_SUFFIXES).
const TRANSCRIPT_SUFFIXES: [&str; 3] = [".jsonl.zstd", ".jsonl", ".json"];

fn is_transcript_path(path: &std::path::Path) -> bool {
    TRANSCRIPT_SUFFIXES
        .iter()
        .any(|suffix| path.to_string_lossy().ends_with(suffix))
}

/// Resolve the configured providers' watch targets for this machine.
fn provider_watch_targets(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Option<Vec<WatchTarget>> {
    let SettingsRead::Ok(persisted) = read_persisted_provider_settings(home) else {
        return None;
    };
    let runtime =
        create_configured_builtin_provider_runtime(home, cwd, &persisted, &Default::default());
    let registry = Arc::new(runtime.registry);
    let configured_roots: std::collections::HashMap<String, String> = runtime
        .roots
        .into_iter()
        .map(|(id, path)| (id, path.to_string_lossy().into_owned()))
        .collect();
    let targets: Vec<WatchTarget> = registry
        .watch_targets(&configured_roots)
        .into_iter()
        .map(|target| match target.kind {
            WatchTargetKind::Tree => WatchTarget::Tree(PathBuf::from(target.path)),
            WatchTargetKind::File => WatchTarget::File(PathBuf::from(target.path)),
        })
        .collect();
    (!targets.is_empty()).then_some(targets)
}

fn build_registry(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Option<Arc<obelisk_core::providers::types::ProviderRegistry>> {
    let SettingsRead::Ok(persisted) = read_persisted_provider_settings(home) else {
        return None;
    };
    let runtime =
        create_configured_builtin_provider_runtime(home, cwd, &persisted, &Default::default());
    Some(Arc::new(runtime.registry))
}

/// Start the watcher daemon for one app entity. The spawned task owns the
/// watcher and exits with the entity (weak handle upgrade fails in
/// `run_build`), at which point the watcher stops being refreshed; its
/// threads wind down through the closed-flag checks.
/// All live app entities (one per open window). The daemon refreshes every
/// one of them after a build; entries whose window closed are pruned on
/// registration and upgrade failure.
#[derive(Default)]
pub struct AppRegistryGlobal {
    pub apps: Vec<gpui::WeakEntity<ObeliskApp>>,
}

impl gpui::Global for AppRegistryGlobal {}

/// Keeps the single daemon loop task alive for the process lifetime.
struct DaemonGlobal {
    _task: gpui::Task<()>,
}

impl gpui::Global for DaemonGlobal {}

/// Snapshot of the registered app windows (for cross-window reloads).
pub fn registered_apps(cx: &gpui::App) -> Vec<gpui::WeakEntity<ObeliskApp>> {
    cx.global::<AppRegistryGlobal>().apps.clone()
}

/// Register one app entity (its window) for daemon refreshes.
pub fn register_app(cx: &mut gpui::App, app: gpui::WeakEntity<ObeliskApp>) {
    let registry = cx.default_global::<AppRegistryGlobal>();
    registry.apps.retain(|entry| entry.upgrade().is_some());
    registry.apps.push(app);
}

/// Start the process-wide watcher daemon (idempotent: one daemon regardless
/// of how many windows open). The task owns the watcher and exits with the
/// process; the UI refresh walks the app registry.
pub fn start(cx: &mut gpui::App, home: PathBuf, cwd: PathBuf) {
    if cx.has_global::<DaemonGlobal>() {
        return;
    }
    let Some(targets) = provider_watch_targets(&home, &cwd) else {
        return;
    };
    if build_registry(&home, &cwd).is_none() {
        return;
    }

    let (tx, rx) = mpsc::unbounded::<WatchInvalidation>();
    let watcher = Arc::new(AdaptiveWatcher::new(AdaptiveWatcherOptions {
        targets,
        on_invalidate: Box::new(move |invalidation| {
            // Watcher callbacks run on their own threads; the unbounded
            // sender never blocks or drops events.
            let _ = tx.unbounded_send(invalidation);
        }),
        should_promote: Some(Box::new(is_transcript_path)),
        ..Default::default()
    }));

    let task = cx.spawn(async move |cx| {
        let mut rx = rx;
        let mut invalidations: Option<mpsc::UnboundedReceiver<WatchInvalidation>> = None;
        let mut pending_paths: Vec<PathBuf> = Vec::new();
        let mut pending_rescans: Vec<PathBuf> = Vec::new();
        let mut first_event: Option<Instant> = None;
        let mut last_build;
        let mut next_heartbeat = Instant::now();
        let mut last_settings_mtime = std::fs::metadata(
            obelisk_core::provider_settings::settings_path(&home),
        )
        .and_then(|meta| meta.modified())
        .ok();
        let recap_dir = crate::data::recap_dir(&home);
        let mut last_recap_mtime = std::fs::metadata(&recap_dir)
            .and_then(|meta| meta.modified())
            .ok();
        let mut watcher = watcher;

        // Build on launch (the TS app's first-run inventory): a fresh
        // install has no index and no watch events to react to, so without
        // this the first build would only land at the 5-minute reconcile.
        // Incremental semantics: cold start does the full work, an already
        // fresh index skips in milliseconds.
        eprintln!("obelisk daemon: initial index build");
        run_build(cx, &home, &cwd, &watcher, false, false).await;
        last_build = Instant::now();

        loop {
            // A watcher swap (settings change) replaces the event channel.
            if let Some(new_rx) = invalidations.take() {
                rx = new_rx;
            }
            if first_event.is_none() {
                // Idle: wait for the next invalidation or the earlier of the
                // reconcile and heartbeat deadlines. The first heartbeat
                // fires immediately — the daemon claims ownership at startup.
                let reconcile_at = last_build + RECONCILE;
                let deadline = reconcile_at.min(next_heartbeat);
                let timer = cx
                    .background_executor()
                    .timer(deadline.saturating_duration_since(Instant::now()));
                match futures::future::select(rx.next(), timer).await {
                    futures::future::Either::Left((invalidation, _)) => {
                        let Some(invalidation) = invalidation else {
                            // Channel closed: the watcher is gone.
                            return;
                        };
                        absorb(
                            invalidation,
                            &mut pending_paths,
                            &mut pending_rescans,
                            &mut first_event,
                        );
                    }
                    futures::future::Either::Right((_, _)) => {
                        let now = Instant::now();
                        if now >= next_heartbeat {
                            let beat_home = home.clone();
                            cx.background_spawn(async move {
                                obelisk_core::indexer::write_daemon_heartbeat(&beat_home);
                            })
                            .await;
                            next_heartbeat = now + HEARTBEAT;
                            // Provider-root edits must not need a restart:
                            // when settings.json changes, re-resolve the
                            // watch targets and swap the watcher (P0-9).
                            let settings_mtime = std::fs::metadata(
                                obelisk_core::provider_settings::settings_path(&home),
                            )
                            .and_then(|meta| meta.modified())
                            .ok();
                            // Recap directory watcher (R12): a new/removed
                            // recap file refreshes the open Recap view.
                            let recap_mtime = std::fs::metadata(&recap_dir)
                                .and_then(|meta| meta.modified())
                                .ok();
                            if recap_mtime != last_recap_mtime {
                                last_recap_mtime = recap_mtime;
                                let _ = cx.update(|cx| {
                                    let apps: Vec<gpui::WeakEntity<ObeliskApp>> = {
                                        let registry = cx.global::<AppRegistryGlobal>();
                                        registry.apps.clone()
                                    };
                                    for app in apps {
                                        let _ = app.update(cx, |app, cx| {
                                            if app.view == crate::views::AppView::Recap {
                                                // Reload, not clear: rendering maps
                                                // None to an empty list, which
                                                // would flash the empty state.
                                                app.recaps = Some(std::rc::Rc::new(
                                                    crate::data::list_recap_entries(&home),
                                                ));
                                                cx.notify();
                                            }
                                        });
                                    }
                                });
                            }
                            if settings_mtime != last_settings_mtime {
                                last_settings_mtime = settings_mtime;
                                if let Some(targets) = provider_watch_targets(&home, &cwd) {
                                    eprintln!(
                                        "obelisk daemon: settings changed — rebuilding watcher ({} targets)",
                                        targets.len()
                                    );
                                    let (tx, new_rx) =
                                        mpsc::unbounded::<WatchInvalidation>();
                                    watcher = Arc::new(AdaptiveWatcher::new(
                                        AdaptiveWatcherOptions {
                                            targets,
                                            on_invalidate: Box::new(move |invalidation| {
                                                let _ = tx.unbounded_send(invalidation);
                                            }),
                                            should_promote: Some(Box::new(is_transcript_path)),
                                            ..Default::default()
                                        },
                                    ));
                                    // Seed the loop with a full rescan so the
                                    // new roots index immediately.
                                    pending_rescans.push(home.clone());
                                    first_event.get_or_insert(now);
                                    invalidations = Some(new_rx);
                                }
                            }
                        }
                        if now >= reconcile_at {
                            // Periodic full reconcile build (TS RECONCILE_MS):
                            // bounds staleness from silently dropped events.
                            run_build(cx, &home, &cwd, &watcher, true, true).await;
                            last_build = Instant::now();
                        }
                        continue;
                    }
                }
            }

            // Coalescing a burst: drain whatever is available now, then wait
            // for a quiet window (debounce) or the burst ceiling (max-wait).
            let started = first_event.expect("burst in progress");
            let debounce_at = started + DEBOUNCE;
            let max_wait_at = started + MAX_WAIT;
            let wake_at = debounce_at.min(max_wait_at);
            let timer = cx
                .background_executor()
                .timer(wake_at.saturating_duration_since(Instant::now()));
            match futures::future::select(rx.next(), timer).await {
                futures::future::Either::Left((invalidation, _)) => {
                    let Some(invalidation) = invalidation else {
                        return;
                    };
                    absorb(
                        invalidation,
                        &mut pending_paths,
                        &mut pending_rescans,
                        &mut first_event,
                    );
                    // More events may have queued behind the awaited one.
                    while let Ok(invalidation) = rx.try_recv() {
                        absorb(
                            invalidation,
                            &mut pending_paths,
                            &mut pending_rescans,
                            &mut first_event,
                        );
                    }
                }
                futures::future::Either::Right((_, _)) => {
                    while let Ok(invalidation) = rx.try_recv() {
                        absorb(
                            invalidation,
                            &mut pending_paths,
                            &mut pending_rescans,
                            &mut first_event,
                        );
                    }
                }
            }

            let now = Instant::now();
            if now >= debounce_at || now >= max_wait_at {
                let rescans = std::mem::take(&mut pending_rescans);
                let changed = std::mem::take(&mut pending_paths);
                first_event = None;
                // A rescan (coverage increase) is a full build; otherwise
                // the incremental path reuses index_state cursors.
                let full = !rescans.is_empty();
                eprintln!(
                    "obelisk daemon: incremental build (full={full}, {} changed paths)",
                    changed.len()
                );
                run_build(cx, &home, &cwd, &watcher, full, true).await;
                last_build = Instant::now();
            }
        }
    });
    cx.set_global(DaemonGlobal { _task: task });
}

fn absorb(
    invalidation: WatchInvalidation,
    pending_paths: &mut Vec<PathBuf>,
    pending_rescans: &mut Vec<PathBuf>,
    first_event: &mut Option<Instant>,
) {
    if first_event.is_none() {
        *first_event = Some(Instant::now());
    }
    match invalidation {
        WatchInvalidation::Paths(paths) => pending_paths.extend(paths),
        WatchInvalidation::Rescan { roots, .. } => pending_rescans.extend(roots),
    }
}

/// One debounced build. Write arbitration stays inside obelisk-core (writer
/// lease); ownership suppression is bypassed because this daemon is the
/// heartbeat owner (Stage-3).
async fn run_build(
    cx: &mut gpui::AsyncApp,
    home: &std::path::Path,
    cwd: &std::path::Path,
    watcher: &Arc<AdaptiveWatcher>,
    full: bool,
    ignore_recent: bool,
) {
    let build_home = home.to_path_buf();
    let build_cwd = cwd.to_path_buf();
    let result = cx
        .background_executor()
        .spawn(async move {
            // The registry is rebuilt per build from the CURRENT settings —
            // provider-root edits take effect on the next build without an
            // app restart (P0-9).
            let registry = build_registry(&build_home, &build_cwd).unwrap_or_else(|| {
                let persisted =
                    match obelisk_core::provider_settings::read_persisted_provider_settings(
                        &build_home,
                    ) {
                        obelisk_core::provider_settings::SettingsRead::Ok(value) => value,
                        obelisk_core::provider_settings::SettingsRead::Failed(_) => {
                            serde_json::json!({})
                        }
                    };
                Arc::new(
                    obelisk_core::provider_settings::create_configured_builtin_provider_runtime(
                        &build_home,
                        &build_cwd,
                        &persisted,
                        &Default::default(),
                    )
                    .registry,
                )
            });
            build_index(
                &build_home,
                BuildIndexOptions {
                    force: full,
                    ignore_recent_build: ignore_recent,
                    // This daemon owns the heartbeat marker; builds must
                    // not be suppressed by our own liveness row.
                    ignore_daemon_ownership: true,
                    provider_registry: Some(registry),
                },
            )
        })
        .await;

    // Recently written transcripts are seeded into the hot set (ADR-0009):
    // appends after the build finished are covered by polling.
    if let Some(hints) = &result.watch_hints {
        for hint in hints {
            watcher.promote(std::path::Path::new(hint));
        }
    }

    let home = home.to_path_buf();
    let cwd = cwd.to_path_buf();
    let _ = cx.update(|cx| {
        let apps: Vec<gpui::WeakEntity<ObeliskApp>> = {
            let registry = cx.global::<AppRegistryGlobal>();
            registry.apps.clone()
        };
        for app in apps {
            let _ = app.update(cx, |app, cx| {
                app.data = AppData::load(&home, &cwd);
                app.refresh_timeline_daemon(cx);
                cx.notify();
            });
        }
    });
}
