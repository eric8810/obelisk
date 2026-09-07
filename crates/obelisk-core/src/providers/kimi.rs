// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Kimi Code provider adapter (port of packages/core/src/providers/kimi.ts
//! and kimi-manifest.ts, merged here per ADR-0013).
//!
//! Pure: discovers Kimi session directories and projects one into a record
//! stream. Never touches the Obelisk database.
//!
//! Layout (`$KIMI_CODE_HOME`, default `~/.kimi-code`):
//!   sessions/<workspace>/<session>/state.json
//!   sessions/<workspace>/<session>/agents/<agent>/wire.jsonl
//!   sessions/<workspace>/<session>/wire.jsonl        (legacy main wire)
//!
//! The unit key is the session DIRECTORY, not a file: the manifest cursor is
//! a body-free digest (sha256/base64url over the stat metadata of every
//! member), so an append inside the directory is detected even when mtime is
//! restored. Undo/clear wire events are replayed during projection (messages
//! removed); identity changes fail closed to one replay or a tombstone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::parsing::{normalize_path, trunc, trunc_json_default};
use crate::providers::types::{
    Cursor, DiscoverContext, IndexUnit, IndexedSession, InventoryIssue, MessageRecord,
    MessageVisibility, ParseStream, ProviderAdapter, ProviderDescriptor, RawLookup, RawRecord,
    SessionCountMode, StreamItem, SubagentRecord, SummaryRecord, ToolCallPresentation,
    ToolCallRecord, ToolResultRecord, TranscriptRecord, WatchTarget, WatchTargetKind,
};

pub const NAME: &str = "kimi";
pub const KIMI_CANONICAL_TRANSCRIPT_MARKER: &str = "__kimi_canonical_transcript_v6__";
const KIMI_MANIFEST_CURSOR_FORMAT: &str = "kimi-manifest-v1";

// ---------------------------------------------------------------------------
// kimi-manifest.ts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct KimiWireFile {
    agent_id: String,
    main: bool,
    path: String,
}

#[derive(Debug, Clone)]
struct KimiSessionSnapshot {
    session_dir: String,
    state_path: String,
    wire_files: Vec<KimiWireFile>,
    current_cursor: String,
}

#[derive(Debug, Clone)]
struct KimiManifestMember {
    relative_path: String,
    dev: String,
    ino: String,
    size: String,
    mtime_ns: String,
    ctime_ns: String,
}

struct KimiIdentityWitness {
    session_id: String,
    snapshots: Vec<KimiSessionSnapshot>,
}

/// Only an exact current-format match can prove that a Kimi session is clean.
/// Legacy and unknown cursors fail closed to one replay. true = 'current'.
fn classify_kimi_cursor(stored_cursor: &Cursor, current_cursor: &str) -> bool {
    stored_cursor.as_deref() == Some(current_cursor)
}

/// One directory entry: (name, path, is_directory). Sorted by name.
fn dirents(dir: &Path) -> std::io::Result<Vec<(String, PathBuf, bool)>> {
    let mut entries: Vec<(String, PathBuf, bool)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((
            entry.file_name().to_string_lossy().into_owned(),
            entry.path(),
            is_dir,
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// TS `optionalDirectoryEntries`: ENOENT/ENOTDIR mean "missing" (empty
/// snapshot); any other error propagates.
fn optional_dirents(dir: &Path) -> Option<std::io::Result<Vec<(String, PathBuf, bool)>>> {
    match dirents(dir) {
        Ok(entries) => Some(Ok(entries)),
        Err(error) => match error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => None,
            _ => Some(Err(error)),
        },
    }
}

/// The caller observed this exact entry in a directory listing. Any stat
/// failure, including ENOENT, means the snapshot raced a mutation rather than
/// proving that the member was absent.
fn listed_member(session_dir: &Path, path: &Path) -> Result<KimiManifestMember, String> {
    let relative_path = path
        .strip_prefix(session_dir)
        .map(|rest| rest.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    let (dev, ino, size, mtime_ns, ctime_ns) = {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.dev().to_string(),
            metadata.ino().to_string(),
            metadata.len().to_string(),
            (metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128).to_string(),
            (metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128).to_string(),
        )
    };
    #[cfg(not(unix))]
    let (dev, ino, size, mtime_ns, ctime_ns) = (
        "0".to_string(),
        "0".to_string(),
        metadata.len().to_string(),
        "0".to_string(),
        "0".to_string(),
    );
    Ok(KimiManifestMember {
        relative_path,
        dev,
        ino,
        size,
        mtime_ns,
        ctime_ns,
    })
}

/// Byte-identical to TS `JSON.stringify(members)` — the field order is part
/// of the cursor format contract.
fn members_json(members: &[KimiManifestMember]) -> String {
    fn field(out: &mut String, name: &str, value: &str) {
        out.push_str(name);
        out.push(':');
        out.push_str(&serde_json::to_string(value).unwrap_or_default());
    }
    let mut out = String::from("[");
    for (index, member) in members.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('{');
        field(&mut out, "\"relativePath\"", &member.relative_path);
        out.push(',');
        field(&mut out, "\"dev\"", &member.dev);
        out.push(',');
        field(&mut out, "\"ino\"", &member.ino);
        out.push(',');
        field(&mut out, "\"size\"", &member.size);
        out.push(',');
        field(&mut out, "\"mtimeNs\"", &member.mtime_ns);
        out.push(',');
        field(&mut out, "\"ctimeNs\"", &member.ctime_ns);
        out.push('}');
    }
    out.push(']');
    out
}

fn manifest_cursor(members: &[KimiManifestMember]) -> String {
    let max_mtime_ns: i128 = members
        .iter()
        .map(|member| member.mtime_ns.parse::<i128>().unwrap_or(0))
        .max()
        .unwrap_or(0);
    let digest = base64url(&sha256(members_json(members).as_bytes()));
    let max_mtime_ms = max_mtime_ns / 1_000_000;
    format!("{max_mtime_ms}:0:{KIMI_MANIFEST_CURSOR_FORMAT}:{digest}")
}

fn capture_kimi_session(session_dir: &Path) -> Result<KimiSessionSnapshot, String> {
    let state_path = session_dir.join("state.json");
    let session_entries = match optional_dirents(session_dir) {
        None => {
            return Ok(KimiSessionSnapshot {
                session_dir: session_dir.to_string_lossy().into_owned(),
                state_path: state_path.to_string_lossy().into_owned(),
                wire_files: Vec::new(),
                current_cursor: manifest_cursor(&[]),
            });
        }
        Some(result) => result.map_err(|error| error.to_string())?,
    };

    let mut members: Vec<KimiManifestMember> = Vec::new();
    if session_entries
        .iter()
        .any(|(name, _, _)| name == "state.json")
    {
        members.push(listed_member(session_dir, &state_path)?);
    }

    let mut wire_files: Vec<KimiWireFile> = Vec::new();
    if let Some((_, agents_dir, _)) = session_entries
        .iter()
        .find(|(name, _, is_dir)| name == "agents" && *is_dir)
    {
        let agent_entries = dirents(agents_dir).map_err(|error| error.to_string())?;
        for (agent_name, agent_dir, is_entry_dir) in agent_entries {
            if !is_entry_dir {
                continue;
            }
            let entries = dirents(&agent_dir).map_err(|error| error.to_string())?;
            if !entries.iter().any(|(name, _, _)| name == "wire.jsonl") {
                continue;
            }
            let path = agent_dir.join("wire.jsonl");
            members.push(listed_member(session_dir, &path)?);
            let is_main = agent_name == "main";
            wire_files.push(KimiWireFile {
                agent_id: agent_name,
                main: is_main,
                path: path.to_string_lossy().into_owned(),
            });
        }
    }

    if !wire_files.iter().any(|file| file.main)
        && session_entries
            .iter()
            .any(|(name, _, _)| name == "wire.jsonl")
    {
        let path = session_dir.join("wire.jsonl");
        members.push(listed_member(session_dir, &path)?);
        wire_files.push(KimiWireFile {
            agent_id: "main".to_string(),
            main: true,
            path: path.to_string_lossy().into_owned(),
        });
    }

    members.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    wire_files.sort_by(|a, b| {
        b.main
            .cmp(&a.main)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    Ok(KimiSessionSnapshot {
        session_dir: session_dir.to_string_lossy().into_owned(),
        state_path: state_path.to_string_lossy().into_owned(),
        wire_files,
        current_cursor: manifest_cursor(&members),
    })
}

/// Capture one deterministic, body-free view of the Kimi session members.
/// The double capture rejects a member added between the two listings.
fn snapshot_kimi_session(session_dir: &Path) -> Result<KimiSessionSnapshot, String> {
    let before = capture_kimi_session(session_dir)?;
    let after = capture_kimi_session(session_dir)?;
    if before.current_cursor != after.current_cursor {
        return Err(format!(
            "Kimi session changed while snapshotting: {}",
            session_dir.to_string_lossy()
        ));
    }
    Ok(after)
}

// ---------------------------------------------------------------------------
// SHA-256 + base64url (no hash crate is available to this package; vendored
// so the manifest cursor stays byte-compatible with the TS
// `createHash('sha256')…digest('base64url')`).
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

// pub(super): exercised by the sibling test module, not part of the
// provider's public surface.
pub(super) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub(super) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).map_or(0, |b| *b as u32);
        let b2 = chunk.get(2).map_or(0, |b| *b as u32);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Wire projection helpers (kimi.ts)
// ---------------------------------------------------------------------------

fn read_state(path: &Path) -> Value {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return json!({});
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(value) if value.is_object() => value,
        _ => json!({}),
    }
}

/// Read wire.jsonl into (line, record) pairs. A corrupted line errors —
/// except an unterminated final line, which is a torn tail and is skipped.
fn read_wire(path: &Path) -> Result<Vec<(usize, Value)>, String> {
    let mut records: Vec<(usize, Value)> = Vec::new();
    let mut line_num = 0usize;
    let mut error: Option<String> = None;
    let read = crate::parsing::read_lines(path, |line, terminated| {
        line_num += 1;
        if line.is_empty() {
            return true;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(record) => records.push((line_num, record)),
            Err(err) => {
                if !terminated {
                    // Torn final line: it may still be growing.
                    return false;
                }
                error = Some(format!(
                    "wire.jsonl: corrupted line {line_num} in {}: {err}",
                    path.to_string_lossy()
                ));
                return false;
            }
        }
        true
    });
    if let Some(error) = error {
        return Err(error);
    }
    if let Err(error) = read {
        return Err(error.to_string());
    }
    Ok(records)
}

fn normalize_time(value: &Value) -> Option<String> {
    if let Some(number) = value.as_f64() {
        if number.is_finite() {
            return chrono::DateTime::from_timestamp_millis(number as i64)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        }
        return None;
    }
    if let Some(text) = value.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
            return Some(dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        }
        // TS Date.parse treats a bare YYYY-MM-DD as UTC midnight.
        if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
            return date
                .and_hms_opt(0, 0, 0)
                .and_then(|ndt| {
                    chrono::DateTime::from_timestamp_millis(ndt.and_utc().timestamp_millis())
                })
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        }
    }
    None
}

