// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Session list view — virtualized so a full-corpus list stays smooth.
//! M2.2 upgrades this to fc-ui's uniform virtual list; the skeleton renders
//! a plain scrollable list first so the vertical slice compiles and runs
//! against real data end-to-end.
//!
//! M2.4 adds the Vue session-list search: `/` focuses the header input, the
//! query filters client-side by title/project (case-insensitive substring)
//! and matching title segments are highlighted.

use adabraka_ui::components::input::Input;
use adabraka_ui::components::input_state::InputState;
use gpui::{
    px, App, FocusHandle, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled,
};

use crate::data::SessionSummary;

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub project: String,
    pub source: String,
    pub started_at: String,
    pub ended_at: String,
    pub message_count: i64,
}

impl From<SessionSummary> for SessionRow {
    fn from(s: SessionSummary) -> Self {
        Self {
            id: s.id,
            title: s.title,
            project: s.project,
            source: s.source,
            started_at: s.started_at,
            ended_at: s.ended_at,
            message_count: s.message_count,
        }
    }
}

/// Search-box context for the session list: the shared input state, the
/// active query, and the full-text hits it produced.
pub struct SessionSearch {
    pub query: String,
    pub hits: Vec<crate::data::SearchHit>,
    pub input: gpui::Entity<InputState>,
    pub focus: FocusHandle,
}

#[derive(IntoElement)]
pub struct SessionListView {
    sessions: Vec<SessionRow>,
    scope: Option<String>,
    app: gpui::Entity<crate::ObeliskApp>,
    home: std::path::PathBuf,
    search: SessionSearch,
}

pub fn session_list_view(
    sessions: Vec<SessionRow>,
    scope: Option<String>,
    app: gpui::Entity<crate::ObeliskApp>,
    home: std::path::PathBuf,
    search: SessionSearch,
) -> SessionListView {
    SessionListView {
        sessions,
        scope,
        app,
        home,
        search,
    }
}

/// One full-text hit row: snippet plus session title; clicking opens the
/// session timeline (aligned with `obelisk --search` evidence rows).
fn search_hit_row(
    hit: &crate::data::SearchHit,
    app: gpui::Entity<crate::ObeliskApp>,
    home: std::path::PathBuf,
) -> impl IntoElement + use<> {
    let session_id = hit.session_id.clone();
    let title = if hit.session_title.is_empty() {
        "(untitled)".to_string()
    } else {
        hit.session_title.clone()
    };
    gpui::div()
        .id(gpui::SharedString::from(format!(
            "hit-{}",
            hit.message_uuid
        )))
        .px_6()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .border_b_1()
        .border_color(gpui::rgb(0x222226))
        .hover(|s| s.bg(gpui::rgb(0x1c1c21)))
        .cursor_pointer()
        .child(
            gpui::div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(0xa9b1d6))
                .line_height(gpui::relative(1.45))
                .child(hit.snippet.clone()),
        )
        .child(
            gpui::div()
                .flex()
                .gap_2()
                .text_size(px(11.0))
                .text_color(gpui::rgb(0x77777f))
                .child(gpui::div().child(title))
                .child(gpui::div().child("full-text match")),
        )
        .on_click(move |_event, window, cx| {
            let session_id = session_id.clone();
            let home = home.clone();
            app.update(cx, |app, cx| {
                app.open_session(session_id, home, window, cx);
            });
        })
}

