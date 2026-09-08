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
    /// Git branch at session start (search filter, M4.5).
    pub git_branch: String,
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
            git_branch: s.git_branch,
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

// Keyboard actions for the sessions panel (Vue: `s` toggles sort order,
// Escape clears the query, `/` focuses search — the last one is handled
// via on_key_down like before).
gpui::actions!(
    obelisk_app,
    [SessionToggleSort, SessionClearQuery, SessionToggleNoise]
);

#[derive(IntoElement)]
pub struct SessionListView {
    sessions: Vec<SessionRow>,
    scope: Option<String>,
    app: gpui::Entity<crate::ObeliskApp>,
    home: std::path::PathBuf,
    search: SessionSearch,
    /// Whether the index has completed a build — distinguishes "building
    /// on first launch" from "no transcripts found".
    index_ready: bool,
    /// Sort by start/end time descending (Vue state.sortDesc; `s` flips).
    sort_desc: bool,
    /// Whether the quiet (untitled) session group is expanded.
    show_noise: bool,
}

/// Bundle of list presentation state (M4.5) so the constructor stays
/// under the argument-count lint.
pub struct SessionListOptions {
    pub index_ready: bool,
    pub sort_desc: bool,
    pub show_noise: bool,
}

pub fn session_list_view(
    sessions: Vec<SessionRow>,
    scope: Option<String>,
    app: gpui::Entity<crate::ObeliskApp>,
    home: std::path::PathBuf,
    search: SessionSearch,
    options: SessionListOptions,
) -> SessionListView {
    SessionListView {
        sessions,
        scope,
        app,
        home,
        search,
        index_ready: options.index_ready,
        sort_desc: options.sort_desc,
        show_noise: options.show_noise,
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
    let message_uuid = hit.message_uuid.clone();
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
            let message_uuid = message_uuid.clone();
            let home = home.clone();
            app.update(cx, |app, cx| {
                // Jump straight to the matched message (M4.5): reuse the
                // memory jump machinery, which locates and highlights; the
                // query rides along for the matched chip (parity #52).
                app.open_session_focused(
                    session_id,
                    message_uuid,
                    Some(app.search_query.clone()),
                    home,
                    window,
                    cx,
                );
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
        let mut rows: Vec<SessionRow> = if query.is_empty() {
            self.sessions
        } else {
            self.sessions
                .into_iter()
                .filter(|row| {
                    row.title.to_lowercase().contains(&query)
                        || row.project.to_lowercase().contains(&query)
                        || row.id.to_lowercase().contains(&query)
                        || row.git_branch.to_lowercase().contains(&query)
                })
                .collect()
        };
        // Sort (Vue state.sortDesc, toggled with `s`): by ended_at falling
        // back to started_at, string comparison (ISO-8601 sorts lexically).
        rows.sort_by(|a, b| {
            let key = |row: &SessionRow| {
                if row.ended_at.is_empty() {
                    row.started_at.clone()
                } else {
                    row.ended_at.clone()
                }
            };
            if self.sort_desc {
                key(b).cmp(&key(a))
            } else {
                key(a).cmp(&key(b))
            }
        });
        // Quiet-session fold (Vue isNoise = !title): hidden behind a banner
        // while the query is empty; the query reveals everything.
        let (normal_rows, noise_rows) = if query.is_empty() {
            let mut normal = Vec::new();
            let mut noise = Vec::new();
            for row in rows.into_iter() {
                if row.title.is_empty() {
                    noise.push(row);
                } else {
                    normal.push(row);
                }
            }
            (normal, noise)
        } else {
            (rows, Vec::new())
        };
        let sort_desc = self.sort_desc;
        let show_noise = self.show_noise;
        let noise_visible = show_noise || !query.is_empty();
        let normal_count = normal_rows.len();
        let noise_count = if noise_visible { noise_rows.len() } else { 0 };
        let count = normal_count + noise_count;
        let searching = !query.is_empty();
        let search_focus_handle = search.clone();

        let mut list = gpui::div()
            .id("session-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for row in normal_rows {
            let app = app.clone();
            let home = home.clone();
            let session_id = row.id.clone();
            let query = query.clone();
            list = list.child(session_row(row, session_id, app, home, query));
        }
        if !noise_rows.is_empty() && query.is_empty() {
            // Fold banner (Vue .fold-banner): N quiet sessions hidden.
            let app_banner = app.clone();
            list = list.child(
                gpui::div()
                    .id("quiet-fold-banner")
                    .px_6()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(0x77777f))
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui::rgb(0x1c1c21)))
                    .child(gpui::div().child("\u{25b8}"))
                    .child(gpui::div().child(format!(
                        "{} quiet sessions hidden — untitled, likely tests or incomplete runs.",
                        noise_rows.len()
                    )))
                    .on_click(move |_event, _window, cx| {
                        app_banner.update(cx, |app, cx| {
                            app.show_noise_sessions = !app.show_noise_sessions;
                            cx.notify();
                        });
                    }),
            );
            if show_noise {
                for row in noise_rows {
                    let app = app.clone();
                    let home = home.clone();
                    let session_id = row.id.clone();
                    let query = query.clone();
                    list = list.child(session_row(row, session_id, app, home, query));
                }
            }
        }

        let empty = normal_count == 0 && noise_count == 0 && hits.is_empty();

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
                                        "{count} session{} · {}",
                                        if count == 1 { "" } else { "s" },
                                        if sort_desc { "newest" } else { "oldest" }
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
                    } else if self.index_ready {
                        "No sessions found in your provider transcripts."
                    } else {
                        "Building the index from your transcripts — the first build can take a minute on large histories. This screen fills in automatically."
                    })
                    .into_any_element()
            } else {
                // The panel carries the `/` shortcut: focusing the search box
                // when the list itself (not the input) holds keyboard focus.
                let mut panel = gpui::div()
                    .id("sessions-panel")
                    .track_focus(&focus)
                    .key_context("SessionsList")
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
                    })
                    .on_action({
                        let app = app.clone();
                        move |_: &SessionToggleSort, _window, cx| {
                            app.update(cx, |app, cx| {
                                app.sessions_sort_desc = !app.sessions_sort_desc;
                                cx.notify();
                            });
                        }
                    })
                    .on_action({
                        let app = app.clone();
                        move |_: &SessionToggleNoise, _window, cx| {
                            app.update(cx, |app, cx| {
                                app.show_noise_sessions = !app.show_noise_sessions;
                                cx.notify();
                            });
                        }
                    })
                    .on_action({
                        let app = app.clone();
                        let search = search.clone();
                        move |_: &SessionClearQuery, window, cx| {
                            app.update(cx, |app, cx| {
                                app.search_query.clear();
                                app.search_hits.clear();
                                cx.notify();
                            });
                            // Blur the input so Escape does not re-trigger.
                            let handle = search.read(cx).focus_handle(cx);
                            if window.focused(cx) == Some(handle.clone()) {
                                window.focus(&focus);
                            }
                        }
                    });
                if let Some(hits) = hits_section {
                    panel = panel.child(hits);
                }
                panel = panel.child(list);
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
                        .children(
                            (!row.git_branch.is_empty())
                                .then(|| gpui::div().child(row.git_branch.clone())),
                        )
                        .child(gpui::div().child(crate::views::fmt_list_time(&when))),
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
///
/// Matching happens over the case-folded character stream, but segments are
/// always cut on original-character boundaries — a byte-offset approach
/// panics whenever case folding changes the byte length (e.g. `İ`, `ß`).
fn highlighted_title(title: &str, query: &str) -> gpui::AnyElement {
    let mut parts = gpui::div().flex().gap_0p5().flex_wrap();
    for (text, hit) in highlight_segments(title, query) {
        if hit {
            parts = parts.child(gpui::div().text_color(gpui::rgb(0xbb9af7)).child(text));
        } else {
            parts = parts.child(text);
        }
    }
    parts.into_any_element()
}

