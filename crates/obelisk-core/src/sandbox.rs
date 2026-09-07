// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! CodeAct sandbox: QuickJS-ng embedded via rquickjs (ADR-0013 Decision 2).
//!
//! The TS sandbox (`core.ts runInSandbox`) runs the agent script as an
//! async IIFE inside a `node:vm` context whose globals are the helper API
//! plus a fixed JS builtin whitelist. This port keeps the same script
//! contract inside QuickJS-ng (rquickjs ≥0.12 binds quickjs-ng 0.15.1 with
//! the CVE-2026-0821 fix), with the two-layer 30s wall-clock kill:
//!
//! * **Sync runaway**: the runtime interrupt handler compares the wall
//!   clock on every engine interrupt check, so a synchronous infinite loop
//!   is aborted mid-execution.
//! * **Pending awaits**: the host job loop notices the deadline with no
//!   engine work in flight (a script suspended across a promise that only a
//!   timer would resolve) and kills the run. The TS build hangs forever in
//!   that case; the host timer closes the hole (bug-fix-grade strengthening
//!   recorded in ADR-0013).
//!
//! Host helpers cross the boundary as JSON strings parsed engine-side; the
//! globals whitelist mirrors the TS one exactly (only grows, never shrinks),
//! and no fs/net/process surface exists (QuickJS without quickjs-libc).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::{CatchResultExt, Context, Runtime};
use serde_json::Value;

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Generous ceiling for query scripts; the TS sandbox has no limit (this is
/// a strengthening, not a parity break).
pub const DEFAULT_MEMORY_LIMIT: usize = 256 * 1024 * 1024;
pub const DEFAULT_STACK_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SandboxOptions {
    pub timeout_ms: u64,
    pub memory_limit: usize,
    pub stack_size: usize,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            memory_limit: DEFAULT_MEMORY_LIMIT,
            stack_size: DEFAULT_STACK_SIZE,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SandboxOutcome {
    /// The script resolved with a JSON value.
    Value(Value),
    /// The script resolved with `undefined`.
    Undefined,
    /// The script (or a helper) threw; carries the JS error message.
    Error {
        message: String,
        stack: Option<String>,
    },
    /// Killed at the wall-clock deadline (sync loop or pending await).
    Timeout { message: String },
}

/// One host helper call: `(name, args as JSON array) -> result or error`.
/// Owned ('static) because the native sandbox functions must not borrow.
pub type HostDispatch = Box<dyn Fn(&str, &Value) -> Result<Value, String>>;

const TIMEOUT_MESSAGE: fn(u64) -> String =
    |ms: u64| format!("Script execution timed out after {ms} ms");

// The whitelisted JS builtins (TS `runInSandbox` global set) live in the
// SANDBOX_SETUP keep-set below; everything else the QuickJS global object
// exposes is deleted at setup.
const SANDBOX_SETUP: &str = r#"
(() => {
  const keep = new Set(["JSON","Math","Array","Object","Set","Map","Date","RegExp",
    "parseInt","parseFloat","String","Number","Boolean","Error","Promise","console","setTimeout",
    "globalThis",
    "__obelisk_host","__obelisk_done","__obelisk_console","__obelisk_helpers"]);
  for (const key of Object.getOwnPropertyNames(globalThis)) {
    if (!keep.has(key)) { try { delete globalThis[key]; } catch (e) { /* non-configurable */ } }
  }
})();
(() => {
  // console shim: strings pass through; structured values JSON-render.
  // node's util.inspect differs in quoting details (noted deviation).
  const render = (a) => typeof a === "string" ? a : (() => { try { return JSON.stringify(a); } catch (e) { return String(a); } })();
  globalThis.console = {
    log: (...a) => __obelisk_console("log", a.map(render).join(" ")),
    info: (...a) => __obelisk_console("log", a.map(render).join(" ")),
    debug: (...a) => __obelisk_console("log", a.map(render).join(" ")),
    warn: (...a) => __obelisk_console("warn", a.map(render).join(" ")),
    error: (...a) => __obelisk_console("error", a.map(render).join(" ")),
  };
})();
(() => {
  // Timers, fully engine-side: setTimeout queues (cb, at); the host loop
  // fires due timers, which queue more promise jobs.
  const timers = [];
  globalThis.setTimeout = (cb, delay) => {
    const id = timers.length + 1;
    timers.push({ id, cb, at: Date.now() + (Number(delay) || 0) });
    return id;
  };
  globalThis.__obelisk_nextTimerAt = () =>
    timers.length ? Math.min(...timers.map((t) => t.at)) : null;
  globalThis.__obelisk_fireTimers = () => {
    const now = Date.now();
    const due = [];
    for (let i = timers.length - 1; i >= 0; i--) if (timers[i].at <= now) due.push(timers.splice(i, 1)[0]);
    for (const t of due) t.cb();
  };
})();
(() => {
  // Host helper shims: args serialize to a JSON string, results parse back.
  // Host errors throw as Error with the host's message.
  const call = (name, argsJson) => {
    const raw = __obelisk_host(name, argsJson);
    const v = JSON.parse(raw);
    if (v && v.__err !== undefined) throw new Error(v.__err);
    return v.ok;
  };
  const proxy = (name) => (...args) => call(name, JSON.stringify(args));
  for (const name of __obelisk_helpers) globalThis[name] = proxy(name);
})();
"#;

/// Run one CodeAct script inside the sandbox.
pub fn run_sandbox(
    dispatch: HostDispatch,
    helpers: &[&str],
    script: &str,
    options: SandboxOptions,
) -> SandboxOutcome {
    let Ok(runtime) = Runtime::new() else {
        return SandboxOutcome::Error {
            message: "unable to create QuickJS runtime".to_string(),
            stack: None,
        };
    };
    let deadline = Instant::now() + Duration::from_millis(options.timeout_ms);
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
    runtime.set_memory_limit(options.memory_limit);
    runtime.set_max_stack_size(options.stack_size);
    let ctx = match Context::full(&runtime) {
        Ok(ctx) => ctx,
        Err(error) => {
            return SandboxOutcome::Error {
                message: error.to_string(),
                stack: None,
            }
        }
    };

    let done: Rc<RefCell<Option<(u8, String)>>> = Rc::new(RefCell::new(None));
    let console_sink: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));

    // Install the host surface, then evaluate the async IIFE with its
    // resolution handlers.
    let helpers_json = serde_json::to_string(&helpers).unwrap_or_else(|_| "[]".to_string());
    let setup_error: Option<String> = ctx.with(|ctx| -> Option<String> {
        let globals = ctx.globals();
        let dispatch_rc = Rc::new(RefCell::new(dispatch));
        let host = {
            let dispatch_rc = dispatch_rc.clone();
            move |name: String, args: String| -> rquickjs::Result<String> {
                let args_value: Value = serde_json::from_str(&args).unwrap_or(Value::Array(Vec::new()));
                match (dispatch_rc.borrow())(name.as_str(), &args_value) {
                    Ok(value) => Ok(serde_json::to_string(&serde_json::json!({ "ok": value }))
                        .unwrap_or_else(|_| r#"{"ok":null}"#.to_string())),
                    Err(message) => Ok(
                        serde_json::to_string(&serde_json::json!({ "__err": message }))
                            .unwrap_or_else(|_| r#"{"__err":"host error"}"#.to_string()),
                    ),
                }
            }
        };        if let Err(error) = globals.set("__obelisk_host", rquickjs::Function::new(ctx.clone(), host)) {
            return Some(error.to_string());
        }
        let done_slot = done.clone();
        let done_fn = move |kind: i32, payload: Option<String>| -> rquickjs::Result<()> {
            *done_slot.borrow_mut() = Some((
                if kind == 1 { 1 } else { 0 },
                payload.unwrap_or_default(),
            ));
            Ok(())
        };
        if let Err(error) = globals.set(
            "__obelisk_done",
            rquickjs::Function::new(ctx.clone(), done_fn),
        ) {
            return Some(error.to_string());
        }
        let console_slot = console_sink.clone();
        let console_fn = move |level: String, text: String| -> rquickjs::Result<()> {
            console_slot.borrow_mut().push((level, text));
            Ok(())
        };
        if let Err(error) = globals.set(
            "__obelisk_console",
            rquickjs::Function::new(ctx.clone(), console_fn),
        ) {
            return Some(error.to_string());
        }
        if let Err(error) = globals.set(
            "__obelisk_helpers",
            rquickjs::String::from_str(ctx.clone(), &helpers_json)
                .unwrap_or_else(|_| rquickjs::String::from_str(ctx.clone(), "[]").expect("static")),
        ) {
            return Some(error.to_string());
        }
        // helpers arrive as a JSON string; parse it into an array value.
        if let Err(error) = ctx.eval::<rquickjs::Value, _>(
            "globalThis.__obelisk_helpers = JSON.parse(__obelisk_helpers)",
        ) {
            return Some(error.to_string());
        }
        if let Err(error) = ctx.eval::<rquickjs::Value, _>(SANDBOX_SETUP) {
            return Some(error.to_string());
        }
        // The script body: async IIFE whose resolution/rejection is reported
        // through __obelisk_done. A synchronous throw inside the IIFE body
        // rejects the promise (the engine converts it), so eval itself only
        // fails for syntax errors.
        let mut wrapped = String::with_capacity(script.len() + 512);
        wrapped.push_str("(async()=>{");
        wrapped.push_str(script);
        wrapped.push_str(
            "})().then(v=>{let s;try{s=(v===undefined||typeof v===\"function\"||typeof v===\"symbol\")?null:JSON.stringify(v);}catch(e){s=null;}if(s===undefined)s=null;try{__obelisk_done(0,s===null?\"\":s);}catch(e){__obelisk_done(0,\"\");}},e=>{let m,st;try{m=String((e&&e.message!==undefined)?e.message:e);st=String((e&&e.stack!==undefined)?e.stack:\"\");}catch(x){m=String(e);st=\"\";}try{__obelisk_done(1,m+\"\\u0000\"+st);}catch(x2){__obelisk_done(1,\"error\\u0000\");}})",
        );
        match ctx
            .eval::<rquickjs::Value, _>(wrapped.as_str())
            .catch(&ctx)
        {
            Ok(_) => None,
            Err(rquickjs::CaughtError::Exception(exception)) => {
                let message = exception.message().unwrap_or_else(|| "script error".to_string());
                let stack = exception.stack().filter(|s| !s.is_empty());
                Some(format!(
                    "__error__:{}\u{0}{}",
                    message,
                    stack.unwrap_or_default()
                ))
            }
            Err(other) => Some(format!(
                "__error__:{}\u{0}",
                other
            )),
        }
    });

    // Flush sandbox console output to the real stdout/stderr (the TS console
    // prints live; ordering with the result JSON matches because we flush
    // before returning).
    let flush_console = || {
        for (level, text) in console_sink.borrow_mut().drain(..) {
            match level.as_str() {
                "warn" | "error" => eprintln!("{text}"),
                _ => println!("{text}"),
            }
        }
    };

    if let Some(error) = setup_error {
        flush_console();
        // A wall-clock interrupt during the synchronous IIFE prologue
        // surfaces here; the published contract is the timeout message.
        if Instant::now() >= deadline && error.contains("interrupt") {
            return SandboxOutcome::Timeout {
                message: TIMEOUT_MESSAGE(options.timeout_ms),
            };
        }
        if let Some(payload) = error.strip_prefix("__error__:") {
            let (message, stack) = payload
                .split_once('\u{0}')
                .map(|(m, s)| {
                    (
                        m.to_string(),
                        if s.is_empty() {
                            None
                        } else {
                            Some(s.to_string())
                        },
                    )
                })
                .unwrap_or_else(|| (payload.to_string(), None));
            return SandboxOutcome::Error { message, stack };
        }
        return SandboxOutcome::Error {
            message: error,
            stack: None,
        };
    }

    // Drive loop: drain promise jobs, honor timers, enforce the deadline.
    loop {
        // Drain the job queue.
        loop {
            match runtime.execute_pending_job() {
                Ok(true) => continue,
                Ok(false) => break,
                Err(_job_exception) => {
                    // The job ran and threw; the rejection flows through the
                    // promise chain into our then-handler (another job).
                    continue;
                }
            }
        }
        if let Some((kind, payload)) = done.borrow_mut().take() {
            flush_console();
            return match (kind, payload.as_str()) {
                (1, payload) => {
                    let (message, stack) = payload
                        .split_once('\u{0}')
                        .map(|(m, s)| {
                            (
                                m.to_string(),
                                if s.is_empty() {
                                    None
                                } else {
                                    Some(s.to_string())
                                },
                            )
                        })
                        .unwrap_or_else(|| (payload.to_string(), None));
                    // A wall-clock interrupt abort surfaces engine-side as
                    // an "interrupted" rejection; the published contract (TS
                    // node:vm timeout) is the timeout message.
                    if Instant::now() >= deadline && message.contains("interrupt") {
                        return SandboxOutcome::Timeout {
                            message: TIMEOUT_MESSAGE(options.timeout_ms),
                        };
                    }
                    SandboxOutcome::Error { message, stack }
                }
                (0, "") => SandboxOutcome::Undefined,
                (0, json) => {
                    SandboxOutcome::Value(serde_json::from_str(json).unwrap_or(Value::Null))
                }
                _ => SandboxOutcome::Undefined,
            };
        }
        if Instant::now() >= deadline {
            flush_console();
            return SandboxOutcome::Timeout {
                message: TIMEOUT_MESSAGE(options.timeout_ms),
            };
        }
        // Timers: fire due ones (which queue jobs), else wait for the next
        // timer deadline — always bounded by the kill deadline.
        let next_timer: Option<f64> =
            ctx.with(|ctx| ctx.eval::<f64, _>("__obelisk_nextTimerAt()").ok());
        match next_timer {
            Some(at_ms) => {
                let now_ms = now_epoch_ms();
                let wait_ms = (at_ms - now_ms).max(0.0) as u64;
                let remaining = deadline.saturating_duration_since(Instant::now());
                let wait = Duration::from_millis(wait_ms.min(50)).min(remaining);
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
                ctx.with(|ctx| {
                    let _: rquickjs::Result<()> = ctx.eval::<(), _>("__obelisk_fireTimers()");
                });
            }
            None => {
                // No jobs, no timers, promise unresolved: the script is
                // suspended across an await nothing will resolve. TS hangs
                // forever here; the host timer kills at the deadline.
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(25)));
            }
        }
    }
}

fn now_epoch_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}
