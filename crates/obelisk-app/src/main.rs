// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Obelisk desktop app — pure GPUI (ADR-0013 Decision 1).
//!
//! Component base: `fc-ui` over `fc-gpui` (the Apache-2.0 fork providing
//! system tray, global hotkeys, notifications, and daemon mode). The ADR's
//! first choice (longbridge/gpui-component) cannot coexist with fc-gpui in
//! one binary — they depend on different GPUI type families (gpui-pre vs
//! fc-gpui), so the ADR's fallback path applies with the fork's own
//! component library taking gpui-component's place. Verified at this first
//! vertical slice per the ADR's own instruction.
//!
//! Process model (ADR-0013 Stage 3): the app links obelisk-core directly
//! (no IPC), reads the shared ~/.obelisk/obelisk.sqlite, and its resident
//! daemon owns the index writes (heartbeat marker + writer lease).

mod assets;
mod daemon;
mod data;
mod file_reference;
mod session_list;
mod theme;
mod timeline;
mod timeline_view;
mod tool_render;
mod views;

use gpui::{
    px, App, Application, Bounds, Context, StatefulInteractiveElement, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};

use crate::data::AppData;
use gpui::prelude::*;

/// HTTP client for markdown images: fc-ui's markdown renderer turns
/// `![alt](/abs/path.png)` (and `file:///abs/path.png`) into URI image
/// sources, which GPUI fetches through the app's HTTP client. This one serves
/// those from the local filesystem — percent-decoding `file:` URLs the same
/// way the Vue renderer's `hrefToPath` does — and fails for anything else:
/// the desktop app is offline-by-default until network policy is decided.
/// (The Vue contract also allows http(s) images; that needs a real client
/// and lands with the same decision.)
struct LocalImageHttpClient;

/// Resolve a markdown-image URI to a readable local path, or `None` when the
/// URI is not a local reference (caller turns that into an error).
fn local_image_path(uri: &gpui::http_client::Uri) -> Option<std::path::PathBuf> {
    let text = uri.to_string();
    let (path, encoded) = if let Some(rest) = text.strip_prefix("file://") {
        (rest.to_string(), true)
    } else if text.starts_with('/') {
        // Origin-form URI produced for bare absolute paths.
        (text, false)
    } else {
        return None;
    };
    let decoded = if encoded {
        file_reference::percent_decode(&path)
    } else {
        path
    };
    if decoded.contains("..") {
        return None;
    }
    Some(std::path::PathBuf::from(decoded))
}

impl gpui::http_client::HttpClient for LocalImageHttpClient {
    fn type_name(&self) -> &'static str {
        "LocalImageHttpClient"
    }

    fn user_agent(&self) -> Option<&gpui::http_client::http::HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&gpui::http_client::Url> {
        None
    }

    fn send(
        &self,
        req: gpui::http_client::Request<gpui::http_client::AsyncBody>,
    ) -> futures::future::BoxFuture<
        'static,
        gpui::http_client::Result<gpui::http_client::Response<gpui::http_client::AsyncBody>>,
    > {
        use futures::FutureExt;

        async move {
            let uri = req.uri().clone();
            let Some(path) = local_image_path(&uri) else {
                return Err(std::io::Error::other(format!("not a local image URI: {uri}")).into());
            };
            let bytes = std::fs::read(&path)
                .map_err(|error| std::io::Error::new(error.kind(), format!("{path:?}: {error}")))?;
            let response = gpui::http_client::Response::builder()
                .status(200)
                .body(gpui::http_client::AsyncBody::from(bytes))?;
            Ok(response)
        }
        .boxed()
    }
}

/// An open subagent timeline (Vue SubagentDetail route; #54).
struct SubagentScreen {
    agent_id: String,
    /// Parent session id (kept for deep links back to the parent).
    #[allow(dead_code)]
    parent_session_id: String,
    messages: std::rc::Rc<Vec<crate::timeline::TimelineMessage>>,
    focus: gpui::FocusHandle,
    ui: std::rc::Rc<crate::timeline_view::TimelineUiState>,
}

/// The offscreen card surface used by export (R11): renders exactly one
/// recap card on the dark stage background.
struct OffscreenRecapCard {
    value: serde_json::Value,
    card_ix: usize,
    palette: &'static crate::theme::ArchetypePalette,
    on_card: crate::views::RecapCardFn,
}

impl gpui::Render for OffscreenRecapCard {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
            .size_full()
            .bg(gpui::rgba(0x0a0b14ff))
            .flex()
            .items_center()
            .justify_center()
            .child(crate::views::recap_card_stack(
                &self.value,
                self.card_ix,
                self.palette,
                &self.on_card,
            ))
    }
}

// Global view shortcuts (Vue resolveGlobalShortcut: Cmd/Ctrl+1/2/3).
gpui::actions!(
    obelisk_app,
    [
        GlobalOpenSessions,
        GlobalOpenActiveMemories,
        GlobalOpenArchivedMemories,
        GlobalOpenRecap,
        TextScaleUp,
        TextScaleDown,
        TextScaleReset
    ]
);

struct ObeliskApp {
    data: AppData,
    home: std::path::PathBuf,
    selected_project: Option<String>,
    /// Selected session (timeline mode).
    timeline: Option<TimelineScreenState>,
    /// Right-panel view selection (M2.4 secondary views).
    view: crate::views::AppView,
    /// Lazily loaded view data; `None` means "not loaded for this open yet".
    memories: Option<std::rc::Rc<Vec<crate::data::MemoryEntry>>>,
    selected_memory: Option<usize>,
    /// Memory detail body: show the rendered markdown or its source (#11).
    memory_detail_source: bool,
    usage: Option<crate::data::UsageStats>,
    overview: Option<crate::data::OverviewStats>,
    recaps: Option<std::rc::Rc<Vec<crate::data::RecapEntry>>>,
    /// Recap list kind filter (R1): all/weekly/monthly.
    recap_kind_filter: String,
    selected_recap: Option<serde_json::Value>,
    selected_recap_name: Option<String>,
    /// Visible recap card (0=Cover … 4=Closing) + active archetype key
    /// (Vue RecapDetail state; M4.4).
    recap_card_ix: usize,
    recap_archetype: String,
    /// Generate panel visibility (R3).
    recap_show_generate: bool,
    /// Settings-page snapshot, loaded on demand.
    settings: Option<crate::data::SettingsSnapshot>,
    /// Transient settings-page status line (root save result).
    settings_status: Option<String>,
    /// Reading-position cache for recently viewed sessions (LRU 12).
    reader_states: Vec<(String, ReaderState)>,
    /// Memory sub-view (sidebar Active/Archived rows).
    memory_tab: crate::views::MemoryTab,
    /// Memory keyboard cursor (memory id) + checkbox selection.
    memory_cursor: Option<String>,
    memory_selection: std::collections::HashSet<String>,
    memory_sort_desc: bool,
    /// Pending memory undo (label + action target + expiry).
    memory_undo: Option<MemoryUndo>,
    /// Open subagent timeline, if any (Sessions view; #54).
    subagent: Option<SubagentScreen>,
    /// Memory search box + focus target for the keyboard layer.
    memory_search_state: gpui::Entity<adabraka_ui::components::input_state::InputState>,
    memory_focus: gpui::FocusHandle,
    /// Activity chart tab + heatmap day selection + month pagination
    /// (Vue Activity state; M4.4).
    activity_tab: crate::views::ActivityTab,
    activity_day: Option<String>,
    activity_months: usize,
    /// Session-list sort (Vue state.sortDesc, toggled with `s`) and the
    /// quiet-session fold (M4.5).
    sessions_sort_desc: bool,
    show_noise_sessions: bool,
    /// Timeline font scale (parity #5): one of 6 steps.
    text_scale: f32,
    /// Transient toast line (font-scale feedback) + expiry.
    toast: Option<(String, std::time::Instant)>,
    /// Session-list search box state (Vue: `/` focuses, filters by
    /// title/project/branch client-side). Observed → `search_query`.
    search_state: gpui::Entity<adabraka_ui::components::input_state::InputState>,
    search_query: String,
    /// Full-text hits for the current query (aligned with `obelisk --search`
    /// via the same FTS contract, read-only).
    search_hits: Vec<crate::data::SearchHit>,
    /// Focus target for the sessions panel (so `/` can be handled there).
    sessions_focus: gpui::FocusHandle,
    _search_subscription: gpui::Subscription,
}

struct TimelineScreenState {
    session_id: String,
    title: String,
    source: String,
    items: std::rc::Rc<Vec<timeline::TimelineItem>>,
    /// Measured list state + keyboard focus target for the timeline view.
    list_state: gpui::ListState,
    focus: gpui::FocusHandle,
    /// Collapsible-card disclosure flags, retained across refreshes.
    ui_state: std::rc::Rc<timeline_view::TimelineUiState>,
    /// Message uuid receiving a temporary focus highlight (traceability
    /// jumps), with its expiry.
    focus_highlight: Option<(String, std::time::Instant)>,
    /// The FTS query that produced the focus jump (parity #52).
    matched_query: Option<String>,
    /// Header fields (parity #1-2): git branch + relative last-active.
    branch: Option<String>,
    last_active: Option<String>,
}

/// Per-session reading position, restored when the session is reopened
/// (parity: the Vue app's `session-reader-state` LRU).
struct ReaderState {
    /// Stable key of the top visible timeline item at close time.
    anchor_key: String,
    /// Pixel offset within that item.
    offset_in_item: gpui::Pixels,
    /// Whether the reader was following the tail.
    following_tail: bool,
    /// Disclosure flags for collapsible cards.
    ui_state: std::rc::Rc<timeline_view::TimelineUiState>,
}