/// TS `contentParts`: a string becomes one synthetic text part; an array
/// keeps every non-null object member (arrays inside pass typeof 'object').
fn content_parts(content: &Value) -> Vec<&Value> {
    match content {
        Value::Array(parts) => parts
            .iter()
            .filter(|part| !part.is_null() && (part.is_object() || part.is_array()))
            .collect(),
        _ => Vec::new(),
    }
}

fn raw_part_text(part: &Value) -> Option<&str> {
    match part.get("type").and_then(Value::as_str) {
        Some("text") => part.get("text").and_then(Value::as_str),
        Some("think") => part.get("think").and_then(Value::as_str),
        Some("thinking") => part.get("thinking").and_then(Value::as_str),
        _ => None,
    }
}

fn part_text(part: &Value) -> Option<String> {
    raw_part_text(part).map(|text| trunc(Some(text)).unwrap_or_default())
}

fn part_content_type(part: &Value) -> String {
    match part.get("type").and_then(Value::as_str) {
        Some("think") | Some("thinking") => "thinking".to_string(),
        Some(kind) => kind.to_string(),
        None => "unknown".to_string(),
    }
}

fn message_text(content: &Value) -> Option<String> {
    let texts: Vec<String> = if let Value::String(text) = content {
        vec![text.clone()]
    } else {
        content_parts(content)
            .iter()
            .filter_map(|part| part_text(part))
            .collect()
    };
    if texts.is_empty() {
        None
    } else {
        trunc(Some(&texts.join("\n")))
    }
}

fn message_content_type(content: &Value) -> String {
    let mut types: Vec<String> = Vec::new();
    if content.is_string() {
        types.push("text".to_string());
    } else {
        for part in content_parts(content) {
            let kind = part_content_type(part);
            if !types.contains(&kind) {
                types.push(kind);
            }
        }
    }
    if types.len() == 1 {
        types.into_iter().next().unwrap_or_else(|| "unknown".into())
    } else {
        "unknown".to_string()
    }
}

fn namespaced_session_id(native_id: &str) -> String {
    format!("kimi:{native_id}")
}

fn namespaced_agent_id(session_id: &str, agent_id: &str) -> String {
    format!("{session_id}:{agent_id}")
}

fn namespaced_event_id(session_id: &str, agent_id: &str, native_id: &Value, line: usize) -> String {
    let suffix = native_id
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("line-{line}"));
    format!("{session_id}:{agent_id}:{suffix}")
}

