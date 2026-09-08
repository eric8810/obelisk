# Contributing to Obelisk

Thanks for contributing. This document exists because most PRs that stall here
are not low-quality code — they pass lint, typecheck, and their own tests. They
stall on a small number of recurring failures that are easy to avoid once
someone names them.

Read the section for the area you are changing. The verification contract at the
bottom applies to every PR.

---

## Before you open a pull request

- **Bug reports are always welcome** as issues — no permission needed.
- **Bugfix PRs may be opened directly**, but state the reproduction and the root
  cause in the description. Review turnaround is not guaranteed for PRs that did
  not come out of an issue discussion.
- **Feature or behavior-change PRs require an issue first.** Unsolicited ones
  are closed without review — not because the code is bad, but because the
  design conversation has to happen before the implementation, not after.
- **Absorption is a normal outcome.** Sometimes the maintainer lands an
  equivalent change directly instead of merging the PR — when the surrounding
  design is still moving, or the fix touches code with constraints that are
  faster to apply than to explain. When a change is absorbed this way, the
  closing comment will say so.

---

## Six things that decide whether a PR lands

**1. Run every sentence of your PR description end to end.**
The single most common failure is a capability that is advertised but
unreachable. If you describe a config option, use that option from the outermost
entry point before submitting. If you post a screenshot, the input in that
screenshot must be an input the code can actually handle.

**2. Write assertions in the words of the requirement, not the shape of the
implementation.**
Copy the sentence from the issue into your test name. If the issue says "without
causing reader-position jumps", the assertion has to measure reader position —
not "the row got taller". If a test hits behavior you did not expect, decide
whether it is a bug before you pin it as expected.

**3. Anything destructive must converge when re-run after an interruption.**
Validate to the point of actual executability before you mutate. Put the whole
sequence in one transaction. Never use the name of the target state as the
completion marker. The test is: if the process dies on any line, does the next
start heal itself?

**4. Read the neighbouring implementation first, and reuse the concepts that
already exist.**
Adding a provider means reading `claude.ts`, `codex.ts`, and `kimi.ts` in full
first. Needing "don't display this row" means grepping for `visibility` before
inventing a field. The burden of proof for a new concept, field, state, or file
type is on the PR: say why the existing one is insufficient. The ADRs in
`docs/adr/` are constraints, not suggestions.

**5. Treat all transcript content as attacker-controlled.**
It is written by third-party agents. Any path where a transcript value reaches
`shell.*`, `fs.*`, `innerHTML`, or SQL/DDL is deny-by-default.

**6. Re-run verification on the final head.**
Merging main invalidates every claim in your PR description, including your own
"known limitations". Run the suites that cover the line you touched, not only
the test you added.

---

## Desktop app (GPUI) changes

The behavior spec for the desktop app is
[docs/desktop-parity.md](docs/desktop-parity.md): a per-feature matrix of the
original Electron app's behavior, current status, and acceptance criteria.
**Every desktop PR must update the relevant rows' status in that document**
(and new features go in before the code does). "It renders" is not parity —
the acceptance column is the bar.

The app lives in `crates/obelisk-app`. Behavior is verified black-box through
`tests/e2e-desktop/` (11 scenarios, driver + vision assertions over real
windows): run `node tests/e2e-desktop/run-desktop.mjs` after a
`cargo build --release -p obelisk-app`.

- **Scroll-stability assertions are required for any timeline change.** The
  timeline follows tail while pinned and releases on user scroll-up; content
  settling above the viewport must not move what the user is reading. Scenario
  D3 (scroll follow) and D11 (live session follow) cover this — extend them if
  your change can disturb it.
- **Cover three states, not just the final one**: mount, data-loaded, and
  empty/error. Views render from the shared index and may start empty while the
  daemon builds.
- **One visual treatment per user-visible concept.** "Blocked source" and
  "failed to load" are the same thing to a reader; they must not render two
  different ways.
- **Every async probe needs a deadline and an error path.** A future with no
  timeout turns a regression into a hung E2E run instead of a red one.
- The app is tray-resident: closing the window must keep the process (and the
  daemon heartbeat) alive — scenario D8 asserts exactly this. Any change to the
  window lifecycle re-runs D7/D8.
- GPUI exports no accessibility tree on X11, so the E2E driver clicks root pixel
  coordinates. If you change the sidebar/row layout, update the driver's
  coordinate tables in the same commit and re-run the full desktop suite.

## Provider adapters

- **Read `claude.rs`, `codex.rs`, and `kimi.rs` (in
  `crates/obelisk-core/src/providers/`) before writing a new adapter.** The
  conventions there are earned: zero-padded ordinals in ids, the
  `__<provider>_canonical_transcript_vN__` marker, how `git_branch` is handled.
- **Session identity must not be the source id alone.** Use a composite such as
  (normalized cwd, header id). Explicit session ids are usually project-local, so
  two projects may legitimately collide — and the second one indexed will
  overwrite the first.
- **A test must actually exercise discovery.** Asserting the resolved root string
  passes even when the directory-layout assumption is wrong.
- **Verify directory layout against the upstream source or format docs**, not
  against what your own machine happens to look like. A tool's default root and
  its custom root often have different nesting.
- **The canonical transcript invariant (ADR-0007) is a hard gate**: assembling
  directly from your adapter must equal assembling after a SQLite round-trip
  (`session_detail_tests.rs`). Any design where duplicate ids merge or overwrite
  breaks it.
- **Never drop a record just because it has no text.** Image-only messages and
  aborted turns that carry usage must still emit a row (`text: null`,
  `content_type: 'unknown'`), or token accounting and the timeline develop holes.