/// A pending memory archive/restore that can still be undone (5s window).
struct MemoryUndo {
    memory_id: String,
    /// true = it was an archive (undo restores), false = restore (undo archives).
    was_archive: bool,
    label: String,
    until: std::time::Instant,
}

/// Reader-state LRU size (parity: `session-reader-state` keeps 12 sessions).
const READER_STATE_CACHE: usize = 12;

fn remember_reader_state(
    cache: &mut Vec<(String, ReaderState)>,
    session_id: &str,
    state: ReaderState,
) {
    cache.retain(|(id, _)| id != session_id);
    cache.insert(0, (session_id.to_string(), state));
    cache.truncate(READER_STATE_CACHE);
}

/// Resolve a reader anchor to a list offset against the current items:
/// the anchor item is re-found by stable key; `None` keeps the list at top.
fn anchor_offset(
    items: &[timeline::TimelineItem],
    anchor_key: &str,
    offset_in_item: gpui::Pixels,
) -> Option<gpui::ListOffset> {
    items
        .iter()
        .position(|item| item.key == anchor_key)
        .map(|item_ix| gpui::ListOffset {
            item_ix,
            offset_in_item,
        })
}

impl ObeliskApp {
    fn new(
        home: std::path::PathBuf,
        cwd: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let data = AppData::load(&home, &cwd);
        let search_state = cx.new(adabraka_ui::components::input_state::InputState::new);
        let search_home = home.clone();
        let search_cwd = cwd.clone();
        let subscription = cx.observe(&search_state, move |this, state, cx| {
            let query = state.read(cx).content().to_string();
            if query != this.search_query {
                this.search_query = query.clone();
                this.search_hits.clear();
                // Debounced full-text search (Vue: 200ms) — typing a word
                // runs one FTS pass, not one per keystroke.
                let home = search_home.clone();
                let cwd = search_cwd.clone();
                let query_at_fire = query.clone();
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(200))
                        .await;
                    this.update(cx, |this, cx| {
                        if this.search_query == query_at_fire {
                            this.search_hits =
                                crate::data::search_messages(&home, &cwd, &query_at_fire);
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
                cx.notify();
            }
        });
        let sessions_focus = cx.focus_handle();
        let memory_focus = cx.focus_handle();
        let memory_search_state = cx.new(adabraka_ui::components::input_state::InputState::new);
        // Keyboard starts on the sessions panel so `/` opens search right away.
        window.focus(&sessions_focus);
        // Tray-resident watcher daemon (process-wide singleton): incremental
        // index builds under the writer lease, plus the daemon heartbeat that
        // owns CLI-side builds (ADR-0013 M3.1). This window registers for
        // post-build refreshes.
        let app_handle = cx.weak_entity();
        daemon::start(cx, home.clone(), cwd.clone());
        daemon::register_app(cx, app_handle);
        cx.notify();
        Self {
            data,
            home,
            selected_project: None,
            timeline: None,
            view: crate::views::AppView::Sessions,
            memories: None,
            selected_memory: None,
            memory_detail_source: false,
            usage: None,
            overview: None,
            recaps: None,
            recap_kind_filter: "all".to_string(),
            selected_recap: None,
            selected_recap_name: None,
            recap_card_ix: 0,
            recap_archetype: "architect".to_string(),
            recap_show_generate: false,
            settings: None,
            settings_status: None,
            reader_states: Vec::new(),
            search_state,
            sessions_sort_desc: true,
            show_noise_sessions: false,
            text_scale: 1.0,
            toast: None,
            search_query: String::new(),
            activity_tab: crate::views::ActivityTab::Daily,
            activity_day: None,
            activity_months: 1,
            search_hits: Vec::new(),
            sessions_focus,
            _search_subscription: subscription,
            memory_tab: crate::views::MemoryTab::Active,
            memory_cursor: None,
            memory_selection: std::collections::HashSet::new(),
            memory_sort_desc: true,
            memory_undo: None,
            subagent: None,
            memory_search_state,
            memory_focus,
        }
    }

    /// Switch the right panel to a secondary view, loading its data lazily
    /// (a fresh read per switch; cheap against the local SQLite index).
    fn select_view(
        &mut self,
        view: crate::views::AppView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.view = view;
        self.timeline = None;
        // Navigation side effects (parity #16): leaving the session list
        // clears the transient query; entering Memory clears the project
        // filter so the memory list is never scoped by a sessions filter.
        if view != crate::views::AppView::Sessions {
            self.search_query.clear();
            self.search_hits.clear();
        }
        if view == crate::views::AppView::Memory {
            self.selected_project = None;
            self.memory_cursor = None;
            self.memory_selection.clear();
        }
        match view {
            crate::views::AppView::Sessions => {
                window.focus(&self.sessions_focus);
            }
            crate::views::AppView::Memory => {
                window.focus(&self.memory_focus);
                self.memories = Some(std::rc::Rc::new(crate::data::load_memories(&self.home)));
                self.selected_memory = None;
            }
            crate::views::AppView::Activity => {
                self.usage = Some(crate::data::load_usage_stats(&self.home));
                self.overview = Some(crate::data::load_stats(&self.home));
                // Entering the view resets chart state (Vue onMounted: one
                // month block, no day selected).
                self.activity_day = None;
                self.activity_tab = crate::views::ActivityTab::Daily;
                self.activity_months = 1;
            }
            crate::views::AppView::Recap => {
                let entries = crate::data::list_recap_entries(&self.home);
                eprintln!(
                    "obelisk debug: recap view loaded {} entries from {}",
                    entries.len(),
                    crate::data::recap_dir(&self.home).display()
                );
                self.recaps = Some(std::rc::Rc::new(entries));
                self.selected_recap = None;
            }
            crate::views::AppView::Settings => {
                self.settings = Some(crate::data::load_settings(&self.home));
            }
        }
        cx.notify();
    }

    /// Choose the editor scheme for file references (settings.json write —
    /// plain file, not the SQLite index, so it stays Stage-2-legal).
    fn select_editor_scheme(&mut self, scheme: &str, _window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = crate::data::save_editor_scheme(&self.home, scheme) {
            eprintln!("obelisk: saving editor scheme failed: {error}");
        }
        self.settings = Some(crate::data::load_settings(&self.home));
        cx.notify();
    }

    /// Select one memory row (shows its file content below the list).
    fn select_memory(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected_memory = Some(ix);
        cx.notify();
    }

    /// Select one recap file (shows its parsed JSON below the list).
    fn select_recap(&mut self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) {
        // Reset the card stage for the newly selected recap (Vue behavior:
        // Cover first, persona archetype drives the palette).
        self.recap_card_ix = 0;
        self.recap_archetype = self
            .selected_recap
            .as_ref()
            .and_then(|value| {
                value
                    .get("persona")
                    .and_then(|p| p.get("archetype"))
                    .and_then(|a| a.as_str())
            })
            .unwrap_or("architect")
            .to_string();
        let filename = self
            .recaps
            .as_ref()
            .and_then(|entries| entries.get(ix).map(|entry| entry.filename.clone()));
        self.selected_recap = filename
            .as_ref()
            .and_then(|name| crate::data::read_recap(&self.home, name));
        self.selected_recap_name = filename;
        cx.notify();
    }

    fn select_project(
        &mut self,
        project: Option<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_project = project;
        self.timeline = None;
        cx.notify();
    }

    /// Open one session's timeline (loads detail from the shared index).
    fn open_session(
        &mut self,
        session_id: String,
        home: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let detail = obelisk_core::db::open_read_db(&home)
            .ok()
            .and_then(|conn| crate::data::load_session_detail(&conn, &session_id));
        if let Some(detail) = detail {
            let items = std::rc::Rc::new(timeline::timeline_items(&detail.messages));
            // Header fields (parity #1-2): branch + relative last-active.
            let (branch, last_active) = obelisk_core::db::open_read_db(&home)
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COALESCE(git_branch, ''), COALESCE(ended_at, started_at, '') \
                         FROM sessions WHERE id = ?1",
                        [&session_id],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            ))
                        },
                    )
                    .ok()
                })
                .map(|(branch, ended)| {
                    let last = crate::views::fmt_relative(&ended);
                    (
                        (!branch.is_empty()).then_some(branch),
                        (!last.is_empty()).then_some(last),
                    )
                })
                .unwrap_or((None, None));
            let focus = cx.focus_handle();
            window.focus(&focus);
            let list_state = gpui::ListState::new(items.len(), gpui::ListAlignment::Top, px(600.0));
            // Near-bottom auto-arm (parity: the Vue app enters follow-tail
            // within 50px of the end). Upward input stops following inside
            // fc-gpui's list itself.
            install_follow_tail_arm(&list_state);
            // Restore the reading position for a revisited session
            // (scroll anchor + disclosures).
            let cached = self
                .reader_states
                .iter()
                .position(|(id, _)| id == &session_id)
                .map(|ix| self.reader_states.remove(ix));
            if let Some((_, reader)) = cached {
                if let Some(offset) =
                    anchor_offset(&items, &reader.anchor_key, reader.offset_in_item)
                {
                    list_state.scroll_to(offset);
                }
                if reader.following_tail {
                    list_state.set_follow_tail(true);
                    list_state.scroll_to_end();
                }
                self.timeline = Some(TimelineScreenState {
                    session_id,
                    title: detail.session_title,
                    source: detail.session_source,
                    items,
                    list_state,
                    focus,
                    ui_state: reader.ui_state,
                    focus_highlight: None,
                    matched_query: None,
                    branch: branch.clone(),
                    last_active: last_active.clone(),
                });
                cx.notify();
                return;
            }
            self.timeline = Some(TimelineScreenState {
                session_id,
                title: detail.session_title,
                source: detail.session_source,
                items,
                list_state,
                focus,
                ui_state: std::rc::Rc::new(timeline_view::TimelineUiState::new()),
                focus_highlight: None,
                matched_query: None,
                branch: None,
                last_active: None,
            });
        }
        cx.notify();
    }

    /// Switch the Memory sub-view (sidebar Active/Archived sub-rows).
    fn select_memory_tab(
        &mut self,
        tab: crate::views::MemoryTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.memory_tab = tab;
        self.memory_cursor = None;
        self.memory_selection.clear();
        self.selected_memory = None;
        if self.view != crate::views::AppView::Memory {
            self.select_view(crate::views::AppView::Memory, window, cx);
        }
        window.focus(&self.memory_focus);
        cx.notify();
    }

    /// Archive (Active tab) or restore (Archived tab) one memory, arming undo.
    fn toggle_memory_archive(
        &mut self,
        memory_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let archive = self.memory_tab == crate::views::MemoryTab::Active;
        let result = if archive {
            crate::data::archive_memory(&self.home, memory_id)
        } else {
            crate::data::restore_memory(&self.home, memory_id)
        };
        match result {
            Ok(()) => {
                let label = if archive {
                    format!("Archived {memory_id} — Restore?")
                } else {
                    format!("Restored {memory_id} — Archive again?")
                };
                self.memory_undo = Some(MemoryUndo {
                    memory_id: memory_id.to_string(),
                    was_archive: archive,
                    label,
                    until: std::time::Instant::now() + std::time::Duration::from_secs(5),
                });
                // Keep the undo window ticking so the bar disappears on time.
                cx.notify();
                cx.spawn(async move |app, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(5100))
                        .await;
                    if let Some(app) = app.upgrade() {
                        let _ = app.update(cx, |app: &mut Self, cx| {
                            if let Some(undo) = &app.memory_undo {
                                if std::time::Instant::now() >= undo.until {
                                    app.memory_undo = None;
                                    cx.notify();
                                }
                            }
                        });
                    }
                })
                .detach();
                self.reload_memory(cx);
            }
            Err(error) => {
                eprintln!("obelisk: memory mutation failed: {error}");
            }
        }
    }

    /// Undo the pending memory archive/restore.
    fn undo_memory_action(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.memory_undo.take() else {
            return;
        };
        let result = if undo.was_archive {
            crate::data::restore_memory(&self.home, &undo.memory_id)
        } else {
            crate::data::archive_memory(&self.home, &undo.memory_id)
        };
        if let Err(error) = result {
            eprintln!("obelisk: memory undo failed: {error}");
        }
        self.reload_memory(cx);
    }

    /// Reload memories + the app data counts (badges change with archive state).
    fn reload_memory(&mut self, cx: &mut Context<Self>) {
        if let Some(home) = Some(self.home.clone()) {
            self.memories = Some(std::rc::Rc::new(crate::data::load_memories(&home)));
            self.data = AppData::load(&home, &std::path::PathBuf::from("."));
        }
        cx.notify();
    }

    /// Memory keyboard: move the cursor among the visible (filtered) ids.
    fn memory_cursor_move(&mut self, delta: i32, _window: &mut Window, cx: &mut Context<Self>) {
        let memories = self.memories.clone().unwrap_or_default();
        let tab = self.memory_tab;
        let query = self
            .memory_search_state
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        let ids: Vec<String> = memories
            .iter()
            .filter(|m| m.archived() == (tab == crate::views::MemoryTab::Archived))
            .filter(|m| {
                query.is_empty()
                    || m.path.to_lowercase().contains(&query)
                    || m.summary.to_lowercase().contains(&query)
            })
            .map(|m| m.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let ix = self
            .memory_cursor
            .as_ref()
            .and_then(|id| ids.iter().position(|candidate| candidate == id))
            .map(|ix| ix as i64 + delta as i64)
            .unwrap_or(0)
            .clamp(0, ids.len() as i64 - 1) as usize;
        self.memory_cursor = Some(ids[ix].clone());
        cx.notify();
    }

    /// Memory keyboard: open the cursor row's detail.
    fn memory_open_detail(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.memory_cursor.clone() else {
            return;
        };
        let memories = self.memories.clone().unwrap_or_default();
        if let Some(ix) = memories.iter().position(|m| m.id == id) {
            self.selected_memory = Some(ix);
            cx.notify();
        }
    }

    /// Memory keyboard: toggle the check mark on the cursor row.
    fn memory_toggle_check(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.memory_cursor.clone() {
            if !self.memory_selection.remove(&id) {
                self.memory_selection.insert(id);
            }
            cx.notify();
        }
    }

    /// Memory keyboard: archive/restore the selection (or the cursor row).
    fn memory_archive_key_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets: Vec<String> = if self.memory_selection.is_empty() {
            self.memory_cursor.clone().into_iter().collect()
        } else {
            self.memory_selection.iter().cloned().collect()
        };
        for id in &targets {
            self.toggle_memory_archive(id, window, cx);
        }
    }

    /// Memory keyboard: clear the selection.
    fn memory_clear(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.memory_selection.clear();
        cx.notify();
    }

    /// Toggle memory sort (newest/oldest).
    fn memory_sort_toggle(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.memory_sort_desc = !self.memory_sort_desc;
        cx.notify();
    }

    /// Open a session scrolled to (and briefly highlighting) one message —
    /// the memory "View conversation" traceability path.
    /// Export one recap card as a PNG (R11): render the card in an
    /// offscreen X11 window, read the frame back, save or copy it.
    /// Step the timeline font scale through the 6-step ladder and toast.
    fn adjust_text_scale(&mut self, direction: i32, cx: &mut Context<Self>) {
        const STEPS: [f32; 6] = [0.85, 0.93, 1.0, 1.1, 1.2, 1.35];
        let current = STEPS
            .iter()
            .position(|step| (step - self.text_scale).abs() < 0.001)
            .unwrap_or(2) as i32;
        let next = (current + direction).clamp(0, STEPS.len() as i32 - 1) as usize;
        self.text_scale = STEPS[next];
        self.toast = Some((
            format!("Font size: {}%", (STEPS[next] * 100.0).round() as u32),
            std::time::Instant::now(),
        ));
        cx.notify();
    }

    fn export_recap_card(&mut self, card_ix: usize, copy: bool, cx: &mut Context<Self>) {
        let Some(value) = self.selected_recap.clone() else {
            return;
        };
        let archetype = self.recap_archetype.clone();
        let home = self.home.clone();
        let filename = self
            .selected_recap_name
            .clone()
            .unwrap_or_else(|| "recap".to_string())
            .trim_end_matches(".json")
            .to_string();

        // Offscreen window (X11 honors outside-screen coordinates).
        let options = gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds {
                origin: gpui::Point::new(gpui::px(-2600.0), gpui::px(-2600.0)),
                size: gpui::size(gpui::px(560.0), gpui::px(720.0)),
            })),
            ..Default::default()
        };
        let Ok(handle) = cx.open_window(options, move |_window, cx| {
            let palette = crate::theme::archetype(&archetype);
            let on_card: crate::views::RecapCardFn = std::rc::Rc::new(|_ix, _window, _cx| {});
            let value = value.clone();
            cx.new(move |_cx| OffscreenRecapCard {
                value,
                card_ix,
                palette,
                on_card,
            })
        }) else {
            return;
        };

        cx.spawn(async move |_this, cx| {
            // Let the first frame render and settle.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(700))
                .await;
            let result: anyhow::Result<anyhow::Result<()>> =
                handle.update(cx, |_entity, window, cx| {
                    let image = window.render_to_image()?;
                    if copy {
                        // PNG-encode into the clipboard image entry.
                        let mut bytes = std::io::Cursor::new(Vec::new());
                        image
                            .write_to(&mut bytes, image::ImageFormat::Png)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        let item = gpui::ClipboardItem::new_image(&gpui::Image::new(
                            gpui::ImageFormat::Png,
                            bytes.into_inner(),
                        ));
                        cx.write_to_clipboard(item);
                    } else {
                        let dir = crate::data::recap_dir(&home).join("exports");
                        std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("{e}"))?;
                        let path = dir.join(format!("{filename}-card-{}.png", card_ix + 1));
                        image.save(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
                    }
                    Ok(())
                });
            if let Ok(Err(error)) = result {
                eprintln!("obelisk: recap card export failed: {error}");
            }
            // Close the offscreen window.
            let _ = handle.update(cx, |_entity, window, _cx| {
                window.remove_window();
            });
        })
        .detach();
    }

    /// Open one subagent's timeline (Vue SubagentDetail navigation).
    fn open_subagent(&mut self, agent_id: String, _window: &mut Window, cx: &mut Context<Self>) {
        let parent_session_id = self
            .timeline
            .as_ref()
            .map(|screen| screen.session_id.clone())
            .unwrap_or_default();
        let Some((_, messages)) = crate::data::load_subagent_detail(&self.home, &agent_id) else {
            return;
        };
        self.subagent = Some(SubagentScreen {
            agent_id,
            parent_session_id,
            messages: std::rc::Rc::new(messages),
            focus: cx.focus_handle(),
            ui: std::rc::Rc::new(crate::timeline_view::TimelineUiState::new()),
        });
        cx.notify();
    }

    /// Close the subagent view (Back).
    fn close_subagent(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.subagent = None;
        cx.notify();
    }

    fn open_session_focused(
        &mut self,
        session_id: String,
        focus_uuid: String,
        matched_query: Option<String>,
        home: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Entering a timeline is a Sessions-view concern; a memory jump can
        // arrive from any view, so switch first.
        self.view = crate::views::AppView::Sessions;
        self.open_session(session_id, home, window, cx);
        if let Some(screen) = self.timeline.as_mut() {
            if !focus_uuid.is_empty() {
                if let Some(ix) = screen
                    .items
                    .iter()
                    .position(|item| item.message_uuid == focus_uuid)
                {
                    screen.list_state.scroll_to_reveal_item(ix);
                    screen.focus_highlight = Some((
                        focus_uuid.clone(),
                        std::time::Instant::now() + std::time::Duration::from_secs(2),
                    ));
                    screen.matched_query = matched_query.filter(|q| !q.is_empty());
                }
            }
        }
        cx.notify();
    }

    /// Follow-tail refresh: reload the open session's detail from the shared
    /// index. Appends are spliced into the measured list (scroll position and
    /// measurements preserved); a wholesale change falls back to a reset.
    /// When the list is in follow-tail mode (set by the End key), the layout
    /// re-pins to the tail; disclosure flags for messages that disappeared
    /// are dropped (Vue `retainMessages` parity). Driven by the timeline's
    /// `r` key.
    fn refresh_timeline(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_timeline_daemon(cx);
    }

    /// Same refresh, driven by the M2.5 indexing daemon after a build.
    fn refresh_timeline_daemon(&mut self, cx: &mut Context<Self>) {
        let Some(screen) = self.timeline.as_mut() else {
            return;
        };
        let session_id = screen.session_id.clone();
        let old_len = screen.items.len();
        let detail = obelisk_core::db::open_read_db(&self.home)
            .ok()
            .and_then(|conn| crate::data::load_session_detail(&conn, &session_id));
        let Some(detail) = detail else {
            cx.notify();
            return;
        };
        let items = std::rc::Rc::new(timeline::timeline_items(&detail.messages));
        let uuids: std::collections::HashSet<String> =
            items.iter().map(|item| item.message_uuid.clone()).collect();
        screen
            .ui_state
            .retain_messages(&|id: &str| uuids.contains(id));

        let pure_append = items.len() >= old_len
            && items
                .iter()
                .zip(screen.items.iter())
                .all(|(new, old)| new.message_uuid == old.message_uuid);
        if pure_append {
            // splice(old_len..old_len, delta): append-only growth keeps the
            // current scroll offset and all measured heights.
            screen
                .list_state
                .splice(old_len..old_len, items.len() - old_len);
        } else {
            // The wholesale path used to `reset` — which drops every
            // measurement and parks the viewport at the top, so reading a
            // live session felt like a jump-cut on every non-append change
            // (retract, edit, summary insertion). Parity with the Vue
            // settle behavior: capture the top visible item before the
            // reset, re-find it by stable key afterwards, and restore the
            // exact list offset; a tail-follower stays pinned to the tail.
            let anchor = screen.list_state.logical_scroll_top();
            let anchor_key = screen
                .items
                .get(anchor.item_ix)
                .map(|item| item.key.clone())
                .unwrap_or_default();
            let was_following = screen.list_state.is_following_tail();
            screen.list_state.reset(items.len());
            if was_following {
                screen.list_state.set_follow_tail(true);
                screen.list_state.scroll_to_end();
            } else if let Some(offset) = anchor_offset(&items, &anchor_key, anchor.offset_in_item) {
                screen.list_state.scroll_to(offset);
            }
        }
        screen.items = items;
        screen.title = detail.session_title;
        screen.source = detail.session_source;
        cx.notify();
    }

    fn close_timeline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(screen) = self.timeline.take() {
            let anchor = screen.list_state.logical_scroll_top();
            let anchor_key = screen
                .items
                .get(anchor.item_ix)
                .map(|item| item.key.clone())
                .unwrap_or_default();
            remember_reader_state(
                &mut self.reader_states,
                &screen.session_id,
                ReaderState {
                    anchor_key,
                    offset_in_item: anchor.offset_in_item,
                    following_tail: screen.list_state.is_following_tail(),
                    ui_state: screen.ui_state,
                },
            );
        }
        window.focus(&self.sessions_focus);
        cx.notify();
    }

    /// Manual full rebuild (Settings > About, P0-8): a forced build under
    /// the writer lease, with live status feedback.
    fn request_rebuild(&mut self, cx: &mut Context<Self>) {
        self.settings_status = Some("Rebuilding…".to_string());
        cx.notify();
        let home = self.home.clone();
        cx.spawn(async move |app, cx| {
            let build_home = home.clone();
            let result = cx
                .background_spawn(async move {
                    obelisk_core::indexer::build_index(
                        &build_home,
                        obelisk_core::indexer::BuildIndexOptions {
                            force: true,
                            ignore_recent_build: true,
                            ignore_daemon_ownership: true,
                            provider_registry: None,
                        },
                    )
                })
                .await;
            let _ = app.update(cx, |app: &mut Self, cx| {
                app.settings_status = Some(if result.skip {
                    match result.reason.as_deref() {
                        Some("writer_busy") => {
                            "Rebuild deferred: another writer holds the index — try again shortly"
                                .to_string()
                        }
                        other => format!("Rebuild skipped: {}", other.unwrap_or("unknown")),
                    }
                } else {
                    format!("Rebuilt index — {} sessions", app.data.sessions.len())
                });
                app.reload_from_index(cx);
            });
        })
        .detach();
    }

    /// Reload everything from the shared index (data + settings snapshot).
    fn reload_from_index(&mut self, cx: &mut Context<Self>) {
        let home = self.home.clone();
        let cwd = std::path::PathBuf::from(".");
        self.data = AppData::load(&home, &cwd);
        self.settings = Some(crate::data::load_settings(&home));
        cx.notify();
    }

    /// Reload data from the shared index (used by the live-refresh watcher
    /// landing with M2.5; kept on the model for the skeleton).
    #[allow(dead_code)]
    fn refresh(
        &mut self,
        home: std::path::PathBuf,
        cwd: std::path::PathBuf,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.data = AppData::load(&home, &cwd);
        cx.notify();
    }
}