impl RenderOnce for SessionListView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        let header = self
            .scope
            .clone()
            .unwrap_or_else(|| "All sessions".to_string());
        let app = self.app;
        let home = self.home;
        let SessionSearch {
            query,
            hits,
            input: search,
            focus,
        } = self.search;

        // Client-side filter (Vue visibleSessions): title/project/branch
        // substring match, case-insensitive.
        let query = query.trim().to_lowercase();
        let rows: Vec<SessionRow> = if query.is_empty() {
            self.sessions
        } else {
            self.sessions
                .into_iter()
                .filter(|row| {
                    row.title.to_lowercase().contains(&query)
                        || row.project.to_lowercase().contains(&query)
                        || row.id.to_lowercase().contains(&query)
                })
                .collect()
        };
        let count = rows.len();
        let searching = !query.is_empty();
        let search_focus_handle = search.clone();

        let mut list = gpui::div()
            .id("session-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for row in rows {
            let app = app.clone();
            let home = home.clone();
            let session_id = row.id.clone();
            let query = query.clone();
            list = list.child(session_row(row, session_id, app, home, query));
        }

        let empty = count == 0 && hits.is_empty();

        // Full-text matches (same FTS contract as `obelisk --search`) sit
        // above the title/project filter while the query is active.
        let mut hits_section = None;
        if !hits.is_empty() {
            let mut section = gpui::div().flex().flex_col();
            section = section.child(
                gpui::div()
                    .px_6()
                    .pt_4()
                    .pb_1()
                    .text_size(px(11.0))
                    .text_color(gpui::rgb(0x77777f))
                    .child(format!("Full-text matches ({})", hits.len())),
            );
            for hit in &hits {
                section = section.child(search_hit_row(hit, app.clone(), home.clone()));
            }
            hits_section = Some(section);
        }
        let hits_section = hits_section;

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .child(
                gpui::div()
                    .px_6()
                    .py_4()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap_4()
                    .border_b_1()
                    .border_color(gpui::rgb(0x2a2a2e))
                    .child(
                        gpui::div()
                            .text_size(px(16.0))
                            .text_color(gpui::rgb(0xe8e8ee))
                            .child(header),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                gpui::div().w(px(220.0)).child(
                                    Input::new(&search)
                                        .placeholder("Search sessions…")
                                        .cleanable(),
                                ),
                            )
                            .child(
                                gpui::div()
                                    .text_size(px(13.0))
                                    .text_color(gpui::rgb(0x77777f))
                                    .child(format!(
                                        "{count} session{}",
                                        if count == 1 { "" } else { "s" }
                                    )),
                            ),
                    ),
            )
            .child(if empty {
                gpui::div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(14.0))
                    .text_color(gpui::rgb(0x77777f))
                    .child(if searching {
                        "No sessions match this search — try a different term."
                    } else {
                        "No sessions indexed yet — run `obelisk --build`."
                    })
                    .into_any_element()
            } else {
                // The panel carries the `/` shortcut: focusing the search box
                // when the list itself (not the input) holds keyboard focus.
                let mut panel = gpui::div()
                    .id("sessions-panel")
                    .track_focus(&focus)
                    .flex_1()
                    .flex()
                    .flex_col()
                    .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "/" {
                            let handle = search_focus_handle.read(cx).focus_handle(cx);
                            if window.focused(cx) != Some(handle.clone()) {
                                window.focus(&handle);
                            }
                        }
                    });
                if let Some(hits) = hits_section {
                    panel = panel.child(hits);
                }
                panel = panel.child(list);
                let _ = search;
                panel.into_any_element()
            })
    }
}

fn session_row(
    row: SessionRow,
    session_id: String,
    app: gpui::Entity<crate::ObeliskApp>,
    home: std::path::PathBuf,
    query: String,
) -> impl IntoElement {
    let title = if row.title.is_empty() {
        "(untitled)".to_string()
    } else {
        row.title
    };
    let when = if row.ended_at.is_empty() {
        row.started_at.clone()
    } else {
        row.ended_at.clone()
    };
    gpui::div()
        .id(row.id.clone())
        .px_6()
        .py_3()
        .flex()
        .justify_between()
        .border_b_1()
        .border_color(gpui::rgb(0x222226))
        .hover(|s| s.bg(gpui::rgb(0x1c1c21)))
        .cursor_pointer()
        .on_click(move |_event, window, cx| {
            let session_id = session_id.clone();
            let home = home.clone();
            app.update(cx, |app, cx| {
                app.open_session(session_id, home, window, cx);
            });
        })
        .child(
            gpui::div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    gpui::div()
                        .text_size(px(14.0))
                        .text_color(gpui::rgb(0xdcdce4))
                        .child(highlighted_title(&title, &query)),
                )
                .child(
                    gpui::div()
                        .flex()
                        .gap_2()
                        .text_size(px(12.0))
                        .text_color(gpui::rgb(0x77777f))
                        .child(gpui::div().child(format!("● {}", row.source)))
                        .child(gpui::div().child(row.project.clone()))
                        .child(gpui::div().child(when)),
                ),
        )
        .child(
            gpui::div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(0x77777f))
                .child(format!("{} msgs", row.message_count)),
        )
}

/// Title with query matches highlighted (Vue `highlightPlain`): plain
/// segments in the row color, matching segments in the accent color.
fn highlighted_title(title: &str, query: &str) -> gpui::AnyElement {
    let mut parts = gpui::div().flex().gap_0p5().flex_wrap();
    if query.is_empty() {
        return parts.child(title.to_string()).into_any_element();
    }
    let query_lower = query.to_lowercase();
    let title_lower = title.to_lowercase();
    let mut cursor = 0;
    while cursor < title.len() {
        match title_lower[cursor..].find(&query_lower) {
            Some(offset) => {
                let start = cursor + offset;
                let end = start + query.len().min(title.len() - start);
                if start > cursor {
                    parts = parts.child(title[cursor..start].to_string());
                }
                parts = parts.child(
                    gpui::div()
                        .text_color(gpui::rgb(0xbb9af7))
                        .child(title[start..end].to_string()),
                );
                cursor = end;
            }
            None => {
                parts = parts.child(title[cursor..].to_string());
                break;
            }
        }
    }
    parts.into_any_element()
}
