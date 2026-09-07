# Tech selection: all-Rust stack — GPUI desktop, QuickJS-ng sandbox

**Context.** A 2026-09-03 investigation evaluated desktop UI frameworks and
embedded JS engines for Rust-izing Obelisk. Owner direction: go all the way —
no webview, no JS runtime in the desktop stack. This ADR records the
selections, the options weighed, and the target architecture they imply.

**Decision.**

1. **Desktop target is pure GPUI in Rust.** The app is rewritten in Rust on
   GPUI; desktop infrastructure GPUI lacks upstream (system tray, global
   hotkeys, notifications, daemon mode) comes from the Apache-2.0 `fc-gpui`
   fork or hand-written platform code. Platform tiers:
   Linux and macOS are tier-1; Windows ships best-effort (known broken
   accessibility, unreliable zh-CN IME) and is re-checked per release. The
   existing Electron app is transitional: it keeps shipping until the GPUI app
   reaches parity, then is retired.
   *Amendment (2026-09-03, vertical-slice verification):* `gpui-component`
   depends on the `gpui-pre` crate family (Zed snapshots), while `fc-gpui`
   publishes its own distinct GPUI crate — the two type families cannot
   coexist in one binary, so the anticipated single-rev lock is impossible
   and the ADR's fallback applies. The fallback resolves better than
   planned: `fc-gpui` ships its own component library (`fc-ui`, 85+
   shadcn-style components incl. variable-height virtualized lists,
   markdown, code blocks, and charts), so the component base is `fc-ui`
   over `fc-gpui` — one crate family, semver-aligned, no rev pinning.
   Verified end-to-end at the first vertical slice (window + tray + project
   sidebar + session list over the shared index).
2. **The CodeAct sandbox embeds QuickJS-ng via `rquickjs` (>= 0.12)**. JS
   remains the sandbox language (it is the product contract), regardless of the
   desktop stack. This is the only evaluated engine meeting all sandbox
   requirements (async/await, LLM-grade modern ES, whitelisted globals, host
   helpers returning SQLite rows, wall-clock kill, no fs/net by default, ~MB
   binary, AGPL-compatible license). The 30s kill is two-layered: the engine
   interrupt handler terminates synchronous runaway code, and a host-side
   wall-clock timer rejects pending host helper promises and triggers the
   interrupt — a script hung across awaits generates no VM ticks, so the
   engine interrupt alone cannot kill it. (The TS sandbox's `node:vm` timeout
   only covers the pre-await synchronous phase, so a post-await runaway loop
   escapes it today; the host timer closes that hole — a bug-fix-grade
   strengthening to be mirrored into the TS side during the transition.)
3. **One Rust core serves both CLI and desktop.** Providers, persist layer,
   schema/migrations, writer lease, watcher, and sandbox port into a single
   Rust crate; the GPUI app is the daemon and writer-lease owner. The injected
   persist seam (node:sqlite vs better-sqlite3 bindings) is retired with the
   TS core: one rusqlite binding serves both binaries. The dual TS/Rust
   coexistence period ends when the TS core is retired.

**Options considered.**

*Desktop UI*

