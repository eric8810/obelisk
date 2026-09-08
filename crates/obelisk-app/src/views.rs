// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! M2.4 secondary views: memory browser, usage-activity report, weekly recap
//! cards. All read-only over the shared index — the resident daemon owns DB
//! writes (ADR-0013 Stage 3), so memory archive/restore stays deferred until
//! the views get a mutation path owned by that daemon.

use std::rc::Rc;

use adabraka_ui::display::markdown::Markdown;
use gpui::{
    px, App, InteractiveElement, IntoElement, ParentElement, RenderOnce,
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
}

/// Fired when the user clicks "Rebuild index".
pub type RebuildFn = Rc<dyn Fn(&mut gpui::Window, &mut App) + 'static>;

/// One provider row in the data-sources form.
pub struct ProviderRootRow {
    pub id: &'static str,
    pub label: &'static str,
    /// The effective root right now (custom override or builtin default).
    pub current: String,
    pub is_custom: bool,
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
            roots = roots.child(
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
                            ),
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

/// Activity report: total tokens, per-day usage, peak day, longest turn
/// (Vue Activity parity; the bar chart renders the daily series).
#[derive(IntoElement)]
pub struct ActivityView {
    pub stats: UsageStats,
    pub overview: OverviewStats,
}

impl RenderOnce for ActivityView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        let UsageStats {
            daily,
            total_tokens,
            peak_day,
            longest_turn,
        } = &self.stats;
        let max_tokens = daily.iter().map(|day| day.tokens).max().unwrap_or(0).max(1);

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x141417))
            .overflow_hidden()
            .child(header_bar(
                "Activity",
                &format!(
                    "{} sessions · {} memories ({} archived) · {} tokens",
                    self.overview.sessions,
                    self.overview.memories,
                    self.overview.memories_archived,
                    total_tokens
                ),
            ))
            .child(
                gpui::div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .child(
                        gpui::div()
                            .flex()
                            .gap_3()
                            .child(stat_card("Total tokens", &format_tokens(*total_tokens)))
                            .child(stat_card(
                                "Peak day",
                                &peak_day
                                    .as_ref()
                                    .map(|day| {
                                        format!("{} ({})", day.day, format_tokens(day.tokens))
                                    })
                                    .unwrap_or_else(|| "—".to_string()),
                            ))
                            .child(longest_turn.as_ref().map_or_else(
                                || stat_card("Longest turn", "—"),
                                |turn| {
                                    stat_card_with_note(
                                        "Longest turn",
                                        &format_duration(turn.turn_duration_ms),
                                        Some(format!("{} · {}", turn.timestamp, turn.session_id)),
                                    )
                                },
                            )),
                    )
                    .child(if daily.is_empty() {
                        empty_state("No usage recorded yet.").into_any_element()
                    } else {
                        // Simple daily bars: one bar per day, height scaled to
                        // the peak. (fc-ui chart components land with the
                        // polish pass; the data layer is what M2.4 gates.)
                        let mut bars = gpui::div().flex().gap_1().items_end().h(px(120.0));
                        for day in daily.iter().rev().take(30) {
                            let height = ((day.tokens as f32 / max_tokens as f32) * 110.0).max(2.0);
                            let is_peak = peak_day.as_ref().is_some_and(|peak| peak.day == day.day);
                            bars = bars.child(
                                gpui::div().w(px(14.0)).h(px(height)).rounded_t_sm().bg(
                                    if is_peak {
                                        gpui::rgb(0xbb9af7)
                                    } else {
                                        gpui::rgb(0x3b3b44)
                                    },
                                ),
                            );
                        }
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_size(px(13.0))
                                    .text_color(gpui::rgb(0xdcdce4))
                                    .child("Daily token usage"),
                            )
                            .child(
                                gpui::div()
                                    .text_size(px(11.0))
                                    .text_color(gpui::rgb(0x77777f))
                                    .child(format!(
                                        "last {} days · newest on the left",
                                        daily.len().min(30)
                                    )),
                            )
                            .child(bars)
                            .into_any_element()
                    }),
            )
    }
}

