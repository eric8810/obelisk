// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! M2.4 secondary views: memory browser, usage-activity report, weekly recap
//! cards. All read-only over the shared index — the resident daemon owns DB
//! writes (ADR-0013 Stage 3), so memory archive/restore stays deferred until
//! the views get a mutation path owned by that daemon.

use chrono::Datelike;
use std::rc::Rc;

use adabraka_ui::display::markdown::Markdown;
use gpui::{
    px, relative, App, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled,
};

use crate::data::{self, MemoryEntry, OverviewStats, UsageStats};

/// The right-panel view selection (sidebar navigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppView {
    Sessions,
    Memory,
    Activity,
    Recap,
    Settings,
}

impl AppView {}

/// Settings page (Vue Settings parity, Stage-2 slice): the editor scheme for
/// file references is editable (plain settings.json write, no DB writes);
/// provider roots are editable — saving writes settings.json and the caller
/// stays with the daemon configuration until Stage 3.
#[derive(IntoElement)]
pub struct SettingsView {
    pub editor_scheme: String,
    pub on_scheme: EditorSchemeFn,
    /// Editable data-source rows (one per builtin provider).
    pub roots: Vec<ProviderRootRow>,
    /// Save a custom root for one provider (writes settings.json, then the
    /// caller rebuilds the index).
    pub on_save_root: SaveRootFn,
    /// Fired when the user requests a full index rebuild.
    pub on_rebuild: RebuildFn,
    /// Transient save status line.
    pub status: Option<String>,
    /// SQLite index location (Settings #7: Index location + Reveal).
    pub index_path: String,
    /// Fired when "Reveal" is clicked (opens the file manager).
    pub on_reveal: RebuildFn,
    /// Current recap directory (Settings #10).
    pub recap_dir: String,
    /// Input holding a new recap directory (empty = default).
    pub recap_input: gpui::Entity<adabraka_ui::components::input_state::InputState>,
    /// Save the recap directory (Settings #10).
    pub on_save_recap_dir: SettingsSaveRecapDirFn,
    /// Open the platform directory picker ("recap" or a provider id).
    pub on_browse: SettingsBrowseFn,
}

/// Save the recap directory (Settings #10).
pub type SettingsSaveRecapDirFn = Rc<dyn Fn(&str, &mut gpui::Window, &mut App) + 'static>;
/// Open the platform directory picker (Settings #4).
pub type SettingsBrowseFn = Rc<dyn Fn(&str, &mut gpui::Window, &mut App) + 'static>;

/// Fired when the user clicks "Rebuild index".
pub type RebuildFn = Rc<dyn Fn(&mut gpui::Window, &mut App) + 'static>;

/// One provider row in the data-sources form.
pub struct ProviderRootRow {
    pub id: &'static str,
    pub label: &'static str,
    /// The effective root right now (custom override or builtin default).
    pub current: String,
    pub is_custom: bool,
    /// Whether the root directory exists (status light, parity #2).
    pub ok: bool,
    /// Sessions indexed under this root (parity #2 sessionCount).
    pub session_count: i64,
    /// Text input holding a new custom path (empty = reset to default).
    pub input: gpui::Entity<adabraka_ui::components::input_state::InputState>,
}

/// Fired with (provider id, custom root path) on Save.
pub type SaveRootFn = Rc<dyn Fn(&str, &str, &mut gpui::Window, &mut App) + 'static>;

/// Fired with the newly chosen editor scheme id.
pub type EditorSchemeFn = Rc<dyn Fn(&str, &mut gpui::Window, &mut App) + 'static>;

const EDITOR_SCHEMES: [(&str, &str); 5] = [
    ("vscode", "VS Code"),
    ("vscode-insiders", "VS Code Insiders"),
    ("cursor", "Cursor"),
    ("windsurf", "Windsurf"),
    ("zed", "Zed"),
];

impl RenderOnce for SettingsView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        use adabraka_ui::components::input::Input;
        let mut roots = gpui::div().flex().flex_col();
        for row in &self.roots {
            let on_save = self.on_save_root.clone();
            let id = row.id;
            let input = row.input.clone();
            roots =
                roots.child(
                    gpui::div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .py_2()
                        .border_b_1()
                        .border_color(crate::theme::HAIRLINE)
                        .child(
                            gpui::div()
                                .flex()
                                .items_baseline()
                                .gap_2()
                                .child(
                                    // Status light (parity #2): green when the
                                    // root directory exists, dim otherwise.
                                    gpui::div().w(px(7.0)).h(px(7.0)).rounded_full().bg(
                                        if row.ok {
                                            gpui::rgb(0x9ece6aff)
                                        } else {
                                            gpui::rgb(0x565f89ff)
                                        },
                                    ),
                                )
                                .child(
                                    gpui::div()
                                        .text_size(crate::theme::TEXT_BASE)
                                        .text_color(crate::theme::FG)
                                        .child(row.label.to_string()),
                                )
                                .child(
                                    gpui::div()
                                        .font_family(crate::theme::MONO)
                                        .text_size(px(11.0))
                                        .text_color(if row.is_custom {
                                            crate::theme::ACCENT_2
                                        } else {
                                            crate::theme::MUTED
                                        })
                                        .text_ellipsis()
                                        .overflow_hidden()
                                        .child(if row.is_custom {
                                            format!("{} (custom)", row.current)
                                        } else {
                                            row.current.clone()
                                        }),
                                )
                                .child(
                                    gpui::div()
                                        .font_family(crate::theme::MONO)
                                        .text_size(px(11.0))
                                        .text_color(crate::theme::MUTED)
                                        .child(format!("{} sessions", row.session_count)),
                                ),
                        )
                        .child(
                            gpui::div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(gpui::div().flex_1().child(
                                    Input::new(&input).placeholder("custom path (empty = default)"),
                                ))
                                .child(
                                    gpui::div()
                                        .id(gpui::SharedString::from(format!("save-root-{id}")))
                                        .px_3()
                                        .py_1p5()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(crate::theme::HAIRLINE_STRONG)
                                        .text_size(crate::theme::TEXT_SM)
                                        .text_color(crate::theme::FG_2)
                                        .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                                        .cursor_pointer()
                                        .child("Save")
                                        .on_click(move |_event, window, cx| {
                                            let path = input.read(cx).content().to_string();
                                            on_save(id, path.trim(), window, cx);
                                        }),
                                )
                                .child({
                                    let on_browse = self.on_browse.clone();
                                    gpui::div()
                                        .id(gpui::SharedString::from(format!("browse-root-{id}")))
                                        .px_3()
                                        .py_1p5()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(crate::theme::HAIRLINE)
                                        .text_size(crate::theme::TEXT_SM)
                                        .text_color(crate::theme::MUTED)
                                        .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                                        .cursor_pointer()
                                        .child("Browse")
                                        .on_click(move |_event, window, cx| {
                                            on_browse(id, window, cx);
                                        })
                                }),
                        ),
                );
        }

        let mut schemes = gpui::div().flex().gap_2().flex_wrap();
        for (id, label) in EDITOR_SCHEMES {
            let is_selected = id == self.editor_scheme;
            let on_scheme = self.on_scheme.clone();
            schemes = schemes.child(
                gpui::div()
                    .id(gpui::SharedString::from(format!("scheme-{id}")))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_selected {
                        gpui::rgb(0xbb9af7)
                    } else {
                        gpui::rgb(0x2a2a2e)
                    })
                    .text_size(px(12.0))
                    .text_color(if is_selected {
                        gpui::rgb(0xe8e8ee)
                    } else {
                        gpui::rgb(0xc8c8d0)
                    })
                    .bg(if is_selected {
                        gpui::rgb(0x242432)
                    } else {
                        gpui::rgb(0x18181d)
                    })
                    .hover(|s| s.bg(gpui::rgb(0x242429)))
                    .cursor_pointer()
                    .child(label.to_string())
                    .on_click(move |_event, window, cx| {
                        on_scheme(id, window, cx);
                    }),
            );
        }

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x141417))
            .overflow_hidden()
            .child(header_bar("Settings", "obelisk desktop"))
            .child(
                gpui::div()
                    .id("settings-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .p_6()
                    .child(
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_size(px(13.0))
                                    .text_color(gpui::rgb(0xe8e8ee))
                                    .child("Editor scheme"),
                            )
                            .child(
                                gpui::div()
                                    .text_size(px(11.0))
                                    .text_color(gpui::rgb(0x77777f))
                                    .child(
                                        "Used to open file references from the timeline \
                                         (settings.json · editorScheme)",
                                    ),
                            )
                            .child(schemes),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_size(px(13.0))
                                    .text_color(gpui::rgb(0xe8e8ee))
                                    .child("Provider roots"),
                            )
                            .child(
                                gpui::div()
                                    .text_size(px(11.0))
                                    .text_color(crate::theme::MUTED)
                                    .child(
                                        "Where transcripts are discovered and watched. \
                                         Enter an absolute path to point Obelisk at your own \
                                         directory; leave empty and save to reset to default.",
                                    ),
                            )
                            .child(roots)
                            .children(self.status.clone().map(|status| {
                                let color = if status.starts_with("Invalid")
                                    || status.starts_with("Save failed")
                                    || status.starts_with("Rebuild failed")
                                {
                                    crate::theme::DANGER
                                } else {
                                    crate::theme::ACCENT_2
                                };
                                gpui::div()
                                    .text_size(crate::theme::TEXT_SM)
                                    .text_color(color)
                                    .child(status)
                            })),
                    ),
            )
            // Recap directory (Settings #10).
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        gpui::div()
                            .text_size(px(13.0))
                            .text_color(crate::theme::FG)
                            .child("Recap directory"),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(11.0))
                            .text_color(crate::theme::MUTED)
                            .child(
                                "Where weekly recap reports are read from. Empty uses the \
                                 default (~/.obelisk/recap).",
                            ),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(11.0))
                            .font_family(crate::theme::MONO)
                            .text_color(crate::theme::MUTED)
                            .child(self.recap_dir.clone()),
                    )
                    .child(
                        gpui::div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(gpui::div().flex_1().child(
                                Input::new(&self.recap_input)
                                    .placeholder("custom directory (empty = default)"),
                            ))
                            .child({
                                let on_save = self.on_save_recap_dir.clone();
                                let recap_input = self.recap_input.clone();
                                gpui::div()
                                    .id("save-recap-dir")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(crate::theme::HAIRLINE_STRONG)
                                    .text_size(crate::theme::TEXT_SM)
                                    .text_color(crate::theme::FG_2)
                                    .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                                    .cursor_pointer()
                                    .child("Save")
                                    .on_click(move |_event, window, cx| {
                                        let path = recap_input.read(cx).content().to_string();
                                        on_save(path.trim(), window, cx);
                                    })
                            })
                            .child({
                                let on_browse = self.on_browse.clone();
                                gpui::div()
                                    .id("browse-recap-dir")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(crate::theme::HAIRLINE)
                                    .text_size(crate::theme::TEXT_SM)
                                    .text_color(crate::theme::MUTED)
                                    .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                                    .cursor_pointer()
                                    .child("Browse")
                                    .on_click(move |_event, window, cx| {
                                        on_browse("recap", window, cx);
                                    })
                            }),
                    ),
            )
            // About: version + manual rebuild (P0-8).
            .child(
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        gpui::div()
                            .text_size(px(13.0))
                            .text_color(crate::theme::FG)
                            .child("About"),
                    )
                    .child(
                        gpui::div()
                            .font_family(crate::theme::MONO)
                            .text_size(crate::theme::TEXT_SM)
                            .text_color(crate::theme::MUTED)
                            .child(format!("Obelisk {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .child(
                        // Index location + Reveal (Vue Settings #7).
                        gpui::div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .font_family(crate::theme::MONO)
                                    .text_size(crate::theme::TEXT_XS)
                                    .text_color(crate::theme::MUTED)
                                    .overflow_hidden()
                                    .child(self.index_path.clone()),
                            )
                            .child({
                                let on_reveal = self.on_reveal.clone();
                                gpui::div()
                                    .id("reveal-index")
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(crate::theme::HAIRLINE)
                                    .text_size(crate::theme::TEXT_XS)
                                    .font_family(crate::theme::MONO)
                                    .text_color(crate::theme::MUTED)
                                    .cursor_pointer()
                                    .hover(|s| s.opacity(0.8))
                                    .child("Reveal")
                                    .on_click(move |_e, window, cx| on_reveal(window, cx))
                            }),
                    )
                    .child(
                        gpui::div()
                            .id("rebuild-index")
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(crate::theme::ACCENT)
                            .text_size(crate::theme::TEXT_SM)
                            .text_color(crate::theme::ACCENT_2)
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::theme::ACCENT_SOFT))
                            .child("Rebuild index")
                            .on_click({
                                let on_rebuild = self.on_rebuild.clone();
                                move |_e, window, cx| on_rebuild(window, cx)
                            }),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(11.0))
                            .text_color(crate::theme::MUTED)
                            .child(
                                "Rebuilding re-reads every transcript from the configured roots. It does not delete memories or recaps.",
                            ),
                    ),
            )
    }
}