- **Pure GPUI + component library (Rust)** — chosen: no webview, no JS
  runtime, GPU-rendered, small binary/low memory; fits the all-Rust
  direction. Original component pick: `gpui-component`; the vertical slice
  showed it cannot pair with `fc-gpui` (distinct GPUI crate families — see
  the Decision-1 amendment), so the component base is `fc-ui` on `fc-gpui`.
  Accepted risks: `gpui` is pre-1.0 with frequent breaking changes and a
  slowed 2026 upstream (mitigate by the fc-gpui fork's own semver releases;
  Apache-2.0 permits forking), renderer (~6.4k Vue lines) is a full rewrite,
  charts are redrawn via fc-ui's chart components,
  Windows accessibility is broken and zh-CN IME unreliable (zed#59882) —
  accepted for a browse-heavy tool, to be re-checked per release.
- **Stay Electron** — rejected as target (kept only as transitional shipping
  vehicle): mature desktop infrastructure, but resource weight and a JS stack
  that the all-Rust direction rules out.
- **Tauri 2 + Vue** — rejected: still ships a system webview with a JS renderer
  and duplicates the IPC seam; maximal reuse is not the goal, a pure-Rust stack
  is.
- **gpui-vue** — rejected: keeps a full JS runtime (Bun/Node) via N-API, so it
  violates the no-JS-runtime goal; repo created 2026-08-24, single author, no
  releases, npm unpublished, pins a personal `remorses/zed` fork. Useful only
  as a POC vehicle for GPUI capability questions.

*Embedded JS engine (sandbox)*

- **QuickJS-ng + rquickjs** — chosen: ES2023+, interrupt handler for
  synchronous runaway code plus a host wall-clock timer across awaits,
  clean capability model without `quickjs-libc`, MIT, active, production
  precedent (AWS LLRT). Host implements `setTimeout`/`console`; helper data
  crosses the boundary as a JSON string parsed engine-side; keep versions
  current for CVE-2026-0821 (fixed in the 0.15.1 that rquickjs 0.12.x binds).
- **Boa** — rejected: no wall-clock interrupt (loop/recursion counters only),
  which the 30s kill requires; re-evaluate if it ships one.
- **deno_core (V8)** — rejected: ~33 MB minimal embed, overkill.
- **Hermes** — rejected: AOT bytecode design, dynamic `eval` limited.
- **Duktape / JerryScript / Espruino** — rejected: no async/await.
- **quick-js crate** — rejected: unmaintained since 2021.

**Final architecture.**

```
            ┌────────────────────────────────────────────────┐
            │  obelisk-core (Rust crate)                      │
            │  providers: claude, codex, kimi, pi, deepseek   │
            │  persist + schema/migrations  (rusqlite, FTS5)  │
            │  writer lease + index_state arbitration         │
            │  watcher (notify crate)                         │
            │  CodeAct sandbox: rquickjs (QuickJS-ng)         │
            │    whitelisted globals + host helpers,          │
            │    JSON-string data boundary, host setTimeout/  │
            │    console, limits; 30s kill = engine interrupt │
            │    (sync loops) + host timer (pending awaits)   │
            └───────────────┬──────────────────┬──────────────┘
                            │                  │
                 obelisk CLI (thin binary)   GPUI desktop app
                 build/search/query/attune   fc-ui component UI on
                                              fc-gpui (tray/hotkeys/
                                              notifications/daemon)
                                              = daemon & writer-lease owner
```

Shared by both: one `~/.obelisk/obelisk.sqlite`, the same CodeAct contract
(agent-authored JS, four verbs build/search/query/attune unchanged).

Migration staging: (1) port core to Rust, ship Rust CLI against the same
SQLite while the TS daemon still owns writes; (2) GPUI vertical slice
(session list + virtualized timeline) over the Rust core, Electron keeps
shipping; (3) at parity, daemon ownership moves to the GPUI app and the
Electron app, Vue renderer, and TS core are retired.

**Consequences.**

- The desktop rewrite is a full renderer rewrite; provider-adapter semantics
  are ported with the existing 78-file test suite as the spec.
- gpui-vue/GPUix may be used for throwaway POCs (huge-list scrolling, zh IME,
  tray) but never ship in the product; POC results are indicative only, since
  they run on divergent GPUI forks rather than the rev the product pins.
- Boa may replace QuickJS-ng only if it gains a wall-clock interrupt.
- fc-gpui is one release old (2026-08-31); fallback is hand-written platform
  glue, and its tray/hotkey surface should be wrapped so it can be swapped.

---

## Implementation status

**Implemented (2026-09-07).** All three stages are landed:

- **Stage 1 — Rust core + CLI**: providers, persist, schema, lease, watcher,
  and the rquickjs sandbox ported from the TS core with the TS tests as the
  spec (the 241-test Rust suite + golden dump now *are* the spec). The Rust
  CLI is contract-identical (`build`/`search`/`query`/`attune`,
  `obelisk install` unchanged).
- **Stage 2 — GPUI desktop**: `crates/obelisk-app` (fc-ui over fc-gpui):
  virtualized session timeline (images, file references, branch disclosure,
  follow-tail), sessions/memory/activity/recap/settings views, FTS search
  box, tray-resident watcher daemon with incremental indexing. Verified by
  the desktop E2E suite (11 scenarios, D1–D11; four consecutive all-green
  rounds including a post-refactor re-run).
- **Stage 3 — takeover and retirement (M3.1–M3.3)**: the app daemon is the
  writer — it writes the `__app_heartbeat__` marker every 30s under the
  writer lease (startup beat included) and its builds pass
  `ignore_daemon_ownership: true`; CLI mutations skip with `daemon_active`
  while the marker is fresh and age back in within 60s of app exit
  (E2E C13 replays this against the real daemon). `app/`,
  `packages/{core,cli,adaptive-watcher}`, the TS test suite, and the
  Node-test CI workflow are deleted; `install.sh` defaults to the binary
  (npm wrapper retained as `--npm`); the skill chain (dsh-plugin +
  build/publish scripts) is unchanged. The TS-side bug fix mirrored during
  the freeze (undefined `session_id` in `remember()`) lives on in the Rust
  port; the TS patch died with the TS implementation, as the history retains
  the original.

### Verification-tool exemption (recorded)

The `rg -i 'node:sqlite|better-sqlite3|electron|adaptive-watcher'` retirement
gate applies to **product and distribution code** (`crates/`, `packaging/`,
`install.sh`, `packages/dsh-plugin/src`) — that scope is clean. The E2E/golden
**harness** (`tests/e2e/`, `tests/golden/dump-index.mjs`) still uses
`node:sqlite` as a black-box DB assertion tool: it never ships, never runs in
the product path, and Node remains the harness language (tmux-driving the
real TTY). Frozen TS baseline artifacts live in `docs/history/`.

### Outstanding (post-migration gates)

- **User-acceptance soak**: the GPUI app as the only desktop client for two
  weeks without blocking regressions — closes the migration.
- **Electron/GPUI coexistence week**: largely moot now that the TS
  implementation is retired from the tree; the lease-arbitration semantics it
  guarded are covered by C6/C13 and the writer-lease unit tests.
- Windows tier-2 re-check per release (known: accessibility, zh-CN IME).
