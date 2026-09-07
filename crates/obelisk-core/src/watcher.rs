// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Domain-specific hybrid watcher (ADR-0009 / the watcher-backend research
//! doc), ported from the original TS watcher package during Stage 1.
//!
//! One `notify` recursive watcher per *tree* target handles create/delete/
//! update events; a bounded poller covers (a) provider-declared exact *file*
//! targets (`history.jsonl`, `session_index.jsonl`, …) that a directory
//! subscription cannot express, and (b) a small hot set of recently active
//! transcripts, closing the long-lived-writer gap where native events do not
//! fire until a descriptor closes. Resources are `O(tree roots + hot files)`,
//! never `O(corpus paths)`.
//!
//! Contract notes carried over from the TS side:
//! - A tree root that appears after the initial pass emits a `Rescan`
//!   invalidation (coverage increase); roots present at creation are quiet —
//!   the caller's startup build already covers them.
//! - A native event promotes a matching transcript into the hot set
//!   *before* the invalidation is delivered, silently baselined: the event
//!   itself already delivered the change, polling covers later appends.
//! - A build hint (`promote`) is *not* silent: appends after the build
//!   finished have no delivered signal, so the first observation reports.
//! - Deletes never promote, and a hot file that disappears reports once and
//!   releases its slot; a hot directory is dropped from the set instead of
//!   being polled forever.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use notify::Watcher as _;

/// Typed watch target (the provider contract's `WatchTarget`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchTarget {
    /// Recursive directory subscription root.
    Tree(PathBuf),
    /// Exact file, polled forever (never a subscription root).
    File(PathBuf),
}

/// What the watcher tells the caller to rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchInvalidation {
    /// Changed paths (tree events, poll deltas, appearances).
    Paths(Vec<PathBuf>),
    /// A watch root was (re-)established after the initial pass — coverage
    /// grew, so a full reconcile over these roots is required.
    Rescan { roots: Vec<PathBuf>, reason: String },
}

/// Caller hook deciding whether an event path is a transcript worth
/// promoting into the hot set.
pub type PromotePredicate = Box<dyn Fn(&Path) -> bool + Send>;

/// Observed file identity; any change to these fields is a change.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FileSignature {
    size: u64,
    mtime_ms: u128,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

fn file_signature(metadata: &std::fs::Metadata) -> FileSignature {
    FileSignature {
        size: metadata.len(),
        mtime_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
        #[cfg(unix)]
        dev: {
            use std::os::unix::fs::MetadataExt;
            metadata.dev()
        },
        #[cfg(unix)]
        ino: {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        },
    }
}

impl FileSignature {
    fn changed(prev: Option<Self>, next: Option<Self>) -> bool {
        match (prev, next) {
            (Some(prev), Some(next)) => {
                prev.size != next.size
                    || prev.mtime_ms != next.mtime_ms
                    || (cfg!(unix) && (prev.dev != next.dev || prev.ino != next.ino))
            }
            (None, None) => false,
            _ => true,
        }
    }
}

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(5000);
const DEFAULT_MAX_HOT_FILES: usize = 64;

/// Shared mutable state for the tree-watch and poll loops.
struct WatcherState {
    /// Sorted tree roots to (re)establish.
    tree_roots: Vec<PathBuf>,
    /// Established subscriptions: root → notify watcher.
    subscriptions: HashMap<PathBuf, notify::RecommendedWatcher>,
    /// Roots with a subscribe attempt in flight.
    pending: HashSet<PathBuf>,
    /// Whether the initial establishment pass has settled.
    initial_pass_done: bool,
    /// Pinned exact-file targets (never evicted).
    pinned_files: Vec<PathBuf>,
    /// Hot (non-pinned) files, LRU order: most recently used last.
    hot_files: Vec<PathBuf>,
    /// Baseline per polled path. `None` in the map = never observed.
    /// `Some(None)` = observed missing; `Some(Some(sig))` = observed present.
    baselines: HashMap<PathBuf, Option<FileSignature>>,
    /// Hot paths whose first observation baselines silently (event-promoted).
    silent_first_baseline: HashSet<PathBuf>,
    /// Whether the hot overlay is active (ADR-0009: macOS by default).
    hot_enabled: bool,
    max_hot_files: usize,
    /// Caller hook deciding whether an event path is a transcript worth
    /// promoting into the hot set.
    should_promote: Option<PromotePredicate>,
}