#[derive(IntoElement)]
pub struct MemoryView {
    /// Active vs Archived sub-view (sidebar Memory/Active/Archived rows).
    pub tab: MemoryTab,
    /// Memories already filtered by tab + project.
    pub memories: Rc<Vec<MemoryEntry>>,
    /// Query from the memory search box (path + summary substring).
    pub query: String,
    /// Selected row index for the detail panel.
    pub selected: Option<usize>,
    /// Keyboard cursor (memory id) and checkbox selection (memory ids).
    pub cursor_id: Option<String>,
    pub selection: Rc<std::collections::HashSet<String>>,
    pub sort_desc: bool,
    /// Pending undo (label + expiry), shown as a bottom bar.
    pub undo: Option<(String, std::time::Instant)>,
    pub search: gpui::Entity<adabraka_ui::components::input_state::InputState>,
    pub focus: gpui::FocusHandle,
    pub on_select: MemorySelectFn,
    pub on_toggle_archive: MemoryArchiveFn,
    pub on_undo: MemoryUndoFn,
    pub on_open_session: MemoryOpenSessionFn,
    pub on_sort: MemorySortFn,
    /// Keyboard: move the cursor by `delta` rows.
    pub on_cursor_move: MemoryCursorFn,
    /// Keyboard: open the cursor row's detail (Enter).
    pub on_open_detail: MemoryUndoFn,
    /// Keyboard: toggle the check mark on the cursor row (x).
    pub on_toggle_check: MemoryUndoFn,
    /// Keyboard: archive/restore the selection or cursor row (d/D).
    pub on_archive_key: MemoryUndoFn,
    /// Keyboard: clear selection (Esc).
    pub on_clear: MemoryUndoFn,
    /// Header tab switch (Active/Archived chips).
    pub on_tab: MemoryTabFn,
}

pub type MemoryTabFn = Rc<dyn Fn(MemoryTab, &mut gpui::Window, &mut App) + 'static>;

pub type MemoryCursorFn = Rc<dyn Fn(i32, &mut gpui::Window, &mut App) + 'static>;

/// Memory sub-view selection (sidebar Active/Archived sub-rows).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoryTab {
    Active,
    Archived,
}

impl MemoryTab {
    #[allow(dead_code)] // reserved for future sidebar badges
    pub fn label(self) -> &'static str {
        match self {
            MemoryTab::Active => "Active",
            MemoryTab::Archived => "Archived",
        }
    }
}

pub type MemorySelectFn = Rc<dyn Fn(usize, &mut gpui::Window, &mut App) + 'static>;
pub type MemoryArchiveFn = Rc<dyn Fn(&str, &mut gpui::Window, &mut App) + 'static>;
pub type MemoryUndoFn = Rc<dyn Fn(&mut gpui::Window, &mut App) + 'static>;
pub type MemoryOpenSessionFn = Rc<dyn Fn(&str, &str, &mut gpui::Window, &mut App) + 'static>;
pub type MemorySortFn = Rc<dyn Fn(&mut gpui::Window, &mut App) + 'static>;

gpui::actions!(
    obelisk_app,
    [
        MemoryCursorDown,
        MemoryCursorUp,
        MemoryOpenDetail,
        MemoryOpenConversation,
        MemoryToggleCheck,
        MemoryArchiveSelected,
        MemoryUndoAction,
        MemoryClearSelection
    ]
);

