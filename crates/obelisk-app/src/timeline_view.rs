// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Timeline view (M2.3): variable-height virtualized message timeline with
//! per-tool presentations, fed by the shared index through obelisk-core.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adabraka_ui::display::markdown::Markdown;
use gpui::{
    px, App, FocusHandle, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled,
};

use crate::timeline::{TimelineItem, TimelineKind};
use crate::tool_render::{render_pretty_tool, ToolPresentation};

/// Pixels per keyboard page step (≈ viewport height for our default window).
const KEY_SCROLL_STEP: f32 = 600.0;

/// Collapsible-card disclosure state (Vue `createSessionDisclosureState`):
/// per-key open/raw flags owned by the app entity so they survive list
/// re-measure and refresh. Keys mirror the Vue scheme: `meta:{uuid}`,
/// `thinking:{uuid}`, `tool:{id}`.
#[derive(Default)]
pub struct TimelineUiState {
    open: RefCell<HashMap<String, bool>>,
    raw: RefCell<HashMap<String, bool>>,
}

impl TimelineUiState {
    pub fn new() -> Self {
        Self::default()
    }

    fn is_open(&self, key: &str) -> bool {
        self.open.borrow().get(key).copied().unwrap_or(false)
    }

    fn toggle_open(&self, key: &str) {
        let mut map = self.open.borrow_mut();
        // `remove` already drops a closed entry's flag; only insert on open.
        if !map.remove(key).unwrap_or(false) {
            map.insert(key.to_string(), true);
        }
    }

    fn is_raw(&self, key: &str) -> bool {
        self.raw.borrow().get(key).copied().unwrap_or(false)
    }

    fn toggle_raw(&self, key: &str) {
        let mut map = self.raw.borrow_mut();
        if !map.remove(key).unwrap_or(false) {
            map.insert(key.to_string(), true);
        }
    }

    /// Drop entries whose message no longer exists after a refresh (Vue
    /// `retainMessages`). Keys are `kind:{message-uuid}` / `tool:{id}`.
    pub fn retain_messages(&self, exists: &dyn Fn(&str) -> bool) {
        self.open
            .borrow_mut()
            .retain(|key, _| key_matches_message(key, exists));
        self.raw
            .borrow_mut()
            .retain(|key, _| key_matches_message(key, exists));
    }
}

fn key_matches_message(key: &str, exists: &dyn Fn(&str) -> bool) -> bool {
    let id = key.split_once(':').map(|(_, id)| id).unwrap_or(key);
    exists(id)
}

/// A disclosure toggle row: a chevron that flips with state + label + optional
/// one-line preview, clicking toggles the card open/closed.
fn disclosure_toggle(
    id: gpui::SharedString,
    ui: Rc<TimelineUiState>,
    key: String,
    label: &str,
    preview: Option<String>,
) -> impl IntoElement + use<> {
    let ui_header = ui.clone();
    let key_header = key.clone();
    let open = ui.is_open(&key);
    gpui::div()
        .id(id)
        .flex()
        .gap_2()
        .items_center()
        .w_full()
        .rounded_md()
        .px_2()
        .py_1()
        .text_size(px(11.0))
        .hover(|s| s.bg(gpui::rgb(0x1c1c21)))
        .cursor_pointer()
        .child(
            gpui::div()
                .text_color(gpui::rgb(0x8f7fe8))
                .child(if open { "▾" } else { "▸" }.to_string()),
        )
        .child(
            gpui::div()
                .text_color(gpui::rgb(0xa9b1d6))
                .child(label.to_string()),
        )
        .children(preview.map(|text| {
            gpui::div()
                .text_color(gpui::rgb(0x565f89))
                .text_ellipsis()
                .overflow_hidden()
                .child(text)
        }))
        .on_click(move |_event, window, _cx| {
            ui_header.toggle_open(&key_header);
            window.refresh();
        })
}