/// Case-insensitive query match segments over `title`, always cut on
/// original-character boundaries. A byte-offset approach panics whenever
/// case folding changes the byte length (e.g. `İ` folds to two chars, `ß`
/// to `ss`); matching instead happens on the folded character stream and
/// ranges map back through a per-character index.
pub fn highlight_segments(title: &str, query: &str) -> Vec<(String, bool)> {
    let title_chars: Vec<char> = title.chars().collect();
    if query.is_empty() || title_chars.is_empty() {
        return vec![(title.to_string(), false)];
    }
    let query_folded: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut folded: Vec<char> = Vec::with_capacity(title_chars.len());
    let mut fold_to_orig: Vec<usize> = Vec::new();
    for (ix, ch) in title_chars.iter().enumerate() {
        for folded_ch in ch.to_lowercase() {
            folded.push(folded_ch);
            fold_to_orig.push(ix);
        }
    }
    if query_folded.is_empty() || query_folded.len() > folded.len() {
        return vec![(title.to_string(), false)];
    }
    let mut segments: Vec<(usize, usize, bool)> = Vec::new(); // (orig start, orig end, hit)
    let mut plain_start = 0usize;
    let mut fold_ix = 0usize;
    while fold_ix + query_folded.len() <= folded.len() {
        if folded[fold_ix..fold_ix + query_folded.len()] == query_folded[..] {
            let orig_start = fold_to_orig[fold_ix];
            let orig_end = fold_to_orig[fold_ix + query_folded.len() - 1] + 1;
            if orig_start > plain_start {
                segments.push((plain_start, orig_start, false));
            }
            segments.push((orig_start, orig_end, true));
            fold_ix += query_folded.len();
            plain_start = orig_end;
        } else {
            fold_ix += 1;
        }
    }
    if plain_start < title_chars.len() {
        segments.push((plain_start, title_chars.len(), false));
    }
    segments
        .into_iter()
        .map(|(start, end, hit)| (title_chars[start..end].iter().collect(), hit))
        .collect()
}

