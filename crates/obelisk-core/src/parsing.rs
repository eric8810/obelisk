// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Core's pure parse/discover helpers (port of packages/core/src/parsing.ts).
//! No database access by construction; limited to std::fs/path only.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::providers::types::{IndexUnit, InventoryIssue};

pub const TEXT_LIMIT: usize = 10000;

pub fn claude_dir(home: &Path) -> PathBuf {
    home.join(".claude")
}

pub fn codex_dir(home: &Path) -> PathBuf {
    home.join(".codex")
}

pub fn claude_projects_dir(home: &Path) -> PathBuf {
    claude_dir(home).join("projects")
}

pub fn codex_sessions_dir(home: &Path) -> PathBuf {
    codex_dir(home).join("sessions")
}

pub fn source_inventory_issue(path: &str, error: &std::io::Error) -> InventoryIssue {
    InventoryIssue {
        path: path.to_string(),
        error: error.to_string(),
    }
}

// ---- message/text helpers ----

/// TS truncates by UTF-16 code units at `TEXT_LIMIT`. Rust has no lone
/// surrogates, so a char that would straddle the boundary is cut whole; the
/// difference is at most one code unit in a 10000-unit truncation.
pub fn trunc(s: Option<&str>) -> Option<String> {
    match s {
        Some(s) if s.chars().map(char::len_utf16).sum::<usize>() > TEXT_LIMIT => {
            let mut out = String::with_capacity(TEXT_LIMIT);
            let mut units = 0usize;
            for ch in s.chars() {
                units += ch.len_utf16();
                if units > TEXT_LIMIT {
                    break;
                }
                out.push(ch);
            }
            Some(out)
        }
        other => other.map(str::to_string),
    }
}

/// Deep-truncate long strings inside a JSON value, then serialize.
/// Mirrors TS `truncJson`: strings longer than `limit` become
/// `"<prefix>...[truncated]"`; arrays/objects are walked recursively.
pub fn trunc_json(value: &serde_json::Value, limit: usize) -> Option<String> {
    fn walk(value: &serde_json::Value, limit: usize) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => {
                if s.chars().map(char::len_utf16).sum::<usize>() > limit {
                    let mut out = String::new();
                    let mut units = 0usize;
                    for ch in s.chars() {
                        units += ch.len_utf16();
                        if units > limit {
                            break;
                        }
                        out.push(ch);
                    }
                    out.push_str("...[truncated]");
                    serde_json::Value::String(out)
                } else {
                    value.clone()
                }
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(|v| walk(v, limit)).collect())
            }
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), walk(v, limit));
                }
                serde_json::Value::Object(out)
            }
            other => other.clone(),
        }
    }
    match value {
        serde_json::Value::Null => None,
        other => Some(walk(other, limit).to_string()),
    }
}

pub fn trunc_json_default(value: &serde_json::Value) -> Option<String> {
    trunc_json(value, TEXT_LIMIT)
}

pub fn extract_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => trunc(Some(s)),
        serde_json::Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                let block = match block.as_object() {
                    Some(block) => block,
                    None => continue,
                };
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                } else if block.get("type").and_then(|v| v.as_str()) == Some("thinking") {
                    if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                trunc(Some(&parts.join("\n")))
            }
        }
        _ => None,
    }
}

pub fn extract_content_type(content: &serde_json::Value) -> &'static str {
    match content {
        serde_json::Value::String(_) => "text",
        serde_json::Value::Array(blocks) if !blocks.is_empty() => {
            let mut saw_text = false;
            let mut saw_thinking = false;
            let mut saw_tool_use = false;
            let mut saw_tool_result = false;
            let mut saw_unknown = false;
            for block in blocks {
                let block = match block.as_object() {
                    Some(block) => block,
                    None => {
                        saw_unknown = true;
                        continue;
                    }
                };
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => saw_text = true,
                    Some("thinking") => saw_thinking = true,
                    Some("tool_use") => saw_tool_use = true,
                    Some("tool_result") => saw_tool_result = true,
                    _ => saw_unknown = true,
                }
            }
            if !saw_unknown && saw_text && !saw_thinking && !saw_tool_use && !saw_tool_result {
                "text"
            } else if !saw_unknown && saw_thinking && !saw_text && !saw_tool_use && !saw_tool_result
            {
                "thinking"
            } else if !saw_unknown && saw_tool_use && !saw_text && !saw_thinking && !saw_tool_result
            {
                "tool_use"
            } else if !saw_unknown && saw_tool_result && !saw_text && !saw_thinking && !saw_tool_use
            {
                "tool_result"
            } else {
                "unknown"
            }
        }
        _ => "unknown",
    }
}