impl gpui::Render for ObeliskApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        gpui::div()
            .flex()
            .h_full()
            .w_full()
            .font_family(".SystemUIFont")
            .on_action(
                cx.listener(|this: &mut ObeliskApp, _: &TextScaleUp, _window, cx| {
                    this.adjust_text_scale(1, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut ObeliskApp, _: &TextScaleDown, _window, cx| {
                    this.adjust_text_scale(-1, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut ObeliskApp, _: &TextScaleReset, _window, cx| {
                    this.text_scale = 1.0;
                    this.toast = Some(("Font size: 100%".to_string(), std::time::Instant::now()));
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this: &mut ObeliskApp, _: &GlobalOpenRecap, _window, cx| {
                    this.select_view(crate::views::AppView::Recap, _window, cx);
                }),
            )
            .on_action(cx.listener(
                |this: &mut ObeliskApp, _: &GlobalOpenSessions, _window, cx| {
                    this.select_view(crate::views::AppView::Sessions, _window, cx);
                },
            ))
            .on_action(cx.listener(
                |this: &mut ObeliskApp, _: &GlobalOpenActiveMemories, _window, cx| {
                    this.select_view(crate::views::AppView::Memory, _window, cx);
                    this.select_memory_tab(crate::views::MemoryTab::Active, _window, cx);
                },
            ))
            .on_action(cx.listener(
                |this: &mut ObeliskApp, _: &GlobalOpenArchivedMemories, _window, cx| {
                    this.select_view(crate::views::AppView::Memory, _window, cx);
                    this.select_memory_tab(crate::views::MemoryTab::Archived, _window, cx);
                },
            ))
            // Sidebar, a 1:1 port of the original renderer's App.vue aside:
            // brand, Library (Sessions/Memory with Active/Archived subs),
            // Stats (Activity/Recap), Projects (filter + list), Settings
            // pinned to the bottom. Metrics and colors come from theme.rs
            // (the ported base.css tokens).
            .child(sidebar(self, cx))
            // Right panel: session list / timeline / secondary views
            .child(match self.view {
                crate::views::AppView::Sessions => sessions_panel(self, cx),
                crate::views::AppView::Memory => {
                    // NOTE: focus is set by select_view/select_memory_tab; calling
                    // window.focus() during render drops the frame (GPUI
                    // re-enters layout) — which made every view switch render
                    // one frame late and look like dead clicks.
                    let app_handle = cx.entity();
                    // Tab + project filtering happens here; search inside the view.
                    let tab = self.memory_tab;
                    let project = self.selected_project.clone();
                    let memories = self
                        .memories
                        .clone()
                        .unwrap_or_default()
                        .iter()
                        .filter(|m| m.archived() == (tab == crate::views::MemoryTab::Archived))
                        .filter(|m| match &project {
                            Some(slug) => &m.project == slug,
                            None => true,
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    crate::views::MemoryView {
                        tab,
                        memories: std::rc::Rc::new(memories),
                        query: self.memory_search_state.read(cx).content().to_string(),
                        selected: self.selected_memory,
                        cursor_id: self.memory_cursor.clone(),
                        selection: std::rc::Rc::new(self.memory_selection.clone()),
                        sort_desc: self.memory_sort_desc,
                        detail_source: self.memory_detail_source,
                        on_toggle_detail_source: {
                            let src_handle = cx.entity();
                            std::rc::Rc::new(move |window: &mut Window, cx| {
                                src_handle.update(cx, |app: &mut ObeliskApp, cx| {
                                    app.memory_detail_source = !app.memory_detail_source;
                                    cx.notify();
                                    let _ = window;
                                });
                            })
                        },
                        undo: self
                            .memory_undo
                            .as_ref()
                            .map(|undo| (undo.label.clone(), undo.until)),
                        search: self.memory_search_state.clone(),
                        focus: self.memory_focus.clone(),
                        on_select: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |ix, window, cx| {
                                app_handle.update(cx, |app, cx| app.select_memory(ix, window, cx));
                            })
                        },
                        on_toggle_archive: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |id, window, cx| {
                                app_handle.update(cx, |app, cx| {
                                    app.toggle_memory_archive(id, window, cx)
                                });
                            })
                        },
                        on_undo: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |window, cx| {
                                app_handle.update(cx, |app, cx| app.undo_memory_action(window, cx));
                            })
                        },
                        on_open_session: {
                            let app_handle = app_handle.clone();
                            let home = self.home.clone();
                            let cursor = self.memory_cursor.clone();
                            let memories = self.memories.clone().unwrap_or_default();
                            std::rc::Rc::new(move |session_id, focus_uuid, window, cx| {
                                // Empty ids mean the keyboard action: use the
                                // cursor row's session and message range.
                                let (session_id, focus_uuid) = if session_id.is_empty() {
                                    let memory =
                                        memories.iter().find(|m| Some(&m.id) == cursor.as_ref());
                                    match memory {
                                        Some(memory) => (
                                            memory.session_id.clone(),
                                            memory.message_start.clone().unwrap_or_default(),
                                        ),
                                        None => return,
                                    }
                                } else {
                                    (session_id.to_string(), focus_uuid.to_string())
                                };
                                let home = home.clone();
                                app_handle.update(cx, |app, cx| {
                                    app.open_session_focused(
                                        session_id, focus_uuid, None, home, window, cx,
                                    )
                                });
                            })
                        },
                        on_sort: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |window, cx| {
                                app_handle.update(cx, |app, cx| app.memory_sort_toggle(window, cx));
                            })
                        },
                        on_cursor_move: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |delta, window, cx| {
                                app_handle.update(cx, |app, cx| {
                                    app.memory_cursor_move(delta, window, cx)
                                });
                            })
                        },
                        on_open_detail: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |window, cx| {
                                app_handle.update(cx, |app, cx| app.memory_open_detail(window, cx));
                            })
                        },
                        on_toggle_check: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |window, cx| {
                                app_handle
                                    .update(cx, |app, cx| app.memory_toggle_check(window, cx));
                            })
                        },
                        on_archive_key: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |window, cx| {
                                app_handle.update(cx, |app, cx| {
                                    app.memory_archive_key_action(window, cx)
                                });
                            })
                        },
                        on_clear: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |window, cx| {
                                app_handle.update(cx, |app, cx| app.memory_clear(window, cx));
                            })
                        },
                        on_tab: {
                            let app_handle = app_handle.clone();
                            std::rc::Rc::new(move |tab, window, cx| {
                                app_handle
                                    .update(cx, |app, cx| app.select_memory_tab(tab, window, cx));
                            })
                        },
                    }
                    .into_any_element()
                }
                crate::views::AppView::Activity => {
                    let app_handle = cx.entity();
                    let stats = self.usage.clone().unwrap_or_default();
                    let overview = self.overview.clone().unwrap_or_default();
                    let sessions =
                        std::rc::Rc::new(crate::data::load_activity_sessions(&self.home));
                    let selected_day = self.activity_day.clone();
                    let tab = self.activity_tab;
                    let loaded_months = self.activity_months;
                    let day_handle = cx.entity();
                    crate::views::ActivityView {
                        stats,
                        overview,
                        sessions,
                        selected_day,
                        tab,
                        loaded_months,
                        on_select_day: std::rc::Rc::new(move |day, _window, cx| {
                            day_handle.update(cx, |app, cx| {
                                app.activity_day = Some(day);
                                cx.notify();
                            });
                        }),
                        on_open_session: std::rc::Rc::new(move |session_id, _window, cx| {
                            app_handle.update(cx, |app, cx| {
                                let home = app.home.clone();
                                app.open_session_focused(
                                    session_id,
                                    String::new(),
                                    None,
                                    home,
                                    _window,
                                    cx,
                                );
                            });
                        }),
                        on_tab: {
                            let tab_handle = cx.entity();
                            std::rc::Rc::new(move |tab, _window, cx| {
                                tab_handle.update(cx, |app, cx| {
                                    app.activity_tab = tab;
                                    cx.notify();
                                });
                            })
                        },
                        on_more: {
                            let more_handle = cx.entity();
                            std::rc::Rc::new(move |_window, cx| {
                                more_handle.update(cx, |app, cx| {
                                    app.activity_months += 1;
                                    cx.notify();
                                });
                            })
                        },
                    }
                    .into_any_element()
                }
                crate::views::AppView::Recap => {
                    let app_handle = cx.entity();
                    let card_handle = cx.entity();
                    let archetype = self.recap_archetype.clone();
                    let card_ix = self.recap_card_ix;
                    crate::views::RecapView {
                        entries: self.recaps.clone().unwrap_or_default(),
                        kind_filter: self.recap_kind_filter.clone(),
                        selected: self.selected_recap.clone(),
                        selected_name: self.selected_recap_name.clone(),
                        card_ix,
                        archetype,
                        show_generate: self.recap_show_generate,
                        on_select: std::rc::Rc::new(move |ix, window, cx| {
                            app_handle.update(cx, |app, cx| app.select_recap(ix, window, cx));
                        }),
                        on_copy_command: std::rc::Rc::new(|command, _window, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(command));
                        }),
                        on_kind_filter: {
                            let kind_handle = cx.entity();
                            std::rc::Rc::new(move |kind: String, _window, cx| {
                                kind_handle.update(cx, |app, cx| {
                                    app.recap_kind_filter = kind;
                                    cx.notify();
                                });
                            })
                        },
                        on_export: {
                            let export_handle = cx.entity();
                            std::rc::Rc::new(move |card_ix: usize, copy: bool, _window, cx| {
                                export_handle.update(cx, |app, cx| {
                                    app.export_recap_card(card_ix, copy, cx);
                                });
                            })
                        },
                        on_toggle_generate: {
                            let gen_handle = cx.entity();
                            std::rc::Rc::new(move |_window, cx| {
                                gen_handle.update(cx, |app, cx| {
                                    app.recap_show_generate = !app.recap_show_generate;
                                    cx.notify();
                                });
                            })
                        },
                        on_card: std::rc::Rc::new(move |ix, _window, cx| {
                            card_handle.update(cx, |app, cx| {
                                app.recap_card_ix = ix.min(4);
                                cx.notify();
                            });
                        }),
                    }
                    .into_any_element()
                }
                crate::views::AppView::Settings => {
                    let app_handle = cx.entity();
                    let snapshot =
                        self.settings
                            .clone()
                            .unwrap_or_else(|| crate::data::SettingsSnapshot {
                                raw: serde_json::json!({}),
                            });
                    // Editable data-source rows: one per builtin provider,
                    // with the effective root (custom override or default).
                    let overrides: std::collections::HashMap<String, String> =
                        snapshot.provider_roots().into_iter().collect();
                    let mut rows = Vec::new();
                    for (id, label, default_root) in
                        obelisk_core::provider_settings::builtin_provider_defaults(
                            &self.home,
                            std::path::Path::new("."),
                        )
                    {
                        let custom = overrides.get(id).cloned();
                        let current = custom
                            .clone()
                            .unwrap_or_else(|| default_root.to_string_lossy().into_owned());
                        // Status light + indexed session count (parity #2).
                        let ok = std::path::Path::new(&current).is_dir();
                        let session_count = std::fs::canonicalize(&current)
                            .ok()
                            .and_then(|root| crate::data::count_sessions_under(&self.home, &root))
                            .unwrap_or(0);
                        rows.push(crate::views::ProviderRootRow {
                            id,
                            label,
                            current,
                            is_custom: custom.is_some(),
                            ok,
                            session_count,
                            input: cx.new(adabraka_ui::components::input_state::InputState::new),
                        });
                    }
                    let save_home = self.home.clone();
                    let save_handle = cx.entity();
                    crate::views::SettingsView {
                        editor_scheme: snapshot.editor_scheme(),
                        on_scheme: {
                            let scheme_handle = app_handle.clone();
                            std::rc::Rc::new(move |scheme, window, cx| {
                                scheme_handle.update(cx, |app, cx| {
                                    app.select_editor_scheme(scheme, window, cx)
                                });
                            })
                        },
                        on_rebuild: {
                            let rebuild_handle = app_handle.clone();
                            std::rc::Rc::new(move |_window, cx| {
                                rebuild_handle.update(cx, |app, cx| app.request_rebuild(cx));
                            })
                        },
                        roots: rows,
                        on_save_root: std::rc::Rc::new(move |id, path, _window, cx| {
                            // Validate before persisting (P0-9): a bad path
                            // gets an immediate red status instead of
                            // silently neutering the provider.
                            let result = if path.is_empty() {
                                clear_provider_root(&save_home, id)
                            } else {
                                crate::data::validate_provider_root(&save_home, path).and_then(
                                    |()| crate::data::save_provider_root(&save_home, id, path),
                                )
                            };
                            match result {
                                Ok(()) => {
                                    // Rebuild with the new roots, then reload
                                    // every open window from the shared index.
                                    let home = save_home.clone();
                                    cx.spawn(async move |cx| {
                                        let build_home = home.clone();
                                        let _ = cx
                                            .background_spawn(async move {
                                                obelisk_core::indexer::build_index(
                                                    &build_home,
                                                    obelisk_core::indexer::BuildIndexOptions {
                                                        force: false,
                                                        ignore_recent_build: true,
                                                        ignore_daemon_ownership: true,
                                                        provider_registry: None,
                                                    },
                                                )
                                            })
                                            .await;
                                        let _ = cx.update(|cx| {
                                            let apps = crate::daemon::registered_apps(cx);
                                            for app in apps {
                                                let _ = app.update(cx, |app, cx| {
                                                    app.reload_from_index(cx);
                                                });
                                            }
                                        });
                                    })
                                    .detach();
                                    save_handle.update(cx, |app, cx| {
                                        app.settings_status =
                                            Some(format!("Saved {id} root — rebuilding index…"));
                                        cx.notify();
                                    });
                                }
                                Err(error) => {
                                    save_handle.update(cx, |app, cx| {
                                        app.settings_status = Some(format!("Save failed: {error}"));
                                        cx.notify();
                                    });
                                }
                            }
                        }),
                        status: self.settings_status.clone(),
                        recap_dir: crate::data::recap_dir(&self.home).display().to_string(),
                        recap_input: cx.new(adabraka_ui::components::input_state::InputState::new),
                        on_save_recap_dir: {
                            let save_handle = cx.entity();
                            let home = self.home.clone();
                            std::rc::Rc::new(move |dir: &str, _window, cx| {
                                let result = crate::data::save_recap_dir(&home, dir);
                                save_handle.update(cx, |app: &mut ObeliskApp, cx| {
                                    match result {
                                        Ok(()) => {
                                            app.settings = Some(crate::data::load_settings(&home));
                                            if app.view == crate::views::AppView::Recap {
                                                app.recaps = Some(std::rc::Rc::new(
                                                    crate::data::list_recap_entries(&home),
                                                ));
                                                app.selected_recap = None;
                                            }
                                        }
                                        Err(error) => {
                                            app.settings_status = Some(error);
                                        }
                                    }
                                    cx.notify();
                                });
                            })
                        },
                        on_browse: {
                            let browse_handle = cx.entity();
                            let home = self.home.clone();
                            std::rc::Rc::new(move |target: &str, _window, cx| {
                                let options = gpui::PathPromptOptions {
                                    files: false,
                                    directories: true,
                                    multiple: false,
                                    prompt: Some("Select directory".into()),
                                };
                                let receiver = cx.prompt_for_paths(options);
                                let target = target.to_string();
                                let home = home.clone();
                                browse_handle.update(cx, |_app: &mut ObeliskApp, cx| {
                                    cx.spawn(async move |this, cx| {
                                        let Ok(Ok(Some(paths))) = receiver.await else {
                                            return;
                                        };
                                        let Some(path) = paths.first() else {
                                            return;
                                        };
                                        let path = path.display().to_string();
                                        this.update(cx, |app, cx| {
                                            if target == "recap" {
                                                let result =
                                                    crate::data::save_recap_dir(&home, &path);
                                                if let Err(error) = result {
                                                    app.settings_status = Some(error);
                                                } else {
                                                    app.settings =
                                                        Some(crate::data::load_settings(&home));
                                                    if app.view == crate::views::AppView::Recap {
                                                        app.recaps = None;
                                                        app.selected_recap = None;
                                                    }
                                                }
                                            } else {
                                                let result = crate::data::validate_provider_root(
                                                    &home, &path,
                                                )
                                                .and_then(|()| {
                                                    crate::data::save_provider_root(
                                                        &home, &target, &path,
                                                    )
                                                });
                                                match result {
                                                    Ok(()) => {
                                                        app.settings =
                                                            Some(crate::data::load_settings(&home));
                                                    }
                                                    Err(error) => {
                                                        app.settings_status = Some(error);
                                                    }
                                                }
                                            }
                                            cx.notify();
                                        })
                                        .ok();
                                    })
                                    .detach();
                                });
                            })
                        },
                        index_path: format!("{}/.obelisk/obelisk.sqlite", self.home.display()),
                        on_reveal: {
                            let home = self.home.clone();
                            std::rc::Rc::new(move |_window, cx| {
                                // Platform reveal (Vue showItemInFolder).
                                let dir = home.join(".obelisk");
                                cx.reveal_path(&dir);
                            })
                        },
                    }
                    .into_any_element()
                }
            })
            .when(
                self.toast
                    .as_ref()
                    .is_some_and(|(_, until)| std::time::Instant::now() < *until),
                |el| {
                    if let Some((text, _)) = &self.toast {
                        el.child(
                            gpui::div()
                                .id("toast")
                                .absolute()
                                .bottom_8()
                                .left_1_2()
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .bg(gpui::rgba(0x1a1b26f2))
                                .border_1()
                                .border_color(crate::theme::HAIRLINE_STRONG)
                                .text_size(px(12.0))
                                .font_family(crate::theme::MONO)
                                .text_color(crate::theme::FG_2)
                                .child(text.clone()),
                        )
                    } else {
                        el
                    }
                },
            )
    }
}