#[derive(IntoElement)]
pub struct TimelineView {
    pub items: Rc<Vec<TimelineItem>>,
    pub title: String,
    pub source: String,
    /// User home: needed to read `~/.obelisk/settings.json` for the editor
    /// scheme when opening file references.
    pub home: Rc<std::path::PathBuf>,
    /// Measured list state (gpui::list): items are laid out by their real
    /// rendered heights, so scrolling reaches the true tail and nothing
    /// overlaps. Owned by the app entity, stable across re-renders.
    pub list_state: gpui::ListState,
    /// Focus target so the timeline receives key events.
    pub focus: FocusHandle,
    /// Disclosure (collapse) flags for the timeline's cards.
    pub ui_state: Rc<TimelineUiState>,
    /// Message uuid receiving a temporary accent border (traceability jumps).
    pub focus_highlight: Option<(String, std::time::Instant)>,
    pub on_back: TimelineBackFn,
    /// Reload the open session from the shared index (follow-tail refresh).
    pub on_refresh: TimelineRefreshFn,
}

/// Callback fired when the user leaves the timeline (back link or Escape).
pub type TimelineBackFn = Rc<dyn Fn(&gpui::ClickEvent, &mut gpui::Window, &mut App) + 'static>;

/// Callback fired when the open session should reload from the shared index.
pub type TimelineRefreshFn = Rc<dyn Fn(&mut gpui::Window, &mut App) + 'static>;

impl RenderOnce for TimelineView {
    fn render(self, _window: &mut gpui::Window, _cx: &mut App) -> impl IntoElement {
        let item_count = self.items.len();
        let title = self.title.clone();
        let source = self.source.clone();
        let on_back = self.on_back.clone();
        let on_back_key = self.on_back.clone();
        let items = self.items.clone();
        let ui_state_items = self.ui_state.clone();
        let home = self.home.clone();
        let list_state = self.list_state.clone();
        let list_state_key = self.list_state.clone();
        let on_refresh = self.on_refresh.clone();
        let focus = self.focus;
        let focus_highlight = self.focus_highlight.clone();

        gpui::div()
            .flex_1()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x141417))
            .child(
                gpui::div()
                    .px_6()
                    .py_4()
                    .flex()
                    .justify_between()
                    .border_b_1()
                    .border_color(gpui::rgb(0x2a2a2e))
                    .child(
                        gpui::div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                gpui::div()
                                    .flex()
                                    .gap_3()
                                    .child(
                                        gpui::div()
                                            .id("timeline-back")
                                            .text_size(px(13.0))
                                            .text_color(gpui::rgb(0x7aa2f7))
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(gpui::rgb(0xbb9af7)))
                                            .child("← Sessions")
                                            .on_click(move |event, window, cx| {
                                                on_back(event, window, cx);
                                            }),
                                    )
                                    .child(
                                        gpui::div()
                                            .text_size(px(16.0))
                                            .text_color(gpui::rgb(0xe8e8ee))
                                            .child(title),
                                    ),
                            )
                            .child(
                                gpui::div()
                                    .flex()
                                    .gap_2()
                                    .text_size(px(12.0))
                                    .text_color(gpui::rgb(0x77777f))
                                    .child(gpui::div().child(format!("● {source}")))
                                    .child(
                                        gpui::div().child(format!("{item_count} timeline items")),
                                    ),
                            ),
                    ),
            )
            .child(
                // The vlist owns its internal scroll container; the wrapper
                // only clips it to the panel and carries keyboard scrolling
                // (PageUp/PageDown/Home/End/Escape). GPUI's X11 backend
                // Measured list (gpui::list): the wrapper clips it and carries
                // keyboard scrolling (PageUp/PageDown/Home/End/Escape).
                // GPUI's X11 backend ignores emulated wheel buttons, so the
                // keyboard path is the automatable one.
                gpui::div()
                    .id("timeline-scroll")
                    .track_focus(&focus)
                    .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                        let key = event.keystroke.key.as_str();
                        match key {
                            "pagedown" | "down" | "space" => {
                                list_state_key.scroll_by(px(KEY_SCROLL_STEP));
                                window.refresh();
                            }
                            "pageup" | "up" => {
                                list_state_key.scroll_by(-px(KEY_SCROLL_STEP));
                                window.refresh();
                            }
                            "home" => {
                                list_state_key.scroll_to(gpui::ListOffset {
                                    item_ix: 0,
                                    offset_in_item: px(0.0),
                                });
                                window.refresh();
                            }
                            "end" => {
                                // Jump to the tail and enter follow-tail mode:
                                // subsequent refreshes stay pinned to the end
                                // until the user scrolls up (scroll_by(-x)
                                // stops following inside gpui::list).
                                list_state_key.scroll_to_end();
                                list_state_key.set_follow_tail(true);
                                window.refresh();
                            }
                            "escape" => {
                                on_back_key(&gpui::ClickEvent::default(), window, cx);
                            }
                            "r" => {
                                // Follow-tail refresh: reload the session from
                                // the shared index; the app keeps the view
                                // pinned to the bottom if we are at the end.
                                on_refresh(window, cx);
                            }
                            _ => {
                                let _ = item_count;
                            }
                        }
                    })
                    .child(
                        gpui::list(list_state, move |ix, _window, _cx| {
                            items.get(ix).map_or_else(
                                || gpui::div().into_any_element(),
                                |item| {
                                    timeline_item_view(
                                        item,
                                        &ui_state_items,
                                        &home,
                                        &focus_highlight,
                                    )
                                    .into_any_element()
                                },
                            )
                        })
                        .flex_1(),
                    ),
            )
    }
}