/// `/^\s*(SELECT|WITH)\b/i` — the sql() read-only entry contract.
pub fn readonly_prefix_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::RegexBuilder::new(r"^\s*(SELECT|WITH)\b")
            .case_insensitive(true)
            .build()
            .expect("readonly prefix regex compiles")
    })
}

/// `/[\p{Letter}\p{Number}]+/gu` — safe FTS tokenization.
pub fn token_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[\p{L}\p{N}]+").expect("token regex compiles"))
}

pub fn command_envelope_text(text: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"^\s*(<command-name>[^<]+</command-name>|<(?:task-notification|system-reminder)\b|<local-command(?:\b|-))",
        )
        .expect("COMMAND_ENVELOPE_RE compiles")
    });
    re.is_match(text)
}

pub fn is_skill_instructions(text: Option<&str>) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^\s*Base directory for this skill(?:\s*:|\s*\r?\n)")
            .expect("SKILL_INSTRUCTIONS_RE compiles")
    });
    text.map(|t| re.is_match(t)).unwrap_or(false)
}

/// `isMeta` from the record or an envelope-shaped text body.
pub fn extract_message_is_meta(record: &serde_json::Value, text: Option<&str>) -> bool {
    let record_flag = record
        .get("isMeta")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let msg_flag = record
        .get("message")
        .and_then(|m| m.get("isMeta"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if record_flag || msg_flag {
        return true;
    }
    text.map(command_envelope_text).unwrap_or(false)
}

pub fn tool_file_path(name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    match name {
        "Read" | "Edit" | "Write" | "NotebookEdit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

pub fn is_dir(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// Stream lines of a file to `callback(line, terminated)`. Returning `false`
/// stops the read early (mirrors TS `readLines`). The final chunk without a
/// trailing newline is delivered with `terminated = false` — it may still be
/// growing, so the caller decides what that means.
pub fn read_lines<F>(path: &Path, mut callback: F) -> std::io::Result<()>
where
    F: FnMut(&str, bool) -> bool,
{
    let file = fs::File::open(path)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    loop {
        let mut line = String::new();
        let bytes = read_line_lossy(&mut reader, &mut line)?;
        if bytes == 0 {
            return Ok(());
        }
        let terminated = line.ends_with('\n');
        if terminated {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        // TS readLines skips empty lines entirely (`if (line && ...)`) —
        // line numbering counts non-empty lines only.
        if !line.is_empty() && !callback(&line, terminated) {
            return Ok(());
        }
    }
}

/// Read one line including its '\n' if present. Non-UTF-8 bytes are replaced
/// (TS Buffer.toString('utf8') replaces invalid sequences too).
fn read_line_lossy<R: BufRead>(reader: &mut R, out: &mut String) -> std::io::Result<usize> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break;
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                bytes.extend_from_slice(&available[..=pos]);
                reader.consume(pos + 1);
                break;
            }
            None => {
                bytes.extend_from_slice(available);
                let len = available.len();
                reader.consume(len);
            }
        }
    }
    let count = bytes.len();
    out.push_str(&String::from_utf8_lossy(&bytes));
    Ok(count)
}

// ---- stat signature (cursor change detection) ----

/// (mtime_ms, size_bytes, ctime_ms, inode). ctime/inode are Unix-only
/// (Windows tier-2: ctime falls back to creation time when available,
/// otherwise 0; inode 0).
pub fn file_signature(path: &Path) -> std::io::Result<(f64, i64, f64, u64)> {
    let metadata = fs::metadata(path)?;
    let mtime_ms = system_time_to_ms(metadata.modified().ok());
    #[cfg(unix)]
    let (ctime_ms, ino) = {
        use std::os::unix::fs::MetadataExt;
        let ctime = metadata.ctime() as f64 + metadata.ctime_nsec() as f64 / 1e9;
        (ctime * 1000.0, metadata.ino())
    };
    #[cfg(not(unix))]
    let (ctime_ms, ino) = (0.0, 0u64);
    Ok((mtime_ms, metadata.len() as i64, ctime_ms, ino))
}

pub fn file_mtime_ms(path: &Path) -> std::io::Result<f64> {
    Ok(system_time_to_ms(fs::metadata(path)?.modified().ok()))
}

fn system_time_to_ms(t: Option<std::time::SystemTime>) -> f64 {
    t.map(|t| {
        let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        d.as_secs() as f64 * 1000.0 + f64::from(d.subsec_nanos()) / 1e6
    })
    .unwrap_or(0.0)
}

// ---- project-path + discovery helpers ----

pub fn legacy_project_path_from_slug(project: Option<&str>) -> Option<String> {
    let project = project?;
    if project.is_empty() {
        return None;
    }
    let replaced = project.replace('-', "/");
    Some(format!("/{}", replaced.trim_start_matches('/')))
}

pub fn normalize_observed_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    if cwd.trim().is_empty() || !Path::new(cwd).is_absolute() {
        return None;
    }
    Some(normalize_path(cwd))
}

/// Lexical path normalization approximating Node's `path.normalize`.
pub fn normalize_path(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let absolute = p.starts_with('/');
    for segment in p.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if !parts.is_empty() && *parts.last().expect("checked") != ".." {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

pub fn project_slug_from_path(project_path: Option<&str>) -> Option<String> {
    let normalized = normalize_observed_cwd(project_path)?;
    let trimmed = normalized.trim_start_matches(['/', '\\']);
    Some(format!("-{}", trimmed.replace(['/', '\\'], "-")))
}

/// Most frequent observed cwd wins; ties resolve to first observed; with no
/// cwds, fall back to the legacy dash-decoded project slug.
pub fn infer_project_path(project: Option<&str>, observed_cwds: &[Option<&str>]) -> Option<String> {
    let mut by_path: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    for cwd in observed_cwds {
        if let Some(normalized) = normalize_observed_cwd(cwd.as_deref()) {
            let next_index = by_path.len();
            let entry = by_path.entry(normalized).or_insert((0, next_index));
            entry.0 += 1;
        }
    }
    let best = by_path
        .into_iter()
        .min_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.1 .1.cmp(&b.1 .1)))
        .map(|(path, _)| path);
    best.or_else(|| legacy_project_path_from_slug(project))
}

/// One discovered Claude transcript/workflow/subagent file.
#[derive(Debug, Clone)]
pub struct ClaudeJsonlFile {
    pub path: PathBuf,
    pub session_id: String,
    pub project: String,
    pub is_subagent: bool,
    pub agent_id: Option<String>,
    pub workflow_run_id: Option<String>,
}

pub fn sorted_read_dir(dir: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        entries.push((
            entry.file_name().to_string_lossy().into_owned(),
            entry.path(),
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

pub fn discover_jsonl_files(
    projects_dir: &Path,
    mut report_issue: Option<&mut dyn FnMut(InventoryIssue)>,
) -> Vec<ClaudeJsonlFile> {
    let mut files = Vec::new();
    if !projects_dir.exists() {
        return files;
    }
    let projects = match sorted_read_dir(projects_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if let Some(report) = report_issue.as_deref_mut() {
                report(source_inventory_issue(
                    &projects_dir.to_string_lossy(),
                    &error,
                ));
            }
            return files;
        }
    };
    for (proj, proj_path) in projects {
        if !is_dir(&proj_path) {
            continue;
        }
        let entries = match sorted_read_dir(&proj_path) {
            Ok(entries) => entries,
            Err(error) => {
                if let Some(report) = report_issue.as_deref_mut() {
                    report(source_inventory_issue(&proj_path.to_string_lossy(), &error));
                }
                continue;
            }
        };
        for (f, path) in &entries {
            if let Some(session_id) = f.strip_suffix(".jsonl") {
                files.push(ClaudeJsonlFile {
                    path: path.clone(),
                    session_id: session_id.to_string(),
                    project: proj.clone(),
                    is_subagent: false,
                    agent_id: None,
                    workflow_run_id: None,
                });
            }
        }
        for (sd, sd_path) in &entries {
            let sa_dir = sd_path.join("subagents");
            if !is_dir(&sa_dir) {
                continue;
            }
            let sa_entries = match sorted_read_dir(&sa_dir) {
                Ok(entries) => entries,
                Err(error) => {
                    if let Some(report) = report_issue.as_deref_mut() {
                        report(source_inventory_issue(&sa_dir.to_string_lossy(), &error));
                    }
                    continue;
                }
            };
            for (sf, sf_path) in &sa_entries {
                if let Some(agent_id) = sf.strip_suffix(".jsonl") {
                    files.push(ClaudeJsonlFile {
                        path: sf_path.clone(),
                        session_id: sd.clone(),
                        project: proj.clone(),
                        is_subagent: true,
                        agent_id: Some(agent_id.to_string()),
                        workflow_run_id: None,
                    });
                }
            }
            let wf_root = sa_dir.join("workflows");
            if !is_dir(&wf_root) {
                continue;
            }
            let wf_dirs = match sorted_read_dir(&wf_root) {
                Ok(entries) => entries,
                Err(error) => {
                    if let Some(report) = report_issue.as_deref_mut() {
                        report(source_inventory_issue(&wf_root.to_string_lossy(), &error));
                    }
                    continue;
                }
            };
            for (wf_dir, wf_path) in &wf_dirs {
                if !is_dir(wf_path) {
                    continue;
                }
                let wf_entries = match sorted_read_dir(wf_path) {
                    Ok(entries) => entries,
                    Err(error) => {
                        if let Some(report) = report_issue.as_deref_mut() {
                            report(source_inventory_issue(&wf_path.to_string_lossy(), &error));
                        }
                        continue;
                    }
                };
                for (wf, wf_file) in &wf_entries {
                    if let Some(agent_id) = wf.strip_suffix(".jsonl") {
                        files.push(ClaudeJsonlFile {
                            path: wf_file.clone(),
                            session_id: sd.clone(),
                            project: proj.clone(),
                            is_subagent: true,
                            agent_id: Some(agent_id.to_string()),
                            workflow_run_id: Some(wf_dir.clone()),
                        });
                    }
                }
            }
        }
    }
    files
}

pub fn discover_codex_jsonl_files(
    sessions_dir: &Path,
    mut report_issue: Option<&mut dyn FnMut(InventoryIssue)>,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !sessions_dir.exists() {
        return files;
    }
    fn walk(
        dir: &Path,
        files: &mut Vec<PathBuf>,
        report_issue: &mut Option<&mut dyn FnMut(InventoryIssue)>,
    ) {
        let entries = match sorted_read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                if let Some(report) = report_issue.as_deref_mut() {
                    report(source_inventory_issue(&dir.to_string_lossy(), &error));
                }
                return;
            }
        };
        for (name, path) in entries {
            if path.is_dir() {
                walk(&path, files, report_issue);
            } else if name.ends_with(".jsonl") && path.is_file() {
                files.push(path);
            }
        }
    }
    walk(sessions_dir, &mut files, &mut report_issue);
    files
}

// ---- Codex pure helpers ----

pub fn codex_db_id(id: &serde_json::Value) -> Option<String> {
    let id = id.as_str()?;
    if id.is_empty() {
        return None;
    }
    Some(format!("codex:{}", id.trim_start_matches("codex:")))
}

pub fn codex_raw_id(id: &serde_json::Value) -> Option<String> {
    let id = id.as_str()?;
    if id.is_empty() {
        return None;
    }
    Some(id.trim_start_matches("codex:").to_string())
}

pub fn codex_line_uuid(thread_id: &serde_json::Value, line_num: usize) -> String {
    format!(
        "codex:{}:{:06}",
        codex_raw_id(thread_id).unwrap_or_default(),
        line_num
    )
}

pub fn codex_call_id(thread_id: &serde_json::Value, call_id: &serde_json::Value) -> Option<String> {
    let thread_id = codex_raw_id(thread_id)?;
    let call_id = call_id.as_str()?;
    if call_id.is_empty() {
        return None;
    }
    Some(format!(
        "codex:{}:{}",
        thread_id,
        call_id.trim_start_matches("codex:")
    ))
}

pub fn codex_parent_thread_id(meta: &serde_json::Value) -> Option<String> {
    let subagent = meta.get("source").and_then(|s| s.get("subagent"));
    subagent
        .and_then(|s| s.get("thread_spawn"))
        .and_then(|t| t.get("parent_thread_id"))
        .and_then(|v| v.as_str())
        .or_else(|| meta.get("forked_from_id").and_then(|v| v.as_str()))
        .or_else(|| {
            subagent
                .and_then(|s| s.get("parent_thread_id"))
                .and_then(|v| v.as_str())
        })
        .map(str::to_string)
}

pub fn codex_is_guardian_thread(
    meta: &serde_json::Value,
    records: &[(usize, serde_json::Value)],
) -> bool {
    let subagent = meta.get("source").and_then(|s| s.get("subagent"));
    if subagent
        .and_then(|s| s.get("other"))
        .and_then(|v| v.as_str())
        == Some("guardian")
    {
        return true;
    }
    if meta.get("thread_source").and_then(|v| v.as_str()) != Some("subagent") {
        return false;
    }
    records.iter().any(|(_, obj)| {
        obj.get("payload")
            .and_then(|p| p.get("model"))
            .and_then(|v| v.as_str())
            == Some("codex-auto-review")
            || obj.get("model").and_then(|v| v.as_str()) == Some("codex-auto-review")
    })
}

/// Read a Codex transcript far enough to decide whether it is a guardian
/// thread, returning (raw thread id, meta line number) when it is.
pub fn read_codex_guardian_thread_info(file_path: &Path) -> Option<(String, usize)> {
    let mut records: Vec<(usize, serde_json::Value)> = Vec::new();
    let mut meta_record: Option<(usize, serde_json::Value)> = None;
    let mut line_num = 0usize;
    let _ = read_lines(file_path, |line, _terminated| {
        line_num += 1;
        // Malformed lines are skipped (TS: the callback returns early).
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            return true;
        };
        records.push((line_num, obj.clone()));
        if obj.get("type").and_then(|v| v.as_str()) == Some("session_meta")
            && obj.get("payload").and_then(|p| p.get("id")).is_some()
        {
            // The meta record is captured unconditionally; the two early
            // exits below only stop reading further lines.
            meta_record = Some((line_num, obj.clone()));
            let meta = obj.get("payload").expect("checked");
            if meta
                .get("source")
                .and_then(|s| s.get("subagent"))
                .and_then(|s| s.get("other"))
                .and_then(|v| v.as_str())
                == Some("guardian")
            {
                return false;
            }
            if meta.get("thread_source").and_then(|v| v.as_str()) != Some("subagent") {
                return false;
            }
        }
        if let Some((_, meta_obj)) = &meta_record {
            if codex_is_guardian_thread(
                meta_obj.get("payload").unwrap_or(&serde_json::Value::Null),
                &records,
            ) {
                return false;
            }
        }
        true
    });
    let (meta_line, meta_obj) = meta_record?;
    let meta = meta_obj.get("payload")?;
    if !codex_is_guardian_thread(meta, &records) {
        return None;
    }
    let thread_raw_id = codex_raw_id(meta.get("id").unwrap_or(&serde_json::Value::Null))?;
    Some((thread_raw_id, meta_line))
}