/// The sessions right panel: the open timeline, or the session list.
fn sessions_panel(app: &mut ObeliskApp, cx: &mut Context<ObeliskApp>) -> gpui::AnyElement {
    let project_labels = app.data.project_labels.clone();
    let selected_project_slug = app.selected_project.clone();
    let sessions: Vec<session_list::SessionRow> = app
        .data
        .sessions_for(app.selected_project.as_deref())
        .into_iter()
        .map(|s| {
            // Vue list #3: the project chip shows the label only while no
            // project filter is active (the filter context is obvious).
            let project = match &selected_project_slug {
                Some(slug) if slug == &s.project => slug.clone(),
                _ => project_labels
                    .get(&s.project)
                    .cloned()
                    .unwrap_or_else(|| s.project.trim_matches('-').to_string()),
            };
            session_list::SessionRow {
                id: s.id,
                title: s.title,
                project,
                source: s.source,
                started_at: s.started_at,
                ended_at: s.ended_at,
                message_count: s.message_count,
                git_branch: s.git_branch,
            }
        })
        .collect();
    let selected = app.selected_project.clone();
    if let Some(screen) = &app.subagent {
        let app_handle = cx.entity();
        let agent_id = screen.agent_id.clone();
        crate::timeline_view::SubagentDetailView {
            agent_id,
            messages: screen.messages.clone(),
            home: std::rc::Rc::new(app.home.clone()),
            focus: screen.focus.clone(),
            ui: screen.ui.clone(),
            on_back: std::rc::Rc::new(move |window, cx| {
                app_handle.update(cx, |app, cx| app.close_subagent(window, cx));
            }),
        }
        .into_any_element()
    } else if let Some(screen) = &app.timeline {
        let app_handle = cx.entity();
        let app_handle_refresh = cx.entity();
        timeline_view::TimelineView {
            items: screen.items.clone(),
            title: screen.title.clone(),
            source: screen.source.clone(),
            home: std::rc::Rc::new(app.home.clone()),
            list_state: screen.list_state.clone(),
            focus: screen.focus.clone(),
            ui_state: screen.ui_state.clone(),
            focus_highlight: screen.focus_highlight.clone(),
            on_back: std::rc::Rc::new(move |_ev, window, cx| {
                app_handle.update(cx, |app, cx| app.close_timeline(window, cx));
            }),
            on_refresh: std::rc::Rc::new(move |window, cx| {
                app_handle_refresh.update(cx, |app, cx| app.refresh_timeline(window, cx));
            }),
            on_open_subagent: {
                let sub_handle = cx.entity();
                std::rc::Rc::new(move |agent_id: String, window, cx| {
                    sub_handle.update(cx, |app, cx| {
                        app.open_subagent(agent_id, window, cx);
                    });
                })
            },
            text_scale: app.text_scale,
            matched_query: screen.matched_query.clone(),
            branch: screen.branch.clone(),
            last_active: screen.last_active.clone(),
        }
        .into_any_element()
    } else {
        let app_handle = cx.entity();
        let home_for_open = app.home.clone();
        gpui::div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x141417))
            .child(session_list::session_list_view(
                sessions,
                selected,
                app_handle,
                home_for_open,
                session_list::SessionSearch {
                    query: app.search_query.clone(),
                    hits: app.search_hits.clone(),
                    input: app.search_state.clone(),
                    focus: app.sessions_focus.clone(),
                },
                session_list::SessionListOptions {
                    index_ready: app.data.index_ready,
                    sort_desc: app.sessions_sort_desc,
                    show_noise: app.show_noise_sessions,
                },
            ))
            .into_any_element()
    }
}

