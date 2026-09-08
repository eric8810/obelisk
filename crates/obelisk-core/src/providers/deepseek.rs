// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! DeepSeek Harness provider adapter (port of
//! packages/core/src/providers/deepseek.ts, ADR-0011/ADR-0013).
//!
//! Pure: discovers DeepSeek Harness session trees and parses one tree into a
//! canonical record stream. Never touches the Obelisk database.
//!
//! One IndexUnit is a whole ROOT SESSION TREE (root file plus every
//! descendant subagent file). The artifact is a concatenation of independent
//! checksummed zstd frames (one per append batch); the framing scanner is a
//! port of the vendored ../vendor/dsh-zstd.ts, decoding each complete frame
//! and tolerating a torn final frame.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::parsing::{
    normalize_observed_cwd, normalize_path, project_slug_from_path, source_inventory_issue,
    tool_file_path, trunc, trunc_json_default,
};
use crate::providers::types::{
    Cursor, DiscoverContext, IndexUnit, InventoryIssue, MessageRecord, MessageVisibility,
    ParseStream, ProviderAdapter, ProviderDescriptor, RawLookup, RawRecord, SessionCountMode,
    SessionRecord, StreamItem, SubagentRecord, ToolCallPresentation, ToolCallRecord,
    ToolResultRecord, TranscriptRecord, WatchTarget, WatchTargetKind,
};

pub const NAME: &str = "deepseek";
const DEEPSEEK_CANONICAL_TRANSCRIPT_MARKER: &str = "__deepseek_canonical_transcript_v2__";

const SESSION_FILENAMES: [&str; 2] = [".jsonl.zstd", ".jsonl"];
const CURSOR_STATE_VERSION: i64 = 1;

// ---- JS value helpers ----

/// JS template-literal stringification (`${value}`): missing → "undefined".
fn template_str(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    template_str(Some(v))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// JS `String(value)` for arbitrary JSON values.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        other => template_str(Some(other)),
    }
}

// ---- encodings ----

/// JS `encodeURIComponent` (RFC 3986 unreserved set, UTF-8 percent-encoded).
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// JS `decodeURIComponent`; None where the TS call would throw a URIError.
fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn base64url_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            continue;
        }
        acc = (acc << 6) | value(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
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
                .wrapping_add(K[i])
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
    h.iter().map(|x| format!("{x:08x}")).collect()
}

// ---- identity ----

/// Deterministic project discriminator (sha256 over the normalized cwd).
fn project_scope(cwd: Option<&Value>) -> String {
    let raw = cwd.and_then(Value::as_str);
    let normalized = normalize_observed_cwd(raw)
        .or_else(|| raw.map(str::to_string))
        .unwrap_or_default();
    sha256_hex(format!("deepseek-cwd-v1\0{normalized}").as_bytes())
}

/// Database identity for one raw session id inside one project scope.
fn dsh_db_id(scope: &str, raw_id: &str) -> String {
    format!("deepseek:{}:{scope}", encode_uri_component(raw_id))
}

fn assistant_message_uuid(
    db_id: &str,
    turn: Option<&Value>,
    step: Option<&Value>,
    kind: &str,
) -> String {
    format!(
        "{db_id}:t{}:s{}:{kind}",
        template_str(turn),
        template_str(step)
    )
}

fn tool_use_uuid(db_id: &str, turn: Option<&Value>, step: Option<&Value>) -> String {
    assistant_message_uuid(db_id, turn, step, "tool_use")
}

fn user_message_uuid(db_id: &str, native_id: Option<&Value>, seq: f64) -> String {
    let native = match native_id {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => template_str(Some(&json!(seq))),
    };
    format!("{db_id}:u{native}")
}

fn call_id(db_id: &str, native_call_id: &str) -> String {
    format!("{db_id}:{}", encode_uri_component(native_call_id))
}

fn step_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r":t(\d+):s(\d+):(reasoning|text|tool_use)$").unwrap())
}

/// The "t<turn>:s<step>" key encoded in an assistant-family uuid, else None.
fn step_key_of(uuid: &str) -> Option<String> {
    step_key_re()
        .captures(uuid)
        .map(|caps| format!("t{}:s{}", &caps[1], &caps[2]))
}

// ---- zstd framing (port of ../vendor/dsh-zstd.ts) ----

/// Locate complete frames without decompressing their blocks. Invalid
/// complete structure rejects; EOF inside the final frame is a torn tail.
fn scan_zstd_frames(buffer: &[u8]) -> Result<Vec<(usize, usize)>, String> {
    const ZSTD_MAGIC: u32 = 0xFD2FB528;
    let mut frames: Vec<(usize, usize)> = Vec::new();
    let mut offset = 0usize;
    while offset < buffer.len() {
        let start = offset;
        if buffer.len() - offset < 4 {
            return Ok(frames); // torn tail carries no committed events
        }
        let magic = u32::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]);
        if magic != ZSTD_MAGIC {
            return Err(format!(
                "corrupt Zstandard session log: invalid frame magic at byte {offset}"
            ));
        }
        offset += 4;
        if offset == buffer.len() {
            return Ok(frames);
        }
        let descriptor = buffer[offset];
        offset += 1;
        if (descriptor & 0x18) != 0 {
            return Err(format!(
                "corrupt Zstandard session log: reserved frame-header bit at byte {}",
                offset - 1
            ));
        }
        let content_size_flag = descriptor >> 6;
        let single_segment = (descriptor & 0x20) != 0;
        let checksum = (descriptor & 0x04) != 0;
        let dictionary_flag = descriptor & 0x03;
        let dictionary_bytes = if dictionary_flag == 3 {
            4
        } else {
            dictionary_flag as usize
        };
        let content_size_bytes = if content_size_flag == 0 {
            usize::from(single_segment)
        } else {
            1 << content_size_flag
        };
        let remaining_header_bytes =
            usize::from(!single_segment) + dictionary_bytes + content_size_bytes;
        if buffer.len() - offset < remaining_header_bytes {
            return Ok(frames);
        }
        offset += remaining_header_bytes;
        loop {
            if buffer.len() - offset < 3 {
                return Ok(frames);
            }
            let block_header = u32::from(buffer[offset])
                | (u32::from(buffer[offset + 1]) << 8)
                | (u32::from(buffer[offset + 2]) << 16);
            offset += 3;
            let last_block = (block_header & 1) != 0;
            let block_type = (block_header >> 1) & 0x03;
            let block_size = (block_header >> 3) as usize;
            if block_type == 0x03 {
                return Err(format!(
                    "corrupt Zstandard session log: reserved block type at byte {}",
                    offset - 3
                ));
            }
            let payload_bytes = if block_type == 0x01 { 1 } else { block_size };
            if buffer.len() - offset < payload_bytes {
                return Ok(frames);
            }
            offset += payload_bytes;
            if last_block {
                break;
            }
        }
        if checksum {
            if buffer.len() - offset < 4 {
                return Ok(frames);
            }
            offset += 4;
        }
        frames.push((start, offset));
    }
    Ok(frames)
}

/// Decode complete frames [from_frame, frames.len()) into plaintext. Each
/// frame is decoded independently (the artifact is concatenated frames).
fn decode_frames(
    buffer: &[u8],
    frames: &[(usize, usize)],
    from_frame: usize,
) -> Result<String, String> {
    if from_frame >= frames.len() {
        return Ok(String::new());
    }
    let mut out = String::new();
    for &(start, end) in &frames[from_frame..] {
        let decoded = zstd::stream::decode_all(&buffer[start..end]).map_err(|_| {
            format!("corrupt Zstandard session log: frame at byte {start} failed validation")
        })?;
        out.push_str(&String::from_utf8_lossy(&decoded));
    }
    Ok(out)
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())
}

/// Parse the header record out of an artifact buffer (no second read).
fn header_from_buffer(path: &str, buffer: &[u8]) -> Option<Value> {
    let first_line: Option<String> = if path.ends_with(".jsonl.zstd") {
        let frames = scan_zstd_frames(buffer).ok()?;
        let (start, end) = *frames.first()?;
        let decoded = zstd::stream::decode_all(&buffer[start..end]).ok()?;
        first_non_empty_line(&String::from_utf8_lossy(&decoded)).map(str::to_string)
    } else {
        first_non_empty_line(&String::from_utf8_lossy(buffer)).map(str::to_string)
    };
    let value: Value = serde_json::from_str(&first_line?).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

/// Read just the immutable header frame (or first line for plaintext).
fn read_dsh_header(path: &Path) -> Option<Value> {
    let buffer = std::fs::read(path).ok()?;
    header_from_buffer(&path.to_string_lossy(), &buffer)
}

// ---- log records ----

struct LogRecord {
    seq: f64,
    type_: String,
    time: Option<f64>,
    data: Value,
}

fn read_log_records(plaintext: &str) -> Vec<LogRecord> {
    let mut records: Vec<LogRecord> = Vec::new();
    for line in plaintext.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue; // TS silently skips unparseable lines
        };
        // Packed chunk rows ('text-chunks' etc.) have a string type but no
        // projection handler, so they are inert by construction here.
        let Some(type_) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !value.is_object() {
            continue;
        }
        records.push(LogRecord {
            seq: value.get("seq").and_then(Value::as_f64).unwrap_or(-1.0),
            type_: type_.to_string(),
            time: value.get("time").and_then(Value::as_f64),
            data: match value.get("data") {
                Some(data @ Value::Object(_)) => data.clone(),
                _ => Value::Object(Map::new()),
            },
        });
    }
    records
}