/// The hybrid watcher handle. Dropping it stops watching; `close` joins the
/// worker threads explicitly.
pub struct AdaptiveWatcher {
    closed: Arc<AtomicBool>,
    /// Invalidations are delivered on the worker threads through this hook.
    on_invalidate: Arc<dyn Fn(WatchInvalidation) + Send + Sync>,
    state: Arc<Mutex<WatcherState>>,
    poll_thread: Option<std::thread::JoinHandle<()>>,
    tree_thread: Option<std::thread::JoinHandle<()>>,
}

pub struct AdaptiveWatcherOptions {
    pub targets: Vec<WatchTarget>,
    /// Delivered from the watcher worker threads.
    pub on_invalidate: Box<dyn Fn(WatchInvalidation) + Send + Sync>,
    /// Poll interval for `File` targets and the hot set.
    pub poll_interval: Option<Duration>,
    /// Delay before re-probing a missing or lost tree root.
    pub retry_delay: Option<Duration>,
    /// Enable the bounded hot-file overlay. Defaults to macOS-only
    /// (ADR-0009 platform policy).
    pub hot_polling: Option<bool>,
    /// Hard cap on hot (non-pinned) polled files.
    pub max_hot_files: Option<usize>,
    /// Seeds the hot set at creation.
    pub initial_hot_files: Vec<PathBuf>,
    /// Decides whether a native event path enters the hot set.
    pub should_promote: Option<PromotePredicate>,
}

impl Default for AdaptiveWatcherOptions {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            on_invalidate: Box::new(|_| {}),
            poll_interval: None,
            retry_delay: None,
            hot_polling: None,
            max_hot_files: None,
            initial_hot_files: Vec::new(),
            should_promote: None,
        }
    }
}

/// A native tree event, already reduced to (path, is_delete).
#[derive(Debug)]
struct TreeEvent {
    root: PathBuf,
    path: PathBuf,
    is_delete: bool,
}

impl AdaptiveWatcher {
    pub fn new(options: AdaptiveWatcherOptions) -> Self {
        let AdaptiveWatcherOptions {
            targets,
            on_invalidate,
            poll_interval,
            retry_delay,
            hot_polling,
            max_hot_files,
            initial_hot_files,
            should_promote,
        } = options;
        let poll_interval = poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL);
        let retry_delay = retry_delay.unwrap_or(DEFAULT_RETRY_DELAY);
        let max_hot_files = max_hot_files.unwrap_or(DEFAULT_MAX_HOT_FILES);
        let hot_enabled = hot_polling.unwrap_or(cfg!(target_os = "macos"));

        let mut tree_roots: Vec<PathBuf> = targets
            .iter()
            .filter_map(|target| match target {
                WatchTarget::Tree(path) => Some(path.clone()),
                WatchTarget::File(_) => None,
            })
            .collect();
        tree_roots.sort();
        tree_roots.dedup();
        // A file covered by a tree target stays in the poller: polling exists
        // to close the update-latency gap, not to extend directory coverage.
        let mut pinned_files: Vec<PathBuf> = targets
            .iter()
            .filter_map(|target| match target {
                WatchTarget::File(path) => Some(path.clone()),
                WatchTarget::Tree(_) => None,
            })
            .collect();
        pinned_files.sort();
        pinned_files.dedup();

        let state = Arc::new(Mutex::new(WatcherState {
            tree_roots,
            subscriptions: HashMap::new(),
            pending: HashSet::new(),
            initial_pass_done: false,
            pinned_files,
            hot_files: Vec::new(),
            baselines: HashMap::new(),
            silent_first_baseline: HashSet::new(),
            hot_enabled,
            max_hot_files,
            should_promote,
        }));

        // Seed the hot set and baselines like the TS constructor: pinned
        // files always poll; initial hot seeds respect hot_enabled.
        {
            let mut state = state.lock().unwrap();
            for file in state.pinned_files.clone() {
                state.baselines.entry(file).or_insert(None);
            }
            for file in initial_hot_files {
                promote_locked(&mut state, &file, false);
            }
        }