/// Whether this session exposes a StatusNotifier tray (Linux). macOS and
/// Windows always have a tray surface. On Linux we ask the session bus
/// whether `org.kde.StatusNotifierWatcher` is owned (the StatusNotifierItem
/// protocol every tray implementation registers). Probing falls back to
/// "available" when no bus tool exists — keeping current behavior rather
/// than stranding tray-capable setups.
fn status_notifier_available() -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    let name = "org.kde.StatusNotifierWatcher";
    for (command, args) in [
        (
            "gdbus",
            vec![
                "call".to_string(),
                "--session".to_string(),
                "--dest".to_string(),
                "org.freedesktop.DBus".to_string(),
                "--object-path".to_string(),
                "/org/freedesktop/DBus".to_string(),
                "--method".to_string(),
                "org.freedesktop.DBus.NameHasOwner".to_string(),
                name.to_string(),
            ],
        ),
        (
            "busctl",
            vec![
                "--user".to_string(),
                "call".to_string(),
                "org.freedesktop.DBus".to_string(),
                "/org/freedesktop/DBus".to_string(),
                "org.freedesktop.DBus".to_string(),
                "NameHasOwner".to_string(),
                "s".to_string(),
                name.to_string(),
            ],
        ),
    ] {
        if let Ok(output) = std::process::Command::new(command).args(&args).output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
                return stdout.contains("true");
            }
        }
    }
    // No bus tooling reachable: assume the tray exists (current behavior).
    true
}