impl RenderOnce for MemoryView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        use adabraka_ui::components::input::Input;

        let on_select = self.on_select.clone();
        // Search filter (path + summary, case-insensitive substring).
        let query = self.query.trim().to_lowercase();
        let mut entries: Vec<(usize, &MemoryEntry)> = self
            .memories
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                if query.is_empty() {
                    return true;
                }
                m.path.to_lowercase().contains(&query) || m.summary.to_lowercase().contains(&query)
            })
            .collect();
        if !self.sort_desc {
            entries.reverse();
        }

        let active_count = self.memories.iter().filter(|m| !m.archived()).count();
        let archived_count = self.memories.len() - active_count;
        let selected_memory = self.selected.and_then(|ix| self.memories.get(ix));

        let on_open_session = self.on_open_session.clone();
        let detail = selected_memory.map(|memory| {
            let content = data::read_memory_file(&memory.path);
            let on_open_session = on_open_session.clone();
            memory_view_detail(memory, content.as_deref(), &on_open_session)
        });

        let mut container = gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(crate::theme::BG_2)
            .overflow_hidden()
            .key_context("MemoryList")
            .track_focus(&self.focus)
            .on_action({
                let on_cursor = self.on_cursor_move.clone();
                move |_: &MemoryCursorDown, window, cx| on_cursor(1, window, cx)
            })
            .on_action({
                let on_cursor = self.on_cursor_move.clone();
                move |_: &MemoryCursorUp, window, cx| on_cursor(-1, window, cx)
            })
            .on_action({
                let on_detail = self.on_open_detail.clone();
                move |_: &MemoryOpenDetail, window, cx| on_detail(window, cx)
            })
            .on_action({
                let on_open_session = self.on_open_session.clone();
                move |_: &MemoryOpenConversation, window, cx| on_open_session("", "", window, cx)
            })
            .on_action({
                let on_check = self.on_toggle_check.clone();
                move |_: &MemoryToggleCheck, window, cx| on_check(window, cx)
            })
            .on_action({
                let on_archive = self.on_archive_key.clone();
                move |_: &MemoryArchiveSelected, window, cx| on_archive(window, cx)
            })
            .on_action({
                let on_undo = self.on_undo.clone();
                move |_: &MemoryUndoAction, window, cx| on_undo(window, cx)
            })
            .on_action({
                let on_clear = self.on_clear.clone();
                move |_: &MemoryClearSelection, window, cx| on_clear(window, cx)
            })
            .child(header_bar(
                "Memory",
                &format!("{active_count} active · {archived_count} archived"),
            ))
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_6()
                    .py_2()
                    .border_b_1()
                    .border_color(crate::theme::HAIRLINE)
                    .child(
                        gpui::div()
                            .flex()
                            .gap_1()
                            .child(memory_tab_chip(
                                "Active",
                                self.tab == MemoryTab::Active,
                                MemoryTab::Active,
                                &self.on_tab,
                            ))
                            .child(memory_tab_chip(
                                "Archived",
                                self.tab == MemoryTab::Archived,
                                MemoryTab::Archived,
                                &self.on_tab,
                            )),
                    )
                    .child(
                        gpui::div()
                            .w(px(200.0))
                            .child(Input::new(&self.search).placeholder("Search memories…")),
                    )
                    .child(
                        gpui::div()
                            .id("memory-sort")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(crate::theme::TEXT_SM)
                            .text_color(crate::theme::FG_2)
                            .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                            .cursor_pointer()
                            .child(if self.sort_desc { "newest" } else { "oldest" })
                            .on_click({
                                let on_sort = self.on_sort.clone();
                                move |_e, window, cx| on_sort(window, cx)
                            }),
                    ),
            );

        if self.memories.is_empty() {
            let message = if self.tab == MemoryTab::Archived {
                "No memories archived here."
            } else if query.is_empty() {
                "No memories captured yet — memories are created via `obelisk --attune`."
            } else {
                "No memories match this search — try a different term."
            };
            container = container.child(empty_state(message));
        } else if entries.is_empty() {
            container = container.child(empty_state(
                "No memories match this search — try a different term.",
            ));
        } else {
            // `.flex_1()` is load-bearing: without it the scroll container
            // collapses inside the flex column and every row's hitbox is
            // clipped away (clicks fall through, keys still work).
            let mut list = gpui::div()
                .id("memory-list")
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .overflow_y_scroll();
            for (ix, memory) in entries {
                let is_selected = self.selected == Some(ix);
                let is_cursor = self.cursor_id.as_deref() == Some(memory.id.as_str());
                let checked = self.selection.contains(&memory.id);
                let on_select = on_select.clone();
                let on_toggle = self.on_toggle_archive.clone();
                let id = memory.id.clone();
                let tab_archived = self.tab == MemoryTab::Archived;
                list = list.child(
                    gpui::div()
                        .id(gpui::SharedString::from(format!("memory-{}", memory.id)))
                        .px_6()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .border_b_1()
                        .border_color(crate::theme::HAIRLINE)
                        .bg(if is_selected {
                            crate::theme::SURFACE_STRONG
                        } else if is_cursor {
                            crate::theme::SURFACE
                        } else {
                            crate::theme::rgba(0x00000000)
                        })
                        .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                        .cursor_pointer()
                        .child(
                            gpui::div()
                                .font_family(crate::theme::MONO)
                                .text_size(crate::theme::TEXT_SM)
                                .text_color(if checked {
                                    crate::theme::ACCENT_2
                                } else {
                                    crate::theme::MUTED_2
                                })
                                .child(if checked { "[x]" } else { "[ ]" }),
                        )
                        .child(
                            gpui::div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .overflow_hidden()
                                .child(
                                    gpui::div()
                                        .text_size(crate::theme::TEXT_BASE)
                                        .text_color(crate::theme::FG)
                                        .text_ellipsis()
                                        .overflow_hidden()
                                        .child(memory_basename(&memory.path)),
                                )
                                .child(
                                    gpui::div()
                                        .text_size(crate::theme::TEXT_SM)
                                        .text_color(crate::theme::MUTED)
                                        .text_ellipsis()
                                        .overflow_hidden()
                                        .child(memory.summary.clone()),
                                ),
                        )
                        .child(
                            gpui::div()
                                .font_family(crate::theme::MONO)
                                .text_size(px(11.0))
                                .text_color(crate::theme::MUTED_2)
                                .child(fmt_list_time(&memory.created_at)),
                        )
                        .child(
                            gpui::div()
                                .id(gpui::SharedString::from(format!("archive-{}", memory.id)))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_size(crate::theme::TEXT_SM)
                                .text_color(if tab_archived {
                                    crate::theme::ACCENT_2
                                } else {
                                    crate::theme::MUTED
                                })
                                .hover(|s| s.bg(crate::theme::SURFACE_HI))
                                .cursor_pointer()
                                .child(if tab_archived { "Restore" } else { "Archive" })
                                .on_click(move |_e, window, cx| {
                                    on_toggle(&id, window, cx);
                                }),
                        )
                        .on_click(move |_e, window, cx| {
                            on_select(ix, window, cx);
                        }),
                );
            }
            container = container.child(list);
        }

        container = container.children(detail);

        // Undo bar (5s window).
        if let Some((label, until)) = &self.undo {
            if std::time::Instant::now() < *until {
                let on_undo = self.on_undo.clone();
                container = container.child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_6()
                        .py_2()
                        .border_t_1()
                        .border_color(crate::theme::HAIRLINE)
                        .bg(crate::theme::ACCENT_SOFT)
                        .child(
                            gpui::div()
                                .flex_1()
                                .text_size(crate::theme::TEXT_SM)
                                .text_color(crate::theme::FG_2)
                                .child(label.clone()),
                        )
                        .child(
                            gpui::div()
                                .id("memory-undo")
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .border_color(crate::theme::ACCENT)
                                .text_size(crate::theme::TEXT_SM)
                                .text_color(crate::theme::ACCENT_2)
                                .cursor_pointer()
                                .hover(|s| s.bg(crate::theme::SURFACE_HI))
                                .child("Undo")
                                .on_click(move |_e, window, cx| on_undo(window, cx)),
                        ),
                );
            }
        }
        container
    }
}

/// Memory file name for the row title (Vue parity: basename).
fn memory_basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Small Active/Archived tab chip in the memory header.
fn memory_tab_chip(
    label: &str,
    active: bool,
    tab: MemoryTab,
    on_tab: &MemoryTabFn,
) -> impl IntoElement + use<> {
    let on_tab = on_tab.clone();
    gpui::div()
        .id(gpui::SharedString::from(format!("memory-tab-{label}")))
        .px_2()
        .py_1()
        .rounded_md()
        .text_size(crate::theme::TEXT_SM)
        .text_color(if active {
            crate::theme::FG
        } else {
            crate::theme::MUTED
        })
        .bg(if active {
            crate::theme::ACCENT_SOFT
        } else {
            crate::theme::rgba(0x00000000)
        })
        .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
        .cursor_pointer()
        .child(label.to_string())
        .on_click(move |_e, window, cx| on_tab(tab, window, cx))
}

fn memory_view_detail(
    memory: &MemoryEntry,
    content: Option<&str>,
    on_open_session: &MemoryOpenSessionFn,
) -> impl IntoElement + use<> {
    let on_open_session = on_open_session.clone();
    let session_id = memory.session_id.clone();
    let focus_uuid = memory.message_start.clone().unwrap_or_default();
    let on_open = on_open_session.clone();
    let message_range = match (&memory.message_start, &memory.message_end) {
        (Some(start), Some(end)) => format!(
            "{}… → {}…",
            start.chars().take(8).collect::<String>(),
            end.chars().take(8).collect::<String>()
        ),
        (Some(start), None) => format!("{}…", start.chars().take(8).collect::<String>()),
        _ => String::new(),
    };
    gpui::div()
        .id("memory-detail")
        .flex()
        .flex_col()
        .gap_2()
        .p_6()
        .border_t_1()
        .border_color(crate::theme::HAIRLINE)
        .bg(crate::theme::BG)
        .child(
            gpui::div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    gpui::div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .text_size(crate::theme::TEXT_SM)
                        .child(
                            gpui::div()
                                .text_color(crate::theme::MUTED)
                                .child(memory.project.clone()),
                        )
                        .child(
                            gpui::div()
                                .font_family(crate::theme::MONO)
                                .text_size(px(11.0))
                                .text_color(crate::theme::FG_2)
                                .text_ellipsis()
                                .overflow_hidden()
                                .child(memory_basename(&memory.path)),
                        )
                        .children(if memory.archived() {
                            Some(
                                gpui::div()
                                    .text_color(crate::theme::MUTED)
                                    .child("(archived)"),
                            )
                        } else {
                            None
                        }),
                )
                .child(
                    gpui::div()
                        .font_family(crate::theme::MONO)
                        .text_size(px(11.0))
                        .text_color(crate::theme::MUTED_2)
                        .child(fmt_relative(&memory.created_at)),
                ),
        )
        .child(
            gpui::div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    gpui::div()
                        .id("memory-open-session")
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(crate::theme::ACCENT)
                        .text_size(crate::theme::TEXT_SM)
                        .text_color(crate::theme::ACCENT_2)
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::ACCENT_SOFT))
                        .child("View conversation →")
                        .on_click(move |_e, window, cx| {
                            on_open(&session_id, &focus_uuid, window, cx);
                        }),
                )
                .children(if message_range.is_empty() {
                    None
                } else {
                    Some(
                        gpui::div()
                            .font_family(crate::theme::MONO)
                            .text_size(px(11.0))
                            .text_color(crate::theme::MUTED)
                            .child(message_range),
                    )
                }),
        )
        .children(if memory.anchors.is_empty() {
            None
        } else {
            let mut anchors = gpui::div().flex().flex_col().gap_1().pt_1().child(
                gpui::div()
                    .text_size(px(11.0))
                    .text_color(crate::theme::MUTED)
                    .child(format!("Anchors ({})", memory.anchors.len())),
            );
            for (ix, anchor) in memory.anchors.iter().enumerate() {
                let label = match anchor.line {
                    Some(line) => format!("{}:{}", anchor.path, line),
                    None => anchor.path.clone(),
                };
                let base = gpui::div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .font_family(crate::theme::MONO)
                    .text_size(px(11.0))
                    .text_color(if anchor.exists {
                        crate::theme::ACCENT_2
                    } else {
                        crate::theme::MUTED_2
                    })
                    .id(gpui::SharedString::from(format!("anchor-{ix}")))
                    .child(if anchor.exists {
                        label
                    } else {
                        format!("{label} (file no longer exists)")
                    });
                let row = if anchor.exists {
                    base.cursor_pointer()
                        .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                        .into_any_element()
                } else {
                    base.into_any_element()
                };
                anchors = anchors.child(row);
            }
            Some(anchors)
        })
        .child(
            if let Some(content) = content.filter(|content| !content.is_empty()) {
                gpui::div()
                    .text_size(px(12.0))
                    .child(Markdown::new(content).base_font_size(px(12.0)))
                    .into_any_element()
            } else {
                gpui::div()
                    .text_size(px(12.0))
                    .text_color(crate::theme::MUTED)
                    .child("(memory file is unavailable)")
                    .into_any_element()
            },
        )
}

/// List-style timestamp (Vue `fmtListTime`): today HH:MM; this year
/// MM/DD HH:MM; older years YYYY/MM/DD HH:MM.
pub fn fmt_list_time(iso: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(time) => {
            let local = time.with_timezone(&chrono::Local);
            let now = chrono::Local::now();
            let today = now.date_naive() == local.date_naive();
            let this_year = now.format("%Y").to_string() == local.format("%Y").to_string();
            if today {
                local.format("%H:%M").to_string()
            } else if this_year {
                local.format("%m/%d %H:%M").to_string()
            } else {
                local.format("%Y/%m/%d %H:%M").to_string()
            }
        }
        Err(_) => iso.to_string(),
    }
}