pub fn codex_agent_nickname(meta: &serde_json::Value) -> Option<String> {
    meta.get("agent_nickname")
        .and_then(|v| v.as_str())
        .or_else(|| {
            meta.get("source")
                .and_then(|s| s.get("subagent"))
                .and_then(|s| s.get("thread_spawn"))
                .and_then(|t| t.get("agent_nickname"))
                .and_then(|v| v.as_str())
        })
        .map(str::to_string)
}

pub fn codex_agent_role(meta: &serde_json::Value) -> Option<String> {
    meta.get("agent_role")
        .and_then(|v| v.as_str())
        .or_else(|| {
            meta.get("source")
                .and_then(|s| s.get("subagent"))
                .and_then(|s| s.get("thread_spawn"))
                .and_then(|t| t.get("agent_role"))
                .and_then(|v| v.as_str())
        })
        .map(str::to_string)
}

pub fn parse_codex_json_input(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Null => serde_json::Value::Object(serde_json::Map::new()),
        serde_json::Value::String(s) if s.is_empty() => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        serde_json::Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.clone()))
        }
        other => other.clone(),
    }
}

pub fn codex_usage(payload: &serde_json::Value) -> (Option<i64>, Option<i64>) {
    let usage = payload
        .get("info")
        .and_then(|i| i.get("last_token_usage"))
        .or_else(|| payload.get("info").and_then(|i| i.get("total_token_usage")))
        .or_else(|| payload.get("last_token_usage"));
    let usage = match usage {
        Some(usage) => usage,
        None => return (None, None),
    };
    let finite = |v: &serde_json::Value| -> Option<i64> {
        v.as_f64().filter(|n| n.is_finite()).map(|n| n as i64)
    };
    (
        finite(
            usage
                .get("input_tokens")
                .unwrap_or(&serde_json::Value::Null),
        ),
        finite(
            usage
                .get("output_tokens")
                .unwrap_or(&serde_json::Value::Null),
        ),
    )
}