/// Near-bottom auto-arm (parity: the Vue app enters follow-tail near the
/// end). fc-gpui invokes the scroll handler while the list state's RefCell
/// is mutably borrowed, so the handler must not call any ListState method
/// directly — it derives near-bottom from the event payload and defers the
/// arm until the borrow is released (v0.4.0 macOS crash regression).
fn install_follow_tail_arm(list_state: &gpui::ListState) {
    let follow_list = list_state.clone();
    list_state.set_scroll_handler(move |event, _window, cx| {
        let near_bottom =
            event.count > 0 && event.visible_range.end.saturating_add(1) >= event.count;
        if !event.is_following_tail && near_bottom {
            let list = follow_list.clone();
            cx.defer(move |_cx| {
                list.set_follow_tail(true);
            });
        }
    });
}

fn main() {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .expect("HOME is set");
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    Application::new()
        .with_assets(assets::IconAssets)
        .with_http_client(std::sync::Arc::new(LocalImageHttpClient))
        .run(move |cx: &mut App| {
            adabraka_ui::init(cx);

            // Memory keyboard layer (parity: j/k/Enter/x/d/u/Esc inside the
            // memory list; Cmd/Ctrl+2/3 jump to Active/Archived globally).
            use gpui::KeyBinding;
            cx.bind_keys(vec![
                // Global view shortcuts (Vue resolveGlobalShortcut,
                // modifier = Cmd on macOS / Ctrl elsewhere).
                // Font scale (parity #5): 6 steps.
                KeyBinding::new("ctrl-=", crate::TextScaleUp, None),
                KeyBinding::new("ctrl-plus", crate::TextScaleUp, None),
                KeyBinding::new("ctrl--", crate::TextScaleDown, None),
                KeyBinding::new("ctrl-minus", crate::TextScaleDown, None),
                KeyBinding::new("ctrl-0", crate::TextScaleReset, None),
                KeyBinding::new("ctrl-1", crate::GlobalOpenSessions, None),
                KeyBinding::new("ctrl-2", crate::GlobalOpenActiveMemories, None),
                KeyBinding::new("ctrl-3", crate::GlobalOpenArchivedMemories, None),
                // Desktop superset: deterministic Recap navigation.
                KeyBinding::new("ctrl-4", crate::GlobalOpenRecap, None),
                // Sessions panel: `s` toggles sort. Escape clears the
                // query globally — while typing, the input owns focus and
                // the SessionsList context never matches, so the binding
                // must be context-free (more specific contexts like
                // MemoryList still win for their own escape handling).
                KeyBinding::new(
                    "s",
                    crate::session_list::SessionToggleSort,
                    Some("SessionsList"),
                ),
                KeyBinding::new("escape", crate::session_list::SessionClearQuery, None),
                KeyBinding::new("j", crate::views::MemoryCursorDown, Some("MemoryList")),
                KeyBinding::new("down", crate::views::MemoryCursorDown, Some("MemoryList")),
                KeyBinding::new("k", crate::views::MemoryCursorUp, Some("MemoryList")),
                KeyBinding::new("up", crate::views::MemoryCursorUp, Some("MemoryList")),
                KeyBinding::new("enter", crate::views::MemoryOpenDetail, Some("MemoryList")),
                KeyBinding::new("m", crate::views::MemoryOpenDetail, Some("MemoryList")),
                KeyBinding::new(
                    "v",
                    crate::views::MemoryOpenConversation,
                    Some("MemoryList"),
                ),
                KeyBinding::new("x", crate::views::MemoryToggleCheck, Some("MemoryList")),
                KeyBinding::new("d", crate::views::MemoryArchiveSelected, Some("MemoryList")),
                KeyBinding::new("u", crate::views::MemoryUndoAction, Some("MemoryList")),
                KeyBinding::new(
                    "escape",
                    crate::views::MemoryClearSelection,
                    Some("MemoryList"),
                ),
            ]);

            // Tray-resident background app: quitting happens explicitly via the
            // tray menu, closing the window keeps the app alive. On desktops
            // WITHOUT a status-notifier area (plain GNOME), tray residency
            // would strand the app: closing the window leaves a process the
            // user cannot reopen or quit — so there we quit with the last
            // window instead.
            if status_notifier_available() {
                cx.set_quit_mode(gpui::QuitMode::Explicit);
            } else {
                cx.set_quit_mode(gpui::QuitMode::LastWindowClosed);
            }
            cx.set_tray_tooltip("Obelisk");
            cx.set_tray_menu(vec![
                gpui::TrayMenuItem::Action {
                    label: "Open Obelisk".into(),
                    id: "open".into(),
                },
                gpui::TrayMenuItem::Separator,
                gpui::TrayMenuItem::Action {
                    label: "Quit".into(),
                    id: "quit".into(),
                },
            ]);

            let home_for_open = home.clone();
            let cwd_for_open = cwd.clone();
            cx.on_tray_menu_action(move |id, cx| {
                if id.as_ref() == "quit" {
                    cx.quit();
                } else if id.as_ref() == "open" {
                    let home = home_for_open.clone();
                    let cwd = cwd_for_open.clone();
                    cx.open_window(
                        WindowOptions {
                            titlebar: Some(TitlebarOptions {
                                title: Some("Obelisk".into()),
                                ..Default::default()
                            }),
                            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                                None,
                                gpui::size(px(1080.0), px(760.0)),
                                cx,
                            ))),
                            ..Default::default()
                        },
                        move |window, cx| cx.new(move |cx| ObeliskApp::new(home, cwd, window, cx)),
                    )
                    .expect("window opens");
                }
            });

            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitlebarOptions {
                        title: Some("Obelisk".into()),
                        ..Default::default()
                    }),
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        gpui::size(px(1080.0), px(760.0)),
                        cx,
                    ))),
                    ..Default::default()
                },
                move |window, cx| {
                    cx.new(|cx| ObeliskApp::new(home.clone(), cwd.clone(), window, cx))
                },
            )
            .expect("window opens");
        });
}

