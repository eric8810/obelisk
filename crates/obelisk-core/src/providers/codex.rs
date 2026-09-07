// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Codex provider adapter (port of packages/core/src/providers/codex.ts).
//!
//! Pure: discovers Codex rollout files and parses one into a record
//! stream. Never touches the Obelisk database. Unlike claude, codex is a
//! FULL-REPARSE adapter: it buffers every line and re-emits every record on
//! each run, because the event_msg ↔ response_item dedup needs whole-file
//! knowledge. Hence the session record uses countMode 'total'.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::parsing::{
    codex_agent_nickname, codex_agent_role, codex_call_id, codex_db_id, codex_event_text,
    codex_is_guardian_thread, codex_line_uuid, codex_message_payload_text, codex_parent_thread_id,
    codex_raw_id, codex_tool_input, codex_tool_output, codex_usage, codex_visible_message_key,
    discover_codex_jsonl_files, extract_message_is_meta, file_signature, is_skill_instructions,
    normalize_observed_cwd, normalize_path, project_slug_from_path,
    read_codex_guardian_thread_info, read_lines, sorted_read_dir, trunc, trunc_json_default,
};
use crate::providers::types::{
    Cursor, DiscoverContext, IndexUnit, InventoryIssue, MessageRecord, MessageVisibility,
    ParseStream, ProviderAdapter, ProviderDescriptor, RawLookup, RawRecord, SessionCountMode,
    StreamItem, ToolCallPresentation, ToolCallRecord, ToolResultRecord, TranscriptRecord,
    WatchTarget, WatchTargetKind,
};

pub const NAME: &str = "codex";
pub const CODEX_CANONICAL_TRANSCRIPT_MARKER: &str = "__codex_canonical_transcript_v3__";

fn codex_transcript_dirs(root_dir: &Path) -> Vec<PathBuf> {
    vec![
        root_dir.join("sessions"),
        root_dir.join("archived_sessions"),
    ]
}

// JS truthiness for `x || y` chains on JSON values.
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n
            .as_f64()
            .map(|f| f.is_finite() && f != 0.0)
            .unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

fn truthy_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

// Port of HIDDEN_CONTEXT_ENVELOPE_RE — the regex crate has no backreference
// (\1), so the open/close tag pairing is checked manually:
// ^\s*<(environment_context|codex_internal_context)\b[^>]*>[\s\S]*<\/\1>\s*$
fn hidden_context_envelope(text: &str) -> bool {
    let trimmed = text.trim();
    for tag in ["environment_context", "codex_internal_context"] {
        let open = format!("<{tag}");
        let Some(rest) = trimmed.strip_prefix(&open) else {
            continue;
        };
        // \b: the char after the tag name must not be a word character.
        if rest
            .chars()
            .next()
            .map(|c| c.is_alphanumeric() || c == '_')
            .unwrap_or(false)
        {
            continue;
        }
        let close = format!("</{tag}>");
        let Some(body) = trimmed
            .strip_prefix(&open)
            .and_then(|rest| rest.strip_suffix(&close))
        else {
            continue;
        };
        // [^>]*>: the opening tag closes at the first '>'.
        if body.find('>').is_some() {
            return true;
        }
    }
    false
}

fn message_visibility(role: &str, text: Option<&str>) -> MessageVisibility {
    if role == "user" && text.map(hidden_context_envelope).unwrap_or(false) {
        MessageVisibility::Hidden
    } else {
        MessageVisibility::Visible
    }
}

