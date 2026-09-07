// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Tool-call presentation model (Rust port of the renderer's
//! tool-renderer.js + session-timeline-presentation.mjs rendering rules,
//! expressed as typed data instead of HTML strings — the GPUI view draws
//! from these).

use serde_json::Value;

/// Terminal-recognized tool names (tool-renderer.js renderTerminalTool).
const TERMINAL_TOOLS: &[&str] = &["exec", "terminal", "shell", "bash", "command", "run"];

fn parse_input(input_json: &str) -> Value {
    serde_json::from_str(input_json).unwrap_or_else(|_| serde_json::json!({}))
}

/// TS getArgPreview: the one-line argument summary shown in tool headers.
pub fn arg_preview(name: &str, input_json: &str) -> String {
    let input = parse_input(input_json);
    let candidate_keys = [
        "file_path",
        "command",
        "path",
        "query",
        "description",
        "pattern",
        "url",
        "name",
        "title",
    ];
    for key in candidate_keys {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return value.chars().take(90).collect();
        }
    }
    if let Some(text) = input.as_str() {
        return text.chars().take(90).collect();
    }
    if input.is_object() {
        if let Some(object) = input.as_object() {
            for (key, value) in object {
                if let Some(text) = value.as_str() {
                    if text.chars().count() < 90 {
                        let _ = key;
                        return text.to_string();
                    }
                }
            }
        }
        let rendered = input.to_string();
        return rendered.chars().take(90).collect();
    }
    let _ = name;
    input_json.chars().take(90).collect()
}

/// A rendered diff line.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    pub kind: DiffKind,
    pub text: String,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Add,
    Del,
}

/// The typed presentation of one tool call (replaces the TS HTML builders).
#[derive(Debug, Clone)]
pub enum ToolPresentation {
    /// Read output: gutter-numbered file contents, collapsed past 12 lines.
    FileContents { lines: Vec<String>, collapsed: bool },
    /// Write: the target path plus the new file's content.
    Write {
        path: String,
        content: Option<String>,
        output: String,
        is_error: bool,
    },
    /// Edit: old→new diff plus the result chip.
    Edit {
        rows: Vec<DiffRow>,
        adds: usize,
        dels: usize,
        output: String,
        is_error: bool,
    },
    /// Terminal command + output.
    Terminal {
        command: String,
        output: String,
        is_error: bool,
    },
    /// Structured object output: optional hero (title/url/id) + field grid.
    Object {
        hero: Option<ToolHero>,
        fields: Vec<(String, Value)>,
    },
    /// Row-array output rendered as a table.
    Table {
        columns: Vec<String>,
        rows: Vec<Value>,
    },
    /// Plain text output chip.
    Chip { text: String, is_error: bool },
    /// Multiline non-JSON output: numbered lines, collapsed past 10.
    PlainLines { lines: Vec<String>, collapsed: bool },
    /// Generic: input field grid (+ optional output section).
    InputFields {
        fields: Vec<(String, Value)>,
        output: Option<Box<ToolPresentation>>,
    },
}

#[derive(Debug, Clone)]
pub struct ToolHero {
    pub title: Option<String>,
    pub url: Option<String>,
    pub id: Option<String>,
}

fn is_terminal_tool(name: &str) -> bool {
    TERMINAL_TOOLS.contains(&name.to_lowercase().as_str())
}

fn diff_rows(old: &str, new: &str) -> (Vec<DiffRow>, usize, usize) {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut rows = Vec::new();
    for (index, line) in old_lines.iter().enumerate().take(prefix) {
        rows.push(DiffRow {
            kind: DiffKind::Context,
            text: line.to_string(),
            old_no: Some(index + 1),
            new_no: Some(index + 1),
        });
    }
    let mut adds = 0;
    let mut dels = 0;
    for (index, line) in old_lines
        .iter()
        .enumerate()
        .take(old_lines.len() - suffix)
        .skip(prefix)
    {
        rows.push(DiffRow {
            kind: DiffKind::Del,
            text: line.to_string(),
            old_no: Some(index + 1),
            new_no: None,
        });
        dels += 1;
    }
    for (index, line) in new_lines
        .iter()
        .enumerate()
        .take(new_lines.len() - suffix)
        .skip(prefix)
    {
        rows.push(DiffRow {
            kind: DiffKind::Add,
            text: line.to_string(),
            old_no: None,
            new_no: Some(index + 1),
        });
        adds += 1;
    }
    for index in 0..suffix {
        rows.push(DiffRow {
            kind: DiffKind::Context,
            text: old_lines[old_lines.len() - suffix + index].to_string(),
            old_no: Some(old_lines.len() - suffix + index + 1),
            new_no: Some(new_lines.len() - suffix + index + 1),
        });
    }
    (rows, adds, dels)
}

