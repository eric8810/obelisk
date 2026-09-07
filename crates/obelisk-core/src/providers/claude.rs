// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Claude Code provider adapter (port of packages/core/src/providers/claude.ts).
//!
//! Pure: discovers Claude transcript files and parses one into a record
//! stream. Never touches the Obelisk database.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::parsing::{
    discover_jsonl_files, extract_content_type, extract_message_is_meta, extract_text,
    file_signature, is_dir, is_skill_instructions, normalize_path, read_lines, sorted_read_dir,
    source_inventory_issue, tool_file_path, trunc, trunc_json_default,
};
use crate::providers::types::{
    Cursor, DiscoverContext, IndexUnit, InventoryIssue, MessageRecord, MessageVisibility,
    ParseStream, ProviderAdapter, ProviderDescriptor, RawLookup, RawRecord, SessionCountMode,
    StreamItem, ToolCallPresentation, ToolCallRecord, ToolResultRecord, TranscriptRecord,
    WatchTarget, WatchTargetKind,
};

pub const NAME: &str = "claude";
pub const CLAUDE_CANONICAL_TRANSCRIPT_MARKER: &str = "__claude_canonical_transcript_v2__";

// Claude cursor encodes "<mtimeMs>:<linesProcessed>" (legacy) or
// "<mtimeMs>:<lines>:<size>:<ctimeMs>:<ino>" (current). mtime lets discovery
// detect change; lines lets parse resume without reprocessing.
fn cursor_to_skip(cursor: &Cursor) -> usize {
    let Some(cursor) = cursor.as_deref() else {
        return 0;
    };
    cursor
        .split(':')
        .nth(1)
        .and_then(|part| part.parse::<usize>().ok())
        .unwrap_or(0)
}

// The mtime+ctime+size+inode signature lets a same-mtime tail completion or a
// same-mtime replacement back into discovery. Legacy two-part cursors keep
// the mtime-only gate and upgrade on the next parse.
fn cursor_signature_differs(cursor: &str, file_path: &Path) -> bool {
    let Ok((mtime, size, ctime, ino)) = file_signature(file_path) else {
        // TS statSync throws; discovery treats unreadable files as changed
        // so the parse (and its error handling) sees them.
        return true;
    };
    let parts: Vec<&str> = cursor.split(':').collect();
    if parts.len() < 5 {
        return parts
            .first()
            .and_then(|value| value.parse::<f64>().ok())
            .map(|cursor_mtime| cursor_mtime < mtime)
            .unwrap_or(true);
    }
    parts[0].parse::<f64>().map(|v| v != mtime).unwrap_or(true)
        || parts[2].parse::<i64>().map(|v| v != size).unwrap_or(true)
        || parts[3].parse::<f64>().map(|v| v != ctime).unwrap_or(true)
        || parts[4].parse::<u64>().map(|v| v != ino).unwrap_or(true)
}

fn total_input_tokens(usage: &Value) -> Option<i64> {
    let mut seen = false;
    let mut total: f64 = 0.0;
    for field in [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        if let Some(value) = usage.get(field).and_then(Value::as_f64) {
            if value.is_finite() {
                seen = true;
                total += value;
            }
        }
    }
    if seen {
        Some(total as i64)
    } else {
        None
    }
}

fn output_tokens(usage: &Value) -> Option<i64> {
    // TS `usage.output_tokens || null`: 0 and missing are both null.
    usage
        .get("output_tokens")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v != 0.0)
        .map(|v| v as i64)
}

