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

mod daemon;
mod data;
mod file_reference;
mod session_list;
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
    usage: Option<crate::data::UsageStats>,
    overview: Option<crate::data::OverviewStats>,
    recaps: Option<std::rc::Rc<Vec<String>>>,
    selected_recap: Option<serde_json::Value>,
    selected_recap_name: Option<String>,
    /// Settings-page snapshot, loaded on demand.
    settings: Option<crate::data::SettingsSnapshot>,
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
                this.search_hits = crate::data::search_messages(&search_home, &search_cwd, &query);
                cx.notify();
            }
        });
        let sessions_focus = cx.focus_handle();
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
            usage: None,
            overview: None,
            recaps: None,
            selected_recap: None,
            selected_recap_name: None,
            settings: None,
            search_state,
            search_query: String::new(),
            search_hits: Vec::new(),
            sessions_focus,
            _search_subscription: subscription,
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
        match view {
            crate::views::AppView::Sessions => {
                window.focus(&self.sessions_focus);
            }
            crate::views::AppView::Memory => {
                self.memories = Some(std::rc::Rc::new(crate::data::load_memories(&self.home)));
                self.selected_memory = None;
            }
            crate::views::AppView::Activity => {
                self.usage = Some(crate::data::load_usage_stats(&self.home));
                self.overview = Some(crate::data::load_stats(&self.home));
            }
            crate::views::AppView::Recap => {
                self.recaps = Some(std::rc::Rc::new(crate::data::list_recaps(&self.home)));
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
        let filename = self
            .recaps
            .as_ref()
            .and_then(|names| names.get(ix).cloned());
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
            let focus = cx.focus_handle();
            window.focus(&focus);
            let list_state = gpui::ListState::new(items.len(), gpui::ListAlignment::Top, px(600.0));
            self.timeline = Some(TimelineScreenState {
                session_id,
                title: detail.session_title,
                source: detail.session_source,
                items,
                list_state,
                focus,
                ui_state: std::rc::Rc::new(timeline_view::TimelineUiState::new()),
            });
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
            screen.list_state.reset(items.len());
        }
        screen.items = items;
        screen.title = detail.session_title;
        screen.source = detail.session_source;
        cx.notify();
    }

    fn close_timeline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.timeline = None;
        window.focus(&self.sessions_focus);
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
        let projects = self.data.projects.clone();

        gpui::div()
            .flex()
            .h_full()
            .w_full()
            .font_family(".SystemUIFont")
            // Project sidebar: view navigation + projects
            .child(
                gpui::div()
                    .id("sidebar")
                    .w(px(240.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(gpui::rgb(0x2a2a2e))
                    .bg(gpui::rgb(0x1b1b1f))
                    .overflow_y_scroll()
                    .child(
                        gpui::div()
                            .px_4()
                            .py_3()
                            .text_size(px(13.0))
                            .text_color(gpui::rgb(0x9a9aa5))
                            .child("VIEWS"),
                    )
                    .children(crate::views::AppView::ALL.map(|view| {
                        let is_selected = self.view == view && self.timeline.is_none();
                        gpui::div()
                            .id(view.label())
                            .px_4()
                            .py_2()
                            .flex()
                            .justify_between()
                            .text_size(px(13.0))
                            .text_color(if is_selected {
                                gpui::rgb(0xffffff)
                            } else {
                                gpui::rgb(0xc8c8d0)
                            })
                            .bg(if is_selected {
                                gpui::rgb(0x2d2d33)
                            } else {
                                gpui::rgb(0x1b1b1f)
                            })
                            .hover(|s| s.bg(gpui::rgb(0x242429)))
                            .cursor_pointer()
                            .child(gpui::div().child(view.label()))
                            .on_click(cx.listener(move |app, _ev, window, cx| {
                                app.select_view(view, window, cx);
                            }))
                    }))
                    .child(
                        gpui::div()
                            .px_4()
                            .py_3()
                            .text_size(px(13.0))
                            .text_color(gpui::rgb(0x9a9aa5))
                            .child("PROJECTS"),
                    )
                    .children(projects.iter().map(|p| {
                        let is_selected = self.view == crate::views::AppView::Sessions
                            && self.selected_project.as_deref() == Some(p.slug.as_str());
                        let slug = p.slug.clone();
                        let count = p.session_count;
                        gpui::div()
                            .id(slug.clone())
                            .px_4()
                            .py_2()
                            .flex()
                            .justify_between()
                            .text_size(px(13.0))
                            .text_color(if is_selected {
                                gpui::rgb(0xffffff)
                            } else {
                                gpui::rgb(0xc8c8d0)
                            })
                            .bg(if is_selected {
                                gpui::rgb(0x2d2d33)
                            } else {
                                gpui::rgb(0x1b1b1f)
                            })
                            .hover(|s| s.bg(gpui::rgb(0x242429)))
                            .cursor_pointer()
                            .child(gpui::div().child(slug.clone()))
                            .child(
                                gpui::div()
                                    .text_color(gpui::rgb(0x77777f))
                                    .child(count.to_string()),
                            )
                            .on_click(cx.listener(move |app, _ev, window, cx| {
                                app.select_view(crate::views::AppView::Sessions, window, cx);
                                let next = if app.selected_project.as_deref() == Some(slug.as_str())
                                {
                                    None
                                } else {
                                    Some(slug.clone())
                                };
                                app.select_project(next, window, cx);
                            }))
                    })),
            )
            // Right panel: session list / timeline / secondary views
            .child(match self.view {
                crate::views::AppView::Sessions => sessions_panel(self, cx),
                crate::views::AppView::Memory => {
                    let app_handle = cx.entity();
                    crate::views::MemoryView {
                        memories: self.memories.clone().unwrap_or_default(),
                        selected: self.selected_memory,
                        on_select: std::rc::Rc::new(move |ix, window, cx| {
                            app_handle.update(cx, |app, cx| app.select_memory(ix, window, cx));
                        }),
                    }
                    .into_any_element()
                }
                crate::views::AppView::Activity => crate::views::ActivityView {
                    stats: self.usage.clone().unwrap_or_default(),
                    overview: self.overview.clone().unwrap_or_default(),
                }
                .into_any_element(),
                crate::views::AppView::Recap => {
                    let app_handle = cx.entity();
                    crate::views::RecapView {
                        filenames: self.recaps.clone().unwrap_or_default(),
                        selected: self.selected_recap.clone(),
                        selected_name: self.selected_recap_name.clone(),
                        on_select: std::rc::Rc::new(move |ix, window, cx| {
                            app_handle.update(cx, |app, cx| app.select_recap(ix, window, cx));
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
                    crate::views::SettingsView {
                        provider_roots: snapshot.provider_roots(),
                        editor_scheme: snapshot.editor_scheme(),
                        on_scheme: std::rc::Rc::new(move |scheme, window, cx| {
                            app_handle
                                .update(cx, |app, cx| app.select_editor_scheme(scheme, window, cx));
                        }),
                    }
                    .into_any_element()
                }
            })
    }
}

/// The sessions right panel: the open timeline, or the session list.
fn sessions_panel(app: &mut ObeliskApp, cx: &mut Context<ObeliskApp>) -> gpui::AnyElement {
    let sessions: Vec<session_list::SessionRow> = app
        .data
        .sessions_for(app.selected_project.as_deref())
        .into_iter()
        .map(|s| session_list::SessionRow {
            id: s.id,
            title: s.title,
            project: s.project,
            source: s.source,
            started_at: s.started_at,
            ended_at: s.ended_at,
            message_count: s.message_count,
        })
        .collect();
    let selected = app.selected_project.clone();
    if let Some(screen) = &app.timeline {
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
            on_back: std::rc::Rc::new(move |_ev, window, cx| {
                app_handle.update(cx, |app, cx| app.close_timeline(window, cx));
            }),
            on_refresh: std::rc::Rc::new(move |window, cx| {
                app_handle_refresh.update(cx, |app, cx| app.refresh_timeline(window, cx));
            }),
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
            ))
            .into_any_element()
    }
}

fn main() {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .expect("HOME is set");
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    Application::new()
        .with_http_client(std::sync::Arc::new(LocalImageHttpClient))
        .run(move |cx: &mut App| {
            adabraka_ui::init(cx);

            // Tray-resident background app: quitting happens explicitly via the
            // tray menu, closing the window keeps the app alive.
            cx.set_quit_mode(gpui::QuitMode::Explicit);
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
