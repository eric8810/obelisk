// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Pi coding-agent provider adapter (port of packages/core/src/providers/pi.ts).
//!
//! Pure: discovers Pi session JSONL snapshots and projects one into the
//! canonical record stream. Pi is a snapshot provider: every parse replays the
//! complete session, so records are preceded by a `delete-session` for the
//! same logical id and the cursor is a full stat fingerprint rather than a
//! line watermark.
//!
//! Semantics ported 1:1 from the TS source:
//! - `piSessionId`: project-local header id namespaced by the header cwd.
//! - branch discovery: leaf entries select the active head; orphan parents
//!   (missing parentId) become roots; compactions split legacy
//!   (firstKeptEntryId) vs storage checkpoint (retainedTail) semantics.
//! - visibility: `inactive` is only ever derived from tree position (not on
//!   the active branch); `hidden` only from a custom message with
//!   `display: false`. Assemblers never infer either from text.
//! - tool-scope chains make structured tool results branch-local even when Pi
//!   reuses a native tool id across branches.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::parsing::{
    normalize_observed_cwd, normalize_path, project_slug_from_path, trunc, trunc_json_default,
};
use crate::providers::types::{
    Cursor, DiscoverContext, IndexUnit, InventoryIssue, MessageRecord, MessageVisibility,
    ParseStream, ProviderAdapter, ProviderDescriptor, RawLookup, RawRecord, SessionCountMode,
    StreamItem, ToolCallPresentation, ToolCallRecord, ToolResultRecord, TranscriptRecord,
    WatchTarget, WatchTargetKind,
};

pub const NAME: &str = "pi";
pub const PI_CANONICAL_TRANSCRIPT_MARKER: &str = "__pi_canonical_transcript_v9__";

const MAX_HEADER_BYTES: usize = 1024 * 1024;
const BLOCK_PAD: usize = 4;

// ---------------------------------------------------------------------------
// SHA-256 (no sha2 crate in the workspace; private to this module)
// ---------------------------------------------------------------------------

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
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
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
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Root resolution (port of resolveDefaultPiRoot / resolvePiRoot)
// ---------------------------------------------------------------------------

/// Result of the official Pi root precedence walk (`PiRootResolution`).
#[derive(Debug, Clone)]
pub struct PiRootResolution {
    pub root: PathBuf,
    pub requires_explicit_root: bool,
    pub reason: Option<String>,
}

/// Environment accessor used by root resolution so callers can inject values.
pub type PiEnv<'a> = dyn Fn(&str) -> Option<String> + 'a;

fn os_homedir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(home);
    }
    PathBuf::from("/")
}

fn expand_tilde(path: &str, home_dir: &Path) -> String {
    if path == "~" {
        return home_dir.to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_dir.join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

fn configured_absolute_path(value: &str, home_dir: &Path) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded = expand_tilde(trimmed, home_dir);
    if Path::new(&expanded).is_absolute() {
        Some(normalize_path(&expanded))
    } else {
        None
    }
}

fn default_agent_dir(home_dir: &Path) -> PathBuf {
    home_dir.join(".pi").join("agent")
}

fn read_settings(path: &Path) -> Value {
    if !path.exists() {
        return json!({});
    }
    // Pi 0.83 records the load error and continues with an empty settings
    // scope.
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value @ Value::Object(_)) => value,
            _ => json!({}),
        },
        Err(_) => json!({}),
    }
}

fn sessions_root(agent_dir: &Path) -> PathBuf {
    agent_dir.join("sessions")
}

fn absolute_resolution(root: String) -> PiRootResolution {
    PiRootResolution {
        root: PathBuf::from(root),
        requires_explicit_root: false,
        reason: None,
    }
}

/// Port of `resolveDefaultPiRoot`: env vars, then global/project settings.json,
/// with relative values rejected as launch-cwd-dependent.
pub fn resolve_default_pi_root(
    env: &PiEnv,
    home_dir: &Path,
    cwd: Option<&str>,
) -> PiRootResolution {
    let fallback_agent_dir = default_agent_dir(home_dir);
    let fallback_root = sessions_root(&fallback_agent_dir);
    if let Some(env_session_dir) = env("PI_CODING_AGENT_SESSION_DIR") {
        if !env_session_dir.trim().is_empty() {
            return match configured_absolute_path(&env_session_dir, home_dir) {
                Some(absolute) => absolute_resolution(absolute),
                None => PiRootResolution {
                    root: fallback_root,
                    requires_explicit_root: true,
                    reason: Some(
                        "PI_CODING_AGENT_SESSION_DIR is relative to the Pi launch cwd".to_string(),
                    ),
                },
            };
        }
    }

    let mut agent_dir = fallback_agent_dir;
    if let Some(env_agent_dir) = env("PI_CODING_AGENT_DIR") {
        if !env_agent_dir.trim().is_empty() {
            match configured_absolute_path(&env_agent_dir, home_dir) {
                Some(absolute) => agent_dir = PathBuf::from(absolute),
                None => {
                    return PiRootResolution {
                        root: fallback_root,
                        requires_explicit_root: true,
                        reason: Some(
                            "PI_CODING_AGENT_DIR is relative to the Pi launch cwd".to_string(),
                        ),
                    };
                }
            }
        }
    }

    let global_settings = read_settings(&agent_dir.join("settings.json"));
    let project_cwd = normalize_observed_cwd(cwd);
    let project_settings = match &project_cwd {
        Some(project_cwd) => {
            read_settings(&PathBuf::from(project_cwd).join(".pi").join("settings.json"))
        }
        None => json!({}),
    };
    let project_overrides_session_dir = project_settings.get("sessionDir").is_some();
    let session_dir = if project_overrides_session_dir {
        project_settings.get("sessionDir")
    } else {
        global_settings.get("sessionDir")
    };
    let Some(session_dir) = session_dir else {
        return PiRootResolution {
            root: sessions_root(&agent_dir),
            requires_explicit_root: false,
            reason: None,
        };
    };
    if let Some(session_dir) = session_dir.as_str() {
        if !session_dir.trim().is_empty() {
            let expanded = expand_tilde(session_dir.trim(), home_dir);
            if Path::new(&expanded).is_absolute() {
                return absolute_resolution(normalize_path(&expanded));
            }
            if project_overrides_session_dir {
                if let Some(project_cwd) = &project_cwd {
                    return absolute_resolution(normalize_path(
                        &PathBuf::from(project_cwd).join(&expanded).to_string_lossy(),
                    ));
                }
            }
            return PiRootResolution {
                root: sessions_root(&agent_dir),
                requires_explicit_root: true,
                reason: Some(
                    "Pi global settings.json sessionDir is relative to the Pi launch cwd"
                        .to_string(),
                ),
            };
        }
        // Empty string selects the default corpus (falls through).
    } else if !session_dir.is_null() {
        // Present but not a string (false, 0, ...): never select the corpus.
        return PiRootResolution {
            root: sessions_root(&agent_dir),
            requires_explicit_root: true,
            reason: Some("Pi settings.json sessionDir must be a string".to_string()),
        };
    }
    PiRootResolution {
        root: sessions_root(&agent_dir),
        requires_explicit_root: false,
        reason: None,
    }
}