        let closed = Arc::new(AtomicBool::new(false));
        let on_invalidate: Arc<dyn Fn(WatchInvalidation) + Send + Sync> = Arc::from(on_invalidate);

        // Tree loop: consumes notify events, retries missing roots.
        let (tree_tx, tree_rx) = mpsc::channel::<TreeEvent>();
        let tree_state = state.clone();
        let tree_closed = closed.clone();
        let tree_invalidate = on_invalidate.clone();
        let tree_thread = std::thread::Builder::new()
            .name("obelisk-watcher-trees".into())
            .spawn(move || {
                let mut last_refresh = std::time::Instant::now();
                // Initial establishment pass (quiet — the caller's startup
                // build already covers roots present at creation).
                refresh_trees(&tree_state, &tree_tx, &tree_closed);
                loop {
                    if tree_closed.load(Ordering::SeqCst) {
                        return;
                    }
                    // Retry cadence for missing roots: wake on the retry delay
                    // even when no events arrive.
                    let timeout = retry_delay
                        .saturating_sub(last_refresh.elapsed())
                        .max(Duration::from_millis(50));
                    match tree_rx.recv_timeout(timeout) {
                        Ok(event) => {
                            if tree_closed.load(Ordering::SeqCst) {
                                return;
                            }
                            deliver_tree_event(&tree_state, event, &tree_invalidate);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if tree_closed.load(Ordering::SeqCst) {
                                return;
                            }
                            last_refresh = std::time::Instant::now();
                            let established = refresh_trees(&tree_state, &tree_tx, &tree_closed);
                            if !established.is_empty() {
                                tree_invalidate(WatchInvalidation::Rescan {
                                    roots: established,
                                    reason: "root-established".to_string(),
                                });
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("watcher tree thread spawns");

        // Poll loop: pinned files + hot set, single ticker, no overlap.
        let poll_state = state.clone();
        let poll_closed = closed.clone();
        let poll_invalidate = on_invalidate.clone();
        let has_poll_targets = {
            let state = poll_state.lock().unwrap();
            !state.pinned_files.is_empty() || state.hot_enabled
        };
        let poll_thread = if has_poll_targets {
            Some(
                std::thread::Builder::new()
                    .name("obelisk-watcher-poll".into())
                    .spawn(move || loop {
                        std::thread::sleep(poll_interval);
                        if poll_closed.load(Ordering::SeqCst) {
                            return;
                        }
                        let changed = poll_tick(&poll_state);
                        if !changed.is_empty() && !poll_closed.load(Ordering::SeqCst) {
                            poll_invalidate(WatchInvalidation::Paths(changed));
                        }
                    })
                    .expect("watcher poll thread spawns"),
            )
        } else {
            None
        };

        Self {
            closed,
            on_invalidate: on_invalidate.clone(),
            state,
            poll_thread,
            tree_thread: Some(tree_thread),
        }
    }

    /// Move a path into the bounded hot set (LRU-refreshed if already hot).
    /// No-op when the hot overlay is disabled, the path is pinned, or the
    /// watcher is closed. A hint's first observation reports (never silent).
    pub fn promote(&self, path: &Path) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let mut state = self.state.lock().unwrap();
        promote_locked(&mut state, path, false);
    }

    /// Close: stop both loops. After close no invalidation fires.
    pub fn close(mut self) {
        self.closed.store(true, Ordering::SeqCst);
        // Dropping subscriptions stops the notify watchers; the tree thread
        // observes closed on its next wake and exits.
        if let Ok(mut state) = self.state.lock() {
            state.subscriptions.clear();
            state.pending.clear();
            state.tree_roots.clear();
            state.hot_files.clear();
        }
        // Wake the tree loop if it is idle in recv_timeout.
        // (It self-terminates via the closed flag on its next wake.)
        if let Some(handle) = self.tree_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.poll_thread.take() {
            let _ = handle.join();
        }
    }

    /// Number of currently established tree subscriptions (tests/debug).
    pub fn subscription_count(&self) -> usize {
        self.state.lock().unwrap().subscriptions.len()
    }

    /// Snapshot of the hot set, LRU order (tests).
    pub fn hot_files(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap().hot_files.clone()
    }
}

impl Drop for AdaptiveWatcher {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut state) = self.state.lock() {
            state.subscriptions.clear();
        }
        // The worker threads exit on their next wake; joining here would
        // block a struct drop for up to a poll interval, so detach instead —
        // close() is the explicit synchronous path.
        let _ = &self.on_invalidate;
    }
}

// ---- tree subscription management (runs on the tree thread) ----

/// Try to (re)establish every root not currently subscribed or pending.
/// Returns the roots newly established after the initial pass (coverage
/// increases that require a caller-side rescan).
fn refresh_trees(
    state: &Arc<Mutex<WatcherState>>,
    events: &mpsc::Sender<TreeEvent>,
    closed: &Arc<AtomicBool>,
) -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = {
        let state = state.lock().unwrap();
        if state.tree_roots.is_empty() {
            return Vec::new();
        }
        state.tree_roots.clone()
    };
    let mut established = Vec::new();
    for root in roots {
        if closed.load(Ordering::SeqCst) {
            return established;
        }
        if let Some(root) = add_root(state, &root, events) {
            established.push(root);
        }
    }
    established
}

fn add_root(
    state: &Arc<Mutex<WatcherState>>,
    root: &Path,
    events: &mpsc::Sender<TreeEvent>,
) -> Option<PathBuf> {
    let should_subscribe = {
        let state = state.lock().unwrap();
        !state.subscriptions.contains_key(root) && !state.pending.contains(root)
    };
    if !should_subscribe {
        return None;
    }
    // access probe: missing roots retry quietly (ENOENT), others warn once.
    match std::fs::metadata(root) {
        Ok(_) => subscribe_root(state, root, events),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Quiet: a not-yet-created root is a normal cold start.
            note_settled(state);
            None
        }
        Err(_) => {
            // Inaccessible root: warn-worthy, but the TS side rate-limits
            // logging; we simply keep retrying without spam.
            note_settled(state);
            None
        }
    }
}