fn timeline_item_view(
    item: &TimelineItem,
    ui: &Rc<TimelineUiState>,
    home: &Rc<std::path::PathBuf>,
    focus_highlight: &Option<(String, std::time::Instant)>,
) -> impl IntoElement + use<> {
    let highlighted = focus_highlight.as_ref().is_some_and(|(uuid, until)| {
        item.message_uuid == *uuid && std::time::Instant::now() < *until
    });
    let message = &item.message;
    let kind_label = match item.kind {
        TimelineKind::Meta => "meta",
        TimelineKind::Workflow => "workflow",
        TimelineKind::WorkflowTools => "workflow-tools",
        TimelineKind::Skill => "skill",
        TimelineKind::Thinking => "thinking",
        TimelineKind::Message => "",
    };
    let role = message
        .role
        .clone()
        .unwrap_or_else(|| message.r#type.clone());
    let timestamp = message.timestamp.clone().unwrap_or_default();
    let is_user = role == "user";

    let mut column = gpui::div().flex().flex_col().gap_2();

    if !kind_label.is_empty()
        && item.kind != TimelineKind::Meta
        && item.kind != TimelineKind::Thinking
    {
        column = column.child(
            gpui::div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(0x8f7fe8))
                .child(kind_label.to_uppercase()),
        );
    }

    // Collapsible meta ("System") and thinking cards, collapsed by default
    // (Vue parity: meta:{uuid} / thinking:{uuid} disclosures).
    let disclosure_key = match item.kind {
        TimelineKind::Meta => Some(format!("meta:{}", message.uuid)),
        TimelineKind::Thinking => Some(format!("thinking:{}", message.uuid)),
        _ => None,
    };
    if let Some(key) = disclosure_key {
        let label = if item.kind == TimelineKind::Meta {
            "System"
        } else {
            "Thinking"
        };
        // Vue: strip tags, keep 80 chars.
        let preview = message
            .text
            .as_deref()
            .map(strip_html_tags)
            .filter(|text| !text.is_empty())
            .map(|text| text.chars().take(80).collect());
        column = column.child(disclosure_toggle(
            gpui::SharedString::from(format!("toggle-{key}")),
            ui.clone(),
            key.clone(),
            label,
            preview,
        ));
        if ui.is_open(&key) {
            if let Some(text) = message.text.as_deref().filter(|text| !text.is_empty()) {
                column = column.child(
                    gpui::div()
                        .text_size(px(12.0))
                        .text_color(gpui::rgb(0x9a9aa5))
                        .line_height(gpui::relative(1.5))
                        .child(markdown_body(text, px(12.0), &message.cwd, home)),
                );
            }
        }
    } else if let Some(text) = message.text.as_deref() {
        // Message text: markdown for every role (Vue renders all message
        // bodies through marked, incl. images).
        if !text.is_empty() {
            column = column.child(gpui::div().text_size(px(13.0)).child(markdown_body(
                text,
                px(13.0),
                &message.cwd,
                home,
            )));
        }
    }

    // Tool calls with their presentations.
    let tool_calls: &[crate::data::TimelineToolCall] = match item.kind {
        TimelineKind::WorkflowTools => &item.tool_calls,
        _ => &message.tool_calls,
    };
    for call in tool_calls {
        column = column.child(tool_call_view(call, ui));
    }

    // Workflow run card.
    if item.kind == TimelineKind::Workflow {
        if let Some(workflow) = message.workflow.as_ref().or_else(|| {
            message
                .tool_calls
                .iter()
                .find(|c| c.name == "Workflow")
                .and_then(|c| c.workflow.as_ref())
        }) {
            column = column.child(workflow_card(workflow));
        }
    }

    let card = gpui::div()
        .id(gpui::SharedString::from(item.message_uuid.clone()))
        .w_full()
        .px_6()
        .py_3()
        .flex()
        .flex_col()
        .gap_2()
        .border_b_1()
        .border_color(if highlighted {
            crate::theme::ACCENT
        } else {
            gpui::rgb(0x222226)
        });
    let card = if highlighted {
        card.rounded_md().border_1().bg(crate::theme::ACCENT_SOFT)
    } else {
        card
    };
    card.child(
        gpui::div()
            .w_full()
            .flex()
            .justify_between()
            .text_size(px(11.0))
            .text_color(gpui::rgb(0x77777f))
            .child(
                gpui::div()
                    .text_color(if is_user {
                        gpui::rgb(0x7aa2f7)
                    } else {
                        gpui::rgb(0x9ece6a)
                    })
                    .child(role),
            )
            .child(gpui::div().child(timestamp)),
    )
    .child(column)
}