/// Weekly recap cards: list `~/.obelisk/recap/*.json` and render the selected
/// one's content (Vue RecapList/RecapDetail parity, JSON passthrough for now).
#[derive(IntoElement)]
pub struct RecapView {
    pub filenames: Rc<Vec<String>>,
    /// Parsed JSON of the selected recap, when any.
    pub selected: Option<serde_json::Value>,
    /// Name of the selected file (for list highlight).
    pub selected_name: Option<String>,
    /// Fired with the file index when a recap is selected.
    pub on_select: RecapSelectFn,
}

/// Callback fired when a recap row is clicked (index into filenames).
pub type RecapSelectFn = Rc<dyn Fn(usize, &mut gpui::Window, &mut App) + 'static>;

impl RenderOnce for RecapView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        let count = self.filenames.len();
        let detail = self.selected.map(|value| {
            let pretty = serde_json::to_string_pretty(&value).unwrap_or_default();
            let lines: Vec<String> = pretty.split('\n').map(str::to_string).collect();
            code_lines(&lines)
        });

        let mut rows = gpui::div().flex().flex_col();
        for (ix, name) in self.filenames.iter().enumerate() {
            let is_selected = self.selected_name.as_deref() == Some(name.as_str());
            let on_select = self.on_select.clone();
            rows = rows.child(
                gpui::div()
                    .id(gpui::SharedString::from(name.clone()))
                    .px_6()
                    .py_2()
                    .border_b_1()
                    .border_color(gpui::rgb(0x222226))
                    .bg(if is_selected {
                        gpui::rgb(0x1c1c21)
                    } else {
                        gpui::rgb(0x141417)
                    })
                    .hover(|s| s.bg(gpui::rgb(0x1c1c21)))
                    .cursor_pointer()
                    .child(
                        gpui::div()
                            .text_size(px(13.0))
                            .text_color(gpui::rgb(0xdcdce4))
                            .child(name.clone()),
                    )
                    .on_click(move |_event, window, cx| {
                        on_select(ix, window, cx);
                    }),
            );
        }

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x141417))
            .overflow_hidden()
            .child(header_bar(
                "Recap",
                &format!("{count} report{}", if count == 1 { "" } else { "s" }),
            ))
            .child(gpui::div().flex_1().flex().flex_col().child(if count == 0 {
                empty_state("No weekly recaps yet — they appear here once generated.")
                    .into_any_element()
            } else {
                rows.into_any_element()
            }))
            .children(detail)
    }
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

fn stat_card(label: &str, value: &str) -> gpui::AnyElement {
    stat_card_with_note(label, value, None)
}

fn stat_card_with_note(label: &str, value: &str, note: Option<String>) -> gpui::AnyElement {
    gpui::div()
        .px_4()
        .py_3()
        .rounded_md()
        .bg(gpui::rgb(0x18181d))
        .border_1()
        .border_color(gpui::rgb(0x2a2a2e))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            gpui::div()
                .text_size(px(11.0))
                .text_color(gpui::rgb(0x77777f))
                .child(label.to_string()),
        )
        .child(
            gpui::div()
                .text_size(px(15.0))
                .text_color(gpui::rgb(0xe8e8ee))
                .child(value.to_string()),
        )
        .children(note.map(|note| {
            gpui::div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(0x565f89))
                .text_ellipsis()
                .overflow_hidden()
                .child(note)
        }))
        .into_any_element()
}

fn code_lines(lines: &[String]) -> impl IntoElement + use<> {
    let mut body = gpui::div().flex().flex_col().p_6();
    for line in lines {
        body = body.child(
            gpui::div()
                .text_size(px(11.0))
                .line_height(gpui::relative(1.45))
                .text_color(gpui::rgb(0xa9b1d6))
                .child(line.clone()),
        );
    }
    body
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