/// TS `noteSettled`: the initial pass ends once no root is pending — whether
/// every root subscribed or every attempt already failed. Roots established
/// after this point are coverage increases and emit a rescan.
fn note_settled(state: &Arc<Mutex<WatcherState>>) {
    let mut state = state.lock().unwrap();
    if !state.initial_pass_done && state.pending.is_empty() {
        state.initial_pass_done = true;
    }
}

fn subscribe_root(
    state: &Arc<Mutex<WatcherState>>,
    root: &Path,
    events: &mpsc::Sender<TreeEvent>,
) -> Option<PathBuf> {
    let is_pending = {
        let mut state = state.lock().unwrap();
        state.pending.insert(root.to_path_buf())
    };
    if !is_pending {
        return None; // Already pending from another refresh.
    }
    let root_owned = root.to_path_buf();
    let events = events.clone();
    let watcher =
        notify::recommended_watcher(move |result: Result<notify::Event, notify::Error>| {
            match result {
                Ok(event) => {
                    // @parcel/watcher parity: only create/modify/remove count
                    // as changes. Access events (open/read/close from our own
                    // indexing reads, or any reader) are noise that would
                    // feed a build loop.
                    if matches!(
                        event.kind,
                        notify::EventKind::Access(_) | notify::EventKind::Other
                    ) {
                        return;
                    }
                    for path in event.paths {
                        let is_delete = matches!(event.kind, notify::EventKind::Remove(_));
                        let _ = events.send(TreeEvent {
                            root: root_owned.clone(),
                            path,
                            is_delete,
                        });
                    }
                }
                Err(_) => {
                    // Stream errors surface as a synthetic delete for the root:
                    // drop_root handles unsubscribe + retry + rescan semantics.
                    let _ = events.send(TreeEvent {
                        root: root_owned.clone(),
                        path: root_owned.clone(),
                        is_delete: true,
                    });
                }
            }
        });
    match watcher {
        Ok(mut watcher) => {
            let watch_result = watcher.watch(root, notify::RecursiveMode::Recursive);
            let mut state = state.lock().unwrap();
            let was_pending = state.pending.remove(root);
            let during_initial_pass = !state.initial_pass_done;
            if !state.initial_pass_done && state.pending.is_empty() {
                state.initial_pass_done = true;
            }
            match watch_result {
                Ok(()) if was_pending => {
                    state.subscriptions.insert(root.to_path_buf(), watcher);
                    drop(state);
                    if during_initial_pass {
                        None
                    } else {
                        // Coverage increased after the initial pass.
                        Some(root.to_path_buf())
                    }
                }
                Ok(()) => {
                    // Subscribed but no longer pending (dropped meanwhile).
                    drop(watcher);
                    drop(state);
                    None
                }
                Err(_) => {
                    drop(watcher);
                    drop(state);
                    // scheduleRetry: the refresh cadence picks it up again.
                    None
                }
            }
        }
        Err(_) => {
            note_settled(state);
            None
        }
    }
}