// ---- content helpers ----

fn join_part_text(content: &Value, part_type: &str) -> Option<String> {
    let Value::Array(parts) = content else {
        return None;
    };
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|part| {
            let part = part.as_object()?;
            if part.get("type").and_then(|v| v.as_str()) == Some(part_type) {
                part.get("text").and_then(|v| v.as_str())
            } else {
                None
            }
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

struct AssistantClassification {
    reasoning_text: Option<String>,
    visible_text: Option<String>,
    has_tool_calls: bool,
}

fn classify_assistant_content(content: &Value) -> AssistantClassification {
    AssistantClassification {
        reasoning_text: join_part_text(content, "reasoning"),
        visible_text: join_part_text(content, "text"),
        has_tool_calls: content.as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                matches!(part.as_object(), Some(part)
                    if part.get("type").and_then(|v| v.as_str()) == Some("tool-call")
                        && part.get("id").and_then(|v| v.as_str()).is_some())
            })
        }),
    }
}

fn parse_tool_arguments(value: &Value) -> Value {
    let Value::String(text) = value else {
        return value.clone();
    };
    serde_json::from_str(text).unwrap_or_else(|_| value.clone())
}

/// DeepSeek Harness file tools use lowercase names, unlike Claude's.
fn dsh_tool_file_path(tool_name: &str, input: Option<&Value>) -> Option<String> {
    if let Some(obj) = input {
        if matches!(tool_name, "read" | "edit" | "write") {
            if let Some(file_path) = obj.get("file_path").and_then(Value::as_str) {
                return Some(file_path.to_string());
            }
        }
    }
    tool_file_path(tool_name, input)
}

fn tool_result_content(content: &Value) -> String {
    let Value::Array(blocks) = content else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        let Some(block) = block.as_object() else {
            continue;
        };
        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                parts.push(text.to_string());
            }
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool-result") {
            if let Some(inner) = block.get("content") {
                if inner.is_array() {
                    parts.push(tool_result_content(inner));
                }
            }
        }
    }
    parts.join("\n")
}

fn tool_result_is_error(data: &Value, content: &Value) -> bool {
    if data.get("error").is_some() {
        return true;
    }
    let Value::Array(blocks) = content else {
        return false;
    };
    blocks.iter().any(|block| {
        matches!(block.as_object(), Some(block)
            if block.get("isError").and_then(|v| v.as_bool()) == Some(true))
    })
}

fn total_input_tokens(usage: &Value) -> Option<i64> {
    if !usage.is_object() {
        return None;
    }
    let mut seen = false;
    let mut total = 0.0f64;
    for field in ["inputTokens", "cacheReadTokens"] {
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
    if !usage.is_object() {
        return None;
    }
    usage
        .get("outputTokens")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .map(|v| v as i64)
}

// ---- discovery: files → root session trees ----

struct SessionFile {
    path: String,
    project_dir: String,
    session_dir: String,
}

enum Probe {
    Present,
    Gone,
    Error,
}

/// 'gone' only on ENOENT/ENOTDIR; anything else (EACCES, EIO, ...) is 'error'.
fn probe_path(path: &Path) -> Probe {
    match std::fs::metadata(path) {
        Ok(_) => Probe::Present,
        Err(error) => match error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => Probe::Gone,
            _ => Probe::Error,
        },
    }
}

fn collect_session_files(
    sessions_dir: &Path,
    issues: Option<&mut Vec<InventoryIssue>>,
) -> Vec<SessionFile> {
    let mut result: Vec<SessionFile> = Vec::new();
    if !matches!(probe_path(sessions_dir), Probe::Present) {
        return result;
    }
    let mut issues = issues;
    let projects = match crate::parsing::sorted_read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if let Some(issues) = issues.as_deref_mut() {
                issues.push(source_inventory_issue(
                    &sessions_dir.to_string_lossy(),
                    &error,
                ));
            }
            return result;
        }
    };
    for (project, project_path) in projects {
        if !crate::parsing::is_dir(&project_path) {
            continue;
        }
        let sessions = match crate::parsing::sorted_read_dir(&project_path) {
            Ok(entries) => entries,
            Err(error) => {
                if let Some(issues) = issues.as_deref_mut() {
                    issues.push(source_inventory_issue(
                        &project_path.to_string_lossy(),
                        &error,
                    ));
                }
                continue;
            }
        };
        for (_, session_path) in sessions {
            if !crate::parsing::is_dir(&session_path) {
                continue;
            }
            let session_dir = session_path.to_string_lossy().into_owned();
            for suffix in SESSION_FILENAMES {
                let path = session_path.join(format!("session{suffix}"));
                let probe = probe_path(&path);
                if matches!(probe, Probe::Error) {
                    // A permission/transient I/O error is NOT a deletion.
                    if let Some(issues) = issues.as_deref_mut() {
                        issues.push(InventoryIssue {
                            path: path.to_string_lossy().into_owned(),
                            error: "Session artifact is present but not stat-able".to_string(),
                        });
                    }
                }
                if !matches!(probe, Probe::Gone) {
                    result.push(SessionFile {
                        path: path.to_string_lossy().into_owned(),
                        project_dir: project.clone(),
                        session_dir,
                    });
                    break;
                }
            }
        }
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

fn find_session_file(root_dir: &Path, raw_session_id: &str, scope: Option<&str>) -> Option<String> {
    for file in collect_session_files(root_dir, None) {
        let header = read_dsh_header(Path::new(&file.path));
        let dir_matches = file.session_dir.ends_with(&format!("/{raw_session_id}"));
        let id_matches = header
            .as_ref()
            .and_then(|h| h.get("id"))
            .and_then(Value::as_str)
            == Some(raw_session_id);
        if dir_matches || id_matches {
            // Raw ids may collide across projects: disambiguate by scope.
            if scope.is_none_or(|s| project_scope(header.as_ref().and_then(|h| h.get("cwd"))) == s)
            {
                return Some(file.path);
            }
        }
    }
    None
}

/// Split watcher paths into session-file paths and directory-level events.
/// Paths outside this provider's root belong to other providers.
fn split_changed_paths(sessions_dir: &Path, changed_paths: &[String]) -> (HashSet<String>, bool) {
    let mut files: HashSet<String> = HashSet::new();
    let mut has_dir_event = false;
    let sessions_dir_str = sessions_dir.to_string_lossy().into_owned();
    let mut root_prefixes = vec![format!("{}/", normalize_path(&sessions_dir_str))];
    if let Ok(resolved) = std::fs::canonicalize(sessions_dir) {
        let prefix = format!("{}/", normalize_path(&resolved.to_string_lossy()));
        if prefix != root_prefixes[0] {
            root_prefixes.push(prefix);
        }
    }
    for changed_path in changed_paths {
        let absolute = if Path::new(changed_path.as_str()).is_absolute() {
            normalize_path(changed_path)
        } else {
            normalize_path(
                &Path::new(&sessions_dir_str)
                    .join(changed_path)
                    .to_string_lossy(),
            )
        };
        if !root_prefixes
            .iter()
            .any(|prefix| absolute.starts_with(prefix))
        {
            continue; // foreign provider's path
        }
        if SESSION_FILENAMES
            .iter()
            .any(|suffix| absolute.ends_with(suffix))
        {
            files.insert(absolute);
        } else {
            // A directory rename cannot be routed to a file: full reconcile.
            has_dir_event = true;
        }
    }
    (files, has_dir_event)
}

fn dirname(p: &str) -> String {
    match Path::new(p).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ if p == "/" => "/".to_string(),
        _ => ".".to_string(),
    }
}

/// Canonicalize via the longest EXISTING ancestor: a deleted or renamed-away
/// path cannot be realpath'd itself, but its symlink aliases resolve through
/// the deepest existing ancestor.
fn canon(p: &str) -> String {
    let mut current = p.to_string();
    let mut missing_tail: Vec<String> = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&current) {
            let mut out = resolved;
            for part in &missing_tail {
                out = out.join(part);
            }
            return out.to_string_lossy().into_owned();
        }
        let parent = dirname(&current);
        if parent == current || parent.is_empty() {
            return p.to_string();
        }
        missing_tail.insert(0, current[parent.len() + 1..].to_string());
        current = parent;
    }
}