// Cursor format `${mtime}:${lines}:${size}:${ctimeMs}:${ino}`. The
// mtime+ctime+size+inode signature (#123) lets a same-mtime tail completion
// or replacement back into discovery. Unlike claude's legacy gate, two-part
// cursors fail closed: codex never shipped a five-part cursor before v3, so
// a legacy cursor can only prove "mtime not older", never "unchanged".
fn cursor_signature_differs(cursor: &str, file_path: &Path) -> bool {
    let Ok((mtime, size, ctime, ino)) = file_signature(file_path) else {
        // TS statSync throws; discovery treats unreadable files as changed
        // so the parse (and its error handling) sees them.
        return true;
    };
    let parts: Vec<&str> = cursor.split(':').collect();
    if parts.len() < 5 {
        return true;
    }
    parts[0].parse::<f64>().map(|v| v != mtime).unwrap_or(true)
        || parts[2].parse::<i64>().map(|v| v != size).unwrap_or(true)
        || parts[3].parse::<f64>().map(|v| v != ctime).unwrap_or(true)
        || parts[4].parse::<u64>().map(|v| v != ino).unwrap_or(true)
}

fn discover_at(root_dir: &Path, ctx: &mut DiscoverContext) -> Vec<IndexUnit> {
    let transcript_dirs = codex_transcript_dirs(root_dir);
    let sessions_dir = transcript_dirs[0].clone();
    if !sessions_dir.exists() && !ctx.indexed_sessions().is_empty() {
        ctx.report_incomplete(InventoryIssue {
            path: sessions_dir.to_string_lossy().into_owned(),
            error: "Source folder is unavailable".to_string(),
        });
    }
    let session_index_path = PathBuf::from(normalize_path(
        &root_dir.join("session_index.jsonl").to_string_lossy(),
    ));
    let mut session_index: HashMap<String, (String, Option<String>)> = HashMap::new();
    if session_index_path.exists() {
        let _ = read_lines(&session_index_path, |line, _terminated| {
            if let Ok(item) = serde_json::from_str::<Value>(line) {
                let id = item.get("id");
                let thread_name = item.get("thread_name");
                if js_truthy(id) && js_truthy(thread_name) {
                    if let (Some(id), Some(title)) = (
                        codex_raw_id(id.unwrap_or(&Value::Null)),
                        truthy_str(thread_name),
                    ) {
                        session_index.insert(
                            id,
                            (
                                title.to_string(),
                                truthy_str(item.get("updated_at")).map(str::to_string),
                            ),
                        );
                    }
                }
            }
            true
        });
    }
    let mut changed_files: HashSet<String> = HashSet::new();
    let mut session_index_changed = false;
    if let Some(changed_paths) = ctx.changed_paths {
        for changed_path in changed_paths {
            let changed_path: &str = changed_path;
            let root_relative = if Path::new(changed_path).is_absolute() {
                normalize_path(changed_path)
            } else {
                normalize_path(&root_dir.join(changed_path).to_string_lossy())
            };
            if root_relative == session_index_path.to_string_lossy() {
                session_index_changed = true;
            }
            for transcript_dir in &transcript_dirs {
                let absolute = if Path::new(changed_path).is_absolute() {
                    normalize_path(changed_path)
                } else {
                    normalize_path(&transcript_dir.join(changed_path).to_string_lossy())
                };
                let absolute_path = PathBuf::from(&absolute);
                let inside_ok = absolute_path
                    .strip_prefix(transcript_dir)
                    .map(|rest| {
                        let rest = rest.to_string_lossy();
                        !rest.is_empty() && !rest.starts_with("..")
                    })
                    .ok()
                    .unwrap_or(false);
                if !inside_ok {
                    continue;
                }
                if absolute.to_lowercase().ends_with(".jsonl") {
                    changed_files.insert(absolute);
                }
            }
        }
    }

    let mut files: Vec<PathBuf> = Vec::new();
    {
        let mut report = |issue: InventoryIssue| ctx.report_incomplete(issue);
        for transcript_dir in &transcript_dirs {
            files.extend(discover_codex_jsonl_files(
                transcript_dir,
                Some(&mut report),
            ));
        }
    }
    let mut units: Vec<IndexUnit> = Vec::new();
    for file in files {
        let file_changed = changed_files.contains(&normalize_path(&file.to_string_lossy()));
        if ctx.changed_paths.is_some() && !session_index_changed && !file_changed {
            continue;
        }
        let key = file.to_string_lossy().into_owned();
        let cursor = (ctx.last_cursor)(&key);
        // Skip unchanged files before paying for guardian detection
        // (#121): guardian status is content-derived, so a file whose
        // cursor signature still matches cannot have changed status.
        if !session_index_changed
            && !file_changed
            && cursor.is_some()
            && !cursor_signature_differs(cursor.as_deref().unwrap(), &file)
        {
            continue;
        }
        let guardian = read_codex_guardian_thread_info(&file);
        let mut meta: Option<Value> = None;
        let _ = read_lines(&file, |line, _terminated| {
            if let Ok(obj) = serde_json::from_str::<Value>(line) {
                if obj.get("type").and_then(Value::as_str) == Some("session_meta")
                    && js_truthy(obj.get("payload").and_then(|p| p.get("id")))
                {
                    meta = obj.get("payload").cloned();
                    return false;
                }
            }
            true
        });
        let raw_id = meta
            .as_ref()
            .and_then(|m| codex_raw_id(m.get("id").unwrap_or(&Value::Null)));
        let parent_id = meta.as_ref().and_then(codex_parent_thread_id);
        let indexed = raw_id.as_deref().and_then(|id| session_index.get(id));
        let session_id = if guardian.is_none() {
            codex_db_id(&json!(parent_id.clone().or_else(|| raw_id.clone()))).unwrap_or_default()
        } else {
            String::new()
        };
        units.push(IndexUnit {
            key,
            session_id,
            project: None,
            is_subagent: false,
            agent_id: None,
            meta: Some(json!({
                "source": "codex",
                "guardian": guardian.is_some(),
                "indexedTitle": indexed.map(|(title, _)| title.clone()),
                "indexedUpdatedAt": indexed.and_then(|(_, updated)| updated.clone()),
            })),
            retract_session_ids: Vec::new(),
        });
    }
    units
}