fn tool_call_view(
    call: &crate::data::TimelineToolCall,
    ui: &Rc<TimelineUiState>,
) -> impl IntoElement + use<> {
    let output = call
        .result
        .as_ref()
        .map(|result| result.content.clone())
        .unwrap_or_default();
    let is_error = call.result.as_ref().map(|r| r.is_error).unwrap_or(false);
    let key = format!("tool:{}", call.id);
    let open = ui.is_open(&key);
    let raw = ui.is_raw(&key);

    // Collapsed header row: chevron + tool name + arg preview (+ error chip).
    // Clicking expands the card body (Vue toolcall-toggle parity).
    let ui_header = ui.clone();
    let key_header = key.clone();
    let mut card = gpui::div()
        .id(gpui::SharedString::from(call.id.clone()))
        .rounded_md()
        .bg(gpui::rgb(0x18181d))
        .border_1()
        .border_color(if is_error {
            gpui::rgb(0x54253a)
        } else {
            gpui::rgb(0x2a2a2e)
        })
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            gpui::div()
                .id(gpui::SharedString::from(format!("{}-toggle", call.id)))
                .flex()
                .gap_2()
                .items_center()
                .w_full()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .child(
                    gpui::div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(0x8f7fe8))
                        .child(if open { "▾" } else { "▸" }.to_string()),
                )
                .child(
                    gpui::div()
                        .text_size(px(12.0))
                        .text_color(if is_error {
                            gpui::rgb(0xf7768e)
                        } else {
                            gpui::rgb(0xbb9af7)
                        })
                        .child(format!("⚙ {}", call.name)),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(0x77777f))
                        .text_ellipsis()
                        .overflow_hidden()
                        .child(crate::tool_render::arg_preview(
                            &call.name,
                            &call.input_json,
                        )),
                )
                .children(if is_error {
                    Some(
                        gpui::div()
                            .text_size(px(10.0))
                            .text_color(gpui::rgb(0xf7768e))
                            .child("error".to_string()),
                    )
                } else {
                    None
                })
                .on_click(move |_event, window, _cx| {
                    ui_header.toggle_open(&key_header);
                    window.refresh();
                }),
        );

    if !open {
        return card;
    }

    // Body strip: tool name + the raw-view toggle.
    let ui_raw = ui.clone();
    let key_raw = key.clone();
    card = card.child(
        gpui::div()
            .flex()
            .justify_between()
            .items_center()
            .text_size(px(11.0))
            .child(
                gpui::div()
                    .text_color(gpui::rgb(0x77777f))
                    .child(call.name.clone()),
            )
            .child(
                gpui::div()
                    .id(gpui::SharedString::from(format!("{}-raw", call.id)))
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .text_color(if raw {
                        gpui::rgb(0xbb9af7)
                    } else {
                        gpui::rgb(0x565f89)
                    })
                    .border_1()
                    .border_color(gpui::rgb(0x2a2a2e))
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .child("{ } Raw".to_string())
                    .on_click(move |_event, window, _cx| {
                        ui_raw.toggle_raw(&key_raw);
                        window.refresh();
                    }),
            ),
    );

    if raw {
        // Raw view: verbatim input JSON + output text (Vue toolcall-raw).
        let input_lines: Vec<String> = call.input_json.split('\n').map(str::to_string).collect();
        let output_lines: Vec<String> = if output.is_empty() {
            vec!["(empty)".to_string()]
        } else {
            output.split('\n').map(str::to_string).collect()
        };
        let section = if is_error { "Error" } else { "Output" };
        return card.child(
            gpui::div()
                .flex()
                .flex_col()
                .gap_2()
                .child(code_block("Input", &input_lines, false))
                .child(code_block(section, &output_lines, false)),
        );
    }

    let presentation = render_pretty_tool(&call.name, &call.input_json, &output, is_error);

    let mut body = gpui::div().flex().flex_col().gap_2();
    match &presentation {
        ToolPresentation::FileContents { lines, collapsed } => {
            body = body.child(code_block("File contents", lines, *collapsed));
        }
        ToolPresentation::Write {
            path,
            content,
            output,
            is_error,
        } => {
            body = body.child(
                gpui::div()
                    .flex()
                    .gap_2()
                    .text_size(px(12.0))
                    .text_color(gpui::rgb(0xc0caf5))
                    .child(gpui::div().text_color(gpui::rgb(0x8f7fe8)).child("Writing"))
                    .child(gpui::div().child(path.clone())),
            );
            if let Some(content) = content {
                let lines: Vec<String> = content.split('\n').map(str::to_string).collect();
                body = body.child(code_block("New file", &lines, true));
            }
            body = body.child(chip(output, *is_error));
        }
        ToolPresentation::Edit {
            rows,
            adds,
            dels,
            output,
            is_error,
        } => {
            body = body.child(diff_block(rows, *adds, *dels));
            body = body.child(chip(output, *is_error));
        }
        ToolPresentation::Terminal {
            command,
            output,
            is_error,
        } => {
            body = body.child(
                gpui::div()
                    .id("terminal")
                    .rounded_md()
                    .bg(gpui::rgb(0x101014))
                    .border_1()
                    .border_color(gpui::rgb(0x2a2a2e))
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        gpui::div()
                            .flex()
                            .gap_2()
                            .text_size(px(12.0))
                            .child(
                                gpui::div()
                                    .text_color(gpui::rgb(0x7aa2f7))
                                    .child("$".to_string()),
                            )
                            .child(
                                gpui::div()
                                    .text_color(gpui::rgb(0xdcdce4))
                                    .child(command.clone()),
                            ),
                    )
                    .children(if output.is_empty() {
                        Vec::new()
                    } else {
                        vec![gpui::div()
                            .text_size(px(11.0))
                            .text_color(if *is_error {
                                gpui::rgb(0xf7768e)
                            } else {
                                gpui::rgb(0xa9b1d6)
                            })
                            .line_height(gpui::relative(1.4))
                            .child(output.clone())
                            .into_any_element()]
                    }),
            );
        }
        ToolPresentation::Object { hero, fields } => {
            if let Some(hero) = hero {
                let mut card = gpui::div()
                    .border_l_2()
                    .border_color(gpui::rgb(0x8f7fe8))
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1();
                if let Some(title) = &hero.title {
                    card = card.child(
                        gpui::div()
                            .text_size(px(13.0))
                            .text_color(gpui::rgb(0xe8e8ee))
                            .child(title.clone()),
                    );
                }
                let subtitle: Vec<String> = [hero.id.as_ref(), hero.url.as_ref()]
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect();
                if !subtitle.is_empty() {
                    card = card.child(
                        gpui::div()
                            .text_size(px(11.0))
                            .text_color(gpui::rgb(0x77777f))
                            .child(subtitle.join(" · ")),
                    );
                }
                body = body.child(card);
            }
            body = body.child(field_grid(fields));
        }
        ToolPresentation::Table { columns, rows } => {
            let mut table = gpui::div().flex().flex_col().gap_1();
            let mut head = gpui::div()
                .flex()
                .gap_4()
                .text_size(px(11.0))
                .text_color(gpui::rgb(0x77777f));
            for column in columns {
                head = head.child(gpui::div().w(px(140.0)).child(column.clone()));
            }
            table = table.child(head);
            for row in rows.iter().take(50) {
                let mut line = gpui::div().flex().gap_4().text_size(px(11.0));
                for column in columns {
                    let cell = row
                        .get(column)
                        .map(|value| match value {
                            serde_json::Value::String(s) => {
                                let truncated: String = s.chars().take(60).collect();
                                truncated
                            }
                            other => other.to_string(),
                        })
                        .unwrap_or_else(|| "—".to_string());
                    line = line.child(gpui::div().w(px(140.0)).child(cell));
                }
                table = table.child(line);
            }
            body = body.child(
                gpui::div()
                    .id("result-table")
                    .rounded_md()
                    .bg(gpui::rgb(0x101014))
                    .border_1()
                    .border_color(gpui::rgb(0x2a2a2e))
                    .p_3()
                    .child(
                        gpui::div()
                            .flex()
                            .justify_between()
                            .pb_2()
                            .text_size(px(11.0))
                            .text_color(gpui::rgb(0x77777f))
                            .child(gpui::div().child("Result"))
                            .child(gpui::div().child(format!(
                                "{} items · {} columns",
                                rows.len(),
                                columns.len()
                            ))),
                    )
                    .child(table),
            );
        }
        ToolPresentation::Chip { text, is_error } => {
            body = body.child(chip(text, *is_error));
        }
        ToolPresentation::PlainLines { lines, collapsed } => {
            body = body.child(code_block("Output", lines, *collapsed));
        }
        ToolPresentation::InputFields { fields, output } => {
            body = body.child(field_grid(fields));
            if let Some(output) = output {
                body = body.child(tool_output_view(output));
            }
        }
    }

    card.child(body)
}