/// Remove a providerRoots override (empty input = reset to default).
fn clear_provider_root(home: &std::path::Path, provider_id: &str) -> Result<(), String> {
    let path = obelisk_core::provider_settings::settings_path(home);
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if let Some(roots) = value
        .get_mut("providerRoots")
        .and_then(|r| r.as_object_mut())
    {
        roots.remove(provider_id);
    }
    let serialized = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, serialized).map_err(|e| e.to_string())?;
    Ok(())
}

// ---- Sidebar (App.vue aside port) ------------------------------------------

/// One inline SVG icon from the embedded asset table, tinted by `color`.
fn icon(name: &str, color: gpui::Rgba, size: gpui::Pixels) -> gpui::Svg {
    gpui::svg()
        .path(format!("icons/{name}.svg"))
        .size(size)
        .text_color(color)
}

/// Sidebar section title (`.sidebar-section-title`: 10.5px, muted, tracked).
fn section_title(label: &str) -> gpui::Div {
    gpui::div()
        .px_2p5()
        .pt_1()
        .pb_1p5()
        .text_size(px(10.5))
        .text_color(theme::MUTED)
        .font_weight(gpui::FontWeight::MEDIUM)
        .child(label.to_string())
}

/// Hairline divider between sidebar sections.
fn section_divider() -> gpui::Div {
    gpui::div().mx_1p5().h(px(1.0)).bg(theme::HAIRLINE)
}

/// Sidebar row click handler: switches view / selects project.
type SidebarClick = Box<dyn Fn(&mut ObeliskApp, &mut gpui::Window, &mut Context<ObeliskApp>)>;

/// One sidebar row (`.sidebar-item`): icon + label + mono badge, 28px tall,
/// 5px radius; active gets the accent-soft pill plus the 2px accent rail.
/// Everything one sidebar row needs to render (keeps `sidebar_row` under
/// clippy's argument budget).
pub struct SidebarRowSpec {
    pub id: gpui::SharedString,
    pub icon_name: Option<&'static str>,
    pub label: String,
    pub badge: Option<usize>,
    pub active: bool,
    pub sub: bool,
}

fn sidebar_row(
    spec: SidebarRowSpec,
    on_click: SidebarClick,
    cx: &mut Context<ObeliskApp>,
) -> gpui::AnyElement {
    let SidebarRowSpec {
        id,
        icon_name,
        label,
        badge,
        active,
        sub,
    } = spec;
    let row = gpui::div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px_2p5()
        .h(if sub {
            theme::ROW_H_SUB
        } else {
            theme::ROW_H_COMPACT
        })
        .rounded_sm()
        .text_size(if sub {
            theme::TEXT_SM
        } else {
            theme::TEXT_BASE
        })
        .when(sub, |row| row.pl(px(30.0)))
        // Active rows carry the 2px accent glow bar (Vue parity #24).
        .when(active, |row| row.border_l_2().border_color(theme::ACCENT))
        .text_color(if active { theme::FG } else { theme::FG_2 })
        .bg(if active {
            theme::ACCENT_SOFT
        } else {
            theme::rgba(0x00000000)
        })
        .hover(|s| s.bg(theme::SURFACE_STRONG))
        .cursor_pointer()
        .on_click(cx.listener(move |app, _ev, window, cx| {
            (on_click)(app, window, cx);
        }));
    let icon_size = if sub { px(12.0) } else { px(14.0) };
    let row = match icon_name {
        Some(name) => row.child(icon(
            name,
            if active {
                theme::ACCENT_2
            } else {
                theme::MUTED
            },
            icon_size,
        )),
        None => row,
    };
    let row = row.child(
        gpui::div()
            .flex_1()
            .overflow_x_hidden()
            .child(label.to_string()),
    );
    let row = match badge {
        Some(count) => row.child(
            gpui::div()
                .font_family(theme::MONO)
                .text_size(px(10.5))
                .text_color(if active { theme::FG_2 } else { theme::MUTED })
                .child(count.to_string()),
        ),
        None => row,
    };
    row.into_any_element()
}