struct ParseState {
    session_id: String,
    agent_id: Option<String>,
    is_sidechain: bool,
    out: Vec<TranscriptRecord>,
    msg_by_uuid: HashMap<String, usize>,
    started_at: Option<String>,
    ended_at: Option<String>,
    git_branch: Option<String>,
    version: Option<String>,
    title: Option<String>,
    n: i64,
    last_message_uuid: Option<String>,
    last_text_assistant_uuid: Option<String>,
    total_input_tokens: i64,
    total_output_tokens: i64,
    current_cwd: Option<String>,
    current_model: Option<String>,
}

impl ParseState {
    fn update_bounds(&mut self, ts: Option<&str>) {
        let Some(ts) = ts.filter(|ts| !ts.is_empty()) else {
            return;
        };
        if self
            .started_at
            .as_ref()
            .map(|s| ts < s.as_str())
            .unwrap_or(true)
        {
            self.started_at = Some(ts.to_string());
        }
        if self
            .ended_at
            .as_ref()
            .map(|s| ts > s.as_str())
            .unwrap_or(true)
        {
            self.ended_at = Some(ts.to_string());
        }
    }

    fn insert_message(
        &mut self,
        uuid: String,
        r#type: &str,
        role: &str,
        text: Option<&str>,
        content_type: &str,
        timestamp: Option<&str>,
    ) -> String {
        let visibility = message_visibility(role, text);
        let skill_instructions = role == "user" && is_skill_instructions(text);
        let is_meta = visibility == MessageVisibility::Hidden
            || skill_instructions
            || extract_message_is_meta(&json!({}), text);
        self.out.push(TranscriptRecord::Message(MessageRecord {
            uuid: uuid.clone(),
            session_id: self.session_id.clone(),
            r#type: r#type.to_string(),
            parent_uuid: self.last_message_uuid.clone(),
            timestamp: timestamp.filter(|ts| !ts.is_empty()).map(str::to_string),
            role: Some(role.to_string()),
            text: trunc(text),
            content_type: Some(
                if skill_instructions {
                    "skill_instructions"
                } else {
                    content_type
                }
                .to_string(),
            ),
            is_meta,
            visibility,
            model: self.current_model.clone(),
            is_sidechain: self.is_sidechain,
            agent_id: self.agent_id.clone(),
            input_tokens: None,
            output_tokens: None,
            cwd: self.current_cwd.clone(),
            skill: None,
            source: "codex".to_string(),
        }));
        self.msg_by_uuid.insert(uuid.clone(), self.out.len() - 1);
        self.last_message_uuid = Some(uuid.clone());
        if self.agent_id.is_none() && visibility == MessageVisibility::Visible {
            self.n += 1;
        }
        if r#type == "assistant" && content_type == "text" {
            self.last_text_assistant_uuid = Some(uuid.clone());
        }
        self.update_bounds(timestamp);
        uuid
    }
}