/// Relative timestamp (Vue `fmtRelative`): "just now", "5m ago", "3d ago",
/// … falling back to the list format past 30 days.
pub fn fmt_relative(iso: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return iso.to_string();
    };
    let delta = chrono::Utc::now().signed_duration_since(time.with_timezone(&chrono::Utc));
    let minutes = delta.num_minutes();
    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 60 * 24 {
        format!("{}h ago", minutes / 60)
    } else if minutes < 60 * 24 * 30 {
        format!("{}d ago", minutes / (60 * 24))
    } else {
        fmt_list_time(iso)
    }
}

/// Activity tab kind (Vue usage-view-tabs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityTab {
    Daily,
    Weekly,
    Cumulative,
}

/// Fired when a heatmap cell is clicked (date key "YYYY-MM-DD").
pub type ActivityDayFn = Rc<dyn Fn(String, &mut gpui::Window, &mut App) + 'static>;
/// Fired when a ledger row opens a session (session id).
pub type ActivityOpenFn = Rc<dyn Fn(String, &mut gpui::Window, &mut App) + 'static>;
/// Fired when the chart tab changes.
pub type ActivityTabFn = Rc<dyn Fn(ActivityTab, &mut gpui::Window, &mut App) + 'static>;
/// Fired when "Show more activity" is clicked.
pub type ActivityMoreFn = Rc<dyn Fn(&mut gpui::Window, &mut App) + 'static>;

/// Vue Activity parity: token stats bar, Daily/Weekly/Cumulative charts,
/// heatmap with day selection, and the session activity ledger.
#[derive(IntoElement)]
pub struct ActivityView {
    pub stats: UsageStats,
    pub overview: OverviewStats,
    pub sessions: Rc<Vec<crate::data::ActivitySession>>,
    pub selected_day: Option<String>,
    pub tab: ActivityTab,
    /// Loaded month blocks (Vue loadedMonths; starts at 1).
    pub loaded_months: usize,
    pub on_select_day: ActivityDayFn,
    pub on_open_session: ActivityOpenFn,
    pub on_tab: ActivityTabFn,
    pub on_more: ActivityMoreFn,
}

impl RenderOnce for ActivityView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        let today = chrono::Local::now().date_naive();
        let heatmap = crate::data::heatmap_grid(&self.stats.daily, today);
        let (current_streak, longest_streak) = crate::data::streaks(&self.stats.daily, today);

        // Stats bar: five equal cells separated by hairlines (Vue usage-stats).
        let stats_bar = gpui::div()
            .flex()
            .rounded(px(8.0))
            .bg(crate::theme::SURFACE)
            .border_1()
            .border_color(crate::theme::HAIRLINE)
            .child(usage_stat(
                &format_tokens(self.stats.total_tokens),
                "Lifetime tokens",
            ))
            .child(usage_stat(
                &self
                    .stats
                    .peak_day
                    .as_ref()
                    .map(|d| format_tokens(d.tokens))
                    .unwrap_or_else(|| "—".into()),
                "Peak tokens",
            ))
            .child(usage_stat(
                &self
                    .stats
                    .longest_turn
                    .as_ref()
                    .map(|t| format_duration(t.turn_duration_ms))
                    .unwrap_or_else(|| "—".into()),
                "Longest task",
            ))
            .child(usage_stat(&format!("{current_streak}d"), "Current streak"))
            .child(usage_stat(&format!("{longest_streak}d"), "Longest streak"));

        // Tabs.
        fn tab(
            label: &str,
            kind: ActivityTab,
            current: ActivityTab,
            on_tab: &ActivityTabFn,
        ) -> impl IntoElement + use<> {
            let is_active = current == kind;
            let on_tab = on_tab.clone();
            let label_text: gpui::SharedString = label.to_string().into();
            let base = gpui::div()
                .id(gpui::SharedString::from(format!("activity-tab-{label}")))
                .px(px(12.0))
                .py(px(5.0))
                .text_size(crate::theme::TEXT_SM)
                .font_family(crate::theme::MONO)
                .text_color(if is_active {
                    crate::theme::FG
                } else {
                    crate::theme::MUTED
                })
                .bg(if is_active {
                    crate::theme::ACCENT_SOFT
                } else {
                    gpui::rgba(0x00000000)
                })
                .border_1()
                .border_color(if is_active {
                    crate::theme::ACCENT_SOFT
                } else {
                    crate::theme::HAIRLINE
                });
            let shaped = match kind {
                ActivityTab::Daily => base.rounded_tl(px(4.0)).rounded_bl(px(4.0)),
                ActivityTab::Cumulative => base.rounded_tr(px(4.0)).rounded_br(px(4.0)),
                ActivityTab::Weekly => base,
            };
            shaped
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                .child(label_text)
                .on_click(move |_e, _window, cx| on_tab(kind, _window, cx))
        }

        let chart = match self.tab {
            ActivityTab::Daily => {
                heatmap_element(&heatmap, self.selected_day.as_deref(), &self.on_select_day)
            }
            ActivityTab::Weekly => weekly_bars_element(&self.stats.daily, today),
            ActivityTab::Cumulative => cumulative_chart_element(&self.stats.daily),
        };

        // Ledger: one day block when a cell is selected, else month blocks.
        let ledger: gpui::AnyElement = if let Some(day) = self.selected_day.as_deref() {
            let block = crate::data::day_ledger(&self.sessions, day);
            gpui::div()
                .flex()
                .flex_col()
                .child(month_heading(
                    &block.header,
                    &block.event_date,
                    block.session_total,
                ))
                .child(ledger_block(&block, &self.on_open_session))
                .into_any_element()
        } else {
            let mut col = gpui::div().flex().flex_col();
            for offset in 0..self.loaded_months {
                let (y, m0) = month_offset(today, offset);
                let block = crate::data::month_ledger(&self.sessions, y, m0);
                col = col
                    .child(month_heading(&block.header, "", block.session_total))
                    .child(ledger_block(&block, &self.on_open_session));
            }
            let on_more = self.on_more.clone();
            col.child(
                gpui::div()
                    .id("activity-show-more")
                    .px(px(12.0))
                    .py(px(8.0))
                    .mx_auto()
                    .border_1()
                    .border_color(crate::theme::HAIRLINE)
                    .rounded(px(6.0))
                    .text_size(crate::theme::TEXT_XS)
                    .font_family(crate::theme::MONO)
                    .text_color(crate::theme::MUTED)
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
                    .child("Show more activity")
                    .on_click(move |_e, window, cx| on_more(window, cx)),
            )
            .into_any_element()
        };

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(crate::theme::BG_2)
            .overflow_hidden()
            .child(header_bar(
                "Activity",
                &format!(
                    "{} sessions · {} memories ({} archived)",
                    self.overview.sessions, self.overview.memories, self.overview.memories_archived
                ),
            ))
            .child(
                gpui::div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(20.0))
                    .p(px(24.0))
                    .child(
                        gpui::div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                gpui::div()
                                    .text_size(px(16.0))
                                    .text_color(crate::theme::FG)
                                    .child("Token activity"),
                            )
                            .child(
                                gpui::div()
                                    .flex()
                                    .child(tab("Daily", ActivityTab::Daily, self.tab, &self.on_tab))
                                    .child(tab(
                                        "Weekly",
                                        ActivityTab::Weekly,
                                        self.tab,
                                        &self.on_tab,
                                    ))
                                    .child(tab(
                                        "Cumulative",
                                        ActivityTab::Cumulative,
                                        self.tab,
                                        &self.on_tab,
                                    )),
                            ),
                    )
                    .child(stats_bar)
                    .child(chart)
                    .child(ledger),
            )
    }
}

/// (year, month0) for `offset` months back from today.
fn month_offset(today: chrono::NaiveDate, offset: usize) -> (i32, u32) {
    let total = today.year() * 12 + today.month0() as i32 - offset as i32;
    (
        ((total / 12), (total % 12) as u32).0,
        (total.rem_euclid(12)) as u32,
    )
}

fn usage_stat(value: &str, label: &str) -> impl IntoElement + use<> {
    gpui::div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.0))
        .py(px(16.0))
        .px(px(12.0))
        .border_r_1()
        .border_color(crate::theme::HAIRLINE)
        .child(
            gpui::div()
                .text_size(px(18.0))
                .text_color(crate::theme::FG)
                .child(value.to_string()),
        )
        .child(
            gpui::div()
                .text_size(px(10.5))
                .font_family(crate::theme::MONO)
                .text_color(crate::theme::MUTED)
                .child(label.to_string()),
        )
}

/// 53-week heatmap grid with month labels and a Less/More legend.
fn heatmap_element(
    heatmap: &crate::data::ActivityHeatmap,
    selected_day: Option<&str>,
    on_select_day: &ActivityDayFn,
) -> gpui::AnyElement {
    if heatmap.cells.is_empty() {
        return empty_state("No usage recorded yet.").into_any_element();
    }
    // Month label row: one slot per column; labels may overflow their slot.
    let mut label_row = gpui::div().flex().gap(px(2.0)).mb(px(4.0));
    let mut labels: std::collections::HashMap<usize, &'static str> =
        std::collections::HashMap::new();
    for (col, label) in &heatmap.month_labels {
        labels.insert(*col, label);
    }
    for col in 0..heatmap.cols {
        let text = labels.get(&col).copied().unwrap_or("");
        label_row = label_row.child(
            gpui::div()
                .w(px(11.0))
                .text_size(px(10.0))
                .font_family(crate::theme::MONO)
                .text_color(crate::theme::MUTED_2)
                .child(text),
        );
    }

    // Grid: 7 rows (Sun..Sat) of per-week columns. Bucket cells per row
    // first (Div is not Clone).
    let mut bucketed: [Vec<&crate::data::ActivityCell>; 7] = Default::default();
    for cell in &heatmap.cells {
        bucketed[cell.row].push(cell);
    }
    let mut grid = gpui::div().flex().flex_col().gap(px(2.0));
    for row in bucketed.iter() {
        let mut row_el = gpui::div().flex().gap(px(2.0));
        for cell in row {
            let color = crate::theme::HEAT_LEVELS[cell.level as usize];
            let is_selected = Some(cell.day.as_str()) == selected_day;
            let day = cell.day.clone();
            let on_select_day = on_select_day.clone();
            let mut el = gpui::div()
                .id(gpui::SharedString::from(format!("heat-{}", cell.day)))
                .w(px(11.0))
                .h(px(11.0))
                .rounded(px(2.0))
                .bg(color)
                .cursor_pointer()
                .hover(|s| s.opacity(0.7))
                .on_click(move |_e, _window, cx| on_select_day(day.clone(), _window, cx));
            if is_selected {
                el = el.border_1().border_color(crate::theme::FG);
            }
            row_el = row_el.child(el);
        }
        grid = grid.child(row_el);
    }

    // Legend.
    let mut legend_cells = gpui::div().flex().gap(px(3.0));
    for color in crate::theme::HEAT_LEVELS {
        legend_cells = legend_cells.child(
            gpui::div()
                .w(px(11.0))
                .h(px(11.0))
                .rounded(px(2.0))
                .bg(color),
        );
    }

    gpui::div()
        .flex()
        .flex_col()
        .child(label_row)
        .child(grid)
        .child(
            gpui::div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .mt(px(12.0))
                .justify_end()
                .child(
                    gpui::div()
                        .text_size(px(10.0))
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::MUTED_2)
                        .child("Less"),
                )
                .child(legend_cells)
                .child(
                    gpui::div()
                        .text_size(px(10.0))
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::MUTED_2)
                        .child("More"),
                ),
        )
        .into_any_element()
}