pub fn codex_event_text(payload: &serde_json::Value) -> Option<String> {
    if let Some(message) = payload.get("message").and_then(|v| v.as_str()) {
        return Some(message.to_string());
    }
    if let Some(elements) = payload.get("text_elements").and_then(|v| v.as_array()) {
        if !elements.is_empty() {
            let parts: Vec<String> = elements
                .iter()
                .filter_map(|item| {
                    item.as_str().map(str::to_string).or_else(|| {
                        item.get("text")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                })
                .collect();
            if !parts.is_empty() {
                return Some(parts.join("\n"));
            }
        }
    }
    payload
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub fn codex_message_payload_text(payload: &serde_json::Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < content.len() {
        let block = &content[index];
        let image = content.get(index + 1);
        let close = content.get(index + 2);
        if block.get("type").and_then(|v| v.as_str()) == Some("input_text")
            && block.get("text").and_then(|v| v.as_str()).map(str::trim) == Some("<image>")
            && image.and_then(|i| i.get("type")).and_then(|v| v.as_str()) == Some("input_image")
            && close.and_then(|c| c.get("type")).and_then(|v| v.as_str()) == Some("input_text")
            && close
                .and_then(|c| c.get("text"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                == Some("</image>")
        {
            index += 3;
            continue;
        }
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            parts.push(text.to_string());
        }
        index += 1;
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

pub fn codex_visible_message_key(role: Option<&str>, text: Option<&str>) -> String {
    format!("{}\u{0}{}", role.unwrap_or(""), text.unwrap_or(""))
}

pub fn codex_tool_input(payload: &serde_json::Value) -> serde_json::Value {
    match payload.get("type").and_then(|v| v.as_str()) {
        Some("custom_tool_call") => {
            parse_codex_json_input(payload.get("input").unwrap_or(&serde_json::Value::Null))
        }
        Some("tool_search_call") => {
            parse_codex_json_input(payload.get("arguments").unwrap_or(&serde_json::Value::Null))
        }
        Some("web_search_call") => serde_json::json!({
            "action": payload.get("action").cloned().unwrap_or(serde_json::Value::Null),
        }),
        _ => parse_codex_json_input(payload.get("arguments").unwrap_or(&serde_json::Value::Null)),
    }
}

pub fn codex_tool_output(payload: &serde_json::Value) -> Option<String> {
    if let Some(output) = payload.get("output") {
        return match output {
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
    }
    if let Some(tools) = payload.get("tools") {
        return Some(tools.to_string());
    }
    if let Some(execution) = payload.get("execution") {
        return Some(execution.to_string());
    }
    None
}

// ---- JSONL line reading into memory (records with line numbers) ----

/// Read every parseable JSON line of a file with its 1-based line number.
/// Mirrors the TS pattern of `readLines` + JSON.parse with errors skipped.
pub fn read_json_lines(path: &Path) -> std::io::Result<Vec<(usize, serde_json::Value)>> {
    let mut records = Vec::new();
    let mut line_num = 0usize;
    read_lines(path, |line, _terminated| {
        line_num += 1;
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
            records.push((line_num, obj));
        }
        true
    })?;
    Ok(records)
}

#[allow(dead_code)]
fn _index_unit_shape_check(unit: &IndexUnit) -> bool {
    // Compile-time-ish sanity that IndexUnit stays Default-constructible.
    let _ = unit.key.clone();
    true
}