struct TreeMember {
    path: String,
    db_id: String,
    agent_id: Option<String>,
    is_subagent: bool,
    header: Value,
}

struct TreeGroup {
    scope: String,
    root_raw_id: String,
    paths: Vec<String>,
}

/// Walk the parentSession chain inside one project scope to the root id.
fn resolve_root_raw_id(
    raw_id: &str,
    header: &Value,
    headers_by_scoped_id: &HashMap<String, Value>,
) -> String {
    let scope = project_scope(header.get("cwd"));
    let mut root = raw_id.to_string();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current: Option<&Value> = Some(header);
    while let Some(header) = current {
        let Some(parent) = header.get("parentSession").and_then(Value::as_str) else {
            break;
        };
        if parent.is_empty() || seen.contains(parent) {
            break;
        }
        seen.insert(parent.to_string());
        root = parent.to_string();
        current = headers_by_scoped_id.get(&format!("{scope}\0{root}"));
    }
    root
}

fn add_route(map: &mut HashMap<String, HashSet<usize>>, key: &str, index: usize) {
    map.entry(key.to_string()).or_default().insert(index);
}

enum Routing {
    All,
    Touched(HashSet<usize>),
}

fn discover_at(root_dir: &Path, ctx: &mut DiscoverContext) -> Vec<IndexUnit> {
    let sessions_dir = root_dir;
    // Inventory issues accumulate during discovery and are forwarded to the
    // context at the end (callback order does not matter, only completeness).
    let mut issues: Vec<InventoryIssue> = Vec::new();
    let root_probe = probe_path(sessions_dir);
    match root_probe {
        Probe::Error => issues.push(InventoryIssue {
            path: sessions_dir.to_string_lossy().into_owned(),
            error: "Sessions root is present but not accessible".to_string(),
        }),
        Probe::Gone if !ctx.indexed_sessions().is_empty() => issues.push(InventoryIssue {
            path: sessions_dir.to_string_lossy().into_owned(),
            error: "Source folder is unavailable".to_string(),
        }),
        _ => {}
    }
    let files = collect_session_files(sessions_dir, Some(&mut issues));
    let changed = ctx
        .changed_paths
        .map(|paths| split_changed_paths(sessions_dir, paths));

    let mut headers_by_scoped_id: HashMap<String, Value> = HashMap::new();
    let mut file_by_path: HashMap<String, String> = HashMap::new(); // path → project dir
    let mut header_by_path: HashMap<String, Value> = HashMap::new();
    let mut raw_id_by_path: HashMap<String, String> = HashMap::new();
    let mut suppressed_project_dirs: HashSet<String> = HashSet::new();
    for file in &files {
        file_by_path.insert(file.path.clone(), file.project_dir.clone());
        let header = read_dsh_header(Path::new(&file.path));
        let raw_id = header
            .as_ref()
            .and_then(|h| h.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let Some((header, raw_id)) = header.zip(raw_id) else {
            issues.push(InventoryIssue {
                path: file.path.clone(),
                error: "Session artifact has no readable header".to_string(),
            });
            suppressed_project_dirs.insert(file.project_dir.clone());
            continue;
        };
        // Version gate: never parse a higher format as v0 (skip and record).
        if let Some(version) = header.get("version") {
            if *version != json!(0) {
                issues.push(InventoryIssue {
                    path: file.path.clone(),
                    error: format!("Unsupported session format version {}", js_string(version)),
                });
                suppressed_project_dirs.insert(file.project_dir.clone());
                continue;
            }
        }
        raw_id_by_path.insert(file.path.clone(), raw_id.clone());
        header_by_path.insert(file.path.clone(), header.clone());
        headers_by_scoped_id.insert(
            format!("{}\0{raw_id}", project_scope(header.get("cwd"))),
            header.clone(),
        );
    }

    // Group files into root session trees (per-scope ancestry).
    let mut groups: Vec<TreeGroup> = Vec::new();
    let mut group_index_by_key: HashMap<String, usize> = HashMap::new();
    for file in &files {
        let Some(raw_id) = raw_id_by_path.get(&file.path) else {
            continue;
        };
        let Some(header) = header_by_path.get(&file.path) else {
            continue;
        };
        let scope = project_scope(header.get("cwd"));
        let root_raw_id = resolve_root_raw_id(raw_id, header, &headers_by_scoped_id);
        let key = format!("{scope}\0{root_raw_id}");
        match group_index_by_key.get(&key) {
            Some(&index) => groups[index].paths.push(file.path.clone()),
            None => {
                group_index_by_key.insert(key, groups.len());
                groups.push(TreeGroup {
                    scope,
                    root_raw_id,
                    paths: vec![file.path.clone()],
                });
            }
        }
    }

    // Two files with the SAME scoped identity are one logical member;
    // DIVERGENT copies have no safe authority rule, so the tree fails closed.
    let mut divergent: HashSet<usize> = HashSet::new();
    for (group_index, group) in groups.iter_mut().enumerate() {
        let mut by_identity: Vec<(String, Vec<String>)> = Vec::new();
        let mut identity_index: HashMap<String, usize> = HashMap::new();
        for path in &group.paths {
            let raw_id = raw_id_by_path.get(path).cloned().unwrap_or_default();
            let is_sub = matches!(
                header_by_path
                    .get(path)
                    .and_then(|h| h.get("parentSession"))
                    .and_then(Value::as_str),
                Some(parent) if !parent.is_empty()
            );
            let identity = format!("{is_sub}\0{raw_id}");
            match identity_index.get(&identity) {
                Some(&j) => by_identity[j].1.push(path.clone()),
                None => {
                    identity_index.insert(identity.clone(), by_identity.len());
                    by_identity.push((identity, vec![path.clone()]));
                }
            }
        }
        let mut kept_paths: Vec<String> = Vec::new();
        for (_, mut paths) in by_identity {
            paths.sort();
            let canonical = paths[0].clone();
            let canonical_bytes = std::fs::read(&canonical);
            for dup in &paths[1..] {
                let divergent_copy = match (&canonical_bytes, std::fs::read(dup)) {
                    (Ok(a), Ok(b)) => a != &b,
                    // Unreadable copy: no safe authority rule — fail closed.
                    _ => true,
                };
                if divergent_copy {
                    divergent.insert(group_index);
                    issues.push(InventoryIssue {
                        path: dup.clone(),
                        error: "Divergent session artifacts share one scoped identity".to_string(),
                    });
                }
            }
            kept_paths.push(canonical);
        }
        group.paths = kept_paths;
    }

    // Changed-path routing table: member files, session dirs, project dirs,
    // checkpointed member paths and indexed jsonl_paths all route to trees.
    let routing = changed.as_ref().map(|(changed_files, has_dir_event)| {
        let mut file_to_groups: HashMap<String, HashSet<usize>> = HashMap::new();
        let mut dir_to_groups: HashMap<String, HashSet<usize>> = HashMap::new();
        let mut group_by_session_id: HashMap<String, usize> = HashMap::new();
        for (i, group) in groups.iter().enumerate() {
            if divergent.contains(&i) {
                continue;
            }
            let project_dir = group
                .paths
                .first()
                .and_then(|p| file_by_path.get(p))
                .cloned()
                .unwrap_or_default();
            if suppressed_project_dirs.contains(&project_dir) {
                continue;
            }
            let root_path = group
                .paths
                .iter()
                .find(|p| {
                    raw_id_by_path
                        .get(*p)
                        .is_some_and(|id| id == &group.root_raw_id)
                })
                .cloned();
            let Some(root_path) = root_path else {
                continue;
            };
            group_by_session_id.insert(dsh_db_id(&group.scope, &group.root_raw_id), i);
            for path in &group.paths {
                add_route(&mut file_to_groups, &canon(path), i);
                add_route(
                    &mut dir_to_groups,
                    &format!("{}/", canon(&dirname(path))),
                    i,
                );
                add_route(
                    &mut dir_to_groups,
                    &format!("{}/", canon(&dirname(&dirname(path)))),
                    i,
                );
            }
            // Checkpointed member paths: a DELETED member still routes to its
            // tree precisely.
            if let Some(state) = (ctx.last_cursor)(&root_path)
                .as_deref()
                .and_then(decode_cursor_state)
            {
                for member_path in state.members.keys() {
                    add_route(&mut file_to_groups, &canon(member_path), i);
                    add_route(
                        &mut dir_to_groups,
                        &format!("{}/", canon(&dirname(member_path))),
                        i,
                    );
                }
            }
        }
        // Indexed identities: a recorded jsonl_path routes to its tree even
        // when the tree moved (the old path is not a current member).
        for indexed in ctx.indexed_sessions() {
            if let Some(&gi) = group_by_session_id.get(&indexed.session_id) {
                add_route(&mut file_to_groups, &canon(&indexed.jsonl_path), gi);
            }
        }
        let mut touched: HashSet<usize> = HashSet::new();
        let mut reconcile_all = *has_dir_event;
        for raw_path in changed_files {
            let c = canon(raw_path);
            if let Some(hit) = file_to_groups.get(&c) {
                touched.extend(hit);
                continue;
            }
            // Fixed directory shape (project/session/file): at most two
            // ancestor levels can hold a route.
            let mut dir = dirname(&c);
            let mut routed = false;
            for _ in 0..2 {
                if let Some(hit) = dir_to_groups.get(&format!("{}/", canon(&dir))) {
                    touched.extend(hit);
                    routed = true;
                    break;
                }
                let parent = dirname(&dir);
                if parent == dir {
                    break;
                }
                dir = parent;
            }
            if !routed {
                reconcile_all = true;
            }
        }
        if reconcile_all {
            Routing::All
        } else {
            Routing::Touched(touched)
        }
    });

    let mut units: Vec<IndexUnit> = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        if divergent.contains(&group_index) {
            continue; // divergent copies: fail closed
        }
        let project_dir = group
            .paths
            .first()
            .and_then(|p| file_by_path.get(p))
            .cloned()
            .unwrap_or_default();
        if suppressed_project_dirs.contains(&project_dir) {
            continue;
        }
        let Some(root_path) = group
            .paths
            .iter()
            .find(|p| {
                raw_id_by_path
                    .get(*p)
                    .is_some_and(|id| id == &group.root_raw_id)
            })
            .cloned()
        else {
            // The root file is gone while children survive: skip the live
            // unit; the tombstone path retracts the identity.
            continue;
        };
        let session_id = dsh_db_id(&group.scope, &group.root_raw_id);
        let mut member_paths = group.paths.clone();
        member_paths.sort_by(|a, b| {
            if a == &root_path {
                std::cmp::Ordering::Less
            } else if b == &root_path {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        let members: Vec<TreeMember> = member_paths
            .iter()
            .map(|path| {
                let header = header_by_path
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let raw_id = raw_id_by_path
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| path.rsplit('/').nth(1).unwrap_or("").to_string());
                let is_subagent = matches!(
                    header.get("parentSession").and_then(Value::as_str),
                    Some(parent) if !parent.is_empty()
                );
                let db_id = dsh_db_id(&group.scope, &raw_id);
                TreeMember {
                    path: path.clone(),
                    db_id: db_id.clone(),
                    agent_id: if is_subagent { Some(db_id) } else { None },
                    is_subagent,
                    header,
                }
            })
            .collect();
        let root_header = header_by_path
            .get(&root_path)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let cursor = (ctx.last_cursor)(&root_path);
        // Skip only when the checkpoint says the whole tree is unchanged.
        if changed.is_none() {
            if let Some(state) = cursor.as_deref().and_then(decode_cursor_state) {
                if tree_matches_state(&members, &state) {
                    continue;
                }
            }
        }
        if let Some(Routing::Touched(touched)) = &routing {
            if !touched.contains(&group_index) {
                continue;
            }
        }
        let members_meta: Vec<Value> = members
            .iter()
            .map(|m| {
                json!({
                    "path": m.path,
                    "rawId": raw_id_by_path.get(&m.path).cloned().unwrap_or_default(),
                    "dbId": m.db_id,
                    "agentId": m.agent_id,
                    "isSubagent": m.is_subagent,
                    "header": m.header,
                })
            })
            .collect();
        units.push(IndexUnit {
            key: root_path.clone(),
            session_id,
            project: root_header
                .get("cwd")
                .and_then(Value::as_str)
                .and_then(|cwd| project_slug_from_path(Some(cwd))),
            is_subagent: false,
            agent_id: None,
            meta: Some(json!({
                "kind": "session-tree",
                "scope": group.scope,
                "rootRawId": group.root_raw_id,
                "members": members_meta,
            })),
            retract_session_ids: Vec::new(),
        });
    }

    // Tombstones: indexed sessions whose IDENTITY no longer exists on disk.
    // They are only safe when the inventory is complete (fail closed).
    let inventory_complete = matches!(root_probe, Probe::Present) && issues.is_empty();
    let mut live_session_ids: HashSet<String> = HashSet::new();
    for group in &groups {
        if group.paths.iter().any(|p| {
            raw_id_by_path
                .get(p)
                .is_some_and(|id| id == &group.root_raw_id)
        }) {
            live_session_ids.insert(dsh_db_id(&group.scope, &group.root_raw_id));
        }
    }
    if inventory_complete {
        for indexed in ctx.indexed_sessions() {
            if live_session_ids.contains(&indexed.session_id) {
                continue; // moved or still indexed
            }
            if file_by_path.contains_key(&indexed.jsonl_path) {
                continue; // still a member of a discovered tree
            }
            if !matches!(probe_path(Path::new(&indexed.jsonl_path)), Probe::Gone) {
                continue; // present — or unreachable: keep last-good
            }
            units.push(IndexUnit {
                key: indexed.jsonl_path.clone(),
                session_id: indexed.session_id.clone(),
                project: None,
                is_subagent: false,
                agent_id: None,
                meta: Some(json!({
                    "kind": "session-tree", "scope": "", "rootRawId": "", "members": [],
                })),
                retract_session_ids: vec![indexed.session_id.clone()],
            });
        }
    }
    for issue in issues {
        ctx.report_incomplete(issue);
    }
    units
}

// ---- cursor checkpoint ----

struct MemberCheckpoint {
    agent_id: Option<String>,
    /// sha256 of the header line — identity must not change.
    header_hash: String,
    inode: u64,
    /// Committed frame count (zstd) or non-empty line count (plaintext).
    count: usize,
    /// sha256 over ALL committed entry bytes [0, count).
    prefix_hash: String,
}

struct CursorState {
    /// The session id this checkpoint belongs to — identity changes retract it.
    session_id: String,
    members: HashMap<String, MemberCheckpoint>,
    /// Last message-bearing uuid emitted per member path (parent-chain seed).
    last_message_uuid: HashMap<String, String>,
    last_message_parent_uuid: HashMap<String, Option<String>>,
    /// Per member path, the steps that already have a tool_use anchor.
    anchor_steps: HashMap<String, Vec<String>>,
}

/// Decode the opaque cursor state; None for legacy/foreign/invalid cursors.
fn decode_cursor_state(cursor: &str) -> Option<CursorState> {
    let encoded = cursor.split(':').nth(2)?;
    let value: Value = serde_json::from_slice(&base64url_decode(encoded)?).ok()?;
    if !value.is_object() {
        return None;
    }
    if value.get("v") != Some(&json!(CURSOR_STATE_VERSION)) {
        return None;
    }
    let session_id = value.get("sessionId")?.as_str()?.to_string();
    let members_obj = value.get("members")?.as_object()?;
    let mut members = HashMap::new();
    for (path, cp) in members_obj {
        members.insert(
            path.clone(),
            MemberCheckpoint {
                agent_id: cp
                    .get("agentId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                header_hash: cp.get("headerHash")?.as_str()?.to_string(),
                inode: cp.get("inode")?.as_u64()?,
                count: cp.get("count")?.as_u64()? as usize,
                prefix_hash: cp.get("prefixHash")?.as_str()?.to_string(),
            },
        );
    }
    let decode_str_map = |key: &str| -> HashMap<String, String> {
        value
            .get(key)
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let last_message_parent_uuid = value
        .get("lastMessageParentUuid")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string)))
                .collect()
        })
        .unwrap_or_default();
    let anchor_steps = value
        .get("anchorSteps")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Some(CursorState {
        session_id,
        members,
        last_message_uuid: decode_str_map("lastMessageUuid"),
        last_message_parent_uuid,
        anchor_steps,
    })
}