/// 53 weekly bars (Vue weeklyBars: Monday-aligned, bar 10px / gap 3).
fn weekly_bars_element(
    daily: &[crate::data::UsageDay],
    today: chrono::NaiveDate,
) -> gpui::AnyElement {
    let map: std::collections::HashMap<&str, i64> =
        daily.iter().map(|d| (d.day.as_str(), d.tokens)).collect();
    let start_base = today - chrono::Duration::days(364);
    let js_day = (start_base
        .format("%u")
        .to_string()
        .parse::<u32>()
        .unwrap_or(1))
        % 7;
    let days_until_monday = match js_day {
        0 => 1,
        other => (8 - other) % 8,
    };
    let start = start_base + chrono::Duration::days(days_until_monday as i64);
    let mut weeks: Vec<i64> = Vec::new();
    for w in 0..53 {
        let week_start = start + chrono::Duration::days(w * 7);
        if week_start > today {
            break;
        }
        let mut tokens = 0i64;
        for d in 0..7 {
            let date = week_start + chrono::Duration::days(d);
            if date > today {
                break;
            }
            tokens += map
                .get(date.format("%Y-%m-%d").to_string().as_str())
                .copied()
                .unwrap_or(0);
        }
        weeks.push(tokens);
    }
    let max_val = weeks.iter().copied().max().unwrap_or(1).max(1);
    let mut bars = gpui::div().flex().items_end().gap(px(3.0)).h(px(120.0));
    for tokens in &weeks {
        let height = ((*tokens as f32 / max_val as f32) * 120.0).max(0.5);
        bars = bars.child(
            gpui::div()
                .w(px(10.0))
                .h(px(height))
                .rounded_t(px(2.0))
                .bg(crate::theme::ACCENT)
                .opacity(0.8),
        );
    }
    gpui::div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            gpui::div()
                .text_size(px(12.0))
                .text_color(crate::theme::FG)
                .child("Weekly tokens"),
        )
        .child(bars)
        .into_any_element()
}

/// Cumulative area chart (fc-ui AreaChart over the sorted daily totals).
fn cumulative_chart_element(daily: &[crate::data::UsageDay]) -> gpui::AnyElement {
    let mut sorted: Vec<&crate::data::UsageDay> = daily.iter().collect();
    sorted.sort_by(|a, b| a.day.cmp(&b.day));
    if sorted.is_empty() {
        return empty_state("No data").into_any_element();
    }
    let mut cumulative = 0f64;
    let points: Vec<(f64, f64)> = sorted
        .iter()
        .enumerate()
        .map(|(i, d)| {
            cumulative += d.tokens as f64;
            (i as f64, cumulative)
        })
        .collect();
    let series = adabraka_ui::charts::area_chart::AreaChartSeries::new("Cumulative tokens", points)
        .color(gpui::hsla(0.66, 0.71, 0.62, 1.0));
    adabraka_ui::charts::area_chart::AreaChart::new()
        .add_series(series)
        .into_any_element()
}

/// Month/day heading: title + rule + session count (Vue
/// activity-month-heading).
fn month_heading(header: &str, event_date: &str, total: usize) -> impl IntoElement + use<> {
    gpui::div()
        .flex()
        .items_center()
        .gap(px(16.0))
        .mb(px(22.0))
        .child(
            gpui::div()
                .text_size(crate::theme::TEXT_MD)
                .text_color(crate::theme::FG)
                .child(format!(
                    "{}{}",
                    header,
                    if event_date.is_empty() {
                        String::new()
                    } else {
                        format!(" — {event_date}")
                    }
                )),
        )
        .child(gpui::div().flex_1().h(px(1.0)).bg(crate::theme::HAIRLINE))
        .child(
            gpui::div()
                .text_size(px(10.0))
                .font_family(crate::theme::MONO)
                .text_color(crate::theme::MUTED_2)
                .child(format!(
                    "{total} session{}",
                    if total == 1 { "" } else { "s" }
                )),
        )
}

/// One ledger block: the three classified groups (Vue ActivityLedger).
fn ledger_block(block: &crate::data::LedgerBlock, on_open: &ActivityOpenFn) -> gpui::AnyElement {
    if block.is_empty {
        return gpui::div()
            .py(px(12.0))
            .text_size(px(12.0))
            .text_color(crate::theme::MUTED)
            .child("No sessions.")
            .into_any_element();
    }
    let mut col = gpui::div().flex().flex_col().gap(px(24.0));
    if block.new_workspaces.total > 0 {
        col = col.child(ledger_group(
            "workspace",
            &format!(
                "Created {} new workspace{}",
                block.new_workspaces.total,
                if block.new_workspaces.total == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            &block.new_workspaces,
            true,
            on_open,
        ));
    }
    if block.new_sessions.total > 0 {
        let projects: std::collections::HashSet<&str> = block
            .new_sessions
            .normal
            .iter()
            .chain(block.new_sessions.noise.iter())
            .map(|s| s.project.as_str())
            .collect();
        col = col.child(ledger_group(
            "started",
            &format!(
                "Started {} session{} in {} project{}",
                block.new_sessions.total,
                if block.new_sessions.total == 1 {
                    ""
                } else {
                    "s"
                },
                projects.len(),
                if projects.len() == 1 { "" } else { "s" }
            ),
            &block.new_sessions,
            true,
            on_open,
        ));
    }
    if block.continued.total > 0 {
        col = col.child(ledger_group(
            "continued",
            &format!(
                "Continued {} session{}",
                block.continued.total,
                if block.continued.total == 1 { "" } else { "s" }
            ),
            &block.continued,
            false,
            on_open,
        ));
    }
    col.into_any_element()
}

/// One group: node + heading + rows (normal first, noise folded).
fn ledger_group(
    kind: &str,
    heading: &str,
    split: &crate::data::NoiseSplit,
    show_project: bool,
    on_open: &ActivityOpenFn,
) -> gpui::AnyElement {
    let node_color = match kind {
        "workspace" => gpui::rgba(0xf59e0bff),
        "started" => crate::theme::ACCENT_2,
        _ => crate::theme::MUTED,
    };
    let mut rows = gpui::div().flex().flex_col().gap(px(2.0)).ml(px(38.0));
    for session in &split.normal {
        rows = rows.child(ledger_row(session, show_project, false, on_open));
    }
    if !split.noise.is_empty() {
        rows = rows.child(
            gpui::div()
                .id(gpui::SharedString::from(format!("noise-{kind}")))
                .py(px(4.0))
                .text_size(px(11.0))
                .font_family(crate::theme::MONO)
                .text_color(crate::theme::MUTED)
                .cursor_pointer()
                .hover(|s| s.opacity(0.8))
                .child(format!(
                    "{} hidden, likely test or throwaway runs",
                    split.noise.len()
                ))
                .on_click(move |_e, _window, _cx| {
                    // Noise rows stay visible below (expansion handled by
                    // rendering them alongside; the fold is informational).
                }),
        );
        for session in &split.noise {
            rows = rows.child(ledger_row(session, show_project, true, on_open));
        }
    }

    gpui::div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            gpui::div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    gpui::div()
                        .w(px(26.0))
                        .h(px(26.0))
                        .rounded_full()
                        .bg(gpui::rgba(0xf59e0b1a))
                        .border_1()
                        .border_color(match kind {
                            "workspace" => gpui::rgba(0xf59e0b47),
                            _ => crate::theme::HAIRLINE,
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            gpui::div()
                                .w(px(8.0))
                                .h(px(8.0))
                                .rounded_full()
                                .bg(node_color),
                        ),
                )
                .child(
                    gpui::div()
                        .text_size(crate::theme::TEXT_BASE)
                        .text_color(crate::theme::FG_2)
                        .child(heading.to_string()),
                ),
        )
        .child(rows)
        .into_any_element()
}

/// One session row: title + meta (Vue ActivityLedgerRow).
fn ledger_row(
    session: &crate::data::ActivitySession,
    include_project: bool,
    is_noise: bool,
    on_open: &ActivityOpenFn,
) -> gpui::AnyElement {
    let title = if session.title.is_empty() {
        "(untitled)".to_string()
    } else {
        session.title.clone()
    };
    let mut meta_parts: Vec<String> = Vec::new();
    if include_project && !session.label.is_empty() {
        meta_parts.push(session.label.clone());
    }
    meta_parts.push(format!("{} msg", session.message_count));
    let meta = meta_parts.join(" · ");
    let id = session.id.clone();
    let on_open = on_open.clone();
    gpui::div()
        .id(gpui::SharedString::from(format!("ledger-{}", session.id)))
        .py(px(3.0))
        .pr(px(6.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::SURFACE_STRONG))
        .opacity(if is_noise { 0.7 } else { 1.0 })
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            gpui::div()
                .text_size(crate::theme::TEXT_BASE)
                .text_color(if is_noise {
                    crate::theme::FG_2
                } else {
                    crate::theme::ACCENT_2
                })
                .child(title),
        )
        .child(
            gpui::div()
                .text_size(px(11.0))
                .font_family(crate::theme::MONO)
                .text_color(crate::theme::MUTED)
                .child(meta),
        )
        .on_click(move |_e, _window, cx| on_open(id.clone(), _window, cx))
        .into_any_element()
}