fn render_output(output: &str, is_error: bool) -> ToolPresentation {
    if output.is_empty() {
        return ToolPresentation::Chip {
            text: String::new(),
            is_error,
        };
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(output) {
        if parsed.is_array() {
            let rows = parsed.as_array().cloned().unwrap_or_default();
            if !rows.is_empty() && rows.iter().all(|item| item.is_object()) {
                let mut columns: Vec<String> = Vec::new();
                for row in rows.iter().take(5) {
                    for key in row.as_object().expect("checked").keys() {
                        if !columns.iter().any(|c| c == key) {
                            columns.push(key.clone());
                        }
                    }
                }
                return ToolPresentation::Table { columns, rows };
            }
            let fields = rows
                .iter()
                .enumerate()
                .map(|(index, item)| (index.to_string(), item.clone()))
                .collect();
            return ToolPresentation::Object { hero: None, fields };
        }
        if parsed.is_object() {
            let object = parsed.as_object().expect("checked").clone();
            let pick = |keys: &[&str]| {
                keys.iter()
                    .find_map(|key| object.get(*key).and_then(Value::as_str).map(str::to_string))
            };
            let hero_title = pick(&["title", "name", "summary"]);
            let hero_url = pick(&["url", "permalink", "href", "link"])
                .filter(|value| value.starts_with("http://") || value.starts_with("https://"));
            let hero_id = pick(&["id", "identifier", "uuid", "key"]);
            let hero = if hero_title.is_some() || hero_url.is_some() || hero_id.is_some() {
                Some(ToolHero {
                    title: hero_title,
                    url: hero_url,
                    id: hero_id,
                })
            } else {
                None
            };
            let mut fields: Vec<(String, Value)> = Vec::new();
            for (key, value) in &object {
                let is_hero = matches!(
                    key.as_str(),
                    "title"
                        | "name"
                        | "summary"
                        | "url"
                        | "permalink"
                        | "href"
                        | "link"
                        | "id"
                        | "identifier"
                        | "uuid"
                        | "key"
                ) && hero.is_some();
                if !is_hero {
                    fields.push((key.clone(), value.clone()));
                }
            }
            return ToolPresentation::Object { hero, fields };
        }
    }
    if output.contains('\n') {
        let lines: Vec<String> = output.split('\n').map(str::to_string).collect();
        let collapsed = lines.len() > 10;
        return ToolPresentation::PlainLines { lines, collapsed };
    }
    ToolPresentation::Chip {
        text: output.to_string(),
        is_error,
    }
}

/// TS renderPrettyTool: one presentation per tool call.
pub fn render_pretty_tool(
    name: &str,
    input_json: &str,
    output: &str,
    is_error: bool,
) -> ToolPresentation {
    let input = parse_input(input_json);
    if name == "Read" {
        if output.is_empty() {
            return ToolPresentation::Chip {
                text: String::new(),
                is_error: false,
            };
        }
        let lines: Vec<String> = output.split('\n').map(str::to_string).collect();
        let collapsed = lines.len() > 12;
        return ToolPresentation::FileContents { lines, collapsed };
    }
    if name == "Write" {
        let path = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        return ToolPresentation::Write {
            path,
            content,
            output: output.to_string(),
            is_error,
        };
    }
    if name == "Edit" {
        let old = input
            .get("old_string")
            .and_then(Value::as_str)
            .unwrap_or("");
        let new = input
            .get("new_string")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !old.is_empty() || !new.is_empty() {
            let (rows, adds, dels) = diff_rows(old, new);
            return ToolPresentation::Edit {
                rows,
                adds,
                dels,
                output: output.to_string(),
                is_error,
            };
        }
        return ToolPresentation::Chip {
            text: output.to_string(),
            is_error,
        };
    }
    if is_terminal_tool(name) {
        let command = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return ToolPresentation::Terminal {
            command,
            output: output.to_string(),
            is_error,
        };
    }
    let fields: Vec<(String, Value)> = input
        .as_object()
        .map(|object| object.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let output_section = if output.is_empty() {
        None
    } else {
        Some(Box::new(render_output(output, is_error)))
    };
    ToolPresentation::InputFields {
        fields,
        output: output_section,
    }
}