/// Port of `resolvePiRoot`: an explicitly configured absolute root wins;
/// anything else defers to the automatic resolution.
pub fn resolve_pi_root(
    root_dir: Option<&str>,
    cwd: Option<&str>,
    env: &PiEnv,
    home_dir: &Path,
) -> PiRootResolution {
    if let Some(root_dir) = root_dir {
        if let Some(absolute) = configured_absolute_path(root_dir, home_dir) {
            return absolute_resolution(absolute);
        }
    }
    let automatic = resolve_default_pi_root(env, home_dir, cwd);
    if root_dir.is_none() {
        return automatic;
    }
    PiRootResolution {
        root: automatic.root,
        requires_explicit_root: true,
        reason: Some("Obelisk Pi providerRoot must be absolute or start with ~".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Header inspection / session identity
// ---------------------------------------------------------------------------

fn as_header(value: &Value, path: &str) -> Result<Value, String> {
    let header = match value {
        Value::Object(map) => map,
        _ => return Err(format!("Pi session header is not an object: {path}")),
    };
    // `header.version ?? 1` + `Number.isInteger(version)`.
    let version = match header.get("version") {
        None | Some(Value::Null) => 1f64,
        Some(v) => match v.as_f64() {
            Some(number) if number == number.trunc() => number,
            _ => {
                return Err(format!(
                    "Unsupported or malformed Pi session header: {path}"
                ))
            }
        },
    };
    let field_is_string_or_absent = |field: &str| match header.get(field) {
        None | Some(Value::Null) => true,
        Some(Value::String(_)) => true,
        Some(_) => false,
    };
    let id_ok = header
        .get("id")
        .and_then(Value::as_str)
        .map(|id| !id.is_empty())
        .unwrap_or(false);
    if header.get("type").and_then(Value::as_str) != Some("session")
        || !id_ok
        || !field_is_string_or_absent("timestamp")
        || !field_is_string_or_absent("cwd")
        || !field_is_string_or_absent("parentSession")
        || !(1.0..=3.0).contains(&version)
    {
        return Err(format!(
            "Unsupported or malformed Pi session header: {path}"
        ));
    }
    Ok(value.clone())
}

/// JS `encodeURIComponent`: escape everything except `A-Za-z0-9-_.!~*'()`.
fn js_encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')')
        {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Port of `piSessionId`: `pi:<encoded header id>:<sha256 cwd namespace>`.
///
/// Pi's --session-id lookup is project-local, so the header id alone is not
/// globally unique. The immutable header cwd supplies a path-independent
/// namespace. Pi's permissive v1 loader also accepts a missing cwd; those
/// sessions share a deterministic legacy namespace rather than inheriting
/// Obelisk's unrelated launch cwd.
pub fn pi_session_id(header: &Value) -> String {
    let raw_cwd = header.get("cwd").and_then(Value::as_str);
    let cwd = normalize_observed_cwd(raw_cwd)
        .or_else(|| raw_cwd.map(str::to_string))
        .unwrap_or_default();
    let mut digest_input = b"pi-cwd-v1\0".to_vec();
    digest_input.extend_from_slice(cwd.as_bytes());
    let project_scope = sha256_hex(&digest_input);
    let id = header.get("id").and_then(Value::as_str).unwrap_or_default();
    format!("pi:{}:{project_scope}", js_encode_uri_component(id))
}

fn invalid_unit_id(path: &str) -> String {
    let digest = sha256_hex(path.as_bytes());
    format!("pi:invalid:{}", &digest[..24])
}

/// Port of `inspectHeader`: scan the first 1 MiB for the first parseable
/// line; it must be a valid session header.
fn inspect_header(path: &Path) -> Result<Value, String> {
    let display_path = path.to_string_lossy().into_owned();
    let mut file = std::fs::File::open(path).map_err(|error| format!("{display_path}: {error}"))?;
    let mut pending: Vec<u8> = Vec::new();
    let mut scanned = 0usize;
    let inspect_line = |line: &[u8]| -> Option<Result<Value, String>> {
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\r');
        if text.trim().is_empty() {
            return None;
        }
        match serde_json::from_str::<Value>(text) {
            // Pi's loader skips malformed physical lines while looking for
            // the first parsed entry. Full transcript parsing remains strict.
            Err(_) => None,
            Ok(value) => Some(as_header(&value, &display_path)),
        }
    };
    use std::io::Read;
    while scanned < MAX_HEADER_BYTES {
        let chunk_len = 4096.min(MAX_HEADER_BYTES - scanned);
        let mut buffer = vec![0u8; chunk_len];
        let bytes = file
            .read(&mut buffer)
            .map_err(|error| format!("{display_path}: {error}"))?;
        if bytes == 0 {
            if !pending.is_empty() {
                if let Some(header) = inspect_line(&pending) {
                    return header;
                }
            }
            return Err(format!("Malformed Pi session header in {display_path}"));
        }
        scanned += bytes;
        pending.extend_from_slice(&buffer[..bytes]);
        while let Some(newline) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..newline).collect();
            pending.remove(0);
            if let Some(header) = inspect_line(&line) {
                return header;
            }
        }
    }
    let mut probe = [0u8; 1];
    let bytes = file
        .read(&mut probe)
        .map_err(|error| format!("{display_path}: {error}"))?;
    if bytes == 0 {
        if !pending.is_empty() {
            if let Some(header) = inspect_line(&pending) {
                return header;
            }
        }
        return Err(format!("Malformed Pi session header in {display_path}"));
    }
    Err(format!(
        "Pi session header exceeds {MAX_HEADER_BYTES} bytes: {display_path}"
    ))
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

struct ListResult {
    files: Vec<String>,
    complete: bool,
    root_missing: bool,
    issue: Option<InventoryIssue>,
}

fn inventory_issue(source_path: &str, error: &std::io::Error) -> InventoryIssue {
    InventoryIssue {
        path: source_path.to_string(),
        error: error.to_string(),
    }
}

fn list_jsonl_files(root: &Path) -> ListResult {
    let mut stack = vec![root.to_path_buf()];
    let mut files: Vec<String> = Vec::new();
    let mut complete = true;
    let mut root_missing = false;
    let mut issue: Option<InventoryIssue> = None;
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) => {
                complete = false;
                issue.get_or_insert_with(|| inventory_issue(&current.to_string_lossy(), &error));
                if current == root && error.kind() == std::io::ErrorKind::NotFound {
                    root_missing = true;
                }
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    complete = false;
                    issue
                        .get_or_insert_with(|| inventory_issue(&current.to_string_lossy(), &error));
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = current.join(&name);
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    complete = false;
                    issue.get_or_insert_with(|| inventory_issue(&path.to_string_lossy(), &error));
                    continue;
                }
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if name.to_lowercase().ends_with(".jsonl") {
                if file_type.is_file() {
                    files.push(normalize_path(&path.to_string_lossy()));
                } else if file_type.is_symlink() {
                    // Pi lists session names first and follows the path when
                    // opening it, so a readable file symlink is live session
                    // provenance.
                    match std::fs::metadata(&path) {
                        Ok(metadata) if metadata.is_file() => {
                            files.push(normalize_path(&path.to_string_lossy()));
                        }
                        Ok(_) => {
                            complete = false;
                            issue.get_or_insert(InventoryIssue {
                                path: path.to_string_lossy().into_owned(),
                                error: "Expected a file symlink target".to_string(),
                            });
                        }
                        Err(error) => {
                            complete = false;
                            issue.get_or_insert_with(|| {
                                inventory_issue(&path.to_string_lossy(), &error)
                            });
                        }
                    }
                }
            }
        }
    }
    files.sort();
    ListResult {
        files,
        complete,
        root_missing,
        issue,
    }
}

fn normalized_changed_paths(root: &Path, changed_paths: Option<&[String]>) -> Option<Vec<String>> {
    changed_paths.map(|paths| {
        paths
            .iter()
            .map(|path| {
                if Path::new(path).is_absolute() {
                    normalize_path(path)
                } else {
                    normalize_path(&root.join(path).to_string_lossy())
                }
            })
            .collect()
    })
}

/// TS `relative(changedPath, path)` is non-empty, stays inside, and relative.
fn path_affected(path: &str, changed_paths: &[String]) -> bool {
    changed_paths.iter().any(|changed| {
        if path == changed {
            return true;
        }
        path.starts_with(changed.as_str())
            && path.len() > changed.len()
            && path.as_bytes()[changed.len()] == b'/'
    })
}

/// `${mtimeMs}:0:pi-snapshot-v1:${ctimeMs}:${size}:${ino}` — keeps the legacy
/// mtime/line slots first while the opaque tail retains the stat fingerprint.
fn snapshot_cursor(stat: (f64, i64, f64, u64)) -> String {
    let (mtime_ms, size, ctime_ms, ino) = stat;
    format!("{mtime_ms}:0:pi-snapshot-v1:{ctime_ms}:{size}:{ino}")
}

fn snapshot_of(path: &Path) -> Result<String, std::io::Error> {
    crate::parsing::file_signature(path).map(snapshot_cursor)
}

fn file_digest(path: &Path, expected_cursor: &str) -> Result<String, std::io::Error> {
    let digest = sha256_hex(&std::fs::read(path)?);
    let changed = match snapshot_of(path) {
        Ok(cursor) => cursor != expected_cursor,
        Err(_) => true,
    };
    if changed {
        return Err(std::io::Error::other(format!(
            "Pi session changed during discovery: {}",
            path.to_string_lossy()
        )));
    }
    Ok(digest)
}

struct InspectedFile {
    path: String,
    header: Option<Value>,
    session_id: String,
    current_cursor: String,
}