/// Copy one generation command to the clipboard (R3).
pub type RecapCopyCommandFn = Rc<dyn Fn(String, &mut gpui::Window, &mut App) + 'static>;
/// Toggle the Generate panel (R3).
pub type RecapGenerateFn = Rc<dyn Fn(&mut gpui::Window, &mut App) + 'static>;
/// Fired when a recap row is selected (index into filenames).
pub type RecapSelectFn = Rc<dyn Fn(usize, &mut gpui::Window, &mut App) + 'static>;
/// Fired when the visible card changes (0..4).
pub type RecapCardFn = Rc<dyn Fn(usize, &mut gpui::Window, &mut App) + 'static>;

/// Weekly recap: list + five-card detail (Vue RecapList/RecapDetail parity;
/// Cover/Path/Vibe/Workflow/Closing cards with archetype theming).
#[derive(IntoElement)]
pub struct RecapView {
    /// Timeline list entries (R1: year groups, persona, metrics).
    pub entries: Rc<Vec<crate::data::RecapEntry>>,
    /// Kind filter: "all" | "weekly" | "monthly".
    pub kind_filter: String,
    /// Parsed JSON of the selected recap, when any.
    pub selected: Option<serde_json::Value>,
    /// Name of the selected file (for list highlight).
    pub selected_name: Option<String>,
    /// Visible card index (0=Cover … 4=Closing).
    pub card_ix: usize,
    /// Active archetype key (persona.archetype by default; `p` cycles).
    pub archetype: String,
    /// Whether the Generate panel is expanded (R3).
    pub show_generate: bool,
    pub on_select: RecapSelectFn,
    pub on_card: RecapCardFn,
    /// Copy one generation command to the clipboard (R3).
    pub on_copy_command: RecapCopyCommandFn,
    /// Toggle the Generate panel (R3).
    pub on_toggle_generate: RecapGenerateFn,
    /// Kind filter changed (R1): "all" | "weekly" | "monthly".
    pub on_kind_filter: RecapKindFilterFn,
    /// Export the visible card as a PNG (R11).
    pub on_export: RecapExportFn,
}

/// Kind filter changed (R1).
pub type RecapKindFilterFn = Rc<dyn Fn(String, &mut gpui::Window, &mut App) + 'static>;

/// Export one recap card: (card index, copy-to-clipboard instead of file).
pub type RecapExportFn = Rc<dyn Fn(usize, bool, &mut gpui::Window, &mut App) + 'static>;

impl RenderOnce for RecapView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        let palette = crate::theme::archetype(&self.archetype);

        // Kind filter (Vue route query kind; desktop chips).
        let kind_chips = ["all", "weekly", "monthly"];
        let mut chips_row = gpui::div().flex().gap_2().px(px(16.0)).pb(px(8.0));
        for kind in kind_chips {
            let is_active = self.kind_filter == kind;
            let on_kind = self.on_kind_filter.clone();
            chips_row = chips_row.child(
                gpui::div()
                    .id(gpui::SharedString::from(format!("recap-kind-{kind}")))
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(if is_active {
                        crate::theme::ACCENT_SOFT
                    } else {
                        crate::theme::HAIRLINE
                    })
                    .text_size(px(10.0))
                    .font_family(crate::theme::MONO)
                    .text_color(if is_active {
                        crate::theme::FG
                    } else {
                        crate::theme::MUTED
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::SURFACE))
                    .child(kind.to_string())
                    .on_click(move |_e, _window, cx| on_kind(kind.to_string(), _window, cx)),
            );
        }

        // Filtered entries, then year groups (Vue byYear, desc).
        let filtered: Vec<&crate::data::RecapEntry> = self
            .entries
            .iter()
            .filter(|entry| self.kind_filter == "all" || entry.kind == self.kind_filter)
            .collect();
        let count = filtered.len();
        let mut rows = gpui::div().flex().flex_col();
        let mut year_start = 0usize;
        while year_start < filtered.len() {
            let year = filtered[year_start].year.clone();
            let mut year_end = year_start;
            while year_end < filtered.len() && filtered[year_end].year == year {
                year_end += 1;
            }
            let items = &filtered[year_start..year_end];
            rows = rows.child(
                // Year section head.
                gpui::div()
                    .flex()
                    .justify_between()
                    .px(px(16.0))
                    .py(px(8.0))
                    .child(
                        gpui::div()
                            .text_size(px(12.0))
                            .font_family(crate::theme::MONO)
                            .text_color(crate::theme::FG_2)
                            .child(year.clone()),
                    )
                    .child(
                        gpui::div()
                            .text_size(px(10.0))
                            .font_family(crate::theme::MONO)
                            .text_color(crate::theme::MUTED)
                            .child(format!(
                                "{} recap{}",
                                items.len(),
                                if items.len() == 1 { "" } else { "s" }
                            )),
                    ),
            );
            for entry in items {
                let is_selected = self.selected_name.as_deref() == Some(entry.filename.as_str());
                let on_select = self.on_select.clone();
                // Index by filename position in the full list (on_select
                // takes the position in self.entries).
                let full_ix = self
                    .entries
                    .iter()
                    .position(|e| e.filename == entry.filename)
                    .unwrap_or(0);
                let node_color = crate::theme::archetype(&entry.archetype).tc;
                rows = rows.child(
                    gpui::div()
                        .id(gpui::SharedString::from(entry.filename.clone()))
                        .flex()
                        .gap_3()
                        .items_start()
                        .px(px(16.0))
                        .py(px(8.0))
                        .rounded_md()
                        .bg(if is_selected {
                            crate::theme::SURFACE
                        } else {
                            gpui::rgba(0x00000000)
                        })
                        .hover(|s| s.bg(crate::theme::SURFACE))
                        .cursor_pointer()
                        // Timeline node (archetype-colored).
                        .child(
                            gpui::div()
                                .mt(px(4.0))
                                .w(px(10.0))
                                .h(px(10.0))
                                .rounded_full()
                                .bg(node_color),
                        )
                        .child(
                            gpui::div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    // period label · date range.
                                    gpui::div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            gpui::div()
                                                .text_size(px(12.0))
                                                .font_family(crate::theme::MONO)
                                                .text_color(crate::theme::FG_2)
                                                .child(entry.period_label.clone()),
                                        )
                                        .child(
                                            gpui::div()
                                                .text_size(px(11.0))
                                                .font_family(crate::theme::MONO)
                                                .text_color(crate::theme::MUTED)
                                                .child(entry.date_range.clone()),
                                        ),
                                )
                                .child(
                                    gpui::div()
                                        .text_size(crate::theme::TEXT_BASE)
                                        .text_color(if is_selected {
                                            crate::theme::FG
                                        } else {
                                            crate::theme::FG_2
                                        })
                                        .child(entry.persona_title.clone()),
                                )
                                .child(if entry.persona_claim.is_empty() {
                                    gpui::div().into_any_element()
                                } else {
                                    gpui::div()
                                        .text_size(px(11.0))
                                        .text_color(crate::theme::MUTED)
                                        .child(entry.persona_claim.clone())
                                        .into_any_element()
                                })
                                .child(
                                    gpui::div()
                                        .flex()
                                        .gap_2()
                                        .text_size(px(10.5))
                                        .font_family(crate::theme::MONO)
                                        .text_color(crate::theme::MUTED)
                                        .child(
                                            gpui::div()
                                                .child(format!("{} sessions", entry.sessions)),
                                        )
                                        .child(gpui::div().child("·"))
                                        .child(
                                            gpui::div().child(format!("{} tokens", entry.tokens)),
                                        ),
                                ),
                        )
                        .on_click(move |_event, window, cx| {
                            on_select(full_ix, window, cx);
                        }),
                );
            }
            year_start = year_end;
        }

        // Detail: the five-card stack for the selected recap.
        let detail: gpui::AnyElement = match &self.selected {
            None => empty_state("Select a recap to view its cards.").into_any_element(),
            Some(value) => recap_card_stack(value, self.card_ix, palette, &self.on_card),
        };

        let nav = recap_nav(self.card_ix, palette, &self.on_card, &self.on_export);

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(crate::theme::BG_2)
            .overflow_hidden()
            .child(header_bar(
                "Recap",
                &format!("{count} report{}", if count == 1 { "" } else { "s" }),
            ))
            .child(if count == 0 {
                empty_state("No weekly recaps yet — they appear here once generated.")
                    .into_any_element()
            } else {
                gpui::div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(
                        // List rail (Vue list style: fixed left column).
                        gpui::div()
                            .id("recap-list")
                            .w(px(280.0))
                            .flex()
                            .flex_col()
                            .border_r_1()
                            .border_color(crate::theme::HAIRLINE)
                            .overflow_y_scroll()
                            .child(
                                // Generate entry (R3): four commands, each
                                // copyable to the clipboard.
                                gpui::div()
                                    .p(px(16.0))
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child({
                                        let on_toggle = self.on_toggle_generate.clone();
                                        let expanded = self.show_generate;
                                        gpui::div()
                                            .id("recap-generate-toggle")
                                            .text_size(px(10.0))
                                            .font_family(crate::theme::MONO)
                                            .text_color(crate::theme::MUTED)
                                            .cursor_pointer()
                                            .hover(|s| s.opacity(0.8))
                                            .child(if expanded {
                                                "Generate a recap: (hide commands)"
                                            } else {
                                                "Generate a recap: show commands"
                                            })
                                            .on_click(move |_e, _window, cx| {
                                                on_toggle(_window, cx);
                                            })
                                    })
                                    .child(if self.show_generate {
                                        let mut panel = gpui::div().flex().flex_col().gap_1();
                                        for command in [
                                            "/obelisk recap this week",
                                            "/obelisk recap last week",
                                            "/obelisk recap this month",
                                            "/obelisk recap last month",
                                        ] {
                                            let on_copy = self.on_copy_command.clone();
                                            panel = panel.child(
                                                gpui::div()
                                                    .id(gpui::SharedString::from(format!(
                                                        "gen-{}",
                                                        command.replace(" ", "-")
                                                    )))
                                                    .flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .gap_2()
                                                    .px_2()
                                                    .py_1()
                                                    .rounded_sm()
                                                    .border_1()
                                                    .border_color(crate::theme::HAIRLINE)
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(crate::theme::SURFACE))
                                                    .child(
                                                        gpui::div()
                                                            .font_family(crate::theme::MONO)
                                                            .text_size(px(10.5))
                                                            .text_color(crate::theme::FG_2)
                                                            .child(command.to_string()),
                                                    )
                                                    .child(
                                                        gpui::div()
                                                            .text_size(px(10.0))
                                                            .font_family(crate::theme::MONO)
                                                            .text_color(crate::theme::MUTED)
                                                            .child("Copy"),
                                                    )
                                                    .on_click(move |_e, _window, cx| {
                                                        on_copy(command.to_string(), _window, cx);
                                                    }),
                                            );
                                        }
                                        panel.into_any_element()
                                    } else {
                                        gpui::div().into_any_element()
                                    }),
                            )
                            .child(rows),
                    )
                    .child(
                        // Card stage.
                        gpui::div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .bg(gpui::rgba(0x0a0b14ff))
                            .child(gpui::div().flex_1().flex().child(detail))
                            .child(nav),
                    )
                    .into_any_element()
            })
    }
}