fn tool_output_view(presentation: &ToolPresentation) -> impl IntoElement + use<> {
    match presentation {
        ToolPresentation::Chip { text, is_error } => chip(text, *is_error).into_any_element(),
        ToolPresentation::PlainLines { lines, collapsed } => {
            code_block("Output", lines, *collapsed).into_any_element()
        }
        ToolPresentation::Table { columns, rows } => gpui::div()
            .text_size(px(11.0))
            .text_color(gpui::rgb(0x77777f))
            .child(format!(
                "Result: {} rows × {} columns",
                rows.len(),
                columns.len()
            ))
            .into_any_element(),
        other => gpui::div()
            .text_size(px(11.0))
            .text_color(gpui::rgb(0x77777f))
            .child(format!("{other:?}"))
            .into_any_element(),
    }
}

/// Vue parity: `(text || '').replace(/<[^>]+>/g, '').slice(0, 80)` for the
/// meta disclosure preview.
fn strip_html_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Markdown body with the file-reference link handler: local links
/// (`[roadmap.md](/abs/path:162)` and `file:` URLs) resolve against the
/// message cwd and open in the configured editor (Vue
/// `installFileReferenceHandler` parity); anything else is ignored.
fn markdown_body(
    text: &str,
    size: gpui::Pixels,
    cwd: &Option<String>,
    home: &Rc<std::path::PathBuf>,
) -> Markdown {
    let cwd = cwd.clone();
    let home = home.clone();
    Markdown::new(text)
        .base_font_size(size)
        .on_link_click(move |href: &str, _window, _cx| {
            let cwd = cwd.as_deref().map(std::path::Path::new);
            crate::file_reference::open_markdown_link(href, cwd, &home);
        })
}