fn discover_at(root: &Path, ctx: &mut DiscoverContext) -> Vec<IndexUnit> {
    let inventory = list_jsonl_files(root);
    let indexed_sessions: Vec<(String, String)> = ctx
        .indexed_sessions()
        .into_iter()
        .map(|session| (session.session_id, normalize_path(&session.jsonl_path)))
        .collect();
    let indexed_path_by_session_id: HashMap<String, String> =
        indexed_sessions.iter().cloned().collect();
    // A provider root that has never contributed a session is safely empty.
    // Once Pi provenance exists, the same absence is ambiguous and must
    // preserve it.
    let safely_missing_root = inventory.root_missing && indexed_sessions.is_empty();
    let mut inventory_complete = inventory.complete || safely_missing_root;
    let mut reported_issue = if safely_missing_root {
        None
    } else {
        inventory.issue.clone()
    };
    let mut on_incomplete = |issue: InventoryIssue| {
        inventory_complete = false;
        reported_issue.get_or_insert(issue);
    };
    let files = inventory.files;
    let file_set: HashSet<String> = files.iter().cloned().collect();
    let changed_paths = normalized_changed_paths(root, ctx.changed_paths);

    let mut inspected: Vec<InspectedFile> = Vec::new();
    for path in &files {
        let before_cursor = match snapshot_of(Path::new(path)) {
            Ok(cursor) => Some(cursor),
            Err(cause) => {
                on_incomplete(inventory_issue(path, &cause));
                None
            }
        };
        let header = inspect_header(Path::new(path)).ok();
        let current_cursor = match snapshot_of(Path::new(path)) {
            Ok(cursor) => cursor,
            Err(cause) => {
                on_incomplete(inventory_issue(path, &cause));
                continue;
            }
        };
        let Some(before_cursor) = before_cursor else {
            continue;
        };
        if before_cursor != current_cursor {
            on_incomplete(InventoryIssue {
                path: path.clone(),
                error: "Session changed during discovery".to_string(),
            });
            continue;
        }
        let session_id = match &header {
            Some(header) => pi_session_id(header),
            None => invalid_unit_id(path),
        };
        inspected.push(InspectedFile {
            path: path.clone(),
            header,
            session_id,
            current_cursor,
        });
    }

    // Group by session id, preserving first-seen order (TS Map semantics).
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    for (index, file) in inspected.iter().enumerate() {
        let next = groups.len();
        let slot = group_index.entry(file.session_id.clone()).or_insert(next);
        if *slot == next {
            groups.push((file.session_id.clone(), Vec::new()));
        }
        groups[*slot].1.push(index);
    }
    struct SelectedGroup {
        /// All group members (retractions and changed-path gating consider
        /// the whole group, matching the TS `group` loop variable).
        member_indexes: Vec<usize>,
        /// Files this run may publish (deduplicated identical copies).
        candidate_indexes: Vec<usize>,
        collision_paths: Option<Vec<String>>,
    }
    let mut selected_groups: Vec<(String, SelectedGroup)> = Vec::new();
    for (group_session_id, indexes) in &groups {
        let mut candidates = indexes.clone();
        let mut collision_paths: Option<Vec<String>> = None;
        let all_error_free = indexes.iter().all(|&i| inspected[i].header.is_some());
        if indexes.len() > 1 && all_error_free {
            let mut digests: HashSet<String> = HashSet::new();
            let mut digest_failed = false;
            for &i in indexes {
                let file = &inspected[i];
                match file_digest(Path::new(&file.path), &file.current_cursor) {
                    Ok(digest) => {
                        digests.insert(digest);
                    }
                    Err(error) => {
                        on_incomplete(inventory_issue(&file.path, &error));
                        digest_failed = true;
                    }
                }
            }
            if digest_failed {
                continue;
            }
            if digests.len() == 1 {
                candidates = vec![indexes[0]];
            } else {
                collision_paths =
                    Some(indexes.iter().map(|&i| inspected[i].path.clone()).collect());
            }
        }
        selected_groups.push((
            group_session_id.clone(),
            SelectedGroup {
                member_indexes: indexes.clone(),
                candidate_indexes: candidates,
                collision_paths,
            },
        ));
    }
    if !inventory_complete {
        ctx.report_incomplete(reported_issue.unwrap_or(InventoryIssue {
            path: root.to_string_lossy().into_owned(),
            error: "Source inventory is incomplete".to_string(),
        }));
    }

    let valid_by_path: HashMap<&str, &InspectedFile> = inspected
        .iter()
        .filter(|file| file.header.is_some())
        .map(|file| (file.path.as_str(), file))
        .collect();
    let valid_session_ids: HashSet<&str> = inspected
        .iter()
        .filter(|file| file.header.is_some())
        .map(|file| file.session_id.as_str())
        .collect();
    let identity_census_complete = inventory_complete
        && inspected.len() == files.len()
        && inspected.iter().all(|file| file.header.is_some());
    let mut force_session_ids: HashSet<String> = HashSet::new();
    let mut retractions_by_path: HashMap<String, HashSet<String>> = HashMap::new();
    let mut tombstones: Vec<IndexUnit> = Vec::new();
    for (indexed_session_id, indexed_path) in &indexed_sessions {
        if !inventory_complete {
            continue;
        }
        let should_reconcile = match &changed_paths {
            None => true,
            Some(changed) => path_affected(indexed_path, changed),
        };
        if !should_reconcile {
            continue;
        }
        if let Some(current) = valid_by_path.get(indexed_path.as_str()) {
            if current.session_id != *indexed_session_id {
                if valid_session_ids.contains(indexed_session_id.as_str()) {
                    force_session_ids.insert(indexed_session_id.clone());
                } else if identity_census_complete {
                    retractions_by_path
                        .entry(indexed_path.clone())
                        .or_default()
                        .insert(indexed_session_id.clone());
                }
            }
            continue;
        }
        if valid_session_ids.contains(indexed_session_id.as_str()) {
            // A valid copy survived a move, unlink, or torn duplicate. Reparse
            // it so jsonl_path follows readable provenance instead of a
            // missing/bad source.
            force_session_ids.insert(indexed_session_id.clone());
            continue;
        }
        if file_set.contains(indexed_path) {
            // The source still exists but its header is currently invalid.
            // Preserve the last committed session until a complete
            // replacement can parse.
            continue;
        }
        if !identity_census_complete {
            continue;
        }
        tombstones.push(IndexUnit {
            key: indexed_path.clone(),
            session_id: indexed_session_id.clone(),
            retract_session_ids: vec![indexed_session_id.clone()],
            meta: Some(json!({
                "kind": "pi-tombstone",
                "discoveredSessionId": null,
            })),
            ..Default::default()
        });
    }

    let mut units: Vec<IndexUnit> = Vec::new();
    for (group_session_id, group) in &selected_groups {
        let mut group_retractions: Vec<String> = Vec::new();
        for &i in &group.member_indexes {
            if let Some(retractions) = retractions_by_path.get(&inspected[i].path) {
                for session_id in retractions {
                    if !group_retractions.contains(session_id) {
                        group_retractions.push(session_id.clone());
                    }
                }
            }
        }
        let group_changed = changed_paths.as_ref().is_some_and(|changed| {
            group
                .member_indexes
                .iter()
                .any(|&i| path_affected(&inspected[i].path, changed))
        });
        let forced = force_session_ids.contains(group_session_id) || !group_retractions.is_empty();
        for &i in &group.candidate_indexes {
            let file = &inspected[i];
            if changed_paths.is_some() && !group_changed && !forced {
                continue;
            }
            let cursor = (ctx.last_cursor)(&file.path);
            if changed_paths.is_none()
                && !forced
                && cursor.as_deref() == Some(file.current_cursor.as_str())
            {
                continue;
            }
            let indexed_path = indexed_path_by_session_id.get(&file.session_id);
            // Publishing a different copy for an existing logical session
            // depends on certifying the provider-wide identity census. A
            // readable unit remains source-local only when it is new or owns
            // the committed provenance.
            let source_local = group_retractions.is_empty()
                && indexed_path.is_none_or(|indexed| indexed == &file.path);
            if !inventory_complete && !source_local {
                continue;
            }
            let project = file.header.as_ref().and_then(|header| {
                project_slug_from_path(
                    normalize_observed_cwd(header.get("cwd").and_then(Value::as_str)).as_deref(),
                )
            });
            let mut meta = Map::new();
            meta.insert("kind".to_string(), json!("pi-session"));
            meta.insert("discoveredSessionId".to_string(), json!(file.session_id));
            if let Some(collision_paths) = &group.collision_paths {
                meta.insert("collisionPaths".to_string(), json!(collision_paths));
            }
            units.push(IndexUnit {
                key: file.path.clone(),
                session_id: file.session_id.clone(),
                retract_session_ids: if group_retractions.is_empty() {
                    Vec::new()
                } else {
                    group_retractions.clone()
                },
                project,
                meta: Some(Value::Object(meta)),
                ..Default::default()
            });
        }
    }
    let mut all: Vec<IndexUnit> = units.into_iter().chain(tombstones).collect();
    all.sort_by(|left, right| {
        left.key
            .cmp(&right.key)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    all
}

// ---------------------------------------------------------------------------
// Line parsing (port of parseLines)
// ---------------------------------------------------------------------------

/// One parsed entry with its 0-based ordinal (header = 0) and physical line.
#[derive(Debug, Clone)]
struct PiEntry {
    line: usize,
    ordinal: usize,
    record: Value,
}

impl PiEntry {
    fn id(&self) -> &str {
        self.record.get("id").and_then(Value::as_str).unwrap_or("")
    }
    fn parent_id(&self) -> Option<&str> {
        self.record.get("parentId").and_then(Value::as_str)
    }
    fn r#type(&self) -> &str {
        self.record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

struct ParsedLines {
    header: Value,
    entries: Vec<PiEntry>,
    cursor: String,
    snapshot: String,
}

/// JS `String(value)` coercion used for `message.toolCallId`.
fn js_to_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(|i| i.to_string())
            .or_else(|| n.as_u64().map(|u| u.to_string()))
            .unwrap_or_else(|| {
                let value = n.as_f64().unwrap_or(f64::NAN);
                if value == value.trunc() && value.abs() < 1e21 {
                    format!("{}", value as i64)
                } else {
                    format!("{value}")
                }
            }),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn parse_lines(path: &Path) -> Result<ParsedLines, String> {
    let display_path = path.to_string_lossy();
    let before =
        crate::parsing::file_signature(path).map_err(|error| format!("{display_path}: {error}"))?;
    let raw_bytes = std::fs::read(path).map_err(|error| format!("{display_path}: {error}"))?;
    let raw = String::from_utf8_lossy(&raw_bytes);
    let after =
        crate::parsing::file_signature(path).map_err(|error| format!("{display_path}: {error}"))?;
    if snapshot_cursor(before) != snapshot_cursor(after) {
        return Err(format!("Pi session changed while indexing: {display_path}"));
    }

    let mut lines: Vec<(usize, usize, Value)> = Vec::new();
    for (index, segment) in raw.split('\n').enumerate() {
        let line = segment.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        // Pi 0.83 skips malformed physical lines, including an incomplete
        // tail. Stable-file checks around this parse still make a later
        // append retryable.
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !record.is_object() {
            return Err(format!(
                "Malformed Pi JSONL value at line {} in {display_path}",
                index + 1
            ));
        }
        let ordinal = lines.len();
        lines.push((index + 1, ordinal, record));
    }
    if lines.is_empty() {
        return Err(format!("Empty Pi session: {display_path}"));
    }
    let header = as_header(&lines[0].2, &display_path)?;
    let version = header.get("version").and_then(Value::as_i64).unwrap_or(1);
    let mut source_entries: Vec<PiEntry> = lines[1..]
        .iter()
        .map(|(line, ordinal, record)| PiEntry {
            line: *line,
            ordinal: *ordinal,
            record: record.clone(),
        })
        .collect();

    if version == 1 {
        let mut previous_id: Option<String> = None;
        for entry in &mut source_entries {
            let id = format!("v1-entry-{}", entry.ordinal);
            if let Some(record) = entry.record.as_object_mut() {
                record.insert("id".to_string(), json!(id.clone()));
                record.insert(
                    "parentId".to_string(),
                    previous_id
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
            }
            if entry.r#type() == "compaction" {
                // `typeof firstKeptEntryIndex === 'number'` indexes the full
                // parsed-lines array (header included).
                let first_kept_index = entry
                    .record
                    .get("firstKeptEntryIndex")
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite() && *n == n.trunc())
                    .map(|n| n as i64);
                if let Some(first_kept_index) = first_kept_index {
                    let target = first_kept_index
                        .try_into()
                        .ok()
                        .and_then(|index: usize| lines.get(index));
                    if let Some((_, ordinal, record)) = target {
                        if record.get("type").and_then(Value::as_str) != Some("session") {
                            if let Some(record) = entry.record.as_object_mut() {
                                record.insert(
                                    "firstKeptEntryId".to_string(),
                                    json!(format!("v1-entry-{ordinal}")),
                                );
                            }
                        }
                    }
                    if let Some(record) = entry.record.as_object_mut() {
                        record.remove("firstKeptEntryIndex");
                    }
                }
            }
            previous_id = Some(id);
        }
    }
    if version <= 2 {
        for entry in &mut source_entries {
            if entry.r#type() == "message"
                && entry
                    .record
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("hookMessage")
            {
                if let Some(message) = entry
                    .record
                    .get_mut("message")
                    .and_then(Value::as_object_mut)
                {
                    message.insert("role".to_string(), json!("custom"));
                }
            }
        }
    }

    for entry in &source_entries {
        let id_ok = entry
            .record
            .get("id")
            .and_then(Value::as_str)
            .map(|id| !id.is_empty())
            .unwrap_or(false);
        let parent_ok = matches!(
            entry.record.get("parentId"),
            None | Some(Value::Null) | Some(Value::String(_))
        );
        let type_ok = entry.record.get("type").and_then(Value::as_str).is_some();
        let timestamp_ok = entry
            .record
            .get("timestamp")
            .and_then(Value::as_str)
            .is_some();
        if !id_ok || !parent_ok || !type_ok || !timestamp_ok {
            return Err(format!(
                "Malformed Pi entry at line {} in {display_path}",
                entry.line
            ));
        }
    }
    let cursor = snapshot_cursor(after);
    Ok(ParsedLines {
        header,
        entries: source_entries,
        cursor: cursor.clone(),
        snapshot: cursor,
    })
}

// ---------------------------------------------------------------------------
// Tree analysis (port of analyzeTree / activeContextEntries)
// ---------------------------------------------------------------------------

fn checkpoint_compactions(entries: &[PiEntry]) -> Result<HashSet<String>, String> {
    let mut checkpoints = HashSet::new();
    for entry in entries {
        if entry.r#type() != "compaction" {
            continue;
        }
        match entry.record.get("retainedTail") {
            // `retainedTail === undefined` → not a checkpoint.
            None => continue,
            Some(Value::Array(_)) => {
                checkpoints.insert(entry.id().to_string());
            }
            Some(_) => return Err(format!("Malformed retainedTail at Pi line {}", entry.line)),
        }
    }
    Ok(checkpoints)
}

fn active_context_entries(
    head_id: Option<&str>,
    by_id: &HashMap<String, PiEntry>,
    checkpoints: &HashSet<String>,
) -> Result<HashSet<String>, String> {
    let Some(head_id) = head_id else {
        return Ok(HashSet::new());
    };

    // retainedTail is agent-core's storage checkpoint format: stop there
    // before applying its context transform. Legacy-only chains keep
    // coding-agent's firstKeptEntryId behavior, including nested legacy
    // compactions.
    let mut reverse_path: Vec<&PiEntry> = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut current_id: Option<&str> = Some(head_id);
    while let Some(id) = current_id {
        if visited.contains(id) {
            return Err(format!("Pi active branch contains a cycle at {id}"));
        }
        visited.insert(id);
        let Some(current) = by_id.get(id) else {
            return Err(format!("Pi active head {id} does not exist"));
        };
        reverse_path.push(current);
        if checkpoints.contains(current.id()) {
            break;
        }
        current_id = match current.parent_id() {
            Some(parent) if by_id.contains_key(parent) => Some(parent),
            _ => None,
        };
    }

    // Mirror Pi's defaultContextEntryTransform(): only the latest compaction
    // contributes context, with either its retained tail or its kept
    // ancestors.
    let path: Vec<&PiEntry> = reverse_path.into_iter().rev().collect();
    let mut compaction_index: Option<usize> = None;
    for (index, entry) in path.iter().enumerate() {
        if entry.r#type() == "compaction" {
            compaction_index = Some(index);
        }
    }
    let Some(compaction_index) = compaction_index else {
        return Ok(path.iter().map(|entry| entry.id().to_string()).collect());
    };

    let compaction = path[compaction_index];
    let mut active: HashSet<String> = HashSet::new();
    active.insert(compaction.id().to_string());
    for entry in &path[compaction_index + 1..] {
        active.insert(entry.id().to_string());
    }
    if !checkpoints.contains(compaction.id()) {
        let first_kept_index = compaction
            .record
            .get("firstKeptEntryId")
            .and_then(Value::as_str)
            .and_then(|first_kept_id| {
                path.iter()
                    .enumerate()
                    .find(|(index, entry)| *index < compaction_index && entry.id() == first_kept_id)
                    .map(|(index, _)| index)
            });
        if let Some(first_kept_index) = first_kept_index {
            for entry in &path[first_kept_index..compaction_index] {
                active.insert(entry.id().to_string());
            }
        }
    }
    Ok(active)
}

