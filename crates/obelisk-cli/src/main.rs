// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Obelisk CLI — a thin binary over `obelisk-core`.
//!
//! Contract-identical to the TS CLI (packages/cli/src/obelisk.ts): the same
//! flags (`--build`, `--search "text" [--nonce t]`, `--query <file.js>`,
//! `--attune <file.js>`, `install`, `--version`) and the same output shapes
//! (results pretty-printed 2-space; errors single-line compact on stdout
//! with exit code 1).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use obelisk_core::core::{execute_attune, execute_query, search_text, NonceCandidate};
use obelisk_core::indexer::{build_index, BuildIndexOptions};
use obelisk_core::{db, DB_FILE_NAME, OBELISK_DIR_NAME};

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn db_path(home: &Path) -> String {
    home.join(OBELISK_DIR_NAME)
        .join(DB_FILE_NAME)
        .to_string_lossy()
        .into_owned()
}

/// TS `emit`: JSON.stringify(value, null, 2) + '\n' on stdout.
fn emit(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// TS `emit` for non-JSON values: JSON.stringify(undefined) prints
/// "undefined".
fn emit_undefined() {
    println!("undefined");
}

/// TS `fail`: single-line compact {error, stack} on stdout, exit code 1.
/// The TS stack is node-specific; the Rust build has no equivalent stack, so
/// it reports null (dual-run comparison normalizes stacks on both sides).
fn fail(message: &str) -> ! {
    let payload = serde_json::json!({ "error": message, "stack": serde_json::Value::Null });
    println!("{}", serde_json::to_string(&payload).unwrap_or_default());
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let home = home_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    match args.first().map(String::as_str) {
        Some("--version") | Some("-v") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
        Some("--build") => {
            let result = build_index(
                &home,
                BuildIndexOptions {
                    force: true,
                    ..Default::default()
                },
            );
            if result.complete != Some(true) {
                let reason = result
                    .reason
                    .clone()
                    .unwrap_or_else(|| "incomplete_snapshot".to_string());
                let mut detail = String::new();
                if let Some(error) = &result.error {
                    detail = format!(" ({error})");
                } else if let Some(issue) = result.inventory_issues.first() {
                    detail = format!(" ({} at {}: {})", issue.provider, issue.path, issue.error);
                }
                fail(&format!(
                    "Index rebuild was not published: {reason}{detail}"
                ));
            }
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({ "ok": true, "db": db_path(&home) }))
                    .unwrap_or_default()
            );
        }
        Some("--search") if args.len() > 1 => {
            let mut nonce: Option<String> = None;
            let mut text_parts: Vec<String> = Vec::new();
            let rest = &args[1..];
            let mut i = 0;
            while i < rest.len() {
                if rest[i] == "--nonce" && i + 1 < rest.len() {
                    nonce = Some(rest[i + 1].clone());
                    i += 1;
                } else {
                    text_parts.push(rest[i].clone());
                }
                i += 1;
            }
            let candidates: Option<Vec<NonceCandidate>> = nonce.map(|value| {
                vec![NonceCandidate {
                    value,
                    strict: false,
                }]
            });
            match search_text(
                &home,
                &cwd,
                &text_parts.join(" "),
                serde_json::Value::Null,
                candidates.as_deref(),
            ) {
                Ok(value) => emit(&value),
                Err(error) => fail(&error.message),
            }
        }
        Some("--query") if args.len() > 1 => {
            let script = match std::fs::read_to_string(&args[1]) {
                Ok(script) => script,
                // Node's ENOENT message carries the path; keep it visible.
                Err(error) => fail(&format!("{error}, open '{}'", args[1])),
            };
            // Nonce candidates, tried in order: the file path as typed (not
            // resolved), then the script content itself (strict). Short
            // scripts are not distinctive enough to safely identify a
            // session, so the path stands alone there.
            const CONTENT_NONCE_MIN_CHARS: usize = 40;
            let trimmed = script.trim().to_string();
            let candidates: Vec<NonceCandidate> =
                if trimmed.chars().count() >= CONTENT_NONCE_MIN_CHARS {
                    vec![
                        NonceCandidate {
                            value: args[1].clone(),
                            strict: false,
                        },
                        NonceCandidate {
                            value: trimmed,
                            strict: true,
                        },
                    ]
                } else {
                    vec![NonceCandidate {
                        value: args[1].clone(),
                        strict: false,
                    }]
                };
            match execute_query(&home, &cwd, &script, Some(&candidates)) {
                Ok(obelisk_core::core::QueryOutcome::Value(value)) => emit(&value),
                Ok(obelisk_core::core::QueryOutcome::Undefined) => emit_undefined(),
                Err(error) => fail(&error.message),
            }
        }
        Some("--attune") if args.len() > 1 => {
            let script = match std::fs::read_to_string(&args[1]) {
                Ok(script) => script,
                Err(error) => fail(&format!("{error}, open '{}'", args[1])),
            };
            match execute_attune(&home, &cwd, &script) {
                Ok(value) => emit(&value),
                Err(error) => fail(&error.message),
            }
        }
        Some("install") => {
            let npx = if cfg!(windows) { "npx.cmd" } else { "npx" };
            let status = Command::new(npx)
                .arg("--yes")
                .arg("skills")
                .arg("add")
                .arg("tommy0103/obelisk-skill")
                .args(&args[1..])
                .status();
            match status {
                Ok(status) if status.success() => {}
                Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                Err(error) => {
                    eprintln!("Unable to run the skills installer: {error}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("Usage:\n  obelisk install [skills options]\n  obelisk --build\n  obelisk --search \"text\" [--nonce <token>]\n  obelisk --query <file.js>\n  obelisk --attune <file.js>");
            std::process::exit(1);
        }
    }
}

/// Keep the db module linked (DB_PATH derivation mirrors the TS export).
#[allow(dead_code)]
fn db_path_check(home: &Path) -> PathBuf {
    db::db_path(home)
}

/// Silence unused-import warnings for emit_undefined (used by future
/// undefined-returning query scripts).
#[allow(dead_code)]
fn _unused_emit() {
    let _ = emit_undefined;
    let _ = std::io::stdout().flush();
}