fn deliver_tree_event(
    state: &Arc<Mutex<WatcherState>>,
    event: TreeEvent,
    on_invalidate: &Arc<dyn Fn(WatchInvalidation) + Send + Sync>,
) {
    // A synthetic root-delete means the stream errored: drop the root and
    // let the retry cadence re-establish it (with a rescan when it lands).
    let is_root_stream_error = event.is_delete && event.path == event.root;
    if is_root_stream_error {
        drop_root(state, &event.root);
    }

    // Promote matching paths before delivering the invalidation: the
    // catch-up build reads content written before promotion; polling covers
    // later appends. Deletes never promote.
    if !event.is_delete && !is_root_stream_error {
        let should_promote = {
            let state = state.lock().unwrap();
            (state.hot_enabled && state.should_promote.is_some())
                && (state.should_promote.as_ref().unwrap())(&event.path)
        };
        if should_promote {
            let mut state = state.lock().unwrap();
            promote_locked(&mut state, &event.path, true);
        }
    }

    if !event.is_delete && !is_root_stream_error {
        on_invalidate(WatchInvalidation::Paths(vec![event.path]));
    }
}

fn drop_root(state: &Arc<Mutex<WatcherState>>, root: &Path) {
    let mut state = state.lock().unwrap();
    state.subscriptions.remove(root);
    state.pending.remove(root);
    // Roots removed from the watch set still retry through tree_roots
    // (refresh_trees only skips subscribed/pending entries).
}

// ---- poller (runs on the poll thread) ----

/// One poll tick over pinned + hot files. Returns the changed paths.
fn poll_tick(state: &Arc<Mutex<WatcherState>>) -> Vec<PathBuf> {
    let observed: Vec<(PathBuf, bool)> = {
        let state = state.lock().unwrap();
        let mut observed: Vec<(PathBuf, bool)> = state
            .pinned_files
            .iter()
            .map(|file| (file.clone(), false))
            .collect();
        for file in &state.hot_files {
            observed.push((file.clone(), true));
        }
        observed
    };
    let mut changed = Vec::new();
    let mut notify_invalidations: Vec<WatchInvalidation> = Vec::new();
    let _ = &mut notify_invalidations;
    for (file, is_hot) in observed {
        // Pinned first, then hot in LRU order — matching the TS tick.
        let next: Option<FileSignature> = std::fs::metadata(&file).ok().map(|m| file_signature(&m));
        let mut state = state.lock().unwrap();
        // Evicted while its stat was in flight: stay evicted.
        if is_hot && !state.hot_files.contains(&file) {
            continue;
        }
        // Hot directories are dropped from the set: polling a directory
        // cannot see appends to files inside it, and the tree watch covers it.
        if is_hot
            && next.is_some()
            && std::fs::metadata(&file)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        {
            state.hot_files.retain(|path| path != &file);
            state.baselines.remove(&file);
            state.silent_first_baseline.remove(&file);
            continue;
        }
        let prev = state.baselines.get(&file);
        match prev {
            None => {
                // First observation. Pinned and hint-promoted files report an
                // existing file as an appearance (a redundant build beats a
                // missed event); event-promoted files baseline silently.
                state.baselines.insert(file.clone(), next);
                let silent = is_hot && state.silent_first_baseline.remove(&file);
                if next.is_some() && !silent {
                    changed.push(file);
                }
            }
            Some(prev) => {
                if FileSignature::changed(*prev, next) {
                    state.baselines.insert(file.clone(), next);
                    changed.push(file.clone());
                    if is_hot {
                        if next.is_none() {
                            // Disappeared hot file: report once, release the
                            // slot; recreation re-promotes via directory event.
                            state.hot_files.retain(|path| path != &file);
                            state.baselines.remove(&file);
                            state.silent_first_baseline.remove(&file);
                        } else {
                            // LRU refresh; the baseline survives.
                            state.hot_files.retain(|path| path != &file);
                            state.hot_files.push(file);
                        }
                    }
                }
            }
        }
    }
    changed
}