struct AnalyzedTree {
    active: HashSet<String>,
    checkpoints: HashSet<String>,
}

fn analyze_tree(entries: &[PiEntry]) -> Result<AnalyzedTree, String> {
    let mut by_id: HashMap<String, PiEntry> = HashMap::new();
    for entry in entries {
        if by_id.contains_key(entry.id()) {
            return Err(format!("Duplicate Pi entry id: {}", entry.id()));
        }
        by_id.insert(entry.id().to_string(), entry.clone());
    }
    let checkpoints = checkpoint_compactions(entries)?;

    for entry in entries {
        if entry.r#type() == "leaf" {
            // targetId must be null or a string that exists (undefined fails).
            match entry.record.get("targetId") {
                Some(Value::Null) => {}
                Some(Value::String(target)) if by_id.contains_key(target) => {}
                Some(Value::String(target)) => {
                    return Err(format!("Pi leaf target {target} does not exist"));
                }
                _ => {
                    return Err(format!("Malformed Pi leaf target at line {}", entry.line));
                }
            }
        }
    }
    // Validate the functional parent graph once. Resolved paths are memoized,
    // so a long linear transcript is O(n) rather than walking every prefix.
    let mut resolved: HashSet<String> = HashSet::new();
    for entry in entries {
        let mut path: Vec<String> = Vec::new();
        let mut positions: HashMap<&str, usize> = HashMap::new();
        let mut current_id: Option<&str> = Some(entry.id());
        while let Some(id) = current_id {
            if resolved.contains(id) {
                break;
            }
            if positions.contains_key(id) {
                return Err(format!("Pi session contains a cycle at {id}"));
            }
            positions.insert(id, path.len());
            path.push(id.to_string());
            let Some(current) = by_id.get(id) else {
                return Err(format!(
                    "Pi entry {} has a truncated parent chain",
                    entry.id()
                ));
            };
            current_id = match current.parent_id() {
                Some(parent) if by_id.contains_key(parent) => Some(parent),
                _ => None,
            };
        }
        for id in path {
            resolved.insert(id);
        }
    }

    let mut head_id: Option<String> = None;
    for entry in entries {
        head_id = if entry.r#type() == "leaf" {
            entry
                .record
                .get("targetId")
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            Some(entry.id().to_string())
        };
    }
    let active = active_context_entries(head_id.as_deref(), &by_id, &checkpoints)?;
    Ok(AnalyzedTree {
        active,
        checkpoints,
    })
}

// ---------------------------------------------------------------------------
// Text helpers (ports of the Pi presentation functions)
// ---------------------------------------------------------------------------

fn iso_from_epoch_ms(ms: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| format!("{}Z", dt.format("%Y-%m-%dT%H:%M:%S%.3f")))
}

/// `Date.parse` approximation: RFC3339 (with Z or numeric offset), ISO
/// date-time without offset, and bare dates. JS treats a missing offset as
/// local time; we treat it as UTC (documented port deviation).
fn parse_js_date(value: &str) -> Option<i64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(dt.timestamp_millis());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(parsed.and_utc().timestamp_millis());
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp_millis());
    }
    None
}

fn normalize_time_value(value: &Value, fallback: Option<String>) -> Option<String> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .filter(|ms| ms.is_finite())
            .and_then(|ms| iso_from_epoch_ms(ms as i64))
            .or(fallback),
        Value::String(text) => parse_js_date(text).and_then(iso_from_epoch_ms).or(fallback),
        _ => fallback,
    }
}