fn encode_cursor(mtime: f64, total_count: usize, state: &Value) -> String {
    format!(
        "{mtime}:{total_count}:{}",
        base64url_encode(state.to_string().as_bytes())
    )
}

struct ZstdArtifact {
    buffer: Vec<u8>,
    frames: Vec<(usize, usize)>,
}

struct MemberSnapshot {
    mtime_ms: f64,
    ino: u64,
    count: usize,
    prefix_hash: String,
    header_hash: String,
    zstd: Option<ZstdArtifact>,
    lines: Option<Vec<String>>,
}

fn system_time_ms(time: Option<std::time::SystemTime>) -> f64 {
    time.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn metadata_ino(metadata: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.ino()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// sha256 of the header identity fields, in TS JSON.stringify order.
fn header_hash_of(header: &Value) -> String {
    let normalized_cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(|cwd| normalize_observed_cwd(Some(cwd)));
    let array = json!([
        header.get("id").cloned().unwrap_or(Value::Null),
        header.get("createdAt").cloned().unwrap_or(Value::Null),
        normalized_cwd,
        header.get("parentSession").cloned().unwrap_or(Value::Null),
        header.get("version").cloned().unwrap_or(Value::Null),
    ]);
    sha256_hex(array.to_string().as_bytes())
}

/// sha256 over the committed prefix [0, count) of a snapshot.
fn prefix_hash_of(snap: &MemberSnapshot, count: usize) -> String {
    if let Some(zstd) = &snap.zstd {
        let end = if count == 0 {
            0
        } else {
            zstd.frames.get(count - 1).map(|f| f.1).unwrap_or(0)
        };
        sha256_hex(&zstd.buffer[..end])
    } else {
        let lines = snap.lines.as_deref().unwrap_or(&[]);
        let joined = lines
            .iter()
            .take(count)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        sha256_hex(joined.as_bytes())
    }
}

/// Load a member file once and compute its fresh checkpoint fields. The stat
/// and the content are read under ONE file descriptor, so a replacement
/// mid-snapshot cannot mix generations (TOCTOU).
fn snapshot_member(path: &Path) -> Option<MemberSnapshot> {
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).ok()?;
    let header = header_from_buffer(&path.to_string_lossy(), &buffer)?;
    let path_str = path.to_string_lossy().into_owned();
    if path_str.ends_with(".jsonl.zstd") {
        let frames = scan_zstd_frames(&buffer).ok()?;
        let mut snap = MemberSnapshot {
            mtime_ms: system_time_ms(metadata.modified().ok()),
            ino: metadata_ino(&metadata),
            count: frames.len(),
            prefix_hash: String::new(),
            header_hash: header_hash_of(&header),
            zstd: Some(ZstdArtifact { buffer, frames }),
            lines: None,
        };
        snap.prefix_hash = prefix_hash_of(&snap, snap.count);
        Some(snap)
    } else {
        let lines: Vec<String> = String::from_utf8_lossy(&buffer)
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        let mut snap = MemberSnapshot {
            mtime_ms: system_time_ms(metadata.modified().ok()),
            ino: metadata_ino(&metadata),
            count: lines.len(),
            prefix_hash: String::new(),
            header_hash: header_hash_of(&header),
            zstd: None,
            lines: Some(lines),
        };
        snap.prefix_hash = prefix_hash_of(&snap, snap.count);
        Some(snap)
    }
}

/// Fast-path preconditions for one member against its checkpoint.
fn member_matches_checkpoint(snap: &MemberSnapshot, cp: &MemberCheckpoint) -> bool {
    snap.header_hash == cp.header_hash
        && snap.ino == cp.inode
        && snap.count >= cp.count
        && prefix_hash_of(snap, cp.count) == cp.prefix_hash
}

/// Whether the whole tree matches the checkpoint (discovery skip gate).
fn tree_matches_state(members: &[TreeMember], state: &CursorState) -> bool {
    let mut state_paths: Vec<&String> = state.members.keys().collect();
    state_paths.sort();
    let mut member_paths: Vec<&String> = members.iter().map(|m| &m.path).collect();
    member_paths.sort();
    if state_paths != member_paths {
        return false;
    }
    for member in members {
        let Some(cp) = state.members.get(&member.path) else {
            return false;
        };
        let Some(snap) = snapshot_member(Path::new(&member.path)) else {
            return false;
        };
        if snap.header_hash != cp.header_hash || snap.ino != cp.inode {
            return false;
        }
        if snap.count != cp.count || snap.prefix_hash != cp.prefix_hash {
            return false;
        }
        if member.agent_id != cp.agent_id {
            return false;
        }
    }
    true
}

/// The plaintext tail [from_count, count) of a member snapshot.
fn member_text(snap: &MemberSnapshot, from_count: usize) -> Result<String, String> {
    if let Some(zstd) = &snap.zstd {
        decode_frames(&zstd.buffer, &zstd.frames, from_count)
    } else {
        Ok(snap
            .lines
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .skip(from_count)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

fn timestamp_of_ms(time: Option<f64>) -> Option<String> {
    let time = time?;
    if !time.is_finite() {
        return None;
    }
    chrono::DateTime::from_timestamp_millis(time as i64)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

fn parse_iso_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

// ---- parse: two paths over one root tree ----

fn subagent_result_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"started\s+subagent\s+(\S+)").unwrap())
}

/// JS `String(number)`.
fn js_number_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9.2e18 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[derive(Default)]
struct SubagentPart {
    parent_tool_use_id: Option<String>,
    agent_type: Option<String>,
    description: Option<String>,
    duration_ms: Option<i64>,
}

/// Mutable state threaded through one member's record loop; `push_message`
/// mirrors the TS closure of the same name (seed-parent repair, no self-link,
/// main-message counting, tree-wide ended_at).
struct Emitter {
    agent_id: Option<String>,
    seed_pending: bool,
    seed_uuid: Option<String>,
    seed_parent_uuid: Option<String>,
    last_message_uuid: Option<String>,
    last_message_parent_uuid: Option<String>,
    ended_at: Option<String>,
    member_ended_at: Option<String>,
    main_message_count: i64,
}

impl Emitter {
    fn update_ended_at(&mut self, timestamp: Option<&str>) {
        if let Some(ts) = timestamp {
            if self.ended_at.as_deref().is_none_or(|e| ts > e) {
                self.ended_at = Some(ts.to_string());
            }
        }
    }

    fn push_message(&mut self, mut record: MessageRecord, records: &mut Vec<TranscriptRecord>) {
        // The checkpointed seed belongs to the previous window; if the first
        // message of this window is from the SAME step, its parent is the
        // seed's parent — linking to the seed would create a parent cycle.
        if self.seed_pending && self.seed_uuid.is_some() && record.parent_uuid == self.seed_uuid {
            let key = step_key_of(&record.uuid);
            let seed_key = step_key_of(self.seed_uuid.as_deref().unwrap_or_default());
            if key.is_some() && key == seed_key {
                record.parent_uuid = self.seed_parent_uuid.clone();
            }
        }
        self.seed_pending = false;
        // Never self-link: a re-emitted anchor can be its own seeded parent.
        if record.parent_uuid.as_deref() == Some(record.uuid.as_str()) {
            record.parent_uuid = None;
        }
        self.last_message_uuid = Some(record.uuid.clone());
        self.last_message_parent_uuid = record.parent_uuid.clone();
        // Synthetic tool_use anchors are structural, not transcript content.
        if self.agent_id.is_none()
            && record.visibility == MessageVisibility::Visible
            && record.content_type.as_deref() != Some("tool_use")
        {
            self.main_message_count += 1;
        }
        self.update_ended_at(record.timestamp.as_deref());
        records.push(TranscriptRecord::Message(record));
    }
}

/// Recover the typed tree meta from the unit (TS casts `unit.meta`).
fn tree_unit_meta(unit: &IndexUnit) -> Result<(String, Vec<TreeMember>), String> {
    let meta = unit
        .meta
        .as_ref()
        .ok_or_else(|| "deepseek unit is missing tree meta".to_string())?;
    let obj = meta
        .as_object()
        .ok_or_else(|| "deepseek unit meta is not an object".to_string())?;
    let scope = obj
        .get("scope")
        .and_then(Value::as_str)
        .ok_or_else(|| "deepseek unit meta is missing scope".to_string())?
        .to_string();
    let members = obj
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| "deepseek unit meta is missing members".to_string())?;
    let mut out = Vec::with_capacity(members.len());
    for member in members {
        out.push(TreeMember {
            path: member
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "deepseek member is missing path".to_string())?
                .to_string(),
            db_id: member
                .get("dbId")
                .and_then(Value::as_str)
                .ok_or_else(|| "deepseek member is missing dbId".to_string())?
                .to_string(),
            agent_id: member
                .get("agentId")
                .and_then(Value::as_str)
                .map(str::to_string),
            is_subagent: member
                .get("isSubagent")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            header: member.get("header").cloned().unwrap_or_else(|| json!({})),
        });
    }
    Ok((scope, out))
}