fn code_block(label: &str, lines: &[String], collapsed: bool) -> impl IntoElement + use<> {
    let visible: Vec<&String> = if collapsed {
        lines.iter().take(12).collect()
    } else {
        lines.iter().collect()
    };
    let total = lines.len();
    let mut body = gpui::div().flex().flex_col();
    for (index, line) in visible.iter().enumerate() {
        body = body.child(
            gpui::div()
                .flex()
                .gap_3()
                .text_size(px(11.0))
                .line_height(gpui::relative(1.45))
                .child(
                    gpui::div()
                        .w(px(28.0))
                        .text_color(gpui::rgb(0x565f89))
                        .flex_shrink_0()
                        .child((index + 1).to_string()),
                )
                .child(
                    gpui::div()
                        .text_color(gpui::rgb(0xa9b1d6))
                        .child((**line).to_string()),
                ),
        );
    }
    gpui::div()
        .id("code-block")
        .rounded_md()
        .bg(gpui::rgb(0x101014))
        .border_1()
        .border_color(gpui::rgb(0x2a2a2e))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            gpui::div()
                .flex()
                .justify_between()
                .text_size(px(10.0))
                .text_color(gpui::rgb(0x77777f))
                .child(gpui::div().child(label.to_string()))
                .child(gpui::div().child(format!("{total} lines"))),
        )
        .child(body)
        .children(if collapsed && total > 12 {
            vec![gpui::div()
                .text_size(px(10.0))
                .text_color(gpui::rgb(0x7aa2f7))
                .child(format!("Show all {total} lines"))
                .into_any_element()]
        } else {
            Vec::new()
        })
}