fn meta_history_title(meta: Option<&Value>) -> Option<String> {
    meta.and_then(|meta| {
        meta.get("historyTitle")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn meta_workflow_run_id(meta: Option<&Value>) -> Option<String> {
    meta.and_then(|meta| {
        meta.get("workflowRunId")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn meta_is_workflow(meta: Option<&Value>) -> bool {
    meta.and_then(|meta| meta.get("kind"))
        .and_then(Value::as_str)
        == Some("workflow")
}

fn meta_main_transcript_path(meta: Option<&Value>) -> Option<String> {
    meta.and_then(|meta| {
        meta.get("mainTranscriptPath")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn discover_at(root_dir: &Path, ctx: &mut DiscoverContext) -> Vec<IndexUnit> {
    let projects_dir = root_dir.join("projects");
    let indexed_count = ctx.indexed_sessions().len();
    if !projects_dir.exists() && indexed_count > 0 {
        ctx.report_incomplete(InventoryIssue {
            path: projects_dir.to_string_lossy().into_owned(),
            error: "Source folder is unavailable".to_string(),
        });
    }
    let history_path = PathBuf::from(normalize_path(
        &root_dir.join("history.jsonl").to_string_lossy(),
    ));
    let mut history_titles: HashMap<String, String> = HashMap::new();
    if history_path.exists() {
        let _ = read_lines(&history_path, |line, _terminated| {
            if let Ok(item) = serde_json::from_str::<Value>(line) {
                if let (Some(session_id), Some(title)) = (
                    item.get("sessionId").and_then(Value::as_str),
                    item.get("title").and_then(Value::as_str),
                ) {
                    history_titles.insert(session_id.to_string(), title.to_string());
                }
            }
            true
        });
    }
    let mut changed_transcript_paths: HashSet<String> = HashSet::new();
    let mut changed_workflow_paths: HashSet<String> = HashSet::new();
    let mut forced_paths: HashSet<String> = HashSet::new();
    let mut history_changed = false;
    if let Some(changed_paths) = ctx.changed_paths {
        for changed_path in changed_paths {
            let changed_path: &str = changed_path;
            let changed_path = if Path::new(changed_path).is_absolute() {
                normalize_path(changed_path)
            } else {
                normalize_path(&root_dir.join(changed_path).to_string_lossy())
            };
            if changed_path == history_path.to_string_lossy() {
                history_changed = true;
            }
            let absolute = if Path::new(changed_path.as_str()).is_absolute() {
                normalize_path(&changed_path)
            } else {
                normalize_path(&projects_dir.join(&changed_path).to_string_lossy())
            };
            let absolute_path = PathBuf::from(&absolute);
            let inside = absolute_path
                .strip_prefix(&projects_dir)
                .map(|rest| rest.to_string_lossy().into_owned())
                .ok();
            let inside_ok =
                matches!(&inside, Some(rest) if !rest.is_empty() && !rest.starts_with(".."));
            if !inside_ok {
                continue;
            }
            let lowered = absolute.to_lowercase();
            if let Some(transcript) = lowered.strip_suffix(".meta.json") {
                let transcript = format!("{transcript}.jsonl");
                changed_transcript_paths.insert(transcript.clone());
                forced_paths.insert(transcript);
            } else if lowered.ends_with(".jsonl") {
                changed_transcript_paths.insert(absolute.clone());
            } else if lowered.ends_with(".json") {
                changed_workflow_paths.insert(absolute.clone());
            }
        }
    }

    let mut report = |issue: InventoryIssue| {
        // borrow discipline: forward into ctx
        ctx.report_incomplete(issue);
    };
    let transcript_files = discover_jsonl_files(&projects_dir, Some(&mut report));
    let mut transcript_units: Vec<IndexUnit> = Vec::new();
    for file in &transcript_files {
        let normalized_path = normalize_path(&file.path.to_string_lossy());
        if ctx.changed_paths.is_some()
            && !history_changed
            && !changed_transcript_paths.contains(&normalized_path)
        {
            continue;
        }
        let cursor = (ctx.last_cursor)(&file.path.to_string_lossy());
        let cursor_changed = match &cursor {
            None => true,
            Some(cursor) => cursor_signature_differs(cursor, &file.path),
        };
        if history_changed || forced_paths.contains(&normalized_path) || cursor_changed {
            let mut meta = serde_json::Map::new();
            if let Some(workflow_run_id) = &file.workflow_run_id {
                meta.insert("workflowRunId".to_string(), json!(workflow_run_id));
            }
            if let Some(title) = history_titles.get(&file.session_id) {
                meta.insert("historyTitle".to_string(), json!(title));
            }
            transcript_units.push(IndexUnit {
                key: file.path.to_string_lossy().into_owned(),
                session_id: file.session_id.clone(),
                project: Some(file.project.clone()),
                is_subagent: file.is_subagent,
                agent_id: file.agent_id.clone(),
                meta: Some(Value::Object(meta)),
                retract_session_ids: Vec::new(),
            });
        }
    }

    let mut workflow_units: Vec<IndexUnit> = Vec::new();
    if !projects_dir.exists() {
        return transcript_units;
    }
    let projects = match sorted_read_dir(&projects_dir) {
        Ok(entries) => entries,
        Err(error) => {
            ctx.report_incomplete(source_inventory_issue(
                &projects_dir.to_string_lossy(),
                &error,
            ));
            return transcript_units;
        }
    };
    for (project, project_path) in projects {
        if !is_dir(&project_path) {
            continue;
        }
        let session_ids = match sorted_read_dir(&project_path) {
            Ok(entries) => entries,
            Err(error) => {
                ctx.report_incomplete(source_inventory_issue(
                    &project_path.to_string_lossy(),
                    &error,
                ));
                continue;
            }
        };
        for (session_id, _) in session_ids {
            let workflow_dir = project_path.join(&session_id).join("workflows");
            if !is_dir(&workflow_dir) {
                continue;
            }
            let main_transcript_path = project_path.join(format!("{session_id}.jsonl"));
            let files = match sorted_read_dir(&workflow_dir) {
                Ok(entries) => entries,
                Err(error) => {
                    ctx.report_incomplete(source_inventory_issue(
                        &workflow_dir.to_string_lossy(),
                        &error,
                    ));
                    continue;
                }
            };
            for (file, workflow_path) in files {
                if !file.ends_with(".json") {
                    continue;
                }
                let normalized_path = normalize_path(&workflow_path.to_string_lossy());
                let relationship_changed = changed_transcript_paths
                    .contains(&normalize_path(&main_transcript_path.to_string_lossy()));
                if ctx.changed_paths.is_some()
                    && !changed_workflow_paths.contains(&normalized_path)
                    && !relationship_changed
                {
                    continue;
                }
                let mtime = crate::parsing::file_mtime_ms(&workflow_path).unwrap_or(0.0);
                let key = workflow_path.to_string_lossy().into_owned();
                let cursor = (ctx.last_cursor)(&key);
                if !relationship_changed {
                    if let Some(cursor) = &cursor {
                        let cursor_mtime =
                            cursor.split(':').next().and_then(|v| v.parse::<f64>().ok());
                        if let Some(cursor_mtime) = cursor_mtime {
                            if cursor_mtime >= mtime {
                                continue;
                            }
                        }
                    }
                }
                workflow_units.push(IndexUnit {
                    key,
                    session_id: session_id.clone(),
                    project: Some(project.clone()),
                    is_subagent: false,
                    agent_id: None,
                    meta: Some(json!({
                        "kind": "workflow",
                        "mainTranscriptPath": normalize_path(
                            &main_transcript_path.to_string_lossy()
                        ),
                    })),
                    retract_session_ids: Vec::new(),
                });
            }
        }
    }
    transcript_units.extend(workflow_units);
    transcript_units
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn workflow_parent_tool_use_id(transcript_path: &Path, run_id: &str) -> Option<String> {
    if !transcript_path.exists() {
        return None;
    }
    let mut workflow_tool_ids: HashSet<String> = HashSet::new();
    let mut parent_tool_use_id: Option<String> = None;
    let found = &mut parent_tool_use_id;
    let _ = read_lines(transcript_path, |line, _terminated| {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return true;
        };
        let Some(content) = record.get("message").and_then(|m| m.get("content")) else {
            return true;
        };
        let Some(blocks) = content.as_array() else {
            return true;
        };
        let record_type = record.get("type").and_then(Value::as_str);
        if record_type == Some("assistant") {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && block.get("name").and_then(Value::as_str) == Some("Workflow")
                {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        workflow_tool_ids.insert(id.to_string());
                    }
                }
            }
            return true;
        }
        if record_type != Some("user") {
            return true;
        }
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            if !workflow_tool_ids.contains(tool_use_id) {
                continue;
            }
            let text = tool_result_text(block.get("content").unwrap_or(&Value::Null));
            if !text.contains(run_id) {
                continue;
            }
            *found = Some(tool_use_id.to_string());
            return false;
        }
        true
    });
    parent_tool_use_id
}

fn parse_workflow(unit: &IndexUnit) -> Vec<StreamItem> {
    let Ok(mtime) = crate::parsing::file_mtime_ms(Path::new(&unit.key)) else {
        return vec![StreamItem::Cursor("0:1".to_string())];
    };
    let out_cursor = format!("{mtime}:1");
    let Ok(workflow_raw) = std::fs::read_to_string(&unit.key) else {
        return vec![StreamItem::Cursor(out_cursor)];
    };
    let Ok(workflow) = serde_json::from_str::<Value>(&workflow_raw) else {
        return vec![StreamItem::Cursor(out_cursor)];
    };
    let Some(run_id) = workflow.get("runId").and_then(Value::as_str) else {
        return vec![StreamItem::Cursor(out_cursor)];
    };
    let main_transcript_path = meta_main_transcript_path(unit.meta.as_ref())
        .map(PathBuf::from)
        .unwrap_or_default();
    let progress = workflow
        .get("workflowProgress")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let agents: Vec<&Value> = progress
        .iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("workflow_agent")
                && item.get("agentId").is_some()
        })
        .collect();
    let mut items = Vec::new();
    items.push(StreamItem::Record(TranscriptRecord::Workflow(
        crate::providers::types::WorkflowRecord {
            run_id: run_id.to_string(),
            session_id: unit.session_id.clone(),
            parent_tool_use_id: workflow_parent_tool_use_id(&main_transcript_path, run_id),
            task_id: workflow
                .get("taskId")
                .and_then(Value::as_str)
                .map(str::to_string),
            script: workflow
                .get("script")
                .and_then(Value::as_str)
                .map(str::to_string),
            result_json: workflow
                .get("result")
                .filter(|v| !v.is_null())
                .map(|result| result.to_string()),
            timestamp: workflow
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_string),
            agent_count: agents.len() as i64,
            duration_ms: workflow
                .get("durationMs")
                .and_then(Value::as_f64)
                .filter(|v| *v != 0.0)
                .map(|v| v as i64),
            total_tokens: workflow
                .get("totalTokens")
                .and_then(Value::as_f64)
                .filter(|v| *v != 0.0)
                .map(|v| v as i64),
            status: workflow
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            workflow_name: workflow
                .get("workflowName")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
    )));
    for item in agents {
        let agent_id = item.get("agentId").and_then(Value::as_str).unwrap_or("");
        items.push(StreamItem::Record(TranscriptRecord::WorkflowAgent(
            crate::providers::types::WorkflowAgentRecord {
                agent_id: format!("agent-{agent_id}"),
                run_id: run_id.to_string(),
                session_id: unit.session_id.clone(),
                agent_type: None,
                description: None,
                phase: item
                    .get("phaseTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                label: item
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                model: item
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                state: item
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                duration_ms: item
                    .get("durationMs")
                    .and_then(Value::as_f64)
                    .filter(|v| *v != 0.0)
                    .map(|v| v as i64),
                tokens: item
                    .get("tokens")
                    .and_then(Value::as_f64)
                    .filter(|v| *v != 0.0)
                    .map(|v| v as i64),
                tool_calls: item
                    .get("toolCalls")
                    .and_then(Value::as_f64)
                    .filter(|v| *v != 0.0)
                    .map(|v| v as i64),
            },
        )));
    }
    items.push(StreamItem::Cursor(out_cursor));
    items
}

pub fn parse(unit: &IndexUnit, cursor: Cursor) -> Vec<StreamItem> {
    if meta_is_workflow(unit.meta.as_ref()) {
        return parse_workflow(unit);
    }
    let skip = cursor_to_skip(&cursor);
    let Ok((mtime, size, ctime, ino)) = file_signature(Path::new(&unit.key)) else {
        // TS statSync throws here; the indexer skips the unit with a warning.
        return vec![StreamItem::Error(format!("unable to stat {}", unit.key))];
    };
    let is_subagent = unit.is_subagent;
    let mut records: Vec<TranscriptRecord> = Vec::new();
    struct SessionMerge {
        started_at: Option<String>,
        ended_at: Option<String>,
        git_branch: Option<String>,
        version: Option<String>,
        title: Option<String>,
        n: i64,
    }
    let mut sm = SessionMerge {
        started_at: None,
        ended_at: None,
        git_branch: None,
        version: None,
        title: meta_history_title(unit.meta.as_ref()),
        n: 0,
    };
    let mut subagent_stats = (Option::<String>::None, Option::<String>::None, 0i64);

    let mut line_num = 0usize;
    // Lines the cursor may safely skip on the next parse. A line only counts
    // when it parsed, or when it is newline-terminated (mid-file garbage
    // keeps the legacy count). An unterminated tail that fails to parse may
    // still be growing — counting it would permanently skip the completed line.
    let mut cursor_lines = 0usize;
    let sid = unit.session_id.clone();
    let _ = read_lines(Path::new(&unit.key), |line, terminated| {
        line_num += 1;
        let parsed: Option<Value> = serde_json::from_str(line).ok();
        if parsed.is_some() || terminated {
            cursor_lines = line_num;
        }
        let Some(obj) = parsed else { return true };
        let ts = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let msg = obj.get("message").cloned().unwrap_or(Value::Null);
        let usage = msg.get("usage").cloned().unwrap_or(Value::Null);
        let record_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if is_subagent && (record_type == "user" || record_type == "assistant") {
            if let Some(ts) = &ts {
                if subagent_stats.0.as_ref().map(|s| ts < s).unwrap_or(true) {
                    subagent_stats.0 = Some(ts.clone());
                }
                if subagent_stats.1.as_ref().map(|s| ts > s).unwrap_or(true) {
                    subagent_stats.1 = Some(ts.clone());
                }
            }
            subagent_stats.2 +=
                total_input_tokens(&usage).unwrap_or(0) + output_tokens(&usage).unwrap_or(0);
        }
        if line_num <= skip {
            return true;
        }

        if record_type == "ai-title" {
            if let Some(title) = obj.get("aiTitle").and_then(Value::as_str) {
                sm.title = Some(title.to_string());
            }
            return true;
        }
        if record_type == "system" {
            let subtype = obj.get("subtype").and_then(Value::as_str).unwrap_or("");
            if subtype == "away_summary" {
                if let Some(content) = obj.get("content").and_then(Value::as_str) {
                    let id = obj
                        .get("uuid")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!(
                                "{sid}-away-{}",
                                ts.clone().unwrap_or_else(|| "null".to_string())
                            )
                        });
                    records.push(TranscriptRecord::Summary(
                        crate::providers::types::SummaryRecord {
                            id,
                            session_id: sid.clone(),
                            timestamp: ts.clone(),
                            source: "away_summary".to_string(),
                            content: content.to_string(),
                            visibility: None,
                            input_tokens: None,
                            output_tokens: None,
                        },
                    ));
                }
                return true;
            }
            if subtype == "turn_duration" {
                let parent_uuid = obj.get("parentUuid").and_then(Value::as_str);
                let duration = obj.get("durationMs").and_then(Value::as_f64);
                if let (Some(uuid), Some(duration)) = (parent_uuid, duration) {
                    records.push(TranscriptRecord::MessageTurnDuration {
                        uuid: uuid.to_string(),
                        turn_duration_ms: Some(duration as i64),
                    });
                }
                return true;
            }
        }
        if record_type != "user" && record_type != "assistant" {
            return true;
        }

        if let Some(ts) = &ts {
            if sm.started_at.as_ref().map(|s| ts < s).unwrap_or(true) {
                sm.started_at = Some(ts.clone());
            }
            if sm.ended_at.as_ref().map(|s| ts > s).unwrap_or(true) {
                sm.ended_at = Some(ts.clone());
            }
        }
        if let Some(branch) = obj.get("gitBranch").and_then(Value::as_str) {
            sm.git_branch = Some(branch.to_string());
        }
        if let Some(version) = obj.get("version").and_then(Value::as_str) {
            sm.version = Some(version.to_string());
        }
        sm.n += 1;

        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        let text = extract_text(&content);
        let raw_content_type = extract_content_type(&content);
        let is_meta = extract_message_is_meta(&obj, text.as_deref());
        let content_type = if is_meta && is_skill_instructions(text.as_deref()) {
            "skill_instructions"
        } else {
            raw_content_type
        };
        let agent_id = if is_subagent {
            unit.agent_id.clone()
        } else {
            obj.get("agentId")
                .and_then(Value::as_str)
                .map(str::to_string)
        };

        if let Some(uuid) = obj.get("uuid").and_then(Value::as_str) {
            records.push(TranscriptRecord::Message(MessageRecord {
                uuid: uuid.to_string(),
                session_id: sid.clone(),
                r#type: record_type.to_string(),
                parent_uuid: obj
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                timestamp: ts.clone(),
                role: msg
                    .get("role")
                    .and_then(Value::as_str)
                    .filter(|r| !r.is_empty())
                    .map(str::to_string)
                    .or_else(|| Some(record_type.to_string())),
                text: text.clone(),
                content_type: Some(content_type.to_string()),
                is_meta,
                visibility: MessageVisibility::Visible,
                model: msg.get("model").and_then(Value::as_str).map(str::to_string),
                is_sidechain: obj
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                agent_id: agent_id.clone(),
                input_tokens: total_input_tokens(&usage),
                output_tokens: output_tokens(&usage),
                cwd: obj.get("cwd").and_then(Value::as_str).map(str::to_string),
                skill: obj
                    .get("attributionSkill")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                source: "claude".to_string(),
            }));
        }

        if record_type == "assistant" {
            if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                for block in blocks {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                    let id = block.get("id").and_then(Value::as_str);
                    if block_type == "tool_use" {
                        if let (Some(id), Some(uuid)) =
                            (id, obj.get("uuid").and_then(Value::as_str))
                        {
                            // TS `b.input || {}`: null/undefined/absent all fall
                            // back to an empty object.
                            let input = match block.get("input") {
                                Some(value @ Value::Object(_)) => value.clone(),
                                Some(Value::Array(_))
                                | Some(Value::String(_))
                                | Some(Value::Number(_))
                                | Some(Value::Bool(_)) => block.get("input").cloned().unwrap(),
                                _ => serde_json::json!({}),
                            };
                            records.push(TranscriptRecord::ToolCall(ToolCallRecord {
                                id: id.to_string(),
                                message_uuid: uuid.to_string(),
                                session_id: sid.clone(),
                                name: name.to_string(),
                                presentation: if name == "Skill" {
                                    ToolCallPresentation::Skill
                                } else {
                                    ToolCallPresentation::Default
                                },
                                input_json: trunc_json_default(&input).unwrap_or_default(),
                                file_path: tool_file_path(name, Some(&input)),
                            }));
                        }
                    }
                }
            }
        }

        if record_type == "user" {
            if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let block_content = block.get("content").cloned().unwrap_or(Value::Null);
                    let rt = match &block_content {
                        Value::String(s) => s.clone(),
                        Value::Array(parts) => parts
                            .iter()
                            .map(|c| {
                                c.get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string()
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    records.push(TranscriptRecord::ToolResult(ToolResultRecord {
                        tool_use_id: tool_use_id.to_string(),
                        message_uuid: obj.get("uuid").and_then(Value::as_str).map(str::to_string),
                        session_id: sid.clone(),
                        content: trunc(Some(&rt)).unwrap_or_default(),
                        file_path: obj
                            .get("toolUseResult")
                            .and_then(|r| r.get("filePath"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }));
                }
            }
        }
        true
    });

    if is_subagent {
        if let Some(agent_id) = &unit.agent_id {
            let key = unit.key.trim_end_matches(".jsonl").to_string() + ".meta.json";
            let meta_path = PathBuf::from(key);
            if meta_path.exists() {
                if let Ok(meta_raw) = std::fs::read_to_string(&meta_path) {
                    if let Ok(meta) = serde_json::from_str::<Value>(&meta_raw) {
                        let workflow_run_id = meta_workflow_run_id(unit.meta.as_ref());
                        if let Some(run_id) = workflow_run_id {
                            records.push(TranscriptRecord::WorkflowAgent(
                                crate::providers::types::WorkflowAgentRecord {
                                    agent_id: agent_id.clone(),
                                    run_id,
                                    session_id: unit.session_id.clone(),
                                    agent_type: meta
                                        .get("agentType")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    description: meta
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    phase: None,
                                    label: None,
                                    model: None,
                                    state: None,
                                    duration_ms: None,
                                    tokens: None,
                                    tool_calls: None,
                                },
                            ));
                        } else {
                            let started = subagent_stats.0.as_deref().and_then(parse_iso_ms);
                            let ended = subagent_stats.1.as_deref().and_then(parse_iso_ms);
                            records.push(TranscriptRecord::Subagent(
                                crate::providers::types::SubagentRecord {
                                    agent_id: agent_id.clone(),
                                    session_id: unit.session_id.clone(),
                                    parent_tool_use_id: meta
                                        .get("toolUseId")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    agent_type: meta
                                        .get("agentType")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    description: meta
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                    duration_ms: match (started, ended) {
                                        (Some(started), Some(ended)) => Some(ended - started),
                                        _ => None,
                                    },
                                    total_tokens: Some(subagent_stats.2),
                                },
                            ));
                        }
                    }
                }
            }
        }
    }

    // Subagent transcripts do not own a session row.
    if !is_subagent {
        records.push(TranscriptRecord::Session(
            crate::providers::types::SessionRecord {
                id: unit.session_id.clone(),
                title: sm.title.clone(),
                project: unit.project.clone(),
                started_at: sm.started_at.clone(),
                ended_at: sm.ended_at.clone(),
                git_branch: sm.git_branch.clone(),
                version: sm.version.clone(),
                message_count: sm.n,
                count_mode: if skip > 0 {
                    SessionCountMode::Delta
                } else {
                    SessionCountMode::Total
                },
                jsonl_path: unit.key.clone(),
                source: "claude".to_string(),
            },
        ));
    }

    let out_cursor = format!("{mtime}:{cursor_lines}:{size}:{ctime}:{ino}");
    let mut items: Vec<StreamItem> = records.into_iter().map(StreamItem::Record).collect();
    items.push(StreamItem::Cursor(out_cursor));
    items
}

fn parse_iso_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn raw_claude(input: &RawLookup) -> Option<RawRecord> {
    let session = input.session?;
    let main_path = session.get("jsonl_path").and_then(Value::as_str)?;
    let main_path = PathBuf::from(main_path);
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let source_path = if let Some(agent_id) = input.agent_id {
        let run_id = input
            .workflow_agent
            .and_then(|wf| wf.get("run_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        match run_id {
            Some(run_id) => main_path
                .parent()?
                .join(&session_id)
                .join("subagents")
                .join("workflows")
                .join(run_id)
                .join(format!("{agent_id}.jsonl")),
            None => main_path
                .parent()?
                .join(&session_id)
                .join("subagents")
                .join(format!("{agent_id}.jsonl")),
        }
    } else {
        main_path
    };
    if !source_path.exists() {
        return None;
    }
    let mut found: Option<String> = None;
    let _ = read_lines(&source_path, |line, _terminated| {
        if !line.contains(input.message_uuid) {
            return true;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(line) {
            if obj.get("uuid").and_then(Value::as_str) == Some(input.message_uuid) {
                found = Some(line.to_string());
                return false;
            }
        }
        true
    });
    let raw = found?;
    let mut message_text: Option<String> = None;
    if let Ok(obj) = serde_json::from_str::<Value>(&raw) {
        let content = obj.get("message").and_then(|m| m.get("content"));
        match content {
            Some(Value::String(s)) => message_text = Some(s.clone()),
            Some(Value::Array(parts)) => {
                let texts: Vec<String> = parts
                    .iter()
                    .filter_map(|part| {
                        part.get("text")
                            .or_else(|| part.get("thinking"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect();
                if !texts.is_empty() {
                    message_text = Some(texts.join("\n"));
                }
            }
            _ => {}
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

pub struct ClaudeProvider {
    pub root_dir: PathBuf,
}

impl ClaudeProvider {
    pub fn new(root_dir: PathBuf) -> Self {
        Self { root_dir }
    }
}

impl ProviderAdapter for ClaudeProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: NAME,
            name: "Claude Code",
            vendor: "Anthropic",
            default_root: self.root_dir.to_string_lossy().into_owned(),
            color: "#d97757",
            requires_explicit_root: false,
            root_resolution_reason: None,
        }
    }

    fn index_version_marker(&self) -> Option<&'static str> {
        Some(CLAUDE_CANONICAL_TRANSCRIPT_MARKER)
    }

    fn watch_targets(&self, configured_root: &str) -> Vec<WatchTarget> {
        vec![
            WatchTarget {
                kind: WatchTargetKind::Tree,
                path: Path::new(configured_root)
                    .join("projects")
                    .to_string_lossy()
                    .into_owned(),
            },
            WatchTarget {
                kind: WatchTargetKind::File,
                path: Path::new(configured_root)
                    .join("history.jsonl")
                    .to_string_lossy()
                    .into_owned(),
            },
        ]
    }

    fn discover<'a>(&'a self, ctx: &mut DiscoverContext<'a>) -> Vec<IndexUnit> {
        discover_at(&self.root_dir, ctx)
    }

    fn parse<'a>(&'a self, unit: &'a IndexUnit, cursor: Cursor) -> ParseStream<'a> {
        let items = parse(unit, cursor);
        Box::new(items.into_iter())
    }

    fn raw(&self, input: &RawLookup) -> Option<RawRecord> {
        raw_claude(input)
    }
}