pub fn parse(unit: &IndexUnit, _cursor: Cursor) -> Vec<StreamItem> {
    let Ok((mtime, size, ctime, ino)) = file_signature(Path::new(&unit.key)) else {
        // TS statSync throws here; the indexer skips the unit with a warning.
        return vec![StreamItem::Error(format!("unable to stat {}", unit.key))];
    };
    let mut records: Vec<(usize, Value)> = Vec::new();
    let mut line_num = 0usize;
    let read = read_lines(Path::new(&unit.key), |line, _terminated| {
        line_num += 1;
        // Malformed lines are skipped but still counted toward the cursor.
        if let Ok(obj) = serde_json::from_str::<Value>(line) {
            records.push((line_num, obj));
        }
        true
    });
    if read.is_err() {
        // TS readLines throws (openSync); the generator aborts the unit.
        return vec![StreamItem::Error(format!("unable to read {}", unit.key))];
    }
    let out_cursor = format!("{mtime}:{line_num}:{size}:{ctime}:{ino}");

    let Some((_, meta_obj)) = records.iter().find(|(_, obj)| {
        obj.get("type").and_then(Value::as_str) == Some("session_meta")
            && js_truthy(obj.get("payload").and_then(|p| p.get("id")))
    }) else {
        return vec![StreamItem::Cursor(out_cursor)];
    };
    let meta = meta_obj.get("payload").unwrap_or(&Value::Null);
    let Some(thread_raw_id) = codex_raw_id(meta.get("id").unwrap_or(&Value::Null)) else {
        return vec![StreamItem::Cursor(out_cursor)];
    };
    if codex_is_guardian_thread(meta, &records) {
        return vec![
            StreamItem::Record(TranscriptRecord::DeleteSession {
                session_id: codex_db_id(&json!(thread_raw_id)).unwrap_or_default(),
            }),
            StreamItem::Cursor(out_cursor),
        ];
    }

    let parent_raw_id = codex_parent_thread_id(meta);
    let session_id = codex_db_id(&json!(parent_raw_id
        .clone()
        .unwrap_or_else(|| thread_raw_id.clone())))
    .unwrap_or_default();
    let agent_id = parent_raw_id
        .as_ref()
        .and_then(|_| codex_db_id(&json!(thread_raw_id)));
    let is_sidechain = agent_id.is_some();
    let project = project_slug_from_path(
        normalize_observed_cwd(meta.get("cwd").and_then(Value::as_str)).as_deref(),
    );
    let line_uuid = |n: usize| codex_line_uuid(&json!(thread_raw_id.clone()), n);

    let indexed_meta = unit.meta.as_ref();
    let initial_timestamp = truthy_str(meta.get("timestamp"))
        .map(str::to_string)
        .or_else(|| truthy_str(meta_obj.get("timestamp")).map(str::to_string));
    let indexed_updated_at = indexed_meta
        .and_then(|m| m.get("indexedUpdatedAt"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let ended_at = match indexed_updated_at.as_deref() {
        Some(updated) if !updated.is_empty() => {
            let later = initial_timestamp
                .as_deref()
                .map(|initial| updated > initial)
                .unwrap_or(true);
            if later {
                indexed_updated_at.clone()
            } else {
                initial_timestamp.clone()
            }
        }
        _ => initial_timestamp.clone(),
    };

    let mut sm = ParseState {
        session_id: session_id.clone(),
        agent_id: agent_id.clone(),
        is_sidechain,
        out: Vec::new(),
        msg_by_uuid: HashMap::new(),
        started_at: initial_timestamp,
        ended_at,
        git_branch: meta
            .get("git")
            .and_then(|g| g.get("branch"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        version: truthy_str(meta.get("cli_version")).map(str::to_string),
        title: indexed_meta
            .and_then(|m| m.get("indexedTitle"))
            .and_then(Value::as_str)
            .map(str::to_string),
        n: 0,
        last_message_uuid: None,
        last_text_assistant_uuid: None,
        total_input_tokens: 0,
        total_output_tokens: 0,
        current_cwd: normalize_observed_cwd(meta.get("cwd").and_then(Value::as_str)),
        current_model: None,
    };

    // First pass: collect visible event_msg keys so duplicate response_items
    // drop (the pair may sit ±1 line apart in either order).
    let mut event_message_keys: HashSet<String> = HashSet::new();
    for (_, obj) in &records {
        if obj.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let payload = obj.get("payload").unwrap_or(&Value::Null);
        let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if payload_type != "user_message" && payload_type != "agent_message" {
            continue;
        }
        if let Some(text) = codex_event_text(payload) {
            event_message_keys.insert(codex_visible_message_key(
                Some(if payload_type == "user_message" {
                    "user"
                } else {
                    "assistant"
                }),
                Some(&text),
            ));
        }
    }

    let mut call_message_uuids: HashMap<String, String> = HashMap::new();
    for (current_line, obj) in &records {
        let ts = truthy_str(obj.get("timestamp"));
        let obj_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if obj_type == "session_meta" {
            let payload = obj.get("payload").unwrap_or(&Value::Null);
            if let Some(cwd) = normalize_observed_cwd(payload.get("cwd").and_then(Value::as_str)) {
                sm.current_cwd = Some(cwd);
            }
            if let Some(branch) = truthy_str(payload.get("git").and_then(|g| g.get("branch"))) {
                sm.git_branch = Some(branch.to_string());
            }
            if let Some(version) = truthy_str(payload.get("cli_version")) {
                sm.version = Some(version.to_string());
            }
            sm.update_bounds(truthy_str(payload.get("timestamp")).or(ts));
            continue;
        }
        if obj_type == "turn_context" {
            let payload = obj.get("payload").unwrap_or(&Value::Null);
            if let Some(cwd) = normalize_observed_cwd(payload.get("cwd").and_then(Value::as_str)) {
                sm.current_cwd = Some(cwd);
            }
            if let Some(model) = truthy_str(payload.get("model")) {
                sm.current_model = Some(model.to_string());
            }
            sm.update_bounds(ts);
            continue;
        }
        if obj_type == "event_msg" {
            let payload = obj.get("payload").unwrap_or(&Value::Null);
            let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
            if payload_type == "user_message"
                || payload_type == "agent_message"
                || payload_type == "agent_reasoning"
            {
                let Some(text) = codex_event_text(payload) else {
                    continue;
                };
                let is_reasoning = payload_type == "agent_reasoning";
                let role = if payload_type == "user_message" {
                    "user"
                } else {
                    "assistant"
                };
                sm.insert_message(
                    line_uuid(*current_line),
                    role,
                    role,
                    Some(&text),
                    if is_reasoning { "thinking" } else { "text" },
                    ts,
                );
                continue;
            }
            if payload_type == "collab_agent_spawn_end"
                && js_truthy(payload.get("call_id"))
                && js_truthy(payload.get("new_thread_id"))
            {
                let uuid = sm.insert_message(
                    line_uuid(*current_line),
                    "assistant",
                    "assistant",
                    None,
                    "tool_use",
                    ts,
                );
                let tool_id = codex_call_id(
                    &json!(thread_raw_id.clone()),
                    payload.get("call_id").unwrap_or(&Value::Null),
                )
                .unwrap_or_default();
                let new_agent_role = truthy_str(payload.get("new_agent_role"));
                let description = truthy_str(payload.get("new_agent_nickname"))
                    .or(new_agent_role)
                    .unwrap_or("Agent")
                    .to_string();
                let input = json!({
                    "description": description,
                    "subagent_type": new_agent_role.unwrap_or("Agent"),
                    "prompt": if js_truthy(payload.get("prompt")) {
                        payload.get("prompt").cloned().unwrap()
                    } else {
                        Value::String(String::new())
                    },
                    "new_thread_id": payload.get("new_thread_id").cloned().unwrap_or(Value::Null),
                    "model": if js_truthy(payload.get("model")) {
                        payload.get("model").cloned().unwrap()
                    } else {
                        Value::Null
                    },
                    "reasoning_effort": if js_truthy(payload.get("reasoning_effort")) {
                        payload.get("reasoning_effort").cloned().unwrap()
                    } else {
                        Value::Null
                    },
                });
                sm.out.push(TranscriptRecord::ToolCall(ToolCallRecord {
                    id: tool_id.clone(),
                    message_uuid: uuid.clone(),
                    session_id: session_id.clone(),
                    name: "Agent".to_string(),
                    presentation: ToolCallPresentation::Default,
                    input_json: trunc_json_default(&input).unwrap_or_default(),
                    file_path: None,
                }));
                call_message_uuids.insert(tool_id.clone(), uuid);
                sm.out.push(TranscriptRecord::Subagent(
                    crate::providers::types::SubagentRecord {
                        agent_id: codex_db_id(payload.get("new_thread_id").unwrap_or(&Value::Null))
                            .unwrap_or_default(),
                        session_id: session_id.clone(),
                        parent_tool_use_id: Some(tool_id),
                        agent_type: new_agent_role.map(str::to_string),
                        description: Some(description),
                        duration_ms: None,
                        total_tokens: None,
                    },
                ));
                continue;
            }
            if payload_type == "task_complete" {
                if sm.last_text_assistant_uuid.is_some() && payload.get("duration_ms").is_some() {
                    let turn_duration_ms = payload
                        .get("duration_ms")
                        .and_then(Value::as_f64)
                        .filter(|v| *v != 0.0)
                        .map(|v| v as i64);
                    if let Some(uuid) = sm.last_text_assistant_uuid.clone() {
                        sm.out.push(TranscriptRecord::MessageTurnDuration {
                            uuid,
                            turn_duration_ms,
                        });
                    }
                }
                sm.update_bounds(ts);
                continue;
            }
            if payload_type == "token_count" {
                let (input_tokens, output_tokens) = codex_usage(payload);
                if let Some(input) = input_tokens {
                    sm.total_input_tokens = input;
                }
                if let Some(output) = output_tokens {
                    sm.total_output_tokens = output;
                }
                if sm.last_text_assistant_uuid.is_some()
                    && (input_tokens.is_some() || output_tokens.is_some())
                {
                    if let Some(&index) = sm
                        .msg_by_uuid
                        .get(sm.last_text_assistant_uuid.as_deref().unwrap())
                    {
                        if let TranscriptRecord::Message(record) = &mut sm.out[index] {
                            record.input_tokens = input_tokens;
                            record.output_tokens = output_tokens;
                        }
                    }
                }
                continue;
            }
            if payload_type == "thread_name_updated" {
                if let Some(thread_name) = truthy_str(payload.get("thread_name")) {
                    sm.title = Some(thread_name.to_string());
                }
            }
            continue;
        }
        if obj_type != "response_item" {
            continue;
        }
        let payload = obj.get("payload").unwrap_or(&Value::Null);
        let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if payload_type == "message"
            && payload.get("role").and_then(Value::as_str) != Some("developer")
        {
            let text = codex_message_payload_text(payload);
            let role = truthy_str(payload.get("role")).unwrap_or("assistant");
            if let Some(text) = text {
                if !event_message_keys.contains(&codex_visible_message_key(Some(role), Some(&text)))
                {
                    let r#type = if role == "user" { "user" } else { "assistant" };
                    sm.insert_message(
                        line_uuid(*current_line),
                        r#type,
                        role,
                        Some(&text),
                        "text",
                        ts,
                    );
                }
            }
            continue;
        }
        if matches!(
            payload_type,
            "function_call" | "custom_tool_call" | "tool_search_call" | "web_search_call"
        ) && js_truthy(payload.get("call_id"))
        {
            let uuid = sm.insert_message(
                line_uuid(*current_line),
                "assistant",
                "assistant",
                None,
                "tool_use",
                ts,
            );
            let name = truthy_str(payload.get("name"))
                .or_else(|| truthy_str(payload.get("tool")))
                .map(str::to_string)
                .unwrap_or_else(|| {
                    payload_type
                        .strip_suffix("_call")
                        .unwrap_or(payload_type)
                        .to_string()
                });
            let tool_id = codex_call_id(
                &json!(thread_raw_id.clone()),
                payload.get("call_id").unwrap_or(&Value::Null),
            )
            .unwrap_or_default();
            sm.out.push(TranscriptRecord::ToolCall(ToolCallRecord {
                id: tool_id.clone(),
                message_uuid: uuid.clone(),
                session_id: session_id.clone(),
                presentation: if name == "Skill" {
                    ToolCallPresentation::Skill
                } else {
                    ToolCallPresentation::Default
                },
                input_json: trunc_json_default(&codex_tool_input(payload)).unwrap_or_default(),
                file_path: None,
                name,
            }));
            call_message_uuids.insert(tool_id, uuid);
            continue;
        }
        if matches!(
            payload_type,
            "function_call_output" | "custom_tool_call_output" | "tool_search_output"
        ) && js_truthy(payload.get("call_id"))
        {
            let tool_id = codex_call_id(
                &json!(thread_raw_id.clone()),
                payload.get("call_id").unwrap_or(&Value::Null),
            )
            .unwrap_or_default();
            let message_uuid = call_message_uuids.get(&tool_id).cloned();
            sm.out.push(TranscriptRecord::ToolResult(ToolResultRecord {
                tool_use_id: tool_id,
                message_uuid,
                session_id: session_id.clone(),
                content: trunc(codex_tool_output(payload).as_deref()).unwrap_or_default(),
                file_path: None,
                is_error: payload
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }));
        }
    }

    if let Some(agent_id) = &agent_id {
        let started = sm.started_at.as_deref().and_then(parse_iso_ms);
        let ended = sm.ended_at.as_deref().and_then(parse_iso_ms);
        let token_total = sm.total_input_tokens + sm.total_output_tokens;
        sm.out.push(TranscriptRecord::Subagent(
            crate::providers::types::SubagentRecord {
                agent_id: agent_id.clone(),
                session_id: session_id.clone(),
                parent_tool_use_id: None,
                agent_type: codex_agent_role(meta),
                description: codex_agent_nickname(meta),
                duration_ms: match (started, ended) {
                    (Some(started), Some(ended)) => Some(ended - started),
                    _ => None,
                },
                total_tokens: if token_total != 0 {
                    Some(token_total)
                } else {
                    None
                },
            },
        ));
    } else {
        sm.out.push(TranscriptRecord::Session(
            crate::providers::types::SessionRecord {
                id: session_id.clone(),
                title: sm.title.clone(),
                project,
                started_at: sm.started_at.clone(),
                ended_at: sm.ended_at.clone(),
                git_branch: sm.git_branch.clone(),
                version: sm.version.clone(),
                message_count: sm.n,
                count_mode: SessionCountMode::Total,
                jsonl_path: unit.key.clone(),
                source: "codex".to_string(),
            },
        ));
    }

    let mut items: Vec<StreamItem> = sm.out.into_iter().map(StreamItem::Record).collect();
    items.push(StreamItem::Cursor(out_cursor));
    items
}

fn parse_iso_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn find_codex_file(root_dir: &Path, raw_thread_id: &str) -> Option<PathBuf> {
    let mut stack = codex_transcript_dirs(root_dir);
    let suffix = format!("{raw_thread_id}.jsonl");
    while let Some(current) = stack.pop() {
        if !current.exists() {
            continue;
        }
        let Ok(entries) = sorted_read_dir(&current) else {
            // TS readdirSync throws; treat an unreadable dir as exhausted.
            continue;
        };
        for (name, path) in entries {
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() && name.ends_with(&suffix) {
                return Some(path);
            }
        }
    }
    None
}

fn raw_codex(root_dir: &Path, input: &RawLookup) -> Option<RawRecord> {
    let regex = regex::Regex::new(r"^codex:([^:]+):(\d+)$").ok()?;
    let captures = regex.captures(input.message_uuid)?;
    let raw_thread_id = captures.get(1)?.as_str();
    let line_number: usize = captures.get(2)?.as_str().parse().ok()?;
    let path = if input.agent_id.is_none() {
        input
            .session?
            .get("jsonl_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    } else {
        find_codex_file(root_dir, raw_thread_id)
    }?;
    if !path.exists() {
        return None;
    }
    let mut current_line = 0usize;
    let mut found: Option<String> = None;
    let _ = read_lines(&path, |line, _terminated| {
        current_line += 1;
        if current_line == line_number {
            found = Some(line.to_string());
            return false;
        }
        true
    });
    let raw = found?;
    let mut message_text: Option<String> = None;
    if let Ok(obj) = serde_json::from_str::<Value>(&raw) {
        let payload = obj.get("payload").cloned().unwrap_or(json!({}));
        if obj.get("type").and_then(Value::as_str) == Some("event_msg") {
            message_text = payload
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| payload.get("text").and_then(Value::as_str))
                .map(str::to_string);
        } else if obj.get("type").and_then(Value::as_str) == Some("response_item")
            && payload.get("type").and_then(Value::as_str) == Some("message")
            && payload.get("content").and_then(Value::as_array).is_some()
        {
            message_text = codex_message_payload_text(&payload);
        }
    }
    let total_length = raw.chars().map(char::len_utf16).sum::<usize>();
    Some(RawRecord {
        text: raw,
        total_length: Some(total_length),
        offset: Some(0),
        limit: Some(total_length),
        has_more: Some(false),
        message_text,
    })
}

pub struct CodexProvider {
    pub root_dir: PathBuf,
}

impl CodexProvider {
    pub fn new(root_dir: PathBuf) -> Self {
        Self { root_dir }
    }
}

impl ProviderAdapter for CodexProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: NAME,
            name: "Codex",
            vendor: "OpenAI",
            default_root: self.root_dir.to_string_lossy().into_owned(),
            color: "#10a37f",
            requires_explicit_root: false,
            root_resolution_reason: None,
        }
    }

    fn index_version_marker(&self) -> Option<&'static str> {
        Some(CODEX_CANONICAL_TRANSCRIPT_MARKER)
    }

    fn watch_targets(&self, configured_root: &str) -> Vec<WatchTarget> {
        let mut targets: Vec<WatchTarget> = codex_transcript_dirs(Path::new(configured_root))
            .into_iter()
            .map(|dir| WatchTarget {
                kind: WatchTargetKind::Tree,
                path: dir.to_string_lossy().into_owned(),
            })
            .collect();
        targets.push(WatchTarget {
            kind: WatchTargetKind::File,
            path: Path::new(configured_root)
                .join("session_index.jsonl")
                .to_string_lossy()
                .into_owned(),
        });
        targets
    }

    fn discover<'a>(&'a self, ctx: &mut DiscoverContext<'a>) -> Vec<IndexUnit> {
        discover_at(&self.root_dir, ctx)
    }

    fn parse<'a>(&'a self, unit: &'a IndexUnit, cursor: Cursor) -> ParseStream<'a> {
        let items = parse(unit, cursor);
        Box::new(items.into_iter())
    }

    fn raw(&self, input: &RawLookup) -> Option<RawRecord> {
        raw_codex(&self.root_dir, input)
    }
}