#[cfg(test)]
mod tests {

    /// The highlighter must never panic, whatever the case-folding does to
    /// byte lengths (İ folds to two chars, ß to "ss"), and must emit the
    /// full title across its segments.
    use super::highlight_segments;

    #[test]
    fn highlight_segments_reassemble_the_title_exactly() {
        for (title, query) in [
            ("Fix auth bug", "auth"),
            ("修复登录问题", "登录"),
            ("İstanbul session", "ist"),
            ("straße session", "ss"),
            ("emoji 🎉 title", "🎉"),
            ("tiny", "tiny but longer than title"),
            ("", "x"),
            ("case-insensitive MATCH", "match"),
            ("multi multi multi", "multi"),
        ] {
            let segments = highlight_segments(title, query);
            let reassembled: String = segments.iter().map(|(text, _)| text.as_str()).collect();
            assert_eq!(reassembled, title, "segments must reassemble the title");
            assert!(
                segments.iter().any(|(_, hit)| *hit)
                    || !title.to_lowercase().contains(&query.to_lowercase()),
                "a hit segment must exist whenever the folded query occurs"
            );
        }
    }

    #[test]
    fn highlight_segments_mark_the_right_range() {
        let segments = highlight_segments("Fix auth bug", "AUTH");
        assert_eq!(segments.len(), 3, "{segments:?}");
        assert_eq!(segments[0], ("Fix ".to_string(), false));
        assert_eq!(segments[1], ("auth".to_string(), true));
        assert_eq!(segments[2], (" bug".to_string(), false));

        // `ß` lowercases to itself (same as JS toLowerCase, so "ss" does not
        // match — parity with the original highlighter); no hit, no panic.
        let segments = highlight_segments("straße", "ss");
        assert_eq!(segments, vec![("straße".to_string(), false)]);

        // `İ` folds to two chars ("i" + combining dot). A query carrying the
        // combining dot explicitly matches across the expansion; the hit
        // segment must cover the whole original `İ` char (no mid-char
        // slicing, no panic).
        let segments = highlight_segments("İstanbul ops", "i\u{0307}st");
        let reassembled: String = segments.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(reassembled, "İstanbul ops");
        assert!(segments.iter().any(|(_, hit)| *hit));
        let hit_text: String = segments
            .iter()
            .filter(|(_, hit)| *hit)
            .map(|(text, _)| text.clone())
            .collect();
        assert_eq!(hit_text, "İst");

        // Plain "istan" does not match "İstanbul" (the folded form carries
        // the combining dot) — same as JS toLowerCase in the original app.
        let segments = highlight_segments("İstanbul ops", "istan");
        assert_eq!(segments, vec![("İstanbul ops".to_string(), false)]);
    }
}