/// TS `String(nativeId)`: null → 'null', numbers → decimal text.
fn native_id_string(native_id: &Value) -> String {
    match native_id {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn namespaced_tool_id(session_id: &str, agent_id: &str, native_id: &Value) -> String {
    format!("{session_id}:{agent_id}:{}", native_id_string(native_id))
}

fn numeric_field(record: &Value, fields: &[&str]) -> Option<f64> {
    for field in fields {
        if let Some(value) = record.get(*field).and_then(Value::as_f64) {
            if value.is_finite() {
                return Some(value);
            }
        }
    }
    None
}

fn input_usage(usage: &Value) -> Option<i64> {
    if let Some(value) = numeric_field(usage, &["input_tokens", "inputTokens"]) {
        return Some(value as i64);
    }
    let values: Vec<Option<f64>> = ["inputOther", "inputCacheRead", "inputCacheCreation"]
        .iter()
        .map(|field| numeric_field(usage, &[field]))
        .collect();
    if values.iter().any(|value| value.is_some()) {
        Some(values.iter().map(|value| value.unwrap_or(0.0)).sum::<f64>() as i64)
    } else {
        None
    }
}

fn output_usage(usage: &Value) -> Option<i64> {
    numeric_field(usage, &["output_tokens", "outputTokens", "output"]).map(|v| v as i64)
}

fn is_real_user_message(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(origin) = message.get("origin") else {
        return true;
    };
    let kind = origin.get("kind").and_then(Value::as_str);
    if kind.is_none() || kind == Some("user") {
        return true;
    }
    (kind == Some("skill_activation") || kind == Some("plugin_command"))
        && origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
}

fn slash_command_text(command: String, args: &Value) -> String {
    let trimmed = match args.as_str() {
        Some(text) => text.trim(),
        None => "",
    };
    if !trimmed.is_empty() {
        format!("{command} {trimmed}")
    } else {
        command
    }
}

fn user_slash_command_text(message: &Value) -> Option<String> {
    let origin = message.get("origin")?;
    if message.get("role").and_then(Value::as_str) != Some("user")
        || origin.get("trigger").and_then(Value::as_str) != Some("user-slash")
    {
        return None;
    }
    let kind = origin.get("kind").and_then(Value::as_str);
    if kind == Some("skill_activation") {
        if let Some(skill_name) = origin.get("skillName").and_then(Value::as_str) {
            return Some(slash_command_text(
                format!("/{skill_name}"),
                origin.get("skillArgs").unwrap_or(&Value::Null),
            ));
        }
    }
    if kind == Some("plugin_command") {
        if let (Some(plugin_id), Some(command_name)) = (
            origin.get("pluginId").and_then(Value::as_str),
            origin.get("commandName").and_then(Value::as_str),
        ) {
            return Some(slash_command_text(
                format!("/{plugin_id}:{command_name}"),
                origin.get("commandArgs").unwrap_or(&Value::Null),
            ));
        }
    }
    None
}

fn projected_message_text(message: &Value) -> Option<String> {
    match user_slash_command_text(message) {
        Some(command) => trunc(Some(&command)),
        None => message_text(message.get("content").unwrap_or(&Value::Null)),
    }
}

fn is_meta_message(message: &Value) -> bool {
    match message.get("origin") {
        None => false,
        Some(origin) => {
            let kind = origin.get("kind").and_then(Value::as_str);
            if kind.is_none() || kind == Some("user") {
                return false;
            }
            !is_real_user_message(message)
        }
    }
}

fn canonical_message_content_type(message: &Value) -> String {
    let is_skill_activation = message
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        == Some("skill_activation");
    if is_skill_activation && !is_real_user_message(message) {
        "skill_instructions".to_string()
    } else {
        message_content_type(message.get("content").unwrap_or(&Value::Null))
    }
}

/// TS `filePath(name, input)`: falsy input → null; the tool must be a file
/// tool; `input.file_path || null` (an empty string is falsy).
fn kimi_file_path(name: &str, input: Option<&Value>) -> Option<String> {
    let input = input?;
    if input.is_null() {
        return None;
    }
    if let Value::String(text) = input {
        if text.is_empty() {
            return None;
        }
    }
    if !matches!(name, "Read" | "Edit" | "Write" | "NotebookEdit") {
        return None;
    }
    input
        .get("file_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
}

fn child_agent_id_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?m)^agent_id:\s*(\S+)").expect("child id regex"))
}

// ---------------------------------------------------------------------------
// Session projection (kimi.ts projectSession)
// ---------------------------------------------------------------------------

struct ProjectedSession {
    messages: Vec<MessageRecord>,
    tool_calls: Vec<ToolCallRecord>,
    tool_results: Vec<ToolResultRecord>,
    summaries: Vec<SummaryRecord>,
    subagents: Vec<SubagentRecord>,
    /// (message uuid, turn_duration_ms) — TS {kind:'message-turn-duration'}.
    durations: Vec<(String, i64)>,
    main_message_count: i64,
    main_wire_path: String,
}

#[derive(Default)]
struct WireProjectionState {
    step_starts: HashMap<String, f64>,
    step_messages: HashMap<String, Vec<String>>,
    call_message_uuids: HashMap<String, String>,
    injection_owner_prompt_ids: HashMap<String, Option<String>>,
    real_user_prompt_ids: HashMap<String, Option<String>>,
}

#[allow(clippy::too_many_arguments)]
fn push_message(
    messages: &mut Vec<MessageRecord>,
    main_message_count: &mut i64,
    previous_uuid: &mut Option<String>,
    open: &mut WireProjectionState,
    wire_main: bool,
    step_uuid: Option<&str>,
    message: MessageRecord,
) {
    if wire_main {
        *main_message_count += 1;
    }
    if let Some(step_uuid) = step_uuid {
        open.step_messages
            .entry(step_uuid.to_string())
            .or_default()
            .push(message.uuid.clone());
    }
    *previous_uuid = Some(message.uuid.clone());
    messages.push(message);
}

/// Replay one `context.undo`: remove the last `count` real user prompts (plus
/// everything after them), then any injections the last-removed prompt owns
/// that precede it. Tool calls/results and durations attached to removed
/// messages are removed too.
#[allow(clippy::too_many_arguments)]
fn apply_undo(
    count: i64,
    wire_main: bool,
    wire_message_start: usize,
    undo_floor: usize,
    messages: &mut Vec<MessageRecord>,
    tool_calls: &mut Vec<ToolCallRecord>,
    tool_results: &mut Vec<ToolResultRecord>,
    durations: &mut Vec<(String, i64)>,
    main_message_count: &mut i64,
    open: &mut WireProjectionState,
    previous_uuid: &mut Option<String>,
) {
    if count <= 0 {
        return;
    }
    let mut removed_message_uuids: HashSet<String> = HashSet::new();
    let mut removed_user_count = 0i64;
    let mut anchor_prompt_id: Option<String> = None;
    let mut index = messages.len() as i64 - 1;
    while index >= undo_floor as i64 {
        let message_uuid = messages[index as usize].uuid.clone();
        // Once the requested prompts are gone, keep walking back only through
        // the injections that the last-removed prompt owns and that precede it.
        if removed_user_count >= count {
            let owned_by_anchor = match anchor_prompt_id.as_deref() {
                // TS breaks immediately when the anchor is undefined.
                None => false,
                Some(anchor) => {
                    open.injection_owner_prompt_ids
                        .get(&message_uuid)
                        .and_then(|owner| owner.as_deref())
                        == Some(anchor)
                }
            };
            if !owned_by_anchor {
                break;
            }
        }
        messages.remove(index as usize);
        removed_message_uuids.insert(message_uuid.clone());
        open.injection_owner_prompt_ids.remove(&message_uuid);
        if wire_main {
            *main_message_count -= 1;
        }
        if open.real_user_prompt_ids.contains_key(&message_uuid) {
            let prompt_id = open.real_user_prompt_ids.remove(&message_uuid).flatten();
            removed_user_count += 1;
            if removed_user_count >= count {
                anchor_prompt_id = prompt_id;
            }
        }
        index -= 1;
    }
    let removed_tool_ids: HashSet<String> = tool_calls
        .iter()
        .filter(|call| removed_message_uuids.contains(&call.message_uuid))
        .map(|call| call.id.clone())
        .collect();
    tool_calls.retain(|call| !removed_message_uuids.contains(&call.message_uuid));
    tool_results.retain(|result| {
        let message_uuid = result.message_uuid.as_deref().unwrap_or("");
        !removed_message_uuids.contains(message_uuid)
            && !removed_tool_ids.contains(&result.tool_use_id)
    });
    durations.retain(|(uuid, _)| !removed_message_uuids.contains(uuid));
    *previous_uuid = messages
        .get(wire_message_start..)
        .and_then(|slice| slice.last())
        .map(|message| message.uuid.clone());
    open.step_starts.clear();
    open.step_messages.clear();
    open.call_message_uuids.clear();
}

fn project_session(
    wire_files: &[KimiWireFile],
    session_dir: &str,
    session_id: &str,
    state: &Value,
) -> Result<ProjectedSession, String> {
    let cwd: Option<String> = state
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            state
                .get("workDir")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let mut messages: Vec<MessageRecord> = Vec::new();
    let mut tool_calls: Vec<ToolCallRecord> = Vec::new();
    let mut tool_results: Vec<ToolResultRecord> = Vec::new();
    let mut summaries: Vec<SummaryRecord> = Vec::new();
    let mut durations: Vec<(String, i64)> = Vec::new();
    let mut child_parent_calls: HashMap<String, String> = HashMap::new();
    let mut main_message_count: i64 = 0;

    for wire in wire_files {
        let wire_message_start = messages.len();
        let records = read_wire(Path::new(&wire.path))?;
        let agent_db_id = if wire.main {
            None
        } else {
            Some(namespaced_agent_id(session_id, &wire.agent_id))
        };
        let mut previous_uuid: Option<String> = None;
        let mut model: Option<String> = None;
        let mut open = WireProjectionState::default();
        let mut undo_floor = wire_message_start;

        for (line, record) in records {
            let timestamp = record.get("time").and_then(normalize_time);
            let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
            if record_type == "config.update" {
                if let Some(alias) = record.get("modelAlias").and_then(Value::as_str) {
                    model = Some(alias.to_string());
                }
                continue;
            }
            if record_type == "context.clear" {
                undo_floor = messages.len();
                reset_open_state(&mut open);
                continue;
            }
            if record_type == "context.undo" {
                let count = record.get("count").and_then(Value::as_f64).unwrap_or(0.0);
                apply_undo(
                    count as i64,
                    wire.main,
                    wire_message_start,
                    undo_floor,
                    &mut messages,
                    &mut tool_calls,
                    &mut tool_results,
                    &mut durations,
                    &mut main_message_count,
                    &mut open,
                    &mut previous_uuid,
                );
                continue;
            }
            if record_type == "context.append_message" {
                handle_append_message(
                    &record,
                    session_id,
                    &wire.agent_id,
                    wire.main,
                    agent_db_id.as_deref(),
                    cwd.as_deref(),
                    model.clone(),
                    timestamp.clone(),
                    line,
                    &mut messages,
                    &mut tool_calls,
                    &mut tool_results,
                    &mut main_message_count,
                    &mut previous_uuid,
                    &mut open,
                );
                continue;
            }
            if record_type == "context.apply_compaction" {
                let content = match record.get("contextSummary").and_then(Value::as_str) {
                    Some(summary) => Some(summary.to_string()),
                    None => match record.get("summary") {
                        Some(Value::String(summary)) => Some(summary.clone()),
                        Some(summary) => {
                            message_text(summary.get("content").unwrap_or(&Value::Null))
                        }
                        None => None,
                    },
                };
                if let Some(content) = content {
                    summaries.push(SummaryRecord {
                        id: namespaced_event_id(session_id, &wire.agent_id, &Value::Null, line),
                        session_id: session_id.to_string(),
                        timestamp: timestamp.clone(),
                        source: "compaction".to_string(),
                        content,
                        visibility: None,
                        input_tokens: None,
                        output_tokens: None,
                    });
                }
                undo_floor = messages.len();
                reset_open_state(&mut open);
                continue;
            }
            if record_type != "context.append_loop_event" {
                continue;
            }
            let Some(event) = record
                .get("event")
                .filter(|event| event.get("type").and_then(Value::as_str).is_some())
            else {
                continue;
            };
            handle_loop_event(
                event,
                &record,
                session_id,
                &wire.agent_id,
                wire.main,
                agent_db_id.as_deref(),
                cwd.as_deref(),
                model.clone(),
                line,
                timestamp.clone(),
                &mut messages,
                &mut tool_calls,
                &mut tool_results,
                &mut durations,
                &mut child_parent_calls,
                &mut main_message_count,
                &mut previous_uuid,
                &mut open,
            );
        }
    }

    let mut subagents: Vec<SubagentRecord> = Vec::new();
    if let Some(agents) = state.get("agents").and_then(Value::as_object) {
        for (agent_id, candidate) in agents {
            if agent_id == "main" {
                continue;
            }
            let agent = match candidate.as_object() {
                Some(agent) => agent,
                None => continue,
            };
            let labels = agent.get("labels");
            let agent_type = labels
                .and_then(|l| l.get("profile"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    agent
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            subagents.push(SubagentRecord {
                agent_id: namespaced_agent_id(session_id, agent_id),
                session_id: session_id.to_string(),
                parent_tool_use_id: child_parent_calls.get(agent_id).cloned(),
                agent_type,
                description: agent
                    .get("swarmItem")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                duration_ms: None,
                total_tokens: None,
            });
        }
    }

    let main_wire_path = wire_files
        .iter()
        .find(|wire| wire.main)
        .map(|wire| wire.path.clone())
        .unwrap_or_else(|| {
            Path::new(session_dir)
                .join("wire.jsonl")
                .to_string_lossy()
                .into_owned()
        });

    Ok(ProjectedSession {
        messages,
        tool_calls,
        tool_results,
        summaries,
        subagents,
        durations,
        main_message_count,
        main_wire_path,
    })
}

fn reset_open_state(open: &mut WireProjectionState) {
    open.step_starts.clear();
    open.step_messages.clear();
    open.call_message_uuids.clear();
}

#[allow(clippy::too_many_arguments)]
fn handle_append_message(
    record: &Value,
    session_id: &str,
    agent_id: &str,
    wire_main: bool,
    agent_db_id: Option<&str>,
    cwd: Option<&str>,
    model: Option<String>,
    timestamp: Option<String>,
    line: usize,
    messages: &mut Vec<MessageRecord>,
    tool_calls: &mut Vec<ToolCallRecord>,
    tool_results: &mut Vec<ToolResultRecord>,
    main_message_count: &mut i64,
    previous_uuid: &mut Option<String>,
    open: &mut WireProjectionState,
) {
    let source = match record.get("message") {
        Some(source) if source.get("role").and_then(Value::as_str).is_some() => source,
        _ => return,
    };
    let uuid = namespaced_event_id(
        session_id,
        agent_id,
        source.get("id").unwrap_or(&Value::Null),
        line,
    );
    let origin = source.get("origin");
    let message_uuid = uuid.clone();
    let role = source
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    push_message(
        messages,
        main_message_count,
        previous_uuid,
        open,
        wire_main,
        None,
        MessageRecord {
            uuid: uuid.clone(),
            session_id: session_id.to_string(),
            r#type: role.clone(),
            parent_uuid: previous_uuid.clone(),
            timestamp: timestamp.clone(),
            role: Some(role),
            text: projected_message_text(source),
            content_type: Some(canonical_message_content_type(source)),
            is_meta: is_meta_message(source),
            visibility: MessageVisibility::Visible,
            model,
            is_sidechain: !wire_main,
            agent_id: agent_db_id.map(str::to_string),
            input_tokens: None,
            output_tokens: None,
            cwd: cwd.map(str::to_string),
            skill: None,
            source: NAME.to_string(),
        },
    );
    if origin.and_then(|o| o.get("kind")).and_then(Value::as_str) == Some("injection") {
        open.injection_owner_prompt_ids.insert(
            message_uuid.clone(),
            origin
                .and_then(|o| o.get("ownerPromptId"))
                .and_then(Value::as_str)
                .map(str::to_string),
        );
    }
    if is_real_user_message(source) {
        open.real_user_prompt_ids.insert(
            message_uuid.clone(),
            source.get("id").and_then(Value::as_str).map(str::to_string),
        );
    }
    if let Some(calls) = source.get("toolCalls").and_then(Value::as_array) {
        for call in calls {
            let call = match call.as_object() {
                Some(call) => call,
                None => continue,
            };
            let call_id = match call.get("id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let function = call.get("function");
            // TS `typeof fn?.name === 'string' ? fn.name : 'tool'` — an
            // empty string stays ''.
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            // TS `fn?.arguments ?? {}` — only null/undefined fall back; a
            // string is parsed, anything else passes through.
            let mut args = match function.and_then(|f| f.get("arguments")) {
                None | Some(Value::Null) => json!({}),
                Some(value) => value.clone(),
            };
            if let Value::String(text) = &args {
                args = match serde_json::from_str::<Value>(text) {
                    Ok(parsed) => parsed,
                    Err(_) => json!({ "raw": text }),
                };
            }
            tool_calls.push(ToolCallRecord {
                id: namespaced_tool_id(
                    session_id,
                    agent_id,
                    call.get("id").unwrap_or(&Value::Null),
                ),
                message_uuid: message_uuid.clone(),
                session_id: session_id.to_string(),
                presentation: if name == "Skill" {
                    ToolCallPresentation::Skill
                } else {
                    ToolCallPresentation::Default
                },
                input_json: trunc_json_default(&args).unwrap_or_else(|| "{}".to_string()),
                file_path: kimi_file_path(&name, Some(&args)),
                name,
            });
            open.call_message_uuids
                .insert(call_id, message_uuid.clone());
        }
    }
    if source.get("role").and_then(Value::as_str) == Some("tool")
        && source.get("toolCallId").and_then(Value::as_str).is_some()
    {
        tool_results.push(ToolResultRecord {
            tool_use_id: namespaced_tool_id(
                session_id,
                agent_id,
                source.get("toolCallId").unwrap_or(&Value::Null),
            ),
            message_uuid: Some(message_uuid.clone()),
            session_id: session_id.to_string(),
            content: message_text(source.get("content").unwrap_or(&Value::Null))
                .unwrap_or_default(),
            file_path: None,
            is_error: source.get("isError") == Some(&Value::Bool(true)),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_loop_event(
    event: &Value,
    record: &Value,
    session_id: &str,
    agent_id: &str,
    wire_main: bool,
    agent_db_id: Option<&str>,
    cwd: Option<&str>,
    model: Option<String>,
    line: usize,
    timestamp: Option<String>,
    messages: &mut Vec<MessageRecord>,
    tool_calls: &mut Vec<ToolCallRecord>,
    tool_results: &mut Vec<ToolResultRecord>,
    durations: &mut Vec<(String, i64)>,
    child_parent_calls: &mut HashMap<String, String>,
    main_message_count: &mut i64,
    previous_uuid: &mut Option<String>,
    open: &mut WireProjectionState,
) {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    if event_type == "step.begin" && event.get("uuid").and_then(Value::as_str).is_some() {
        let started = record.get("time").and_then(Value::as_f64).unwrap_or(0.0);
        open.step_starts.insert(
            event
                .get("uuid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            started,
        );
        return;
    }
    if event_type == "content.part" && event.get("stepUuid").and_then(Value::as_str).is_some() {
        let Some(part) = event.get("part") else {
            return;
        };
        let text = part_text(part);
        let keep = text
            .as_deref()
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false);
        if !keep {
            return;
        }
        push_message(
            messages,
            main_message_count,
            previous_uuid,
            open,
            wire_main,
            event.get("stepUuid").and_then(Value::as_str),
            MessageRecord {
                uuid: namespaced_event_id(
                    session_id,
                    agent_id,
                    event.get("uuid").unwrap_or(&Value::Null),
                    line,
                ),
                session_id: session_id.to_string(),
                r#type: "assistant".to_string(),
                parent_uuid: previous_uuid.clone(),
                timestamp: timestamp.clone(),
                role: Some("assistant".to_string()),
                text,
                content_type: Some(part_content_type(part)),
                is_meta: false,
                visibility: MessageVisibility::Visible,
                model,
                is_sidechain: !wire_main,
                agent_id: agent_db_id.map(str::to_string),
                input_tokens: None,
                output_tokens: None,
                cwd: cwd.map(str::to_string),
                skill: None,
                source: NAME.to_string(),
            },
        );
        return;
    }
    if event_type == "tool.call"
        && event.get("stepUuid").and_then(Value::as_str).is_some()
        // TS `event.toolCallId !== undefined` — present (even null) passes.
        && event.get("toolCallId").is_some()
    {
        let uuid = namespaced_event_id(
            session_id,
            agent_id,
            event.get("uuid").unwrap_or(&Value::Null),
            line,
        );
        push_message(
            messages,
            main_message_count,
            previous_uuid,
            open,
            wire_main,
            event.get("stepUuid").and_then(Value::as_str),
            MessageRecord {
                uuid: uuid.clone(),
                session_id: session_id.to_string(),
                r#type: "assistant".to_string(),
                parent_uuid: previous_uuid.clone(),
                timestamp: timestamp.clone(),
                role: Some("assistant".to_string()),
                text: None,
                content_type: Some("tool_use".to_string()),
                is_meta: false,
                visibility: MessageVisibility::Visible,
                model,
                is_sidechain: !wire_main,
                agent_id: agent_db_id.map(str::to_string),
                input_tokens: None,
                output_tokens: None,
                cwd: cwd.map(str::to_string),
                skill: None,
                source: NAME.to_string(),
            },
        );
        // TS `String(event.name ?? 'tool')`: only null/undefined fall back —
        // an empty string stays ''.
        let name = match event.get("name") {
            None | Some(Value::Null) => "tool".to_string(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => native_id_string(other),
        };
        let args = match event.get("args") {
            None | Some(Value::Null) => json!({}),
            Some(value) => value.clone(),
        };
        tool_calls.push(ToolCallRecord {
            id: namespaced_tool_id(
                session_id,
                agent_id,
                event.get("toolCallId").unwrap_or(&Value::Null),
            ),
            message_uuid: uuid.clone(),
            session_id: session_id.to_string(),
            presentation: if event.get("name").and_then(Value::as_str) == Some("Skill") {
                ToolCallPresentation::Skill
            } else {
                ToolCallPresentation::Default
            },
            input_json: trunc_json_default(&args).unwrap_or_else(|| "{}".to_string()),
            file_path: kimi_file_path(&name, event.get("args")),
            name,
        });
        open.call_message_uuids.insert(
            native_id_string(event.get("toolCallId").unwrap_or(&Value::Null)),
            uuid,
        );
        return;
    }
    handle_loop_event_tail(
        event,
        record,
        session_id,
        agent_id,
        event_type,
        tool_results,
        durations,
        child_parent_calls,
        messages,
        open,
    );
}

#[allow(clippy::too_many_arguments, clippy::ptr_arg)]
fn handle_loop_event_tail(
    event: &Value,
    record: &Value,
    session_id: &str,
    agent_id: &str,
    event_type: &str,
    tool_results: &mut Vec<ToolResultRecord>,
    durations: &mut Vec<(String, i64)>,
    child_parent_calls: &mut HashMap<String, String>,
    messages: &mut Vec<MessageRecord>,
    open: &mut WireProjectionState,
) {
    if event_type == "tool.result" && event.get("toolCallId").is_some() {
        let native_tool_id = native_id_string(event.get("toolCallId").unwrap_or(&Value::Null));
        let result = event.get("result");
        let output = result.and_then(|r| r.get("output")).unwrap_or(&Value::Null);
        // TS: string output → trunc(output); anything else →
        // truncJson(output ?? '') ?? '' (null/absent → '""').
        let content = match output {
            Value::String(text) => trunc(Some(text)).unwrap_or_default(),
            Value::Null => trunc_json_default(&json!("")).unwrap_or_default(),
            other => trunc_json_default(other).unwrap_or_default(),
        };
        let tool_id = namespaced_tool_id(
            session_id,
            agent_id,
            event.get("toolCallId").unwrap_or(&Value::Null),
        );
        tool_results.push(ToolResultRecord {
            tool_use_id: tool_id.clone(),
            // TS `callMessageUuids.get(nativeToolId) ?? ''` — an empty string
            // (not null) when the call was never seen.
            message_uuid: Some(
                open.call_message_uuids
                    .get(&native_tool_id)
                    .cloned()
                    .unwrap_or_default(),
            ),
            session_id: session_id.to_string(),
            content: content.clone(),
            file_path: None,
            is_error: result
                .and_then(|r| r.get("isError"))
                .map(|v| v == &Value::Bool(true))
                .unwrap_or(false),
        });
        if let Some(captures) = child_agent_id_re().captures(&content) {
            if let Some(child_id) = captures.get(1) {
                child_parent_calls.insert(child_id.as_str().to_string(), tool_id);
            }
        }
        return;
    }
    if event_type == "step.end" && event.get("uuid").and_then(Value::as_str).is_some() {
        let step_uuid = event
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(last_uuid) = open
            .step_messages
            .get(step_uuid)
            .and_then(|uuids| uuids.last())
            .cloned()
        else {
            return;
        };
        if let Some(usage) = event.get("usage") {
            let input_tokens = input_usage(usage);
            let output_tokens = output_usage(usage);
            if let Some(message) = messages.iter_mut().find(|m| m.uuid == last_uuid) {
                message.input_tokens = input_tokens;
                message.output_tokens = output_tokens;
            }
        }
        if let Some(started) = open.step_starts.get(step_uuid).copied() {
            if let Some(time) = record.get("time").and_then(Value::as_f64) {
                if time >= started {
                    durations.push((last_uuid, (time - started) as i64));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery (kimi.ts discover + session manifest census)
// ---------------------------------------------------------------------------

fn session_directories(
    root_dir: &Path,
    mut report_issue: Option<&mut dyn FnMut(InventoryIssue)>,
) -> Vec<String> {
    let sessions_dir = root_dir.join("sessions");
    if !sessions_dir.exists() {
        return Vec::new();
    }
    let mut result: Vec<String> = Vec::new();
    let workspaces = match dirents(&sessions_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if let Some(report) = report_issue.as_mut() {
                report(InventoryIssue {
                    path: sessions_dir.to_string_lossy().into_owned(),
                    error: error.to_string(),
                });
            }
            return result;
        }
    };
    for (_, workspace_dir, is_dir_flag) in workspaces {
        if !is_dir_flag {
            continue;
        }
        let sessions = match dirents(&workspace_dir) {
            Ok(entries) => entries,
            Err(error) => {
                if let Some(report) = report_issue.as_mut() {
                    report(InventoryIssue {
                        path: workspace_dir.to_string_lossy().into_owned(),
                        error: error.to_string(),
                    });
                }
                continue;
            }
        };
        for (_, session_dir, is_dir_flag) in sessions {
            if is_dir_flag {
                result.push(session_dir.to_string_lossy().into_owned());
            }
        }
    }
    result.sort();
    result
}

/// Approximates Node's `path.relative(from, to)` for normalized paths.
fn path_relative(from: &str, to: &str) -> String {
    let from_parts: Vec<&str> = from.split('/').filter(|p| !p.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|p| !p.is_empty()).collect();
    let mut common = 0usize;
    while common < from_parts.len()
        && common < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from_parts.len() {
        parts.push("..".to_string());
    }
    for part in &to_parts[common..] {
        parts.push((*part).to_string());
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("/")
    }
}

/// The session directories implicated by the changed paths, or None when the
/// change requires full reconciliation (the sessions dir or a workspace dir
/// itself, or anything directly inside the sessions dir).
fn changed_session_directories(
    root_dir: &Path,
    changed_paths: &[String],
) -> Option<HashSet<String>> {
    let sessions_dir = root_dir.join("sessions");
    let sessions_dir_str = sessions_dir.to_string_lossy().into_owned();
    let mut result: HashSet<String> = HashSet::new();
    for changed_path in changed_paths {
        let absolute = if Path::new(changed_path.as_str()).is_absolute() {
            normalize_path(changed_path)
        } else {
            normalize_path(&sessions_dir.join(changed_path).to_string_lossy())
        };
        let inside = path_relative(&sessions_dir_str, &absolute);
        if inside.is_empty() {
            return None;
        }
        if inside.starts_with("..") || inside.starts_with('/') {
            continue;
        }
        let parts: Vec<&str> = inside.split('/').collect();
        let workspace_id = parts.first().copied().unwrap_or("");
        let session_id = parts.get(1).copied();
        if !workspace_id.is_empty() && session_id.is_none() {
            return None;
        }
        if let Some(session_id) = session_id {
            result.insert(
                sessions_dir
                    .join(workspace_id)
                    .join(session_id)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Some(result)
}

/// The session directory owning a wire path: `<session>/agents/main/wire…`
/// collapses to `<session>`; every other layout keeps the wire's parent.
fn session_directory_from_wire_path(wire_path: &str) -> String {
    let parent = Path::new(wire_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let is_main_agent_dir = parent
        .file_name()
        .map(|name| name == "main")
        .unwrap_or(false)
        && parent
            .parent()
            .and_then(|grandparent| grandparent.file_name())
            .map(|name| name == "agents")
            .unwrap_or(false);
    if is_main_agent_dir {
        if let Some(session_dir) = parent.parent().and_then(Path::parent) {
            return session_dir.to_string_lossy().into_owned();
        }
    }
    parent.to_string_lossy().into_owned()
}

fn wire_files_json(wire_files: &[KimiWireFile]) -> Value {
    Value::Array(
        wire_files
            .iter()
            .map(|wire| {
                json!({
                    "agentId": wire.agent_id,
                    "main": wire.main,
                    "path": wire.path,
                })
            })
            .collect(),
    )
}

fn snapshot_json(snapshot: &KimiSessionSnapshot) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("sessionDir".into(), json!(snapshot.session_dir));
    map.insert("statePath".into(), json!(snapshot.state_path));
    map.insert("wireFiles".into(), wire_files_json(&snapshot.wire_files));
    map.insert("currentCursor".into(), json!(snapshot.current_cursor));
    map
}

fn witnesses_json(witnesses: &[KimiIdentityWitness]) -> Value {
    Value::Array(
        witnesses
            .iter()
            .map(|witness| {
                json!({
                    "sessionId": witness.session_id,
                    "snapshots": witness
                        .snapshots
                        .iter()
                        .map(|snapshot| Value::Object(snapshot_json(snapshot)))
                        .collect::<Vec<Value>>(),
                })
            })
            .collect(),
    )
}

fn snapshot_from_json(value: &Value) -> Option<KimiSessionSnapshot> {
    Some(KimiSessionSnapshot {
        session_dir: value.get("sessionDir")?.as_str()?.to_string(),
        state_path: value.get("statePath")?.as_str()?.to_string(),
        wire_files: value
            .get("wireFiles")?
            .as_array()?
            .iter()
            .filter_map(|wire| {
                Some(KimiWireFile {
                    agent_id: wire.get("agentId")?.as_str()?.to_string(),
                    main: wire.get("main")?.as_bool()?,
                    path: wire.get("path")?.as_str()?.to_string(),
                })
            })
            .collect(),
        current_cursor: value.get("currentCursor")?.as_str()?.to_string(),
    })
}

struct KimiSessionUnitMeta {
    mode: String,
    identity_witnesses: Vec<KimiIdentityWitness>,
    snapshot: KimiSessionSnapshot,
}

fn unit_meta_from_json(meta: &Value) -> Option<KimiSessionUnitMeta> {
    let snapshot = snapshot_from_json(meta)?;
    let identity_witnesses = meta
        .get("identityWitnesses")
        .and_then(Value::as_array)
        .map(|witnesses| {
            witnesses
                .iter()
                .filter_map(|witness| {
                    Some(KimiIdentityWitness {
                        session_id: witness.get("sessionId")?.as_str()?.to_string(),
                        snapshots: witness
                            .get("snapshots")?
                            .as_array()?
                            .iter()
                            .filter_map(snapshot_from_json)
                            .collect(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(KimiSessionUnitMeta {
        mode: meta.get("mode")?.as_str()?.to_string(),
        identity_witnesses,
        snapshot,
    })
}

/// Verify that every witnessed identity still maps to the same session
/// directories and manifests. Any drift is an error (parse → StreamItem::Error).
fn verify_kimi_identity_witnesses(
    root_dir: &Path,
    witnesses: &[KimiIdentityWitness],
) -> Result<(), String> {
    if witnesses.is_empty() {
        return Ok(());
    }
    let sessions_dir = root_dir.join("sessions");
    if !sessions_dir.exists() {
        return Err(format!(
            "Kimi session identity changed while indexing: {}",
            sessions_dir.to_string_lossy()
        ));
    }
    let mut issues: Vec<InventoryIssue> = Vec::new();
    let current_session_dirs = session_directories(
        root_dir,
        Some(&mut |issue| {
            issues.push(issue);
        }),
    );
    if let Some(issue) = issues.into_iter().next() {
        // TS throws out of the census helper.
        return Err(format!(
            "Kimi session identity changed while indexing: {}: {}",
            issue.path, issue.error
        ));
    }
    for witness in witnesses {
        let mut expected_dirs: Vec<String> = witness
            .snapshots
            .iter()
            .map(|snapshot| snapshot.session_dir.clone())
            .collect();
        expected_dirs.sort();
        let current_dirs: Vec<String> = current_session_dirs
            .iter()
            .filter(|session_dir| {
                let basename = Path::new(session_dir)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                namespaced_session_id(&basename) == witness.session_id
            })
            .cloned()
            .collect();
        if current_dirs.len() != expected_dirs.len() || current_dirs != expected_dirs {
            return Err(format!(
                "Kimi session identity changed while indexing: {}",
                witness.session_id
            ));
        }
        for expected in &witness.snapshots {
            let current = snapshot_kimi_session(Path::new(&expected.session_dir))?;
            if current.current_cursor != expected.current_cursor {
                return Err(format!(
                    "Kimi session identity changed while indexing: {}",
                    witness.session_id
                ));
            }
        }
    }
    Ok(())
}

fn discover_at(root_dir: &Path, ctx: &mut DiscoverContext) -> Vec<IndexUnit> {
    fn report_issue(
        issue: InventoryIssue,
        ctx: &mut DiscoverContext,
        inventory_complete: &mut bool,
    ) {
        *inventory_complete = false;
        ctx.report_incomplete(issue);
    }

    let mut units: Vec<IndexUnit> = Vec::new();
    let sessions_dir = root_dir.join("sessions");
    let indexed_sessions = ctx.indexed_sessions();
    let mut inventory_complete = true;
    if !sessions_dir.exists() && !indexed_sessions.is_empty() {
        report_issue(
            InventoryIssue {
                path: sessions_dir.to_string_lossy().into_owned(),
                error: "Source folder is unavailable".to_string(),
            },
            ctx,
            &mut inventory_complete,
        );
    }
    let changed_sessions: Option<HashSet<String>> = ctx
        .changed_paths
        .and_then(|changed_paths| changed_session_directories(root_dir, changed_paths));
    let mut indexed_session_dir_by_id: HashMap<String, String> = HashMap::new();
    let mut indexed_session_by_dir: HashMap<String, &IndexedSession> = HashMap::new();
    for indexed in &indexed_sessions {
        let session_dir = session_directory_from_wire_path(&indexed.jsonl_path);
        indexed_session_dir_by_id.insert(indexed.session_id.clone(), session_dir.clone());
        indexed_session_by_dir.insert(session_dir, indexed);
    }
    let discovered_session_dirs = {
        let mut issues: Vec<InventoryIssue> = Vec::new();
        let dirs = session_directories(root_dir, Some(&mut |issue| issues.push(issue)));
        for issue in issues {
            report_issue(issue, ctx, &mut inventory_complete);
        }
        dirs
    };
    let mut discovered_by_session_id: HashMap<String, Vec<String>> = HashMap::new();
    for session_dir in &discovered_session_dirs {
        let basename = session_dir_basename(session_dir);
        discovered_by_session_id
            .entry(namespaced_session_id(&basename))
            .or_default()
            .push(session_dir.clone());
    }
    let mut candidate_session_dirs: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    match &changed_sessions {
        None => {
            candidate_session_dirs.extend(discovered_session_dirs.iter().cloned());
        }
        Some(changed) => {
            candidate_session_dirs.extend(
                discovered_session_dirs
                    .iter()
                    .filter(|session_dir| changed.contains(*session_dir))
                    .cloned(),
            );
        }
    }
    // Duplicate identities are always candidates so they fail closed.
    for session_dirs in discovered_by_session_id.values() {
        if session_dirs.len() < 2 {
            continue;
        }
        for session_dir in session_dirs {
            candidate_session_dirs.insert(session_dir.clone());
        }
    }
    for indexed in &indexed_sessions {
        let indexed_session_dir = session_directory_from_wire_path(&indexed.jsonl_path);
        let inside = path_relative(&sessions_dir.to_string_lossy(), &indexed_session_dir);
        if inside.is_empty() || inside.starts_with("..") || inside.starts_with('/') {
            continue;
        }
        let should_reconcile = changed_sessions
            .as_ref()
            .map(|changed| changed.contains(&indexed_session_dir))
            .unwrap_or(true);
        if !should_reconcile {
            continue;
        }
        if let Some(moved) = discovered_by_session_id.get(&indexed.session_id) {
            if moved.len() == 1 && moved[0] != indexed_session_dir {
                candidate_session_dirs.insert(moved[0].clone());
            }
        }
    }

    let mut snapshots: HashMap<String, KimiSessionSnapshot> = HashMap::new();
    for session_dir in &discovered_session_dirs {
        match snapshot_kimi_session(Path::new(session_dir)) {
            Ok(snapshot) => {
                snapshots.insert(session_dir.clone(), snapshot);
            }
            Err(error) => {
                report_issue(
                    InventoryIssue {
                        path: session_dir.clone(),
                        error,
                    },
                    ctx,
                    &mut inventory_complete,
                );
            }
        }
    }

    let mut live_session_dirs_by_id: HashMap<String, Vec<String>> = HashMap::new();
    for session_dir in &discovered_session_dirs {
        let Some(snapshot) = snapshots.get(session_dir) else {
            continue;
        };
        if snapshot.wire_files.is_empty() {
            continue;
        }
        let basename = session_dir_basename(session_dir);
        live_session_dirs_by_id
            .entry(namespaced_session_id(&basename))
            .or_default()
            .push(session_dir.clone());
    }
    let mut ambiguous_session_ids: HashSet<String> = HashSet::new();
    for (session_id, session_dirs) in &live_session_dirs_by_id {
        if session_dirs.len() < 2 {
            continue;
        }
        ambiguous_session_ids.insert(session_id.clone());
        report_issue(
            InventoryIssue {
                path: sessions_dir.to_string_lossy().into_owned(),
                error: format!(
                    "Multiple live Kimi session directories share identity {session_id}"
                ),
            },
            ctx,
            &mut inventory_complete,
        );
    }

    let live_session_ids: HashSet<String> = live_session_dirs_by_id.keys().cloned().collect();
    let mut touched_session_ids: HashSet<String> = HashSet::new();
    struct ReplayCandidate {
        session_dir: String,
        session_id: String,
        snapshot: KimiSessionSnapshot,
        source_local: bool,
        retract_session_ids: Vec<String>,
    }
    let mut replay_candidates: Vec<ReplayCandidate> = Vec::new();
    let mut planned_retractions: HashSet<String> = HashSet::new();
    for session_dir in &candidate_session_dirs {
        let Some(snapshot) = snapshots.get(session_dir) else {
            continue;
        };
        let session_id = namespaced_session_id(&session_dir_basename(session_dir));
        touched_session_ids.insert(session_id.clone());
        if snapshot.wire_files.is_empty() {
            continue;
        }
        if ambiguous_session_ids.contains(&session_id) {
            continue;
        }
        let stored_cursor = (ctx.last_cursor)(session_dir);
        let indexed_session_dir = indexed_session_dir_by_id.get(&session_id);
        let indexed_at_dir = indexed_session_by_dir.get(session_dir.as_str());
        let owns_indexed_provenance = indexed_session_dir == Some(session_dir);
        let cursor_can_prove_provenance =
            owns_indexed_provenance || (indexed_session_dir.is_none() && indexed_at_dir.is_none());
        let source_local = cursor_can_prove_provenance;
        let retract_session_ids: Vec<String> = match indexed_at_dir {
            Some(indexed)
                if indexed.session_id != session_id
                    && !live_session_ids.contains(&indexed.session_id) =>
            {
                vec![indexed.session_id.clone()]
            }
            _ => Vec::new(),
        };
        if changed_sessions.is_none()
            && cursor_can_prove_provenance
            && classify_kimi_cursor(&stored_cursor, &snapshot.current_cursor)
        {
            continue;
        }
        for retracted in &retract_session_ids {
            planned_retractions.insert(retracted.clone());
        }
        replay_candidates.push(ReplayCandidate {
            session_dir: session_dir.clone(),
            session_id,
            snapshot: snapshot.clone(),
            source_local,
            retract_session_ids,
        });
    }

    struct PendingTombstone {
        session_id: String,
        session_dir: String,
        snapshot: KimiSessionSnapshot,
    }
    let mut pending_tombstones: Vec<PendingTombstone> = Vec::new();
    for indexed in &indexed_sessions {
        if ambiguous_session_ids.contains(&indexed.session_id)
            || live_session_ids.contains(&indexed.session_id)
            || planned_retractions.contains(&indexed.session_id)
        {
            continue;
        }
        let indexed_session_dir = session_directory_from_wire_path(&indexed.jsonl_path);
        let should_reconcile = changed_sessions
            .as_ref()
            .map(|changed| changed.contains(&indexed_session_dir))
            .unwrap_or(true)
            || touched_session_ids.contains(&indexed.session_id);
        if !should_reconcile || !inventory_complete {
            continue;
        }
        let snapshot = match snapshots.get(&indexed_session_dir) {
            Some(snapshot) => snapshot.clone(),
            None => match snapshot_kimi_session(Path::new(&indexed_session_dir)) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    report_issue(
                        InventoryIssue {
                            path: indexed_session_dir.clone(),
                            error,
                        },
                        ctx,
                        &mut inventory_complete,
                    );
                    continue;
                }
            },
        };
        if !snapshot.wire_files.is_empty() {
            report_issue(
                InventoryIssue {
                    path: indexed_session_dir.clone(),
                    error: "Kimi session appeared after the identity census".to_string(),
                },
                ctx,
                &mut inventory_complete,
            );
            continue;
        }
        pending_tombstones.push(PendingTombstone {
            session_id: indexed.session_id.clone(),
            session_dir: indexed_session_dir,
            snapshot,
        });
    }

    let verified_session_dirs = {
        let mut issues: Vec<InventoryIssue> = Vec::new();
        let dirs = session_directories(root_dir, Some(&mut |issue| issues.push(issue)));
        for issue in issues {
            report_issue(issue, ctx, &mut inventory_complete);
        }
        dirs
    };
    if inventory_complete && !sessions_dir.exists() && !indexed_sessions.is_empty() {
        report_issue(
            InventoryIssue {
                path: sessions_dir.to_string_lossy().into_owned(),
                error: "Source folder became unavailable during discovery".to_string(),
            },
            ctx,
            &mut inventory_complete,
        );
    }
    if verified_session_dirs.len() != discovered_session_dirs.len()
        || verified_session_dirs
            .iter()
            .zip(discovered_session_dirs.iter())
            .any(|(verified, discovered)| verified != discovered)
    {
        report_issue(
            InventoryIssue {
                path: sessions_dir.to_string_lossy().into_owned(),
                error: "Kimi session inventory changed during discovery".to_string(),
            },
            ctx,
            &mut inventory_complete,
        );
    }

    // Destructive candidates revalidate every directory of the same identity
    // between discovery and publication.
    let mut destructive_session_ids: HashSet<String> = HashSet::new();
    destructive_session_ids.extend(planned_retractions.iter().cloned());
    destructive_session_ids.extend(
        pending_tombstones
            .iter()
            .map(|tombstone| tombstone.session_id.clone()),
    );
    destructive_session_ids.extend(
        replay_candidates
            .iter()
            .filter(|candidate| !candidate.source_local)
            .map(|candidate| candidate.session_id.clone()),
    );
    for session_id in &destructive_session_ids {
        let Some(session_dirs) = discovered_by_session_id.get(session_id) else {
            continue;
        };
        for session_dir in session_dirs {
            let before = snapshots.get(session_dir);
            let after = match snapshot_kimi_session(Path::new(session_dir)) {
                Ok(after) => after,
                Err(error) => {
                    report_issue(
                        InventoryIssue {
                            path: session_dir.clone(),
                            error,
                        },
                        ctx,
                        &mut inventory_complete,
                    );
                    continue;
                }
            };
            if before.is_none_or(|before| before.current_cursor != after.current_cursor) {
                report_issue(
                    InventoryIssue {
                        path: session_dir.clone(),
                        error: "Kimi session identity changed during discovery".to_string(),
                    },
                    ctx,
                    &mut inventory_complete,
                );
            }
        }
    }

    let identity_witnesses_for = |session_ids: &[String]| -> Vec<KimiIdentityWitness> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut witnesses: Vec<KimiIdentityWitness> = Vec::new();
        for session_id in session_ids {
            if !seen.insert(session_id.clone()) {
                continue;
            }
            let snapshots: Vec<KimiSessionSnapshot> = discovered_by_session_id
                .get(session_id)
                .map(|dirs| {
                    dirs.iter()
                        .filter_map(|dir| snapshots.get(dir).cloned())
                        .collect()
                })
                .unwrap_or_default();
            witnesses.push(KimiIdentityWitness {
                session_id: session_id.clone(),
                snapshots,
            });
        }
        witnesses
    };

    for candidate in &replay_candidates {
        if !inventory_complete && !candidate.source_local {
            continue;
        }
        let state = read_state(Path::new(&candidate.snapshot.state_path));
        let cwd = state
            .get("cwd")
            .and_then(Value::as_str)
            .or_else(|| state.get("workDir").and_then(Value::as_str));
        let identity_witnesses: Vec<KimiIdentityWitness> =
            if candidate.source_local && candidate.retract_session_ids.is_empty() {
                Vec::new()
            } else {
                let mut ids = vec![candidate.session_id.clone()];
                ids.extend(candidate.retract_session_ids.iter().cloned());
                identity_witnesses_for(&ids)
            };
        let mut meta = Map::new();
        meta.insert("kind".into(), json!("session"));
        meta.insert("mode".into(), json!("replay"));
        if !identity_witnesses.is_empty() {
            meta.insert(
                "identityWitnesses".into(),
                witnesses_json(&identity_witnesses),
            );
        }
        for (key, value) in snapshot_json(&candidate.snapshot) {
            meta.insert(key, value);
        }
        units.push(IndexUnit {
            key: candidate.session_dir.clone(),
            session_id: candidate.session_id.clone(),
            project: cwd.and_then(|cwd| crate::parsing::project_slug_from_path(Some(cwd))),
            is_subagent: false,
            agent_id: None,
            meta: Some(Value::Object(meta)),
            retract_session_ids: candidate.retract_session_ids.clone(),
        });
    }
    if inventory_complete {
        for tombstone in &pending_tombstones {
            let identity_witnesses =
                identity_witnesses_for(std::slice::from_ref(&tombstone.session_id));
            let mut meta = Map::new();
            meta.insert("kind".into(), json!("session"));
            meta.insert("mode".into(), json!("tombstone"));
            meta.insert(
                "identityWitnesses".into(),
                witnesses_json(&identity_witnesses),
            );
            for (key, value) in snapshot_json(&tombstone.snapshot) {
                meta.insert(key, value);
            }
            units.push(IndexUnit {
                key: tombstone.session_dir.clone(),
                session_id: tombstone.session_id.clone(),
                project: None,
                is_subagent: false,
                agent_id: None,
                meta: Some(Value::Object(meta)),
                retract_session_ids: vec![tombstone.session_id.clone()],
            });
        }
    }
    units
}

fn session_dir_basename(session_dir: &str) -> String {
    Path::new(session_dir)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

pub fn parse(unit: &IndexUnit, cursor: Cursor) -> Vec<StreamItem> {
    let _ = cursor; // Kimi replays from the manifest snapshot, not a line cursor.
    let Some(meta) = unit.meta.as_ref().and_then(unit_meta_from_json) else {
        return vec![StreamItem::Error(format!(
            "kimi unit meta is missing or malformed for {}",
            unit.key
        ))];
    };
    // The provider rootDir is recovered from the session directory: every
    // unit key produced by discover is <root>/sessions/<workspace>/<session>.
    let session_dir = PathBuf::from(&meta.snapshot.session_dir);
    let root_dir = session_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    parse_at(&root_dir, unit, &meta)
}

fn parse_at(root_dir: &Path, unit: &IndexUnit, meta: &KimiSessionUnitMeta) -> Vec<StreamItem> {
    let session_dir = PathBuf::from(&meta.snapshot.session_dir);
    if let Err(error) = verify_kimi_identity_witnesses(root_dir, &meta.identity_witnesses) {
        return vec![StreamItem::Error(error)];
    }
    let before = match snapshot_kimi_session(&session_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => return vec![StreamItem::Error(error)],
    };
    if before.current_cursor != meta.snapshot.current_cursor {
        return vec![StreamItem::Error(format!(
            "Kimi session changed while indexing: {}",
            meta.snapshot.session_dir
        ))];
    }
    if meta.mode == "tombstone" {
        let after_tombstone = match snapshot_kimi_session(&session_dir) {
            Ok(snapshot) => snapshot,
            Err(error) => return vec![StreamItem::Error(error)],
        };
        if before.current_cursor != after_tombstone.current_cursor {
            return vec![StreamItem::Error(format!(
                "Kimi session changed while indexing: {}",
                meta.snapshot.session_dir
            ))];
        }
        if let Err(error) = verify_kimi_identity_witnesses(root_dir, &meta.identity_witnesses) {
            return vec![StreamItem::Error(error)];
        }
        return vec![StreamItem::Cursor(after_tombstone.current_cursor)];
    }
    let state = read_state(Path::new(&meta.snapshot.state_path));
    let projected = match project_session(
        &meta.snapshot.wire_files,
        &meta.snapshot.session_dir,
        &unit.session_id,
        &state,
    ) {
        Ok(projected) => projected,
        Err(error) => return vec![StreamItem::Error(error)],
    };
    let after = match snapshot_kimi_session(&session_dir) {
        Ok(snapshot) => snapshot,
        Err(error) => return vec![StreamItem::Error(error)],
    };
    if before.current_cursor != after.current_cursor {
        return vec![StreamItem::Error(format!(
            "Kimi session changed while indexing: {}",
            meta.snapshot.session_dir
        ))];
    }
    if let Err(error) = verify_kimi_identity_witnesses(root_dir, &meta.identity_witnesses) {
        return vec![StreamItem::Error(error)];
    }

    let mut items: Vec<StreamItem> = Vec::new();
    items.push(StreamItem::Record(TranscriptRecord::DeleteSession {
        session_id: unit.session_id.clone(),
    }));
    // Kimi persists a titled session before the first prompt; keep it
    // retracted until user evidence exists.
    let has_last_prompt = state
        .get("lastPrompt")
        .and_then(Value::as_str)
        .map(|prompt| !prompt.is_empty())
        .unwrap_or(false);
    let has_projected_user_prompt = projected.messages.iter().any(|message| {
        message.agent_id.is_none() && message.role.as_deref() == Some("user") && !message.is_meta
    });
    let title = state
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            state
                .get("lastPrompt")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let is_placeholder = state.get("title").and_then(Value::as_str) == Some("New Session")
        && !has_last_prompt
        && !has_projected_user_prompt;
    if is_placeholder {
        items.push(StreamItem::Cursor(after.current_cursor));
        return items;
    }
    items.push(StreamItem::Record(TranscriptRecord::Session(
        crate::providers::types::SessionRecord {
            id: unit.session_id.clone(),
            title,
            project: unit.project.clone(),
            started_at: state.get("createdAt").and_then(normalize_time),
            ended_at: state.get("updatedAt").and_then(normalize_time),
            git_branch: None,
            version: None,
            message_count: projected.main_message_count,
            count_mode: SessionCountMode::Total,
            jsonl_path: projected.main_wire_path,
            source: NAME.to_string(),
        },
    )));
    for message in projected.messages {
        items.push(StreamItem::Record(TranscriptRecord::Message(message)));
    }
    for call in projected.tool_calls {
        items.push(StreamItem::Record(TranscriptRecord::ToolCall(call)));
    }
    for result in projected.tool_results {
        items.push(StreamItem::Record(TranscriptRecord::ToolResult(result)));
    }
    for summary in projected.summaries {
        items.push(StreamItem::Record(TranscriptRecord::Summary(summary)));
    }
    for subagent in projected.subagents {
        items.push(StreamItem::Record(TranscriptRecord::Subagent(subagent)));
    }
    for (uuid, duration) in projected.durations {
        items.push(StreamItem::Record(TranscriptRecord::MessageTurnDuration {
            uuid,
            turn_duration_ms: Some(duration),
        }));
    }
    items.push(StreamItem::Cursor(after.current_cursor));
    items
}

// ---------------------------------------------------------------------------
// raw
// ---------------------------------------------------------------------------

fn raw_from_wire(path: &Path, message_uuid: &str) -> Option<RawRecord> {
    if !path.exists() {
        return None;
    }
    let fallback_line: Option<usize> = regex::Regex::new(r":line-(\d+)$")
        .ok()
        .and_then(|re| re.captures(message_uuid))
        .and_then(|captures| captures.get(1))
        .and_then(|m| m.as_str().parse::<usize>().ok());
    let native_id = message_uuid.rsplit(':').next();
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let line: Option<&str> = match fallback_line {
        Some(number) => lines.get(number.checked_sub(1)?).copied(),
        None => lines
            .iter()
            .copied()
            .find(|candidate| native_id.is_some_and(|id| candidate.contains(id))),
    };
    // TS `if (!line) return null` — an empty string is falsy too.
    let line = line.filter(|line| !line.is_empty())?;
    let mut message_text: Option<String> = None;
    if let Ok(record) = serde_json::from_str::<Value>(line) {
        let record_type = record.get("type").and_then(Value::as_str);
        if record_type == Some("context.append_message") {
            if let Some(message) = record.get("message") {
                if let Some(command) = user_slash_command_text(message) {
                    message_text = Some(command);
                } else {
                    let parts: Vec<String> = if let Value::String(text) =
                        message.get("content").unwrap_or(&Value::Null)
                    {
                        vec![text.clone()]
                    } else {
                        content_parts(message.get("content").unwrap_or(&Value::Null))
                            .iter()
                            .filter_map(|part| raw_part_text(part).map(str::to_string))
                            .collect()
                    };
                    message_text = (!parts.is_empty()).then(|| parts.join("\n"));
                }
            }
        } else if record_type == Some("context.append_loop_event") {
            if let Some(part) = record.get("event").and_then(|event| event.get("part")) {
                message_text = raw_part_text(part).map(str::to_string);
            }
        }
    }
    let total_length = line.chars().map(char::len_utf16).sum::<usize>();
    Some(RawRecord {
        text: line.to_string(),
        total_length: Some(total_length),
        offset: Some(0),
        limit: Some(total_length),
        has_more: Some(false),
        message_text,
    })
}

fn raw_kimi(input: &RawLookup) -> Option<RawRecord> {
    let main_path = input
        .session?
        .get("jsonl_path")
        .and_then(Value::as_str)
        .map(PathBuf::from)?;
    match input.agent_id {
        None => raw_from_wire(&main_path, input.message_uuid),
        Some(agent_id) => {
            let raw_agent_id = agent_id.rsplit(':').next()?;
            let session_dir = session_directory_from_wire_path(&main_path.to_string_lossy());
            raw_from_wire(
                &Path::new(&session_dir)
                    .join("agents")
                    .join(raw_agent_id)
                    .join("wire.jsonl"),
                input.message_uuid,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct KimiProvider {
    pub root_dir: PathBuf,
}

impl KimiProvider {
    pub fn new(root_dir: PathBuf) -> Self {
        Self { root_dir }
    }
}

impl ProviderAdapter for KimiProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: NAME,
            name: "Kimi Code",
            vendor: "Moonshot AI",
            default_root: self.root_dir.to_string_lossy().into_owned(),
            color: "#6d6afc",
            requires_explicit_root: false,
            root_resolution_reason: None,
        }
    }

    fn index_version_marker(&self) -> Option<&'static str> {
        Some(KIMI_CANONICAL_TRANSCRIPT_MARKER)
    }

    fn session_unit_key(&self, session: &IndexedSession) -> Option<String> {
        Some(session_directory_from_wire_path(&session.jsonl_path))
    }

    fn watch_targets(&self, configured_root: &str) -> Vec<WatchTarget> {
        vec![
            WatchTarget {
                kind: WatchTargetKind::Tree,
                path: Path::new(configured_root)
                    .join("sessions")
                    .to_string_lossy()
                    .into_owned(),
            },
            WatchTarget {
                kind: WatchTargetKind::File,
                path: Path::new(configured_root)
                    .join("session_index.jsonl")
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
        raw_kimi(input)
    }
}