fn diff_block(
    rows: &[crate::tool_render::DiffRow],
    adds: usize,
    dels: usize,
) -> impl IntoElement + use<> {
    let mut body = gpui::div().flex().flex_col();
    for row in rows.iter().take(200) {
        let color = match row.kind {
            crate::tool_render::DiffKind::Context => gpui::rgb(0xa9b1d6),
            crate::tool_render::DiffKind::Add => gpui::rgb(0x9ece6a),
            crate::tool_render::DiffKind::Del => gpui::rgb(0xf7768e),
        };
        let marker = match row.kind {
            crate::tool_render::DiffKind::Context => " ",
            crate::tool_render::DiffKind::Add => "+",
            crate::tool_render::DiffKind::Del => "-",
        };
        body = body.child(
            gpui::div()
                .flex()
                .gap_2()
                .text_size(px(11.0))
                .line_height(gpui::relative(1.45))
                .child(
                    gpui::div()
                        .w(px(84.0))
                        .flex_shrink_0()
                        .text_color(gpui::rgb(0x565f89))
                        .child(format!(
                            "{:>3} {:>3}",
                            row.old_no.map(|n| n.to_string()).unwrap_or_default(),
                            row.new_no.map(|n| n.to_string()).unwrap_or_default(),
                        )),
                )
                .child(
                    gpui::div()
                        .text_color(color)
                        .child(format!("{marker} {}", row.text)),
                ),
        );
    }
    gpui::div()
        .id("diff")
        .rounded_md()
        .bg(gpui::rgb(0x101014))
        .border_1()
        .border_color(gpui::rgb(0x2a2a2e))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            gpui::div()
                .flex()
                .justify_between()
                .text_size(px(10.0))
                .child(gpui::div().text_color(gpui::rgb(0x77777f)).child("Diff"))
                .child(
                    gpui::div()
                        .flex()
                        .gap_2()
                        .child(
                            gpui::div()
                                .text_color(gpui::rgb(0x9ece6a))
                                .child(format!("+{adds}")),
                        )
                        .child(
                            gpui::div()
                                .text_color(gpui::rgb(0xf7768e))
                                .child(format!("−{dels}")),
                        ),
                ),
        )
        .child(body)
}

