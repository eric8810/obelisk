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
    /// Transient save status line.
    pub status: Option<String>,
}

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
                                gpui::div()
                                    .text_size(crate::theme::TEXT_SM)
                                    .text_color(crate::theme::ACCENT_2)
                                    .child(status)
                            })),
                    ),
            )
    }
}

#[derive(IntoElement)]
pub struct MemoryView {
    pub memories: Rc<Vec<MemoryEntry>>,
    /// Index of the selected memory (into `memories`), when any.
    pub selected: Option<usize>,
    /// Fired with the row index when a memory is selected.
    pub on_select: MemorySelectFn,
}

/// Callback fired when a memory row is clicked (index into the memories vec).
pub type MemorySelectFn = Rc<dyn Fn(usize, &mut gpui::Window, &mut App) + 'static>;

impl RenderOnce for MemoryView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        let on_select = self.on_select.clone();
        let active: Vec<(usize, &MemoryEntry)> = self
            .memories
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.archived())
            .collect();
        let archived: Vec<(usize, &MemoryEntry)> = self
            .memories
            .iter()
            .enumerate()
            .filter(|(_, m)| m.archived())
            .collect();
        let selected = self
            .selected
            .and_then(|ix| self.memories.get(ix))
            .map(|memory| {
                let content = data::read_memory_file(&memory.path);
                memory_view_detail(memory, content.as_deref())
            });

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x141417))
            .overflow_hidden()
            .child(header_bar(
                "Memory",
                &format!("{} active · {} archived", active.len(), archived.len()),
            ))
            .child(
                gpui::div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(if self.memories.is_empty() {
                        empty_state("No memories captured yet — run `obelisk --build`.")
                            .into_any_element()
                    } else {
                        let on_select_active = on_select.clone();
                        let on_select_archived = on_select.clone();
                        gpui::div()
                            .flex()
                            .flex_col()
                            .child(section("Active", active, self.selected, on_select_active))
                            .children(if archived.is_empty() {
                                None
                            } else {
                                Some(section(
                                    "Archived",
                                    archived,
                                    self.selected,
                                    on_select_archived,
                                ))
                            })
                            .into_any_element()
                    }),
            )
            .children(selected)
    }
}

fn memory_view_detail(memory: &MemoryEntry, content: Option<&str>) -> impl IntoElement + use<> {
    gpui::div()
        .flex()
        .flex_col()
        .gap_2()
        .p_6()
        .border_t_1()
        .border_color(gpui::rgb(0x2a2a2e))
        .bg(gpui::rgb(0x101014))
        .child(
            gpui::div()
                .flex()
                .justify_between()
                .gap_2()
                .text_size(px(12.0))
                .text_color(gpui::rgb(0x77777f))
                .child(
                    gpui::div()
                        .flex()
                        .gap_2()
                        .child(gpui::div().child(memory.path.clone()))
                        .child(
                            gpui::div()
                                .text_ellipsis()
                                .overflow_hidden()
                                .child(format!("session {}", memory.session_id)),
                        ),
                )
                .child(gpui::div().child(memory.created_at.clone())),
        )
        .child(
            if let Some(content) = content.filter(|content| !content.is_empty()) {
                gpui::div()
                    .text_size(px(12.0))
                    .child(Markdown::new(content).base_font_size(px(12.0)))
                    .into_any_element()
            } else {
                gpui::div()
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(0x77777f))
                    .child("(memory file is unavailable)")
                    .into_any_element()
            },
        )
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

fn section(
    label: &str,
    entries: Vec<(usize, &MemoryEntry)>,
    selected: Option<usize>,
    on_select: MemorySelectFn,
) -> impl IntoElement + use<> {
    let mut list = gpui::div().flex().flex_col();
    list = list.child(
        gpui::div()
            .px_6()
            .pt_4()
            .pb_1()
            .text_size(px(11.0))
            .text_color(gpui::rgb(0x77777f))
            .child(label.to_string()),
    );
    for (ix, memory) in entries {
        let is_selected = selected == Some(ix);
        let on_select = on_select.clone();
        list = list.child(
            gpui::div()
                .id(gpui::SharedString::from(format!("memory-{}", memory.id)))
                .px_6()
                .py_2()
                .flex()
                .justify_between()
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
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            gpui::div()
                                .text_size(px(13.0))
                                .text_color(gpui::rgb(0xdcdce4))
                                .child(memory.summary.clone()),
                        )
                        .child(
                            gpui::div()
                                .flex()
                                .gap_2()
                                .text_size(px(11.0))
                                .text_color(gpui::rgb(0x77777f))
                                .child(gpui::div().child(memory.project.clone()))
                                .child(gpui::div().child(memory.created_at.clone()))
                                .children(memory.archived().then(|| {
                                    gpui::div().text_color(gpui::rgb(0xf7768e)).child(format!(
                                        "archived{}",
                                        memory
                                            .deleted_reason
                                            .as_deref()
                                            .filter(|reason| !reason.is_empty())
                                            .map(|reason| format!(" · {reason}"))
                                            .unwrap_or_default()
                                    ))
                                })),
                        ),
                )
                .on_click(move |_event, window, cx| {
                    on_select(ix, window, cx);
                }),
        );
    }
    list
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
