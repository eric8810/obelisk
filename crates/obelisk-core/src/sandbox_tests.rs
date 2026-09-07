// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Sandbox behavior tests (ADR-0013 Decision 2): the two-layer kill, the
//! globals whitelist, host JSON boundary, and the async drive loop.

use serde_json::{json, Value};

use crate::sandbox::{run_sandbox, SandboxOptions, SandboxOutcome};

const HELPERS: &[&str] = &["ping", "boom"];

fn dispatch(name: &str, _args: &Value) -> Result<Value, String> {
    match name {
        "ping" => Ok(json!({ "pong": true, "echo": _args })),
        "boom" => Err("boom() exploded".to_string()),
        other => Err(format!("unknown helper: {other}")),
    }
}

fn run(script: &str, timeout_ms: u64) -> SandboxOutcome {
    run_sandbox(
        Box::new(dispatch),
        HELPERS,
        script,
        SandboxOptions {
            timeout_ms,
            memory_limit: 64 * 1024 * 1024,
            stack_size: 512 * 1024,
        },
    )
}

#[test]
fn returns_json_value() {
    let out = run("return { answer: 41 + 1, list: [1, 2, 3] };", 5000);
    assert_eq!(
        out,
        SandboxOutcome::Value(json!({ "answer": 42, "list": [1, 2, 3] }))
    );
}

#[test]
fn returns_undefined() {
    let out = run("return;", 5000);
    assert_eq!(out, SandboxOutcome::Undefined);
    let out2 = run("let x = 1;", 5000);
    assert_eq!(out2, SandboxOutcome::Undefined);
}

#[test]
fn rejects_with_message_on_throw() {
    let out = run("throw new Error('deliberate');", 5000);
    match out {
        SandboxOutcome::Error { message, stack } => {
            assert_eq!(message, "deliberate");
            assert!(stack.is_some());
        }
        other => panic!("expected error outcome, got {other:?}"),
    }
}

#[test]
fn rejects_on_awaited_throw() {
    let out = run(
        "await Promise.resolve(); throw new Error('async deliberate');",
        5000,
    );
    match out {
        SandboxOutcome::Error { message, .. } => assert_eq!(message, "async deliberate"),
        other => panic!("expected error outcome, got {other:?}"),
    }
}

#[test]
fn sync_runaway_loop_is_killed_by_interrupt_handler() {
    let started = std::time::Instant::now();
    let out = run("while (true) {}", 400);
    let elapsed = started.elapsed();
    match out {
        SandboxOutcome::Timeout { message } => {
            assert_eq!(message, "Script execution timed out after 400 ms")
        }
        other => panic!("expected timeout, got {other:?}"),
    }
    assert!(elapsed >= std::time::Duration::from_millis(350));
    assert!(elapsed < std::time::Duration::from_millis(2500));
}

#[test]
fn post_await_runaway_loop_is_killed_by_host_timer() {
    // No VM ticks exist while the promise is pending: only the host timer
    // layer can kill this (ADR-0013).
    let out = run(
        "await new Promise(r => setTimeout(r, 30)); while (true) {}",
        400,
    );
    match out {
        SandboxOutcome::Timeout { message } => {
            assert_eq!(message, "Script execution timed out after 400 ms")
        }
        other => panic!("expected timeout, got {other:?}"),
    }
}

#[test]
fn never_resolving_await_is_killed_by_host_timer() {
    let started = std::time::Instant::now();
    let out = run("await new Promise(() => {}); return 1;", 400);
    assert!(matches!(out, SandboxOutcome::Timeout { .. }));
    let elapsed = started.elapsed();
    assert!(elapsed >= std::time::Duration::from_millis(350));
    assert!(elapsed < std::time::Duration::from_millis(2500));
}

#[test]
fn set_timeout_drives_continuation() {
    let out = run(
        "const v = await new Promise(r => setTimeout(() => r(77), 25)); return v + 1;",
        5000,
    );
    assert_eq!(out, SandboxOutcome::Value(json!(78)));
}

#[test]
fn globals_whitelist_hides_non_ts_builtins() {
    let out = run(
        "return { proxy: typeof Proxy, func: typeof Function, eval: typeof eval, \
             symbol: typeof Symbol, reflect: typeof Reflect, weakmap: typeof WeakMap, \
             arraybuffer: typeof ArrayBuffer, globalThis: typeof globalThis, \
             json: typeof JSON, map: typeof Map, promise: typeof Promise, \
             setTimeout: typeof setTimeout, console: typeof console, \
             process: typeof process, fetch: typeof fetch, require: typeof require };",
        5000,
    );
    let SandboxOutcome::Value(value) = out else {
        panic!("expected value, got {out:?}")
    };
    let check = |key: &str, expected: &str| {
        assert_eq!(
            value.get(key).and_then(Value::as_str),
            Some(expected),
            "{key} must be {expected}: {value}"
        );
    };
    // The TS sandbox surface: present.
    check("json", "object");
    check("map", "function");
    check("promise", "function");
    check("setTimeout", "function");
    check("console", "object");
    check("globalThis", "object");
    // Hidden (not in the TS whitelist) or nonexistent by construction.
    for key in [
        "proxy",
        "func",
        "eval",
        "symbol",
        "reflect",
        "weakmap",
        "arraybuffer",
    ] {
        check(key, "undefined");
    }
    for key in ["process", "fetch", "require"] {
        check(key, "undefined");
    }
}

#[test]
fn host_helpers_cross_the_json_boundary() {
    let out = run(
        "const r = await ping({ a: 1 }, [1, 2]); return r.pong === true && r.echo[0].a === 1;",
        5000,
    );
    assert_eq!(out, SandboxOutcome::Value(json!(true)));
}

#[test]
fn host_errors_throw_with_host_message() {
    let out = run("return await boom();", 5000);
    match out {
        SandboxOutcome::Error { message, .. } => assert_eq!(message, "boom() exploded"),
        other => panic!("expected error outcome, got {other:?}"),
    }
}

#[test]
fn syntax_error_reports_message() {
    let out = run("return {", 5000);
    match out {
        SandboxOutcome::Error { message, .. } => {
            assert!(!message.is_empty());
        }
        other => panic!("expected error outcome, got {other:?}"),
    }
}

#[test]
fn async_await_over_multiple_timers() {
    let out = run(
        "let total = 0; \
         for (const d of [10, 10, 10]) { \
           total += await new Promise(r => setTimeout(() => r(d), d)); \
         } \
         return total;",
        5000,
    );
    assert_eq!(out, SandboxOutcome::Value(json!(30)));
}