fn chip(text: &str, is_error: bool) -> impl IntoElement + use<> {
    gpui::div()
        .rounded_md()
        .bg(if is_error {
            gpui::rgb(0x2d1520)
        } else {
            gpui::rgb(0x101014)
        })
        .border_1()
        .border_color(if is_error {
            gpui::rgb(0xf7768e)
        } else {
            gpui::rgb(0x2a2a2e)
        })
        .p_2()
        .text_size(px(11.0))
        .text_color(if is_error {
            gpui::rgb(0xf7768e)
        } else {
            gpui::rgb(0xa9b1d6)
        })
        .child(if text.is_empty() {
            "No output.".to_string()
        } else {
            text.to_string()
        })
}

fn field_grid(fields: &[(String, serde_json::Value)]) -> impl IntoElement + use<> {
    let mut grid = gpui::div().flex().flex_col().gap_1();
    for (key, value) in fields.iter().take(24) {
        let rendered = match value {
            serde_json::Value::Null => "null".to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => {
                let truncated: String = s.chars().take(120).collect();
                if s.chars().count() > 120 {
                    format!("{truncated}…")
                } else {
                    truncated
                }
            }
            serde_json::Value::Array(array) => format!("Array({})", array.len()),
            serde_json::Value::Object(object) => format!("Object({})", object.len()),
        };
        grid = grid.child(
            gpui::div()
                .flex()
                .gap_3()
                .text_size(px(11.0))
                .child(
                    gpui::div()
                        .w(px(120.0))
                        .flex_shrink_0()
                        .text_color(gpui::rgb(0x7aa2f7))
                        .child(key.clone()),
                )
                .child(gpui::div().text_color(gpui::rgb(0xa9b1d6)).child(rendered)),
        );
    }
    grid
}

fn workflow_card(workflow: &serde_json::Value) -> impl IntoElement + use<> {
    let name = workflow
        .get("workflow_name")
        .and_then(|v| v.as_str())
        .unwrap_or("workflow");
    let status = workflow
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let agents = workflow
        .get("agent_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let duration = workflow
        .get("duration_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let tokens = workflow
        .get("total_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    gpui::div()
        .id("workflow-card")
        .rounded_md()
        .bg(gpui::rgb(0x18181d))
        .border_1()
        .border_color(gpui::rgb(0x8f7fe8))
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            gpui::div()
                .flex()
                .justify_between()
                .child(
                    gpui::div()
                        .text_size(px(13.0))
                        .text_color(gpui::rgb(0xbb9af7))
                        .child(format!("⚡ {name}")),
                )
                .child(
                    gpui::div()
                        .text_size(px(11.0))
                        .text_color(gpui::rgb(0x77777f))
                        .child(status.to_string()),
                ),
        )
        .child(
            gpui::div()
                .flex()
                .gap_4()
                .text_size(px(11.0))
                .text_color(gpui::rgb(0x77777f))
                .child(gpui::div().child(format!("{agents} agents")))
                .child(gpui::div().child(if duration > 1000 {
                    format!("{:.1}s", duration as f64 / 1000.0)
                } else {
                    format!("{duration}ms")
                }))
                .child(gpui::div().child(format!("{tokens} tokens"))),
        )
}