// ---- hot-set management (must hold the state lock) ----

fn promote_locked(state: &mut WatcherState, path: &Path, silent: bool) {
    if !state.hot_enabled || state.pinned_files.iter().any(|p| p == path) {
        return;
    }
    if state.hot_files.iter().any(|p| p == path) {
        // LRU refresh; the baseline survives.
        state.hot_files.retain(|p| p != path);
        state.hot_files.push(path.to_path_buf());
        return;
    }
    // Enforce the hard cap: evict the least recently used hot files.
    while state.hot_files.len() >= state.max_hot_files {
        let Some(oldest) = state.hot_files.first().cloned() else {
            break;
        };
        state.hot_files.remove(0);
        state.baselines.remove(&oldest);
        state.silent_first_baseline.remove(&oldest);
    }
    state.hot_files.push(path.to_path_buf());
    state.baselines.remove(path);
    if silent {
        state.silent_first_baseline.insert(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("obelisk-watcher-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn collect_invalidations(
        watcher_options: &mut AdaptiveWatcherOptions,
    ) -> mpsc::Receiver<WatchInvalidation> {
        let (tx, rx) = mpsc::channel();
        let tx_for_hook = tx.clone();
        watcher_options.on_invalidate =
            Box::new(move |invalidation| drop(tx_for_hook.send(invalidation)));
        rx
    }

    const FAST_POLL: Duration = Duration::from_millis(60);

    fn wait_for<F: Fn(&WatchInvalidation) -> bool>(
        rx: &mpsc::Receiver<WatchInvalidation>,
        predicate: F,
        what: &str,
    ) -> WatchInvalidation {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(120)) {
                Ok(invalidation) if predicate(&invalidation) => return invalidation,
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(err) => panic!("waiting for {what}: channel closed: {err:?}"),
            }
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn tree_events_arrive_as_path_invalidations() {
        let dir = temp_dir("tree-events");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![WatchTarget::Tree(dir.clone())],
            poll_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);

        // Wait for the subscription to establish before writing.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while watcher.subscription_count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(watcher.subscription_count(), 1, "tree root subscribed");

        let file = dir.join("session-a.jsonl");
        std::fs::write(&file, "line\n").expect("write");
        let invalidation = wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(_)),
            "tree event",
        );
        match invalidation {
            WatchInvalidation::Paths(paths) => assert!(paths.contains(&file)),
            other => panic!("expected Paths, got {other:?}"),
        }
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_tree_root_is_retried_then_established_with_rescan() {
        let dir = temp_dir("missing-root");
        let root = dir.join("not-yet-created");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![WatchTarget::Tree(root.clone())],
            poll_interval: Some(Duration::from_secs(30)),
            retry_delay: Some(Duration::from_millis(120)),
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);

        // The root does not exist yet: no rescan during the initial pass.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(watcher.subscription_count(), 0);

        std::fs::create_dir_all(&root).expect("create root");
        let invalidation = wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Rescan { .. }),
            "rescan after late establishment",
        );
        match invalidation {
            WatchInvalidation::Rescan { roots, reason } => {
                assert!(roots.contains(&root), "rescan covers the root");
                assert_eq!(reason, "root-established");
            }
            other => panic!("expected Rescan, got {other:?}"),
        }
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_targets_are_polled_for_appearance_append_and_replacement() {
        let dir = temp_dir("file-targets");
        let file = dir.join("history.jsonl");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![WatchTarget::File(file.clone())],
            poll_interval: Some(FAST_POLL),
            hot_polling: Some(false),
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);

        // Appearance: a pinned target reports its first observation of an
        // existing file (deliberately — a redundant build beats a missed one).
        std::fs::write(&file, "first\n").expect("appear");
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "appearance",
        );

        // Append.
        std::fs::write(&file, "first\nsecond\n").expect("append");
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "append",
        );

        // Replacement (different inode).
        std::fs::remove_file(&file).expect("remove");
        std::fs::write(&file, "replaced\n").expect("replace");
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "replacement",
        );

        // Disappearance.
        std::fs::remove_file(&file).expect("disappear");
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "disappearance",
        );
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hint_promotion_reports_first_observation_then_detects_appends() {
        let dir = temp_dir("hot-hint");
        let file = dir.join("transcript.jsonl");
        std::fs::write(&file, "v1\n").expect("seed");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![],
            poll_interval: Some(FAST_POLL),
            hot_polling: Some(true),
            max_hot_files: Some(4),
            initial_hot_files: vec![file.clone()],
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);

        // Hint-seeded paths report the first observation (never silent).
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "first observation of a hint-seeded hot file",
        );

        // Appends after the hint are detected by polling.
        std::fs::write(&file, "v1\nv2\n").expect("append");
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "hot append",
        );
        assert!(watcher.hot_files().contains(&file));
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hot_set_respects_its_cap_and_evicts_the_lru_file() {
        let dir = temp_dir("hot-cap");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![],
            poll_interval: Some(Duration::from_secs(30)),
            hot_polling: Some(true),
            max_hot_files: Some(2),
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);

        let a = dir.join("a.jsonl");
        let b = dir.join("b.jsonl");
        let c = dir.join("c.jsonl");
        for path in [&a, &b, &c] {
            std::fs::write(path, "x\n").expect("seed");
            watcher.promote(path);
        }
        let hot = watcher.hot_files();
        assert_eq!(hot.len(), 2, "hot set capped: {hot:?}");
        assert!(
            hot.contains(&b) && hot.contains(&c),
            "LRU (a) evicted: {hot:?}"
        );
        assert!(!hot.contains(&a), "evicted file not polled");
        let _ = rx;
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hot_polling_disabled_makes_promote_a_noop() {
        let dir = temp_dir("hot-off");
        let file = dir.join("t.jsonl");
        std::fs::write(&file, "x\n").expect("seed");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![],
            poll_interval: Some(Duration::from_secs(30)),
            hot_polling: Some(false),
            initial_hot_files: vec![file.clone()],
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);
        watcher.promote(&file);
        assert!(watcher.hot_files().is_empty(), "hot overlay disabled");
        let _ = rx;
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_hints_are_dropped_from_the_hot_set() {
        let dir = temp_dir("hot-dir");
        let session_dir = dir.join("session-1");
        std::fs::create_dir_all(&session_dir).expect("mkdir");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![],
            poll_interval: Some(FAST_POLL),
            hot_polling: Some(true),
            initial_hot_files: vec![session_dir.clone()],
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);
        // The first poll tick drops the directory from the hot set.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !watcher.hot_files().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(watcher.hot_files().is_empty(), "directory hint dropped");
        let _ = rx;
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_events_promote_matching_transcripts_silently() {
        let dir = temp_dir("event-promote");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![WatchTarget::Tree(dir.clone())],
            poll_interval: Some(Duration::from_secs(30)),
            hot_polling: Some(true),
            should_promote: Some(Box::new(|path| {
                path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            })),
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while watcher.subscription_count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }

        let file = dir.join("live.jsonl");
        std::fs::write(&file, "v1\n").expect("write");
        // The event delivers the change…
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "native event",
        );
        // …and the path is now hot (event-promoted).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !watcher.hot_files().contains(&file) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            watcher.hot_files().contains(&file),
            "event promoted the file"
        );
        watcher.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn close_stops_all_invalidation() {
        let dir = temp_dir("close");
        let file = dir.join("pinned.jsonl");
        std::fs::write(&file, "v1\n").expect("seed");
        let mut options = AdaptiveWatcherOptions {
            targets: vec![WatchTarget::File(file.clone())],
            poll_interval: Some(FAST_POLL),
            hot_polling: Some(false),
            ..Default::default()
        };
        let rx = collect_invalidations(&mut options);
        let watcher = AdaptiveWatcher::new(options);
        // Consume the appearance invalidation.
        wait_for(
            &rx,
            |i| matches!(i, WatchInvalidation::Paths(p) if p.contains(&file)),
            "appearance before close",
        );
        watcher.close();
        // Changes after close must not fire.
        std::fs::write(&file, "v1\nv2\n").expect("append after close");
        if let Ok(invalidation) = rx.recv_timeout(Duration::from_millis(300)) {
            panic!("invalidation after close: {invalidation:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