/// Build the whole sidebar (App.vue `<aside>`).
fn sidebar(app: &mut ObeliskApp, cx: &mut Context<ObeliskApp>) -> gpui::AnyElement {
    let projects = app.data.projects.clone();
    let session_total: usize = app.data.sessions.len();
    let memory_active = app.data.memory_active;
    let memory_archived = app.data.memory_archived;
    let current_view = app.view;
    let in_timeline = app.timeline.is_some();
    let selected_project = app.selected_project.clone();
    let memory_tab_is_active = app.memory_tab == crate::views::MemoryTab::Active;

    gpui::div()
        .id("sidebar")
        .w(theme::SIDEBAR_W)
        .h_full()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(theme::HAIRLINE_STRONG)
        .bg(theme::SIDEBAR_BG)
        // Brand row (36px, hairline below): logo + name.
        .child(
            gpui::div()
                .id("brand")
                .flex()
                .items_center()
                .gap_2()
                .px(px(14.0))
                .h(px(36.0))
                .border_b_1()
                .border_color(theme::HAIRLINE)
                .child(gpui::svg().path("icons/brand.svg").size(px(18.0)))
                .child(
                    gpui::div()
                        .text_size(theme::TEXT_BASE)
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::FG_2)
                        .child("Obelisk"),
                ),
        )
        // Library: Sessions + Memory (with Active/Archived sub-rows).
        .child(
            gpui::div()
                .px(px(6.0))
                .pt_2()
                .pb_2()
                .child(section_title("Library"))
                .child(sidebar_row(
                    SidebarRowSpec {
                        id: "nav-sessions".into(),
                        icon_name: Some("sessions"),
                        label: "Sessions".to_string(),
                        badge: Some(session_total),
                        active: current_view == crate::views::AppView::Sessions
                            && !in_timeline
                            && selected_project.is_none(),
                        sub: false,
                    },
                    Box::new(|app, window, cx| {
                        app.select_view(crate::views::AppView::Sessions, window, cx);
                    }),
                    cx,
                ))
                .child(sidebar_row(
                    SidebarRowSpec {
                        id: "nav-memory".into(),
                        icon_name: Some("memory"),
                        label: "Memory".to_string(),
                        badge: Some(memory_active + memory_archived),
                        active: current_view == crate::views::AppView::Memory && !in_timeline,
                        sub: false,
                    },
                    Box::new(|app, window, cx| {
                        app.select_view(crate::views::AppView::Memory, window, cx);
                    }),
                    cx,
                ))
                .child(sidebar_row(
                    SidebarRowSpec {
                        id: "nav-memory-active".into(),
                        icon_name: Some("dot-filled"),
                        label: "Active".to_string(),
                        badge: Some(memory_active),
                        active: current_view == crate::views::AppView::Memory
                            && !in_timeline
                            && memory_tab_is_active,
                        sub: true,
                    },
                    Box::new(|app, window, cx| {
                        app.select_memory_tab(crate::views::MemoryTab::Active, window, cx);
                    }),
                    cx,
                ))
                .child(sidebar_row(
                    SidebarRowSpec {
                        id: "nav-memory-archived".into(),
                        icon_name: Some("dot-outline"),
                        label: "Archived".to_string(),
                        badge: Some(memory_archived),
                        active: current_view == crate::views::AppView::Memory
                            && !in_timeline
                            && !memory_tab_is_active,
                        sub: true,
                    },
                    Box::new(|app, window, cx| {
                        app.select_memory_tab(crate::views::MemoryTab::Archived, window, cx);
                    }),
                    cx,
                )),
        )
        .child(section_divider())
        // Stats: Activity + Recap.
        .child(
            gpui::div()
                .px(px(6.0))
                .pt_2()
                .pb_2()
                .child(section_title("Stats"))
                .child(sidebar_row(
                    SidebarRowSpec {
                        id: "nav-activity".into(),
                        icon_name: Some("activity"),
                        label: "Activity".to_string(),
                        badge: None,
                        active: current_view == crate::views::AppView::Activity && !in_timeline,
                        sub: false,
                    },
                    Box::new(|app, window, cx| {
                        app.select_view(crate::views::AppView::Activity, window, cx);
                    }),
                    cx,
                ))
                .child(sidebar_row(
                    SidebarRowSpec {
                        id: "nav-recap".into(),
                        icon_name: Some("recap"),
                        label: "Recap".to_string(),
                        badge: None,
                        active: current_view == crate::views::AppView::Recap && !in_timeline,
                        sub: false,
                    },
                    Box::new(|app, window, cx| {
                        app.select_view(crate::views::AppView::Recap, window, cx);
                    }),
                    cx,
                )),
        )
        .child(section_divider())
        // Projects: title + folder rows with counts. Only rendered in the
        // sessions/memory domains (parity #18).
        .child(
            gpui::div()
                .id("projects")
                // Natural height (the original CSS pins Settings with
                // margin-top:auto; gpui's flex_1 + nested scroll combos
                // over-measure here and pushed Settings off-window).
                .flex()
                .flex_col()
                .px(px(6.0))
                .pt_2()
                .pb_2()
                .child(section_title("Projects"))
                .children(projects.iter().map(|p| {
                    let is_selected = current_view == crate::views::AppView::Sessions
                        && !in_timeline
                        && selected_project.as_deref() == Some(p.slug.as_str());
                    let slug = p.slug.clone();
                    // Count semantics (parity #20): session counts in the
                    // sessions domain, memory counts in the memory domain.
                    let count = if current_view == crate::views::AppView::Memory {
                        app.memories
                            .as_ref()
                            .map(|memories| memories.iter().filter(|m| m.project == slug).count())
                            .unwrap_or(0)
                    } else {
                        p.session_count
                    };
                    // Vue formatProjectLabel: shortest project_path basename.
                    let label = app
                        .data
                        .project_labels
                        .get(&slug)
                        .cloned()
                        .unwrap_or_else(|| slug.trim_matches('-').to_string());
                    sidebar_row(
                        SidebarRowSpec {
                            id: format!("project-{slug}").into(),
                            icon_name: Some("folder"),
                            label: label.clone(),
                            badge: Some(count),
                            active: is_selected,
                            sub: false,
                        },
                        Box::new(move |app, window, cx| {
                            app.select_view(crate::views::AppView::Sessions, window, cx);
                            let next = if app.selected_project.as_deref() == Some(slug.as_str()) {
                                None
                            } else {
                                Some(slug.clone())
                            };
                            app.select_project(next, window, cx);
                        }),
                        cx,
                    )
                })),
        )
        // Settings pinned to the bottom above a hairline.
        .child(section_divider())
        .child(gpui::div().px(px(6.0)).pt_1p5().pb_2().child(sidebar_row(
            SidebarRowSpec {
                id: "nav-settings".into(),
                icon_name: Some("settings"),
                label: "Settings".to_string(),
                badge: None,
                active: current_view == crate::views::AppView::Settings,
                sub: false,
            },
            Box::new(|app, window, cx| {
                app.select_view(crate::views::AppView::Settings, window, cx);
            }),
            cx,
        )))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use crate::install_follow_tail_arm;
    use gpui::{
        div, list, point, px, size, AppContext, Context, IntoElement, ListAlignment, ListState,
        Render, ScrollDelta, ScrollWheelEvent, Styled, TestAppContext, Window,
    };

    // Regression test for the macOS launch crash reported against v0.4.0:
    // fc-gpui invokes the scroll handler while the list state's RefCell is
    // mutably borrowed (ListStateInner::scroll, list.rs). Calling any
    // ListState method from inside the handler (e.g. is_scrolled_to_end())
    // re-borrows the same RefCell and panics with "RefCell already mutably
    // borrowed". The production handler must defer all state mutation and
    // derive near-bottom from the event payload alone.
    #[gpui::test]
    fn test_scroll_handler_defers_list_state_access(cx: &mut TestAppContext) {
        let cx = cx.add_empty_window();

        // Same handler open_session installs (production code path).
        let state = ListState::new(5, ListAlignment::Top, px(0.)).measure_all();
        install_follow_tail_arm(&state);

        struct TestView(ListState);
        impl Render for TestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                list(self.0.clone(), |_, _, _| {
                    div().h(px(20.)).w_full().into_any_element()
                })
                .w_full()
                .h_full()
            }
        }

        // 5 items x 20px in a 40px viewport: scrolling -60px lands exactly
        // at the bottom, so the handler observes visible_range 3..5 == count.
        cx.draw(point(px(0.), px(0.)), size(px(100.), px(40.)), |_, cx| {
            cx.new(|_| TestView(state.clone()))
        });
        cx.simulate_event(ScrollWheelEvent {
            position: point(px(1.), px(1.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-60.))),
            ..Default::default()
        });

        // Drawing the next frame flushes the effect queue, which runs the
        // deferred follow-tail arm once the RefCell borrow is released.
        cx.draw(point(px(0.), px(0.)), size(px(100.), px(40.)), |_, cx| {
            cx.new(|_| TestView(state.clone()))
        });
        assert!(state.is_following_tail());
    }
}