- **Bump the index version marker whenever you change uuid format, role
  normalization, or anything else affecting already-stored rows.** Otherwise the
  mtime short-circuit in discovery leaves old-format rows in the database
  forever.
- **Express "this should not be shown" with the existing `visibility` field**
  (`providers/types.rs`), which is defined as provider-normalized display
  eligibility and already has a consumer in the assembler. Do not add a third
  meaning to `is_sidechain`.
- **Cursors must detect same-millisecond rewrites**: mtime + ctime + size + inode,
  not mtime alone. Reconcile moves, copies, deletes, and replacements.
- **Version gates must tolerate the unknown.** Throwing on an unexpected higher
  version makes one bad file trigger a full re-index every run, because the
  provider's index markers are withheld while any unit fails. Skip and record
  instead of poisoning the provider.

## Schema and migrations

- `crates/obelisk-core/src/schema.sql` is the single source of truth
  (`SCHEMA_SQL` embeds it verbatim), and the golden dump
  (`tests/golden/expected-dump.json`) pins the resulting row shape. Changing the
  schema is an explicit decision plus a full re-index: justify it in the PR and
  regenerate the golden dump in the same commit. Prefer additive changes.
- **Destructive DDL goes in one transaction.** The repository already has the
  transaction runner (`crates/obelisk-core/src/tx.rs`), and the entry points
  already hold the writer lease — you do not need to invent a migration
  marker.
- **Do not use the name of the target state as the completion marker.** If the
  process is interrupted after CREATE but before the rebuild finishes, comparing
  the current setting against the requested one reports success forever and the
  data is never backfilled.
- **Validate to executability, not to lexical shape.** A regex that accepts a
  string SQLite will reject means you drop the table and then fail.
- **Before writing one value across every table, check whether any table carries
  its own arguments.** Overwriting them leaves the migration looking complete,
  so it never self-heals.
- **Any external input spliced into DDL needs an allowlist and an injection test
  case.**

## Untrusted input (sandbox, file references, opener)

- **Transcript paths must not reach the OS file opener unguarded.** The
  file-reference resolver (`crates/obelisk-app/src/file_reference.rs`) confines
  targets to the session's own roots before handing anything to the opener. Keep
  that containment; never open a path straight from transcript text.
- **Reading a file because a transcript said so requires an allowlist**, scoped to
  the session's project cwd or known source roots.
- **The query sandbox is deny-by-default** (ADR-0013): no fs/net/process globals
  reachable from QuickJS, a 30s kill covering both sync loops and loops after
  `await`, and read-only `sql()`. New helpers must state which data they expose
  and why it cannot mutate the index. Probe tests live in
  `crates/obelisk-core/src/sandbox_tests.rs`.
- **Do not broaden interception beyond your feature.** Handling every link
  because you meant to handle one reference type affects everyone else.

## Indexing, daemon, and write ownership

- **The heartbeat decides who may write.** While the desktop daemon is fresh
  (30s cadence, 60s window), the CLI side is read-only: no write connection, no
  schema migration, no PRAGMA change, no checkpoint, no indexing. Two narrow
  carve-outs, both recorded in ADR-0006: the invocation-nonce freshness build,
  and memory mutations (`--attune`), which write only the memory layer. Guarded
  by the indexer tests (`daemon_heartbeat_owns_the_build`,
  `write_daemon_heartbeat_*`) and E2E scenarios C6/C13.
- **If you add something that needs periodic refresh, prove its refresh point is
  actually called repeatedly.** Hanging a full rebuild off a first-run-only gate
  means it runs once and never again — and for existing installations, never at
  all.
- **`daemon_active` must not swallow a configuration change.** Distinguish
  "correctly skipping" from "configuration mismatch"; the latter is an explicit
  error, not a silent fallback to a stale index.
- **State your reasoning when choosing triggers vs. full rebuild.** For example,
  rows written with `INSERT OR REPLACE` do not fire DELETE triggers while
  `recursive_triggers` is off, so a trigger-based refresh would leave stale text
  behind.
- **The daemon writes its own heartbeat under the writer lease**
  (`write_daemon_heartbeat` in `crates/obelisk-core/src/indexer.rs`), and daemon
  builds pass `ignore_daemon_ownership: true` — the process that owns the marker
  must not be suppressed by it. The writer lease stays the sole write
  arbitrator (ADR-0006).

---

## Verification contract

Every PR:

1. `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test --workspace`
   — all green. Quote the **numbers** in the PR description.
2. Touching `crates/` also requires the CLI E2E suite:
   `npm run test:e2e` (needs `cargo build --release` first).
3. Touching the desktop app requires the desktop suite:
   `node tests/e2e-desktop/run-desktop.mjs` (needs a display).
4. Touching the skill chain requires `npm run test:dsh` and
   `npm run build:skill`.
5. **After merging main, re-run everything.** Conclusions from before the merge —
   including any "known limitation" you documented — are void.
6. **Do not loosen an existing assertion.** If one must change, give it its own
   section in the PR description explaining why the original was wrong.
7. **Fixtures are real provider output**, not hand-written approximations.
8. **Confirm your new tests actually run in CI** (`.github/workflows/`).

## Scope and review

- One PR does one thing. Note explicitly anything you deliberately left out.
- If you are unsure about a design decision, say so in the PR instead of
  guessing — an open question is cheaper to resolve than a silent assumption.
- Do not ship a code path you have flagged to yourself as unverified. Writing
  "this call site is worth another look" is honest, but it belongs in a follow-up
  issue, not in the diff.