/// Card stage: the visible card + prev/next dots nav (Vue card stack).
pub fn recap_card_stack(
    value: &serde_json::Value,
    card_ix: usize,
    palette: &crate::theme::ArchetypePalette,
    _on_card: &RecapCardFn,
) -> gpui::AnyElement {
    let persona = value.get("persona").cloned().unwrap_or_default();
    let cards = value.get("cards").cloned().unwrap_or_default();
    let card = cards.get(card_ix).cloned().unwrap_or_default();
    let eyebrow = match card_ix {
        0 => "badge".to_string(),
        1 => "Your thinking path".to_string(),
        2 => "Your vibe this week".to_string(),
        3 => "Workflows".to_string(),
        _ => "The week, carved.".to_string(),
    };
    // Cover shows badge instead of a number.
    let number = if card_ix == 0 {
        eyebrow_of(&card, "badge").unwrap_or_else(|| "WEEKLY RECAP".into())
    } else {
        format!("{:02} · 05", card_ix + 1)
    };

    let body: gpui::AnyElement = match card_ix {
        0 => cover_card(&card, &persona, palette),
        1 => path_card(&card, palette),
        2 => vibe_card(&card, palette),
        3 => workflow_card(&card, palette),
        _ => closing_card(&card, palette),
    };

    gpui::div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .p(px(24.0))
        .child(
            gpui::div()
                .w(px(540.0))
                .min_h(px(640.0))
                .rounded(px(14.0))
                .bg(crate::theme::SURFACE)
                .border_1()
                .border_color(palette.soft)
                .overflow_hidden()
                .flex()
                .flex_col()
                // Eyebrow: rotated square + text + number.
                .child(
                    gpui::div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(28.0))
                        .pt(px(24.0))
                        .child(
                            gpui::div()
                                .w(px(6.0))
                                .h(px(6.0))
                                .bg(palette.tc)
                                .rounded(px(1.0))
                                .opacity(0.9),
                        )
                        .child(
                            gpui::div()
                                .text_size(px(11.0))
                                .font_family(crate::theme::MONO)
                                .text_color(palette.tc)
                                .child(number),
                        )
                        .child(gpui::div().flex_1())
                        .child(
                            gpui::div()
                                .text_size(px(11.0))
                                .font_family(crate::theme::MONO)
                                .text_color(crate::theme::MUTED)
                                .child(eyebrow),
                        ),
                )
                .child(gpui::div().flex_1().flex().flex_col().child(body)),
        )
        .into_any_element()
}