/// Fail-closed early return: keep the last-good checkpoint (TS `return cursor`).
fn keep_cursor(cursor: &Cursor) -> Vec<StreamItem> {
    match cursor {
        Some(cursor) => vec![StreamItem::Cursor(cursor.clone())],
        None => Vec::new(),
    }
}

pub fn parse(unit: &IndexUnit, cursor: Cursor) -> Vec<StreamItem> {
    let (scope, members) = match tree_unit_meta(unit) {
        Ok(value) => value,
        Err(error) => return vec![StreamItem::Error(error)],
    };
    if members.is_empty() {
        return keep_cursor(&cursor); // tombstone: persist retracts via retract_session_ids
    }
    let session_id = unit.session_id.clone();
    let prior = cursor.as_deref().and_then(decode_cursor_state);
    let mut snaps: HashMap<String, MemberSnapshot> = HashMap::new();
    for member in &members {
        let snap = match snapshot_member(Path::new(&member.path)) {
            Some(snap) => snap,
            // Fail closed (ADR-0001): a member that cannot be read right now
            // must not produce a partial tree — and never triggers the
            // fallback's delete-session.
            None => return keep_cursor(&cursor),
        };
        // TOCTOU guard: the file was replaced since discovery's header read.
        if snap.header_hash != header_hash_of(&member.header) {
            return keep_cursor(&cursor);
        }
        snaps.insert(member.path.clone(), snap);
    }

    let mut member_paths: Vec<&String> = members.iter().map(|m| &m.path).collect();
    member_paths.sort();
    let same_member_set = prior.as_ref().is_some_and(|p| {
        let mut prior_paths: Vec<&String> = p.members.keys().collect();
        prior_paths.sort();
        prior_paths == member_paths
    });
    let fast = match &prior {
        Some(prior) => {
            same_member_set
                && members.iter().all(|member| {
                    matches!(
                        (snaps.get(&member.path), prior.members.get(&member.path)),
                        (Some(snap), Some(cp)) if member_matches_checkpoint(snap, cp)
                    )
                })
        }
        None => false,
    };

    let mut records_out: Vec<TranscriptRecord> = Vec::new();
    if !fast {
        // Snapshot fallback always retracts before re-emitting — including
        // the prior-less case, which covers a MOVED tree. The prior identity
        // may differ from the current one; retract both.
        records_out.push(TranscriptRecord::DeleteSession {
            session_id: session_id.clone(),
        });
        if let Some(prior) = &prior {
            if prior.session_id != session_id {
                records_out.push(TranscriptRecord::DeleteSession {
                    session_id: prior.session_id.clone(),
                });
            }
        }
    }

    let mut title: Option<String> = None;
    let mut emitter = Emitter {
        agent_id: None,
        seed_pending: false,
        seed_uuid: None,
        seed_parent_uuid: None,
        last_message_uuid: None,
        last_message_parent_uuid: None,
        ended_at: None,
        member_ended_at: None,
        main_message_count: 0,
    };
    // Subagent rows are contributed by both sides of the delegation; collect
    // both and emit ONE merged record per agent (ADR-0007).
    let mut subagent_parts: HashMap<String, SubagentPart> = HashMap::new();
    let mut max_mtime = 0.0f64;
    let mut total_count = 0usize;
    let mut next_members = Map::new();
    let mut next_last_message_uuid = Map::new();
    let mut next_last_message_parent = Map::new();
    let mut next_anchor_steps = Map::new();

    for member in &members {
        let Some(snap) = snaps.get(&member.path) else {
            continue;
        };
        let db_id = member.db_id.clone();
        let is_subagent = member.is_subagent;
        let cwd = member
            .header
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        let from_count = if fast {
            prior
                .as_ref()
                .and_then(|p| p.members.get(&member.path))
                .map(|cp| cp.count)
                .unwrap_or(0)
        } else {
            0
        };
        let text = match member_text(snap, from_count) {
            Ok(text) => text,
            Err(error) => return vec![StreamItem::Error(error)],
        };
        let records = read_log_records(&text);

        // Steps whose assistant/message is inside this window emit their own
        // canonical tool_use anchor; a durable tool/call for such a step must
        // not emit a provisional anchor over it.
        let mut steps_with_canonical_anchor: HashSet<String> = HashSet::new();
        for record in &records {
            if record.type_ != "assistant/message" {
                continue;
            }
            let content = record
                .data
                .get("message")
                .filter(|m| m.is_object())
                .and_then(|m| m.get("content"))
                .unwrap_or(&Value::Null);
            if classify_assistant_content(content).has_tool_calls {
                steps_with_canonical_anchor.insert(format!(
                    "{}:{}",
                    template_str(record.data.get("turn")),
                    template_str(record.data.get("step"))
                ));
            }
        }

        emitter.agent_id = member.agent_id.clone();
        emitter.last_message_uuid = if fast {
            prior
                .as_ref()
                .and_then(|p| p.last_message_uuid.get(&member.path).cloned())
        } else {
            None
        };
        emitter.last_message_parent_uuid = if fast {
            prior
                .as_ref()
                .and_then(|p| p.last_message_parent_uuid.get(&member.path).cloned())
                .flatten()
        } else {
            None
        };
        emitter.seed_pending = fast && emitter.last_message_uuid.is_some();
        emitter.seed_uuid = emitter.last_message_uuid.clone();
        emitter.seed_parent_uuid = emitter.last_message_parent_uuid.clone();
        emitter.member_ended_at = None;
        let mut current_model: Option<String> = None;
        let mut subagent_descriptor: Option<Value> = None;
        let mut emitted_anchors: HashSet<String> = HashSet::new();
        let mut anchor_steps: HashSet<String> = if fast {
            prior
                .as_ref()
                .and_then(|p| p.anchor_steps.get(&member.path).cloned())
                .unwrap_or_default()
                .into_iter()
                .collect()
        } else {
            HashSet::new()
        };

        let empty = json!({});
        for record in &records {
            let timestamp = timestamp_of_ms(record.time);
            emitter.update_ended_at(timestamp.as_deref());
            if let Some(ts) = timestamp.as_deref() {
                if emitter.member_ended_at.as_deref().is_none_or(|e| ts > e) {
                    emitter.member_ended_at = Some(ts.to_string());
                }
            }
            let data = &record.data;
            match record.type_.as_str() {
                "request/header" => {
                    let config = data
                        .get("header")
                        .filter(|h| h.is_object())
                        .and_then(|h| h.get("config"))
                        .filter(|c| c.is_object());
                    if let Some(model) = config.and_then(|c| c.get("model")).and_then(Value::as_str)
                    {
                        current_model = Some(model.to_string());
                    }
                }
                "subagent/descriptor" => {
                    if data.is_object() {
                        subagent_descriptor = Some(data.clone());
                    }
                }
                "session/title" => {
                    if !is_subagent {
                        if let Some(candidate) = data.get("title").and_then(Value::as_str) {
                            if !candidate.is_empty() {
                                title = Some(candidate.to_string());
                            }
                        }
                    }
                }
                "user/message" => {
                    let content = data.get("content").unwrap_or(&Value::Null);
                    let text = join_part_text(content, "text");
                    let source_kind = data
                        .get("source")
                        .filter(|s| s.is_object())
                        .and_then(|s| s.get("kind"));
                    let is_meta = source_kind.and_then(Value::as_str) != Some("user");
                    emitter.push_message(
                        MessageRecord {
                            uuid: user_message_uuid(&db_id, data.get("id"), record.seq),
                            session_id: session_id.clone(),
                            r#type: "user".to_string(),
                            parent_uuid: emitter.last_message_uuid.clone(),
                            timestamp: timestamp.clone(),
                            role: Some("user".to_string()),
                            text: trunc(text.as_deref()),
                            content_type: Some(
                                if text.is_some() { "text" } else { "unknown" }.to_string(),
                            ),
                            is_meta,
                            visibility: MessageVisibility::Visible,
                            model: None,
                            is_sidechain: is_subagent,
                            agent_id: member.agent_id.clone(),
                            input_tokens: None,
                            output_tokens: None,
                            cwd: cwd.clone(),
                            skill: None,
                            source: "deepseek".to_string(),
                        },
                        &mut records_out,
                    );
                }
                "assistant/message" => {
                    let message = match data.get("message") {
                        Some(message) if message.is_object() => message.clone(),
                        _ => json!({}),
                    };
                    let turn = data.get("turn");
                    let step = data.get("step");
                    let classification =
                        classify_assistant_content(message.get("content").unwrap_or(&Value::Null));
                    let usage = data.get("usage").cloned().unwrap_or(Value::Null);
                    let in_tokens = total_input_tokens(&usage);
                    let out_tokens = output_tokens(&usage);
                    let model = message
                        .get("source")
                        .and_then(|s| s.get("model"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| current_model.clone());
                    let agent_id = member.agent_id.clone();
                    let sid = session_id.clone();
                    let ts = timestamp.clone();
                    let cwd_v = cwd.clone();
                    let make = |uuid: String,
                                parent: Option<String>,
                                text: Option<String>,
                                content_type: String,
                                in_tok: Option<i64>,
                                out_tok: Option<i64>,
                                model: Option<String>| {
                        MessageRecord {
                            uuid,
                            session_id: sid.clone(),
                            r#type: "assistant".to_string(),
                            parent_uuid: parent,
                            timestamp: ts.clone(),
                            role: Some("assistant".to_string()),
                            text,
                            content_type: Some(content_type),
                            is_meta: false,
                            visibility: MessageVisibility::Visible,
                            model,
                            is_sidechain: is_subagent,
                            agent_id: agent_id.clone(),
                            input_tokens: in_tok,
                            output_tokens: out_tok,
                            cwd: cwd_v.clone(),
                            skill: None,
                            source: "deepseek".to_string(),
                        }
                    };
                    let reasoning_uuid = classification
                        .reasoning_text
                        .as_ref()
                        .map(|_| assistant_message_uuid(&db_id, turn, step, "reasoning"));
                    // A step with no projectable parts still emits a
                    // (text-less) message so its usage is never dropped.
                    let text_uuid = if classification.visible_text.is_some()
                        || (classification.reasoning_text.is_none()
                            && !classification.has_tool_calls)
                    {
                        Some(assistant_message_uuid(&db_id, turn, step, "text"))
                    } else {
                        None
                    };
                    let tool_use_anchor = classification
                        .has_tool_calls
                        .then(|| tool_use_uuid(&db_id, turn, step));
                    // Usage lands on the primary message of this step.
                    let tokens_uuid = text_uuid
                        .clone()
                        .or(tool_use_anchor.clone())
                        .or(reasoning_uuid.clone());
                    let tokens_for = |uuid: &Option<String>| {
                        if uuid.as_deref() == tokens_uuid.as_deref() {
                            in_tokens
                        } else {
                            None
                        }
                    };
                    let tokens_out_for = |uuid: &Option<String>| {
                        if uuid.as_deref() == tokens_uuid.as_deref() {
                            out_tokens
                        } else {
                            None
                        }
                    };
                    if let Some(uuid) = reasoning_uuid.clone() {
                        emitter.push_message(
                            make(
                                uuid.clone(),
                                emitter.last_message_uuid.clone(),
                                trunc(classification.reasoning_text.as_deref()),
                                "thinking".to_string(),
                                tokens_for(&Some(uuid.clone())),
                                tokens_out_for(&Some(uuid.clone())),
                                model.clone(),
                            ),
                            &mut records_out,
                        );
                    }
                    if let Some(uuid) = text_uuid.clone() {
                        emitter.push_message(
                            make(
                                uuid.clone(),
                                emitter.last_message_uuid.clone(),
                                trunc(classification.visible_text.as_deref()),
                                if classification.visible_text.is_some() {
                                    "text"
                                } else {
                                    "unknown"
                                }
                                .to_string(),
                                tokens_for(&Some(uuid.clone())),
                                tokens_out_for(&Some(uuid.clone())),
                                model.clone(),
                            ),
                            &mut records_out,
                        );
                    }
                    if let Some(anchor) = tool_use_anchor.clone() {
                        emitted_anchors.insert(anchor.clone());
                        anchor_steps.insert(format!(
                            "{}:{}",
                            template_str(turn),
                            template_str(step)
                        ));
                        emitter.push_message(
                            make(
                                anchor,
                                emitter.last_message_uuid.clone(),
                                None,
                                "tool_use".to_string(),
                                tokens_for(&tool_use_anchor),
                                tokens_out_for(&tool_use_anchor),
                                model.clone(),
                            ),
                            &mut records_out,
                        );
                    }
                    // tool_call records come from the durable tool/call events.
                }
                "tool/call" => {
                    let Some(native_call_id) = data.get("callId").and_then(Value::as_str) else {
                        continue;
                    };
                    let tool_name = data.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let args = parse_tool_arguments(data.get("arguments").unwrap_or(&Value::Null));
                    // The anchor message must exist even when this step's
                    // assistant/message has no tool-call part (or never
                    // landed): tool_calls and tool_results are filtered by
                    // message_uuid downstream.
                    let anchor = tool_use_uuid(&db_id, data.get("turn"), data.get("step"));
                    let step_key = format!(
                        "{}:{}",
                        template_str(data.get("turn")),
                        template_str(data.get("step"))
                    );
                    if !emitted_anchors.contains(&anchor)
                        && !steps_with_canonical_anchor.contains(&step_key)
                        && !anchor_steps.contains(&step_key)
                    {
                        emitted_anchors.insert(anchor.clone());
                        anchor_steps.insert(step_key.clone());
                        emitter.push_message(
                            MessageRecord {
                                uuid: anchor.clone(),
                                session_id: session_id.clone(),
                                r#type: "assistant".to_string(),
                                parent_uuid: emitter.last_message_uuid.clone(),
                                timestamp: timestamp.clone(),
                                role: Some("assistant".to_string()),
                                text: None,
                                content_type: Some("tool_use".to_string()),
                                is_meta: false,
                                visibility: MessageVisibility::Visible,
                                model: current_model.clone(),
                                is_sidechain: is_subagent,
                                agent_id: member.agent_id.clone(),
                                input_tokens: None,
                                output_tokens: None,
                                cwd: cwd.clone(),
                                skill: None,
                                source: "deepseek".to_string(),
                            },
                            &mut records_out,
                        );
                    }
                    records_out.push(TranscriptRecord::ToolCall(ToolCallRecord {
                        id: call_id(&db_id, native_call_id),
                        message_uuid: anchor,
                        session_id: session_id.clone(),
                        name: tool_name.to_string(),
                        presentation: if tool_name == "skill" {
                            ToolCallPresentation::Skill
                        } else {
                            ToolCallPresentation::Default
                        },
                        input_json: trunc_json_default(&args).unwrap_or_else(|| "{}".to_string()),
                        file_path: dsh_tool_file_path(
                            tool_name,
                            if args.is_object() { Some(&args) } else { None },
                        ),
                    }));
                }
                "tool/result" => {
                    let message = match data.get("message") {
                        Some(message) if message.is_object() => message,
                        _ => &empty,
                    };
                    let source = match message.get("source") {
                        Some(source) if source.is_object() => source,
                        _ => &empty,
                    };
                    let Some(native_call_id) = source.get("callId").and_then(Value::as_str) else {
                        continue;
                    };
                    let tool_id = call_id(&db_id, native_call_id);
                    let content =
                        tool_result_content(message.get("content").unwrap_or(&Value::Null));
                    records_out.push(TranscriptRecord::ToolResult(ToolResultRecord {
                        tool_use_id: tool_id.clone(),
                        message_uuid: Some(tool_use_uuid(
                            &db_id,
                            data.get("turn"),
                            data.get("step"),
                        )),
                        session_id: session_id.clone(),
                        content: trunc(Some(&content)).unwrap_or_default(),
                        file_path: None,
                        is_error: tool_result_is_error(
                            data,
                            message.get("content").unwrap_or(&Value::Null),
                        ),
                    }));
                    // Subagent spawns are self-contained in their result
                    // text, so the link survives any parse window.
                    if let Some(caps) = subagent_result_re().captures(&content) {
                        if let Some(agent) = caps.get(1) {
                            subagent_parts
                                .entry(dsh_db_id(&scope, agent.as_str()))
                                .or_default()
                                .parent_tool_use_id = Some(tool_id.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        if is_subagent {
            // Continuable-mode descriptors carry agentProvider/agentModel;
            // one-shot descriptors only carry `provider`.
            let descriptor = subagent_descriptor.clone().unwrap_or_else(|| json!({}));
            let agent_type = ["agentProvider", "agentModel", "provider"]
                .iter()
                .find_map(|key| {
                    descriptor
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let part = subagent_parts
                .entry(member.agent_id.clone().unwrap_or_default())
                .or_default();
            part.agent_type = agent_type;
            part.description = descriptor
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string);
            let started_ms = member.header.get("createdAt").and_then(Value::as_f64);
            // Duration spans the member's own events, not the tree-wide ones.
            let ended_ms = emitter.member_ended_at.as_deref().and_then(parse_iso_ms);
            part.duration_ms = match (started_ms, ended_ms) {
                (Some(started), Some(ended)) => Some(0.max(ended - started as i64)),
                _ => None,
            };
            // total_tokens is derived at query time from sidechain messages.
        }
        if let Some(uuid) = &emitter.last_message_uuid {
            next_last_message_uuid.insert(member.path.clone(), json!(uuid));
            next_last_message_parent.insert(
                member.path.clone(),
                emitter
                    .last_message_parent_uuid
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
        }
        if !anchor_steps.is_empty() {
            next_anchor_steps.insert(
                member.path.clone(),
                json!(anchor_steps.iter().collect::<Vec<_>>()),
            );
        }
        next_members.insert(
            member.path.clone(),
            json!({
                "agentId": member.agent_id,
                "headerHash": snap.header_hash,
                "inode": snap.ino,
                "count": snap.count,
                "prefixHash": snap.prefix_hash,
            }),
        );
        max_mtime = max_mtime.max(snap.mtime_ms);
        total_count += snap.count;
    }

    for (agent, part) in &subagent_parts {
        records_out.push(TranscriptRecord::Subagent(SubagentRecord {
            agent_id: agent.clone(),
            session_id: session_id.clone(),
            parent_tool_use_id: part.parent_tool_use_id.clone(),
            agent_type: part.agent_type.clone(),
            description: part.description.clone(),
            duration_ms: part.duration_ms,
            total_tokens: None,
        }));
    }

    let root_header = members
        .first()
        .map(|m| m.header.clone())
        .unwrap_or_else(|| json!({}));
    records_out.push(TranscriptRecord::Session(SessionRecord {
        id: session_id.clone(),
        title: title.clone(),
        project: root_header
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(|cwd| project_slug_from_path(Some(cwd)))
            .or_else(|| unit.project.clone()),
        started_at: root_header
            .get("createdAt")
            .and_then(Value::as_f64)
            .and_then(|ms| timestamp_of_ms(Some(ms))),
        ended_at: emitter.ended_at.clone(),
        // Branch observed on any member header in the session (M4.5 branch
        // filter; claude.rs parses the same field name).
        git_branch: members
            .iter()
            .filter_map(|m| m.header.get("gitBranch").and_then(Value::as_str))
            .map(str::to_string)
            .find(|branch| !branch.is_empty()),
        version: root_header
            .get("version")
            .and_then(Value::as_f64)
            .map(js_number_string),
        message_count: emitter.main_message_count,
        count_mode: if fast {
            SessionCountMode::Delta
        } else {
            SessionCountMode::Total
        },
        jsonl_path: unit.key.clone(),
        source: "deepseek".to_string(),
    }));

    // A tree is one session timeline: emit messages in the canonical
    // (timestamp, uuid) order; non-message records are keyed maps downstream.
    let mut messages: Vec<MessageRecord> = Vec::new();
    let mut others: Vec<TranscriptRecord> = Vec::new();
    for record in records_out {
        match record {
            TranscriptRecord::Message(message) => messages.push(message),
            other => others.push(other),
        }
    }
    messages.sort_by(|a, b| {
        let at = a.timestamp.as_deref().unwrap_or("");
        let bt = b.timestamp.as_deref().unwrap_or("");
        if at == bt {
            a.uuid.cmp(&b.uuid)
        } else {
            at.cmp(bt)
        }
    });

    let state = json!({
        "v": CURSOR_STATE_VERSION,
        "sessionId": session_id,
        "members": Value::Object(next_members),
        "lastMessageUuid": Value::Object(next_last_message_uuid),
        "lastMessageParentUuid": Value::Object(next_last_message_parent),
        "anchorSteps": Value::Object(next_anchor_steps),
    });
    let mut items: Vec<StreamItem> = others.into_iter().map(StreamItem::Record).collect();
    items.extend(
        messages
            .into_iter()
            .map(TranscriptRecord::Message)
            .map(StreamItem::Record),
    );
    items.push(StreamItem::Cursor(encode_cursor(
        max_mtime,
        total_count,
        &state,
    )));
    items
}

// ---- raw ----

enum RawFinder {
    User { captured: String },
    Assistant { turn: f64, step: f64 },
}

impl RawFinder {
    fn matches(&self, value: &Value) -> bool {
        let data = match value.get("data") {
            Some(data) if data.is_object() => data,
            _ => return false,
        };
        match self {
            RawFinder::User { captured } => {
                if value.get("type").and_then(Value::as_str) != Some("user/message") {
                    return false;
                }
                let id_match = data.get("id") == Some(&Value::String(captured.clone()));
                let seq_match = captured
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|n| data.get("seq").and_then(Value::as_f64) == Some(n));
                id_match || seq_match
            }
            RawFinder::Assistant { turn, step } => {
                value.get("type").and_then(Value::as_str) == Some("assistant/message")
                    && data.get("turn").and_then(Value::as_f64) == Some(*turn)
                    && data.get("step").and_then(Value::as_f64) == Some(*step)
            }
        }
    }
}

fn raw_user_uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^deepseek:([^:]+):([0-9a-f]{64}):u(.+)$").unwrap())
}

fn raw_assistant_uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^deepseek:([^:]+):([0-9a-f]{64}):t(\d+):s(\d+):(reasoning|text|tool_use)$")
            .unwrap()
    })
}

fn raw_agent_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^deepseek:([^:]+):([0-9a-f]{64})$").unwrap())
}

fn raw_deepseek(root_dir: &Path, input: &RawLookup) -> Option<RawRecord> {
    let uuid = input.message_uuid;
    let (mut raw_session_id, mut scope, finder) =
        if let Some(caps) = raw_user_uuid_re().captures(uuid) {
            (
                decode_uri_component(&caps[1])?,
                caps[2].to_string(),
                RawFinder::User {
                    captured: caps[3].to_string(),
                },
            )
        } else {
            let caps = raw_assistant_uuid_re().captures(uuid)?;
            (
                decode_uri_component(&caps[1])?,
                caps[2].to_string(),
                RawFinder::Assistant {
                    turn: caps[3].parse().ok()?,
                    step: caps[4].parse().ok()?,
                },
            )
        };
    // Sidechain lookups: agentId steers to the child session even though the
    // session row points at the root file.
    if let Some(agent_id) = input.agent_id {
        if let Some(caps) = raw_agent_id_re().captures(agent_id) {
            if !caps[1].is_empty() {
                raw_session_id = decode_uri_component(&caps[1])?;
                scope = caps[2].to_string();
            }
        }
    }
    let session_jsonl = input
        .session
        .and_then(|s| s.get("jsonl_path"))
        .and_then(Value::as_str)
        .filter(|_| input.agent_id.is_none());
    let path = match session_jsonl {
        Some(path) => path.to_string(),
        None => find_session_file(root_dir, &raw_session_id, Some(&scope))?,
    };
    let path = Path::new(&path);
    if !path.exists() {
        return None;
    }
    let buffer = std::fs::read(path).ok()?;
    let text = if path.to_string_lossy().ends_with(".jsonl.zstd") {
        let frames = scan_zstd_frames(&buffer).ok()?;
        decode_frames(&buffer, &frames, 0).ok()?
    } else {
        String::from_utf8_lossy(&buffer).into_owned()
    };
    let mut found: Option<String> = None;
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if value.is_object() && finder.matches(&value) {
            found = Some(line.to_string());
            break;
        }
    }
    let found = found?;
    let mut message_text: Option<String> = None;
    if let Ok(value) = serde_json::from_str::<Value>(&found) {
        if value.is_object() {
            let data = match value.get("data") {
                Some(data) if data.is_object() => data,
                _ => &Value::Null,
            };
            match value.get("type").and_then(Value::as_str) {
                Some("user/message") => {
                    message_text =
                        join_part_text(data.get("content").unwrap_or(&Value::Null), "text")
                }
                Some("assistant/message") => {
                    let message = match data.get("message") {
                        Some(message) if message.is_object() => message,
                        _ => &Value::Null,
                    };
                    message_text =
                        join_part_text(message.get("content").unwrap_or(&Value::Null), "text");
                }
                _ => {}
            }
        }
    }
    let total_length = found.chars().map(char::len_utf16).sum::<usize>();
    Some(RawRecord {
        text: found,
        total_length: Some(total_length),
        offset: Some(0),
        limit: Some(total_length),
        has_more: Some(false),
        message_text,
    })
}

/// Resolve the sessions root: `$DSH_HOME/sessions` or `~/.dsh/sessions`.
fn sessions_root() -> PathBuf {
    let env = std::env::var("DSH_HOME")
        .ok()
        .filter(|home| !home.trim().is_empty());
    match env {
        Some(home) => PathBuf::from(home).join("sessions"),
        None => PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".dsh")
            .join("sessions"),
    }
}

pub struct DeepseekProvider {
    pub root_dir: PathBuf,
}

impl DeepseekProvider {
    pub fn new() -> Self {
        Self {
            root_dir: sessions_root(),
        }
    }

    pub fn with_root(root_dir: PathBuf) -> Self {
        Self { root_dir }
    }
}

impl Default for DeepseekProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for DeepseekProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: NAME,
            name: "DeepSeek Harness",
            vendor: "DeepSeek",
            default_root: self.root_dir.to_string_lossy().into_owned(),
            color: "#4d6bfe",
            requires_explicit_root: false,
            root_resolution_reason: None,
        }
    }

    fn index_version_marker(&self) -> Option<&'static str> {
        Some(DEEPSEEK_CANONICAL_TRANSCRIPT_MARKER)
    }

    fn watch_targets(&self, configured_root: &str) -> Vec<WatchTarget> {
        vec![WatchTarget {
            kind: WatchTargetKind::Tree,
            path: configured_root.to_string(),
        }]
    }

    fn discover<'a>(&'a self, ctx: &mut DiscoverContext<'a>) -> Vec<IndexUnit> {
        discover_at(&self.root_dir, ctx)
    }

    fn parse<'a>(&'a self, unit: &'a IndexUnit, cursor: Cursor) -> ParseStream<'a> {
        let items = parse(unit, cursor);
        Box::new(items.into_iter())
    }

    fn raw(&self, input: &RawLookup) -> Option<RawRecord> {
        raw_deepseek(&self.root_dir, input)
    }
}