fn image_placeholder(part: &Value) -> String {
    let mime = part
        .get("mimeType")
        .and_then(Value::as_str)
        .filter(|mime| !mime.is_empty())
        .unwrap_or("unknown");
    let chars = part
        .get("data")
        .and_then(Value::as_str)
        .map(|data| data.chars().map(char::len_utf16).sum::<usize>())
        .unwrap_or(0);
    format!("[image {mime}; base64 chars={chars}]")
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part.get("text").and_then(Value::as_str).map(str::to_string),
                Some("thinking") => part
                    .get("thinking")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some("image") => Some(image_placeholder(part)),
                _ => None,
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// UTF-16-code-unit slice, like JS `String.prototype.slice`.
fn utf16_slice(s: &str, end: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in s.chars() {
        let width = ch.len_utf16();
        if units + width > end {
            break;
        }
        out.push(ch);
        units += width;
    }
    out
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn bash_presentation(message: &Value) -> (String, String) {
    let command = message
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let output = message
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = if !output.is_empty() {
        format!("Ran `{command}`\n```\n{output}\n```")
    } else {
        format!("Ran `{command}`\n(no output)")
    };
    let mut suffix = String::new();
    if message.get("cancelled").and_then(Value::as_bool) == Some(true) {
        suffix.push_str("\n\n(command cancelled)");
    } else if let Some(exit_code) = message
        .get("exitCode")
        .and_then(Value::as_f64)
        .filter(|code| code.is_finite())
    {
        if exit_code != 0.0 {
            suffix.push_str(&format!("\n\nCommand exited with code {exit_code}"));
        }
    }
    if message.get("truncated").and_then(Value::as_bool) == Some(true) {
        if let Some(full_output_path) = message
            .get("fullOutputPath")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        {
            suffix.push_str(&format!(
                "\n\n[Output truncated. Full output: {full_output_path}]"
            ));
        }
    }
    (text, suffix)
}

fn bash_text(message: &Value) -> Option<String> {
    let (text, suffix) = bash_presentation(message);
    if suffix.is_empty() {
        return trunc(Some(&text));
    }
    let suffix_len = utf16_len(&suffix);
    if suffix_len >= crate::parsing::TEXT_LIMIT {
        return Some(utf16_slice(&suffix, crate::parsing::TEXT_LIMIT));
    }
    let head = utf16_slice(&text, crate::parsing::TEXT_LIMIT - suffix_len);
    Some(format!("{head}{suffix}"))
}

fn full_bash_text(message: &Value) -> String {
    let (text, suffix) = bash_presentation(message);
    format!("{text}{suffix}")
}

fn message_display_text(message: &Value) -> Option<String> {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    if role == "bashExecution" {
        return bash_text(message);
    }
    if role == "branchSummary" || role == "compactionSummary" {
        return message
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let text = content_text(message.get("content").unwrap_or(&Value::Null));
    if role == "assistant" {
        if let Some(error_message) = message
            .get("errorMessage")
            .and_then(Value::as_str)
            .filter(|msg| !msg.is_empty())
        {
            return Some(if text.is_empty() {
                error_message.to_string()
            } else {
                format!("{text}\n{error_message}")
            });
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn physical_user_title(message: &Value) -> Option<String> {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    match message.get("content") {
        Some(Value::String(content)) => {
            let title = content.trim();
            (!title.is_empty()).then(|| title.to_string())
        }
        Some(Value::Array(parts)) => {
            let title = parts
                .iter()
                .filter_map(|part| {
                    (part.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| part.get("text").and_then(Value::as_str))
                        .flatten()
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string();
            (!title.is_empty()).then_some(title)
        }
        _ => None,
    }
}

fn usage_fields(message: &Value) -> (Option<i64>, Option<i64>) {
    let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) else {
        return (None, None);
    };
    let numeric = |field: &str| -> Option<i64> {
        usage
            .get(field)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .map(|value| value as i64)
    };
    let inputs = [
        numeric("input"),
        numeric("cacheRead"),
        numeric("cacheWrite"),
    ];
    let input = if inputs.iter().any(|value| value.is_some()) {
        Some(inputs.iter().filter_map(|value| *value).sum::<i64>())
    } else {
        None
    };
    (input, numeric("output"))
}

fn message_uuid(
    session_id: &str,
    ordinal: usize,
    block_index: usize,
    tail_index: Option<usize>,
) -> String {
    let ordinal = format!("{ordinal:06}");
    let block = format!("{block_index:0BLOCK_PAD$}");
    match tail_index {
        None => format!("{session_id}:entry:{ordinal}:message:block:{block}"),
        Some(tail_index) => format!(
            "{session_id}:entry:{ordinal}:message:tail:{:0BLOCK_PAD$}:block:{block}",
            tail_index
        ),
    }
}

fn tool_call_id(message_id: &str) -> String {
    format!("{message_id}:tool")
}

#[derive(Clone)]
struct PiToolOccurrence {
    id: String,
    file_path: Option<String>,
    visibility: MessageVisibility,
}

#[derive(Clone)]
struct PiToolScope {
    native_id: String,
    occurrence: Option<PiToolOccurrence>,
    parent: Option<Box<PiToolScope>>,
}

fn find_tool_occurrence<'a>(
    scope: Option<&'a PiToolScope>,
    native_id: &str,
    visibility: MessageVisibility,
) -> Option<&'a PiToolOccurrence> {
    let mut current = scope;
    while let Some(scope) = current {
        if scope.native_id == native_id {
            let occurrence = scope.occurrence.as_ref()?;
            return if visibility == MessageVisibility::Inactive
                || occurrence.visibility == MessageVisibility::Visible
            {
                Some(occurrence)
            } else {
                None
            };
        }
        current = scope.parent.as_deref();
    }
    None
}

/// Pi-specific tool file paths: lowercase read/edit/write carry `arguments.path`.
fn pi_file_path(name: &str, input: Option<&Value>) -> Option<String> {
    if !matches!(name.to_lowercase().as_str(), "read" | "edit" | "write") {
        return None;
    }
    let input = input?;
    if !input.is_object() {
        return None;
    }
    input
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Session projection (port of projectSession)
// ---------------------------------------------------------------------------

struct ProjectedSession {
    records: Vec<TranscriptRecord>,
    message_count: i64,
    title: Option<String>,
    ended_at: Option<String>,
}

struct Projector<'a> {
    session_id: &'a str,
    header_cwd: Option<String>,
    records: Vec<TranscriptRecord>,
    message_count: i64,
    ended_at: Option<String>,
}

impl<'a> Projector<'a> {
    #[allow(clippy::too_many_arguments)]
    fn emit_message(
        &mut self,
        ordinal: usize,
        parent: &Option<String>,
        block_index: usize,
        tail_index: Option<usize>,
        record_type: &str,
        role: &str,
        text: Option<String>,
        content_type: &str,
        is_meta: bool,
        visibility: MessageVisibility,
        model: Option<String>,
        timestamp: Option<String>,
    ) -> MessageRecord {
        let record = MessageRecord {
            uuid: message_uuid(self.session_id, ordinal, block_index, tail_index),
            session_id: self.session_id.to_string(),
            r#type: record_type.to_string(),
            parent_uuid: parent.clone(),
            timestamp: timestamp.clone(),
            role: Some(role.to_string()),
            text: text.clone(),
            content_type: Some(content_type.to_string()),
            is_meta,
            visibility,
            model,
            is_sidechain: false,
            agent_id: None,
            input_tokens: None,
            output_tokens: None,
            cwd: self.header_cwd.clone(),
            skill: None,
            source: NAME.to_string(),
        };
        if visibility == MessageVisibility::Visible {
            self.message_count += 1;
        }
        if let Some(timestamp) = &timestamp {
            if self.ended_at.as_ref().is_none_or(|ended| timestamp > ended) {
                self.ended_at = Some(timestamp.clone());
            }
        }
        self.records.push(TranscriptRecord::Message(record.clone()));
        record
    }

    /// Apply usage to the message with the given uuid (TS mutates the last
    /// emitted record object in place).
    fn set_usage(&mut self, uuid: &str, input: Option<i64>, output: Option<i64>) {
        for record in self.records.iter_mut().rev() {
            if let TranscriptRecord::Message(message) = record {
                if message.uuid == uuid {
                    message.input_tokens = input;
                    message.output_tokens = output;
                    return;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn project_agent_message(
        &mut self,
        entry: &PiEntry,
        message: &Value,
        parent: Option<String>,
        visibility: MessageVisibility,
        is_meta: bool,
        tail_index: Option<usize>,
        forced_role: Option<&str>,
        account_usage: bool,
        mut tool_scope: Option<Box<PiToolScope>>,
    ) -> (Option<String>, Option<Box<PiToolScope>>) {
        let role = forced_role.map(str::to_string).unwrap_or_else(|| {
            message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string()
        });
        let timestamp = normalize_time_value(
            message.get("timestamp").unwrap_or(&Value::Null),
            normalize_time_value(entry.record.get("timestamp").unwrap_or(&Value::Null), None),
        );
        let mut previous = parent;
        let mut emitted: Vec<String> = Vec::new();
        // Visibility is passed per call: the custom role projects `hidden`
        // when `display: false` without touching the entry's own visibility.
        macro_rules! emit {
            ($visibility:expr, $block_index:expr, $content_type:expr, $text:expr, $type:expr, $projected_role:expr, $meta:expr, $model:expr) => {{
                let record = self.emit_message(
                    entry.ordinal,
                    &previous,
                    $block_index,
                    tail_index,
                    $type,
                    $projected_role,
                    $text,
                    $content_type,
                    $meta,
                    $visibility,
                    $model,
                    timestamp.clone(),
                );
                previous = Some(record.uuid.clone());
                emitted.push(record.uuid.clone());
                record.uuid
            }};
        }

        match role.as_str() {
            "user" => {
                // projectContent: string -> one text block; non-array/empty ->
                // one unknown block; array -> per-part blocks.
                let content = message.get("content").unwrap_or(&Value::Null);
                let blocks: Vec<(usize, &str, Option<String>)> = match content {
                    Value::String(text) => vec![(0, "text", trunc(Some(text)))],
                    Value::Array(parts) if !parts.is_empty() => parts
                        .iter()
                        .enumerate()
                        .map(
                            |(index, part)| match part.get("type").and_then(Value::as_str) {
                                Some("text")
                                    if part.get("text").and_then(Value::as_str).is_some() =>
                                {
                                    (
                                        index,
                                        "text",
                                        trunc(part.get("text").and_then(Value::as_str)),
                                    )
                                }
                                Some("image") => (index, "image", Some(image_placeholder(part))),
                                _ => (index, "unknown", trunc_json_default(part)),
                            },
                        )
                        .collect(),
                    _ => vec![(0, "unknown", None)],
                };
                for (index, content_type, text) in blocks {
                    emit!(
                        visibility,
                        index,
                        content_type,
                        text,
                        "user",
                        "user",
                        is_meta,
                        None
                    );
                }
            }
            "assistant" => {
                let content = match message.get("content") {
                    Some(Value::Array(parts)) => parts.clone(),
                    _ => Vec::new(),
                };
                let model = message
                    .get("responseModel")
                    .and_then(Value::as_str)
                    .or_else(|| message.get("model").and_then(Value::as_str))
                    .map(str::to_string);
                for (index, part) in content.iter().enumerate() {
                    let part_type = part.get("type").and_then(Value::as_str);
                    if part_type == Some("thinking")
                        && part.get("thinking").and_then(Value::as_str).is_some()
                    {
                        let thinking = part.get("thinking").and_then(Value::as_str);
                        if thinking.is_some_and(|thinking| !thinking.is_empty()) {
                            emit!(
                                visibility,
                                index,
                                "thinking",
                                trunc(thinking),
                                "assistant",
                                "assistant",
                                false,
                                model.clone()
                            );
                        }
                    } else if part_type == Some("text")
                        && part.get("text").and_then(Value::as_str).is_some()
                    {
                        let text = part.get("text").and_then(Value::as_str);
                        emit!(
                            visibility,
                            index,
                            "text",
                            trunc(text),
                            "assistant",
                            "assistant",
                            false,
                            model.clone()
                        );
                    } else if part_type == Some("toolCall")
                        && part.get("id").and_then(Value::as_str).is_some()
                        && part.get("name").and_then(Value::as_str).is_some()
                    {
                        let uuid = emit!(
                            visibility,
                            index,
                            "tool_use",
                            None,
                            "assistant",
                            "assistant",
                            false,
                            model.clone()
                        );
                        let input = match part.get("arguments") {
                            Some(value @ Value::Object(_)) => value.clone(),
                            Some(value @ Value::Array(_)) => value.clone(),
                            _ => json!({}),
                        };
                        let name = part.get("name").and_then(Value::as_str).unwrap_or_default();
                        let file_path = pi_file_path(name, Some(&input));
                        let occurrence = PiToolOccurrence {
                            id: tool_call_id(&uuid),
                            file_path: file_path.clone(),
                            visibility,
                        };
                        tool_scope = Some(Box::new(PiToolScope {
                            native_id: part
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            occurrence: Some(occurrence),
                            parent: tool_scope,
                        }));
                        self.records
                            .push(TranscriptRecord::ToolCall(ToolCallRecord {
                                id: tool_call_id(&uuid),
                                message_uuid: uuid,
                                session_id: self.session_id.to_string(),
                                name: name.to_string(),
                                presentation: ToolCallPresentation::Default,
                                input_json: trunc_json_default(&input)
                                    .unwrap_or_else(|| "{}".to_string()),
                                file_path,
                            }));
                    } else {
                        emit!(
                            visibility,
                            index,
                            "unknown",
                            trunc_json_default(part),
                            "assistant",
                            "assistant",
                            true,
                            model.clone()
                        );
                    }
                }
                if let Some(error_message) = message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .filter(|msg| !msg.is_empty())
                {
                    emit!(
                        visibility,
                        content.len(),
                        "error",
                        trunc(Some(error_message)),
                        "assistant",
                        "assistant",
                        false,
                        model.clone()
                    );
                }
                if emitted.is_empty() {
                    emit!(
                        visibility,
                        0,
                        "unknown",
                        None,
                        "assistant",
                        "assistant",
                        false,
                        model.clone()
                    );
                }
                if account_usage {
                    let (input, output) = usage_fields(message);
                    if let Some(last) = emitted.last() {
                        self.set_usage(last, input, output);
                    }
                }
            }
            "toolResult" => {
                let native_call_id = js_to_string(message.get("toolCallId"));
                let occurrence =
                    find_tool_occurrence(tool_scope.as_deref(), &native_call_id, visibility);
                let content = content_text(message.get("content").unwrap_or(&Value::Null));
                let uuid = emit!(
                    visibility,
                    0,
                    "tool_result",
                    trunc(Some(&content)),
                    "user",
                    "toolResult",
                    is_meta,
                    None
                );
                if account_usage {
                    let (input, output) = usage_fields(message);
                    self.set_usage(&uuid, input, output);
                }
                if let Some(occurrence) = occurrence {
                    self.records
                        .push(TranscriptRecord::ToolResult(ToolResultRecord {
                            tool_use_id: occurrence.id.clone(),
                            message_uuid: Some(uuid),
                            session_id: self.session_id.to_string(),
                            content: trunc(Some(&content)).unwrap_or_default(),
                            file_path: occurrence.file_path.clone(),
                            is_error: message.get("isError").and_then(Value::as_bool) == Some(true),
                        }));
                }
                tool_scope = Some(Box::new(PiToolScope {
                    native_id: native_call_id,
                    occurrence: None,
                    parent: tool_scope,
                }));
            }
            "bashExecution" => {
                emit!(
                    visibility,
                    0,
                    "bash_execution",
                    bash_text(message),
                    "user",
                    "bashExecution",
                    false,
                    None
                );
            }
            "custom" => {
                let display = message.get("display").and_then(Value::as_bool) != Some(false);
                let custom_visibility = if display {
                    visibility
                } else {
                    MessageVisibility::Hidden
                };
                let content = message.get("content").unwrap_or(&Value::Null);
                let blocks: Vec<(usize, Option<String>)> = match content {
                    Value::String(text) => vec![(0, trunc(Some(text)))],
                    Value::Array(parts) if !parts.is_empty() => parts
                        .iter()
                        .enumerate()
                        .map(
                            |(index, part)| match part.get("type").and_then(Value::as_str) {
                                Some("text")
                                    if part.get("text").and_then(Value::as_str).is_some() =>
                                {
                                    (index, trunc(part.get("text").and_then(Value::as_str)))
                                }
                                Some("image") => (index, Some(image_placeholder(part))),
                                _ => (index, trunc_json_default(part)),
                            },
                        )
                        .collect(),
                    _ => vec![(0, None)],
                };
                for (index, text) in blocks {
                    emit!(
                        custom_visibility,
                        index,
                        "custom",
                        text,
                        "system",
                        "custom",
                        true,
                        None
                    );
                }
            }
            "branchSummary" | "compactionSummary" => {
                if let Some(summary) = message.get("summary").and_then(Value::as_str) {
                    let (input, output) = if account_usage {
                        usage_fields(message)
                    } else {
                        (None, None)
                    };
                    let retained_identity = match tail_index {
                        None => String::new(),
                        Some(tail_index) => format!(":tail:{tail_index:0BLOCK_PAD$}"),
                    };
                    self.records.push(TranscriptRecord::Summary(
                        crate::providers::types::SummaryRecord {
                            id: format!(
                                "{}:entry:{}:summary{retained_identity}:{role}",
                                self.session_id, entry.ordinal
                            ),
                            session_id: self.session_id.to_string(),
                            timestamp: timestamp.clone(),
                            source: if role == "branchSummary" {
                                "pi:branch_summary"
                            } else {
                                "pi:compaction"
                            }
                            .to_string(),
                            content: trunc(Some(summary)).unwrap_or_default(),
                            visibility: Some(visibility),
                            input_tokens: input,
                            output_tokens: output,
                        },
                    ));
                }
            }
            _ => {
                emit!(
                    visibility,
                    0,
                    "unknown",
                    message_display_text(message),
                    "system",
                    &role,
                    true,
                    None
                );
            }
        }
        (previous, tool_scope)
    }
}

fn project_session(
    header: &Value,
    entries: &[PiEntry],
    session_id: &str,
    active: &HashSet<String>,
    checkpoints: &HashSet<String>,
) -> Result<ProjectedSession, String> {
    let header_cwd = normalize_observed_cwd(header.get("cwd").and_then(Value::as_str));
    let mut projector = Projector {
        session_id,
        header_cwd,
        records: Vec::new(),
        message_count: 0,
        ended_at: normalize_time_value(header.get("timestamp").unwrap_or(&Value::Null), None),
    };
    let mut tail_by_entry: HashMap<String, Option<String>> = HashMap::new();
    let mut tool_scope_by_entry: HashMap<String, Option<Box<PiToolScope>>> = HashMap::new();
    let mut latest_name: Option<String> = None;
    let mut first_user_title: Option<String> = None;

    for entry in entries {
        let source = &entry.record;
        let entry_id = entry.id();
        let in_context = active.contains(entry_id);
        let inherited_tail = match entry.parent_id() {
            None => None,
            Some(parent) => tail_by_entry.get(parent).cloned().flatten(),
        };
        let inherited_tool_scope = match entry.parent_id() {
            None => None,
            Some(parent) => tool_scope_by_entry.get(parent).cloned().flatten(),
        };
        if entry.r#type() == "leaf" {
            let target = source.get("targetId").and_then(Value::as_str);
            tail_by_entry.insert(
                entry_id.to_string(),
                target.and_then(|target| tail_by_entry.get(target).cloned().flatten()),
            );
            tool_scope_by_entry.insert(
                entry_id.to_string(),
                target.and_then(|target| tool_scope_by_entry.get(target).cloned().flatten()),
            );
            continue;
        }
        tail_by_entry.insert(entry_id.to_string(), inherited_tail.clone());
        tool_scope_by_entry.insert(entry_id.to_string(), inherited_tool_scope.clone());
        let visibility = if in_context {
            MessageVisibility::Visible
        } else {
            MessageVisibility::Inactive
        };

        match entry.r#type() {
            "session_info" => {
                latest_name = source
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty());
            }
            "message" => {
                let Some(message) = source.get("message").filter(|message| message.is_object())
                else {
                    return Err(format!("Malformed Pi message at line {}", entry.line));
                };
                if first_user_title.is_none() {
                    first_user_title = physical_user_title(message);
                }
                let (tail, tool_scope) = projector.project_agent_message(
                    entry,
                    message,
                    inherited_tail,
                    visibility,
                    false,
                    None,
                    None,
                    true,
                    inherited_tool_scope,
                );
                tail_by_entry.insert(entry_id.to_string(), tail);
                tool_scope_by_entry.insert(entry_id.to_string(), tool_scope);
            }
            "custom_message" => {
                let mut message = Map::new();
                message.insert("role".to_string(), json!("custom"));
                message.insert(
                    "content".to_string(),
                    source.get("content").cloned().unwrap_or(Value::Null),
                );
                if let Some(custom_type) = source.get("customType") {
                    message.insert("customType".to_string(), custom_type.clone());
                }
                if let Some(display) = source.get("display") {
                    message.insert("display".to_string(), display.clone());
                }
                if let Some(details) = source.get("details") {
                    message.insert("details".to_string(), details.clone());
                }
                message.insert(
                    "timestamp".to_string(),
                    source.get("timestamp").cloned().unwrap_or(Value::Null),
                );
                let message = Value::Object(message);
                let (tail, tool_scope) = projector.project_agent_message(
                    entry,
                    &message,
                    inherited_tail,
                    visibility,
                    true,
                    None,
                    Some("custom"),
                    true,
                    inherited_tool_scope,
                );
                tail_by_entry.insert(entry_id.to_string(), tail);
                tool_scope_by_entry.insert(entry_id.to_string(), tool_scope);
            }
            "compaction" | "branch_summary" => {
                let Some(summary) = source.get("summary").and_then(Value::as_str) else {
                    return Err(format!("Malformed Pi summary at line {}", entry.line));
                };
                let (input, output) = usage_fields(source);
                projector.records.push(TranscriptRecord::Summary(
                    crate::providers::types::SummaryRecord {
                        id: format!(
                            "{}:entry:{}:summary:{}",
                            session_id,
                            entry.ordinal,
                            entry.r#type()
                        ),
                        session_id: session_id.to_string(),
                        timestamp: normalize_time_value(
                            source.get("timestamp").unwrap_or(&Value::Null),
                            None,
                        ),
                        source: if entry.r#type() == "compaction" {
                            "pi:compaction"
                        } else {
                            "pi:branch_summary"
                        }
                        .to_string(),
                        content: trunc(Some(summary)).unwrap_or_default(),
                        visibility: Some(visibility),
                        input_tokens: input,
                        output_tokens: output,
                    },
                ));
                if entry.r#type() == "compaction" && checkpoints.contains(entry_id) && in_context {
                    let tail_messages = source
                        .get("retainedTail")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut tail = inherited_tail;
                    let mut tool_scope: Option<Box<PiToolScope>> = None;
                    for (index, retained) in tail_messages.iter().enumerate() {
                        if !retained.is_object() {
                            return Err(format!(
                                "Malformed Pi retainedTail message at line {}",
                                entry.line
                            ));
                        }
                        let (next_tail, next_tool_scope) = projector.project_agent_message(
                            entry,
                            retained,
                            tail,
                            visibility,
                            false,
                            Some(index),
                            None,
                            false,
                            tool_scope,
                        );
                        tail = next_tail;
                        tool_scope = next_tool_scope;
                    }
                    tail_by_entry.insert(entry_id.to_string(), tail);
                    tool_scope_by_entry.insert(entry_id.to_string(), tool_scope);
                }
            }
            _ => {}
        }
        let entry_time =
            normalize_time_value(source.get("timestamp").unwrap_or(&Value::Null), None);
        if let Some(entry_time) = entry_time {
            if projector
                .ended_at
                .as_ref()
                .is_none_or(|ended| &entry_time > ended)
            {
                projector.ended_at = Some(entry_time);
            }
        }
    }

    Ok(ProjectedSession {
        records: projector.records,
        message_count: projector.message_count,
        title: latest_name.or(first_user_title),
        ended_at: projector.ended_at,
    })
}

// ---------------------------------------------------------------------------
// parse (port of parsePi)
// ---------------------------------------------------------------------------

fn meta_kind(meta: Option<&Value>) -> Option<&str> {
    meta.and_then(|meta| meta.get("kind"))
        .and_then(Value::as_str)
}

fn meta_collision_paths(meta: Option<&Value>) -> Option<Vec<String>> {
    meta.and_then(|meta| meta.get("collisionPaths"))?
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
}

fn meta_discovered_session_id(meta: Option<&Value>) -> Option<String> {
    meta.and_then(|meta| meta.get("discoveredSessionId"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The module-level parse entry point, mirroring claude.rs: parse one unit
/// into a record stream plus its snapshot cursor.
pub fn parse(unit: &IndexUnit, _cursor: Cursor) -> Vec<StreamItem> {
    match parse_pi(unit) {
        Ok((records, cursor)) => {
            let mut items: Vec<StreamItem> = records.into_iter().map(StreamItem::Record).collect();
            items.push(StreamItem::Cursor(cursor));
            items
        }
        Err(error) => vec![StreamItem::Error(error)],
    }
}

fn parse_pi(unit: &IndexUnit) -> Result<(Vec<TranscriptRecord>, String), String> {
    let meta = unit.meta.as_ref();
    if meta_kind(meta) == Some("pi-tombstone") {
        return Ok((
            Vec::new(),
            // A zero watermark keeps recreation discoverable even if no
            // watcher event is available.
            "0:0".to_string(),
        ));
    }
    if let Some(collision_paths) = meta_collision_paths(meta) {
        return Err(format!(
            "Divergent Pi session copies share one header identity: {}",
            collision_paths.join(", ")
        ));
    }
    let parsed = parse_lines(Path::new(&unit.key))?;
    let session_id = pi_session_id(&parsed.header);
    if meta_kind(meta) != Some("pi-session")
        || meta_discovered_session_id(meta).as_deref() != Some(session_id.as_str())
        || session_id != unit.session_id
    {
        return Err(format!(
            "Pi session header changed after discovery: {}",
            unit.key
        ));
    }
    let analyzed = analyze_tree(&parsed.entries)?;
    let projected = project_session(
        &parsed.header,
        &parsed.entries,
        &session_id,
        &analyzed.active,
        &analyzed.checkpoints,
    )?;
    let after_projection =
        snapshot_of(Path::new(&unit.key)).map_err(|error| format!("{}: {error}", unit.key))?;
    if after_projection != parsed.snapshot {
        return Err(format!("Pi session changed while indexing: {}", unit.key));
    }
    let session = TranscriptRecord::Session(crate::providers::types::SessionRecord {
        id: session_id.clone(),
        title: projected.title,
        project: project_slug_from_path(
            normalize_observed_cwd(parsed.header.get("cwd").and_then(Value::as_str)).as_deref(),
        ),
        started_at: normalize_time_value(
            parsed.header.get("timestamp").unwrap_or(&Value::Null),
            None,
        ),
        ended_at: projected.ended_at,
        git_branch: None,
        version: Some(format!(
            "session-v{}",
            parsed
                .header
                .get("version")
                .and_then(Value::as_i64)
                .unwrap_or(1)
        )),
        message_count: projected.message_count,
        count_mode: SessionCountMode::Total,
        jsonl_path: unit.key.clone(),
        source: NAME.to_string(),
    });
    let mut records = vec![
        TranscriptRecord::DeleteSession {
            session_id: session_id.clone(),
        },
        session,
    ];
    records.extend(projected.records);
    Ok((records, parsed.cursor))
}

// ---------------------------------------------------------------------------
// Raw lookup (port of rawPi)
// ---------------------------------------------------------------------------

struct PiRawBlock {
    exists: bool,
    text: Option<String>,
}

fn missing_raw_block() -> PiRawBlock {
    PiRawBlock {
        exists: false,
        text: None,
    }
}

fn full_json(value: &Value) -> Option<String> {
    if value.is_null() {
        None
    } else {
        Some(value.to_string())
    }
}

fn raw_content_block(content: &Value, block_index: i64) -> PiRawBlock {
    if block_index < 0 {
        return missing_raw_block();
    }
    match content {
        Value::String(text) => {
            if block_index == 0 {
                PiRawBlock {
                    exists: true,
                    text: Some(text.clone()),
                }
            } else {
                missing_raw_block()
            }
        }
        Value::Array(parts) if !parts.is_empty() => match parts.get(block_index as usize) {
            None => missing_raw_block(),
            Some(part) => {
                if part.get("type").and_then(Value::as_str) == Some("text")
                    && part.get("text").and_then(Value::as_str).is_some()
                {
                    PiRawBlock {
                        exists: true,
                        text: part.get("text").and_then(Value::as_str).map(str::to_string),
                    }
                } else if part.get("type").and_then(Value::as_str) == Some("image") {
                    PiRawBlock {
                        exists: true,
                        text: Some(image_placeholder(part)),
                    }
                } else {
                    PiRawBlock {
                        exists: true,
                        text: full_json(part),
                    }
                }
            }
        },
        _ => {
            if block_index == 0 {
                PiRawBlock {
                    exists: true,
                    text: None,
                }
            } else {
                missing_raw_block()
            }
        }
    }
}

fn raw_message_block(message: &Value, block_index: i64) -> PiRawBlock {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if role == "user" || role == "custom" {
        return raw_content_block(message.get("content").unwrap_or(&Value::Null), block_index);
    }
    if role == "assistant" {
        let content = match message.get("content") {
            Some(Value::Array(parts)) => parts.clone(),
            _ => Vec::new(),
        };
        let mut emitted = false;
        for (index, part) in content.iter().enumerate() {
            let part_type = part.get("type").and_then(Value::as_str);

            let text: Option<String> = if part_type == Some("thinking")
                && part.get("thinking").and_then(Value::as_str).is_some()
            {
                let thinking = part.get("thinking").and_then(Value::as_str);
                if thinking.is_some_and(|thinking| thinking.is_empty()) {
                    continue;
                }
                thinking.map(str::to_string)
            } else if part_type == Some("text")
                && part.get("text").and_then(Value::as_str).is_some()
            {
                part.get("text").and_then(Value::as_str).map(str::to_string)
            } else if part_type == Some("toolCall")
                && part.get("id").and_then(Value::as_str).is_some()
                && part.get("name").and_then(Value::as_str).is_some()
            {
                None
            } else {
                full_json(part)
            };
            emitted = true;
            if index as i64 == block_index {
                return PiRawBlock { exists: true, text };
            }
        }
        if let Some(error_message) = message
            .get("errorMessage")
            .and_then(Value::as_str)
            .filter(|msg| !msg.is_empty())
        {
            emitted = true;
            if block_index == content.len() as i64 {
                return PiRawBlock {
                    exists: true,
                    text: Some(error_message.to_string()),
                };
            }
        }
        return if !emitted && block_index == 0 {
            PiRawBlock {
                exists: true,
                text: None,
            }
        } else {
            missing_raw_block()
        };
    }
    if role == "toolResult" {
        return if block_index == 0 {
            PiRawBlock {
                exists: true,
                text: Some(content_text(message.get("content").unwrap_or(&Value::Null))),
            }
        } else {
            missing_raw_block()
        };
    }
    if role == "bashExecution" {
        return if block_index == 0 {
            PiRawBlock {
                exists: true,
                text: Some(full_bash_text(message)),
            }
        } else {
            missing_raw_block()
        };
    }
    if role == "branchSummary" || role == "compactionSummary" {
        return missing_raw_block();
    }
    if block_index == 0 {
        PiRawBlock {
            exists: true,
            text: message_display_text(message),
        }
    } else {
        missing_raw_block()
    }
}

fn raw_pi(input: &RawLookup) -> Option<RawRecord> {
    // Raw lookup is best-effort evidence display. A rename, torn write, or
    // replacement session must never escape as an application error.
    let session = input.session?;
    let path = session.get("jsonl_path").and_then(Value::as_str)?;
    let session_id = session.get("id").and_then(Value::as_str)?;
    let cursor = input.cursor?;
    let prefix = format!("{session_id}:entry:");
    let rest = input.message_uuid.strip_prefix(&prefix)?;
    // ^(\d+):message:(?:tail:(\d+):)?block:(\d+)$
    let parts: Vec<&str> = rest.split(':').collect();
    let (ordinal, tail_index, block_index) = match parts.as_slice() {
        [ordinal, "message", "block", block] => (*ordinal, None, *block),
        [ordinal, "message", "tail", tail, "block", block] => (*ordinal, Some(*tail), *block),
        _ => return None,
    };
    let ordinal: usize = ordinal.parse().ok()?;
    if ordinal == 0 {
        return None;
    }
    let tail_index: Option<usize> = match tail_index {
        None => None,
        Some(tail) => Some(tail.parse().ok()?),
    };
    let block_index: usize = block_index.parse().ok()?;

    let parsed = parse_lines(Path::new(path)).ok()?;
    if parsed.cursor != cursor {
        return None;
    }
    if pi_session_id(&parsed.header) != session_id {
        return None;
    }
    let entry = parsed
        .entries
        .iter()
        .find(|entry| entry.ordinal == ordinal)?;
    let source = &entry.record;
    let message: Value = match source.get("type").and_then(Value::as_str) {
        Some("message") if tail_index.is_none() => source
            .get("message")
            .filter(|message| message.is_object())?
            .clone(),
        Some("custom_message") if tail_index.is_none() => json!({
            "role": "custom",
            "content": source.get("content").cloned().unwrap_or(Value::Null),
            "display": source.get("display").cloned().unwrap_or(Value::Null),
        }),
        Some("compaction") if tail_index.is_some() => source
            .get("retainedTail")
            .and_then(Value::as_array)?
            .get(tail_index?)
            .filter(|message| message.is_object())?
            .clone(),
        _ => return None,
    };
    let block = raw_message_block(&message, block_index as i64);
    if !block.exists {
        return None;
    }
    // Pi stores source messages either directly or inside a retained tail.
    // Return the same message-shaped raw container for both forms.
    let raw_text = full_json(&message)?;
    let total_length = raw_text.chars().map(char::len_utf16).sum::<usize>();
    Some(RawRecord {
        text: raw_text,
        total_length: Some(total_length),
        offset: Some(0),
        limit: Some(total_length),
        has_more: Some(false),
        message_text: block.text,
    })
}

// ---------------------------------------------------------------------------
// Provider (port of createPiProvider)
// ---------------------------------------------------------------------------

pub struct PiProvider {
    /// Resolved session root (descriptor default_root).
    pub root_dir: PathBuf,
    /// Launch cwd used for relative default-root resolution.
    pub cwd: Option<PathBuf>,
    requires_explicit_root: bool,
    root_resolution_reason: Option<String>,
}

impl PiProvider {
    /// `createPiProvider({ rootDir })` with an absolute root.
    pub fn new(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            cwd: None,
            requires_explicit_root: false,
            root_resolution_reason: None,
        }
    }

    /// Build from an explicit root-resolution result.
    pub fn from_resolution(resolution: PiRootResolution, cwd: Option<PathBuf>) -> Self {
        Self {
            root_dir: resolution.root,
            cwd,
            requires_explicit_root: resolution.requires_explicit_root,
            root_resolution_reason: resolution.reason,
        }
    }

    /// Full `createPiProvider` port using the real environment and home dir.
    pub fn create(root_dir: Option<String>, cwd: Option<String>) -> Self {
        let home_dir = os_homedir();
        let env = |key: &str| std::env::var(key).ok();
        let resolution = resolve_pi_root(root_dir.as_deref(), cwd.as_deref(), &env, &home_dir);
        Self::from_resolution(resolution, cwd.map(PathBuf::from))
    }

    pub fn root_resolution(&self) -> PiRootResolution {
        PiRootResolution {
            root: self.root_dir.clone(),
            requires_explicit_root: self.requires_explicit_root,
            reason: self.root_resolution_reason.clone(),
        }
    }
}

impl ProviderAdapter for PiProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: NAME,
            name: "Pi",
            vendor: "Pi",
            default_root: self.root_dir.to_string_lossy().into_owned(),
            color: "#f59e0b",
            requires_explicit_root: self.requires_explicit_root,
            root_resolution_reason: self.root_resolution_reason.clone(),
        }
    }

    fn index_version_marker(&self) -> Option<&'static str> {
        Some(PI_CANONICAL_TRANSCRIPT_MARKER)
    }

    fn watch_targets(&self, configured_root: &str) -> Vec<WatchTarget> {
        if self.requires_explicit_root {
            return Vec::new();
        }
        match configured_absolute_path(configured_root, &os_homedir()) {
            None => Vec::new(),
            Some(absolute) => vec![WatchTarget {
                kind: WatchTargetKind::Tree,
                path: absolute,
            }],
        }
    }

    fn discover<'a>(&'a self, ctx: &mut DiscoverContext<'a>) -> Vec<IndexUnit> {
        if self.requires_explicit_root {
            ctx.report_incomplete(InventoryIssue {
                path: self.root_dir.to_string_lossy().into_owned(),
                error: self
                    .root_resolution_reason
                    .clone()
                    .unwrap_or_else(|| "Select a Pi session folder".to_string()),
            });
            return Vec::new();
        }
        discover_at(&self.root_dir, ctx)
    }

    fn parse<'a>(&'a self, unit: &'a IndexUnit, cursor: Cursor) -> ParseStream<'a> {
        let items = parse(unit, cursor);
        Box::new(items.into_iter())
    }

    fn raw(&self, input: &RawLookup) -> Option<RawRecord> {
        raw_pi(input)
    }
}