fn eyebrow_of(card: &serde_json::Value, key: &str) -> Option<String> {
    card.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// Serif display family for recap cards.
const RECAP_SERIF: &str = ".SystemUISerif";

/// Cover card: archetype title, claim, 7-day activity bars, footer.
fn cover_card(
    card: &serde_json::Value,
    persona: &serde_json::Value,
    palette: &crate::theme::ArchetypePalette,
) -> gpui::AnyElement {
    let archetype_name = persona
        .get("archetype")
        .and_then(|v| v.as_str())
        .map(|key| crate::theme::archetype(key).name)
        .unwrap_or(palette.name);
    let title = card
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or(archetype_name);
    let claim = card
        .get("claim")
        .or_else(|| card.get("subtitle"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let footer = card.get("footer").and_then(|v| v.as_str()).unwrap_or("");
    let activity: Vec<f64> = card
        .get("activity")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect())
        .unwrap_or_default();

    // Seven day bars (Mon..Sun), height 32 * val.
    let mut bars = gpui::div().flex().gap(px(10.0)).items_end().h(px(32.0));
    for val in activity.iter().take(7) {
        let height = (*val * 32.0)
            .round()
            .max(if *val > 0.0 { 2.0 } else { 0.0 });
        bars = bars.child(
            gpui::div()
                .w(px(16.0))
                .h(px(height.max(1.0) as f32))
                .rounded_t(px(2.0))
                .bg(palette.tc)
                .opacity(if *val < 0.4 { 0.45 } else { 0.9 }),
        );
    }

    gpui::div()
        .flex()
        .flex_col()
        .flex_1()
        .px(px(28.0))
        .py(px(24.0))
        .gap(px(14.0))
        .child(
            gpui::div()
                .text_size(px(52.0))
                .font_family(RECAP_SERIF)
                .text_color(crate::theme::FG)
                .child(title.to_string()),
        )
        .child(
            gpui::div()
                .text_size(px(17.0))
                .font_family(RECAP_SERIF)
                .text_color(palette.tc2)
                .child(claim.to_string()),
        )
        .child(gpui::div().flex_1())
        .child(bars)
        .child(
            gpui::div()
                .text_size(crate::theme::TEXT_SM)
                .font_family(crate::theme::MONO)
                .text_color(crate::theme::MUTED)
                .child(footer.to_string()),
        )
        .into_any_element()
}

/// Path card: prompt timeline with day nodes and outcome pills.
fn path_card(
    card: &serde_json::Value,
    palette: &crate::theme::ArchetypePalette,
) -> gpui::AnyElement {
    let title = card.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let items: Vec<&serde_json::Value> = card
        .get("items")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().collect())
        .unwrap_or_default();
    let mut col = gpui::div()
        .flex()
        .flex_col()
        .flex_1()
        .px(px(28.0))
        .py(px(24.0))
        .gap(px(14.0))
        .child(
            gpui::div()
                .text_size(px(26.0))
                .font_family(RECAP_SERIF)
                .text_color(crate::theme::FG)
                .child(title.to_string()),
        );
    for item in items {
        let day = item.get("day").and_then(|v| v.as_str()).unwrap_or("");
        let prompt = item.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let turn = item
            .get("turn")
            .or_else(|| item.get("outcome"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        col = col.child(
            gpui::div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .pl(px(14.0))
                .border_l_2()
                .border_color(palette.mid)
                .child(
                    gpui::div()
                        .text_size(px(12.0))
                        .font_family(crate::theme::MONO)
                        .text_color(palette.tc2)
                        .child(day.to_string()),
                )
                .child(
                    gpui::div()
                        .text_size(px(16.0))
                        .font_family(RECAP_SERIF)
                        .text_color(crate::theme::FG)
                        .child(format!("\u{201c}{prompt}\u{201d}")),
                )
                .child(
                    gpui::div()
                        .text_size(px(12.0))
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::FG_2)
                        .pl(px(8.0))
                        .border_l_2()
                        .border_color(palette.tc)
                        .child(turn.to_string()),
                ),
        );
    }
    col.into_any_element()
}

/// Vibe card: voice lines, meter, quote.
fn vibe_card(
    card: &serde_json::Value,
    palette: &crate::theme::ArchetypePalette,
) -> gpui::AnyElement {
    let title = card.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let lines: Vec<&serde_json::Value> = card
        .get("voice_lines")
        .or_else(|| card.get("observations"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().collect())
        .unwrap_or_default();
    let meter = card.get("meter");
    let meter_value = meter.and_then(|m| m.get("value")).and_then(|v| v.as_f64());
    let meter_label = meter
        .and_then(|m| m.get("label"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let quote = card.get("quote");
    let quote_text = quote
        .and_then(|q| q.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let quote_caption = quote
        .and_then(|q| q.get("caption"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut col = gpui::div()
        .flex()
        .flex_col()
        .flex_1()
        .px(px(28.0))
        .py(px(24.0))
        .gap(px(12.0))
        .child(
            gpui::div()
                .text_size(px(26.0))
                .font_family(RECAP_SERIF)
                .text_color(crate::theme::FG)
                .child(title.to_string()),
        );
    for line in lines {
        let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let count = line.get("count").and_then(|v| v.as_i64());
        let label = line.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let time = line.get("time").and_then(|v| v.as_str()).unwrap_or("");
        let mut meta = String::new();
        if let Some(count) = count {
            meta.push_str(&format!("\u{00d7}{count}"));
        }
        if !label.is_empty() {
            if !meta.is_empty() {
                meta.push_str(" \u{00b7} ");
            }
            meta.push_str(label);
        }
        if !time.is_empty() {
            if !meta.is_empty() {
                meta.push_str(" \u{00b7} ");
            }
            meta.push_str(time);
        }
        col = col.child(
            gpui::div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    gpui::div()
                        .text_size(px(17.0))
                        .font_family(RECAP_SERIF)
                        .text_color(crate::theme::FG)
                        .child(format!("\u{201c}{text}\u{201d}")),
                )
                .child(if meta.is_empty() {
                    gpui::div().into_any_element()
                } else {
                    gpui::div()
                        .text_size(px(11.0))
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::MUTED)
                        .child(meta)
                        .into_any_element()
                }),
        );
    }
    if let Some(value) = meter_value {
        col = col.child(
            gpui::div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .mt(px(8.0))
                .child(
                    gpui::div()
                        .text_size(px(11.0))
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::MUTED)
                        .child(meter_label.to_string()),
                )
                .child(
                    gpui::div()
                        .w_full()
                        .h(px(10.0))
                        .rounded(px(5.0))
                        .bg(crate::theme::SURFACE_STRONG)
                        .child(
                            gpui::div()
                                .w(relative(value.clamp(0.0, 1.0) as f32))
                                .h_full()
                                .rounded(px(5.0))
                                .bg(palette.tc),
                        ),
                ),
        );
    }
    if !quote_text.is_empty() {
        col = col.child(
            gpui::div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .mt(px(8.0))
                .child(
                    gpui::div()
                        .text_size(px(20.0))
                        .font_family(RECAP_SERIF)
                        .text_color(crate::theme::FG)
                        .child(quote_text.to_string()),
                )
                .child(
                    gpui::div()
                        .text_size(px(11.0))
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::MUTED)
                        .child(format!("\u{2014} {quote_caption}")),
                ),
        );
    }
    col.into_any_element()
}

/// Workflow card: deck, stats, item list, verdict.
fn workflow_card(
    card: &serde_json::Value,
    palette: &crate::theme::ArchetypePalette,
) -> gpui::AnyElement {
    let title = card.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let deck = card
        .get("deck")
        .or_else(|| card.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stats = card.get("stats").and_then(|v| v.as_str()).unwrap_or("");
    let items: Vec<&serde_json::Value> = card
        .get("items")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().collect())
        .unwrap_or_default();
    let verdict = card.get("verdict").and_then(|v| v.as_str()).unwrap_or("");

    let mut col = gpui::div()
        .flex()
        .flex_col()
        .flex_1()
        .px(px(28.0))
        .py(px(24.0))
        .gap(px(12.0))
        .child(
            gpui::div()
                .text_size(px(26.0))
                .font_family(RECAP_SERIF)
                .text_color(crate::theme::FG)
                .child(title.to_string()),
        );
    if !deck.is_empty() {
        col = col.child(
            gpui::div()
                .text_size(px(15.0))
                .font_family(RECAP_SERIF)
                .text_color(crate::theme::FG_2)
                .child(deck.to_string()),
        );
    }
    if !stats.is_empty() {
        col = col.child(
            gpui::div()
                .text_size(crate::theme::TEXT_SM)
                .font_family(crate::theme::MONO)
                .text_color(palette.tc2)
                .child(stats.to_string()),
        );
    }
    let mut list = gpui::div().flex().flex_col();
    for item in items {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let reaction = item
            .get("reaction")
            .or_else(|| item.get("outcome"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        list = list.child(
            gpui::div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .py(px(8.0))
                .border_b_1()
                .border_color(crate::theme::HAIRLINE)
                .child(
                    gpui::div()
                        .text_size(crate::theme::TEXT_SM)
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::FG)
                        .child(name.to_string()),
                )
                .child(
                    gpui::div()
                        .text_size(px(15.0))
                        .font_family(RECAP_SERIF)
                        .text_color(crate::theme::FG_2)
                        .child(format!("\u{201c}{reaction}\u{201d}")),
                ),
        );
    }
    col = col.child(list);
    if !verdict.is_empty() {
        col = col.child(
            gpui::div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .pl(px(12.0))
                .border_l_2()
                .border_color(palette.tc)
                .mt(px(8.0))
                .child(
                    gpui::div()
                        .text_size(px(10.0))
                        .font_family(crate::theme::MONO)
                        .text_color(crate::theme::MUTED)
                        .child("Verdict —"),
                )
                .child(
                    gpui::div()
                        .text_size(px(20.0))
                        .font_family(RECAP_SERIF)
                        .text_color(crate::theme::FG)
                        .child(verdict.to_string()),
                ),
        );
    }
    col.into_any_element()
}

/// Closing card: big headline, receipt lines, most-said phrase, signoff.
fn closing_card(
    card: &serde_json::Value,
    palette: &crate::theme::ArchetypePalette,
) -> gpui::AnyElement {
    let headline = card.get("headline").and_then(|v| v.as_str()).unwrap_or("");
    let receipts: Vec<String> = card
        .get("receipts")
        .or_else(|| card.get("stats"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let phrase = card
        .get("most_said_phrase")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let signoff = card.get("signoff").and_then(|v| v.as_str()).unwrap_or("");

    let mut col = gpui::div()
        .flex()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .px(px(28.0))
        .py(px(24.0))
        .gap(px(16.0))
        .child(
            gpui::div()
                .text_size(px(56.0))
                .font_family(RECAP_SERIF)
                .text_color(crate::theme::FG)
                .child(headline.to_string()),
        );
    for line in &receipts {
        col = col.child(
            gpui::div()
                .text_size(crate::theme::TEXT_SM)
                .font_family(crate::theme::MONO)
                .text_color(crate::theme::FG_2)
                .child(line.clone()),
        );
    }
    if !phrase.is_empty() {
        col = col.child(
            gpui::div()
                .text_size(px(18.0))
                .font_family(RECAP_SERIF)
                .text_color(palette.tc2)
                .child(format!("\u{201c}{phrase}\u{201d} — most-said phrase")),
        );
    }
    if !signoff.is_empty() {
        col = col.child(
            gpui::div()
                .text_size(px(14.0))
                .font_family(RECAP_SERIF)
                .text_color(crate::theme::MUTED)
                .child(signoff.to_string()),
        );
    }
    col.into_any_element()
}

/// Bottom nav: prev/next arrows + five labeled dots (Vue recap nav).
fn recap_nav(
    card_ix: usize,
    palette: &crate::theme::ArchetypePalette,
    on_card: &RecapCardFn,
    on_export: &RecapExportFn,
) -> impl IntoElement + use<> {
    let labels = ["Cover", "Path", "Vibe", "Workflow", "Closing"];
    let mut dots = gpui::div().flex().items_center().gap(px(14.0));
    for (ix, label) in labels.iter().enumerate() {
        let is_active = ix == card_ix;
        let on_card = on_card.clone();
        dots = dots.child(
            gpui::div()
                .id(gpui::SharedString::from(format!("recap-dot-{ix}")))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.0))
                .cursor_pointer()
                .on_click(move |_e, _window, cx| on_card(ix, _window, cx))
                .child(
                    gpui::div()
                        .w(px(if is_active { 8.0 } else { 6.0 }))
                        .h(px(if is_active { 8.0 } else { 6.0 }))
                        .rounded_full()
                        .bg(if is_active {
                            palette.tc
                        } else {
                            crate::theme::MUTED
                        }),
                )
                .child(
                    gpui::div()
                        .text_size(px(10.0))
                        .font_family(crate::theme::MONO)
                        .text_color(if is_active {
                            palette.tc
                        } else {
                            crate::theme::MUTED
                        })
                        .child(label.to_string()),
                ),
        );
    }

    let prev = {
        let on_card = on_card.clone();
        let disabled = card_ix == 0;
        let base = gpui::div()
            .id("recap-prev")
            .w(px(36.0))
            .h(px(36.0))
            .rounded_full()
            .border_1()
            .border_color(if disabled {
                crate::theme::HAIRLINE
            } else {
                palette.soft
            })
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.0))
            .text_color(if disabled {
                crate::theme::MUTED
            } else {
                palette.tc
            })
            .cursor_pointer()
            .hover(|s| s.bg(palette.soft))
            .child("\u{2190}");
        if disabled {
            base
        } else {
            let on_card = on_card.clone();
            base.on_click(move |_e, _window, cx| on_card(card_ix.saturating_sub(1), _window, cx))
        }
    };
    let next = {
        let disabled = card_ix >= 4;
        let base = gpui::div()
            .id("recap-next")
            .w(px(36.0))
            .h(px(36.0))
            .rounded_full()
            .border_1()
            .border_color(if disabled {
                crate::theme::HAIRLINE
            } else {
                palette.soft
            })
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.0))
            .text_color(if disabled {
                crate::theme::MUTED
            } else {
                palette.tc
            })
            .cursor_pointer()
            .hover(|s| s.bg(palette.soft))
            .child("\u{2192}");
        if disabled {
            base
        } else {
            let on_card = on_card.clone();
            base.on_click(move |_e, _window, cx| on_card((card_ix + 1).min(4), _window, cx))
        }
    };

    gpui::div()
        .h(px(64.0))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(28.0))
        .border_t_1()
        .border_color(crate::theme::HAIRLINE)
        .child(prev)
        .child(dots)
        .child(next)
        .child(gpui::div().w(px(16.0)))
        .child({
            let on_export = on_export.clone();
            gpui::div()
                .id("recap-export-png")
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(palette.soft)
                .text_size(px(10.0))
                .font_family(crate::theme::MONO)
                .text_color(palette.tc)
                .cursor_pointer()
                .hover(|s| s.bg(palette.soft))
                .child("Export PNG")
                .on_click(move |_e, _window, cx| on_export(card_ix, false, _window, cx))
        })
        .child({
            let on_export = on_export.clone();
            gpui::div()
                .id("recap-copy-image")
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(palette.soft)
                .text_size(px(10.0))
                .font_family(crate::theme::MONO)
                .text_color(palette.tc)
                .cursor_pointer()
                .hover(|s| s.bg(palette.soft))
                .child("Copy image")
                .on_click(move |_e, _window, cx| on_export(card_ix, true, _window, cx))
        })
}

// ---- shared bits ----

fn header_bar(title: &str, meta: &str) -> impl IntoElement + use<> {
    gpui::div()
        .px_6()
        .py_4()
        .flex()
        .justify_between()
        .border_b_1()
        .border_color(gpui::rgb(0x2a2a2e))
        .child(
            gpui::div()
                .text_size(px(16.0))
                .text_color(gpui::rgb(0xe8e8ee))
                .child(title.to_string()),
        )
        .child(
            gpui::div()
                .text_size(px(12.0))
                .text_color(gpui::rgb(0x77777f))
                .child(meta.to_string()),
        )
}

fn empty_state(message: &str) -> impl IntoElement + use<> {
    gpui::div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.0))
        .text_color(gpui::rgb(0x77777f))
        .child(message.to_string())
}

fn format_tokens(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_duration(ms: i64) -> String {
    let seconds = ms / 1000;
    if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
