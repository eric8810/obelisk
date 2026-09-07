# Golden corpus & TS baseline (Rust-migration M1.0)

This directory freezes the **TS-oracle golden** for the Obelisk Rust port: a
deterministic five-provider synthetic corpus, the exact index dump the TS CLI
produces from it, and the TS CLI performance baseline. The Rust CLI must
produce a **byte-identical dump** on the same corpus — that equality is the
M1 acceptance gate for the port's parse/persist layers.

Everything here is synthetic (seeded PRNG + fixed epoch 2026-06-10T10:00:00Z).
No real user data, no network.

## Files

| File | What it is |
| --- | --- |
| `generate-corpus.mjs` | Writes the deterministic synthetic corpus for all five providers into `--out <dir>` (home at `<dir>/home`). `--scale N` (default 1) replicates the scale-1 shape N times with distinct ids/timestamps and grows per-session turn counts (`min(48, floor((N-1)/6))` extra turns). `--big-messages M` (default 0) adds one claude session with M messages. `--seed S` (default `0x0be11ace`) reseeds the PRNG. |
| `dump-index.mjs` | Opens `<home>/.obelisk/obelisk.sqlite` read-only and emits the canonical normalized JSON dump (see below). `--home <dir> [--out file]`. |
| `expected-dump.json` | The golden: dump of the scale-1 corpus after `obelisk --build`. This is what the Rust CLI must reproduce byte-for-byte. |
| `corpus-manifest.json` | Quick sanity summary of what the scale-1 corpus contains (per-provider sessions/messages/tool_calls, indexed truth from the dump) plus a coverage list. |
| `measure-baseline.mjs` | Moved to `docs/history/measure-ts-baseline.mjs` — the TS CLI it measured is retired (ADR-0013 Stage 3); kept as the record of how the baseline numbers were produced. |
| `baseline.md` | Moved to `docs/history/ts-baseline.md` — the frozen TS baseline the Rust port was judged against during migration. |

## Regenerating the golden

```sh
# from the repo root (release binary: cargo build --release)

TMP=$(mktemp -d)
node tests/golden/generate-corpus.mjs --out "$TMP/corpus" --scale 1
env HOME="$TMP/corpus/home" USERPROFILE="$TMP/corpus/home" \
  target/release/obelisk --build
node tests/golden/dump-index.mjs --home "$TMP/corpus/home" --out "$TMP/dump.json"

# idempotency: a second --build (always a force rebuild) must not change a byte
env HOME="$TMP/corpus/home" USERPROFILE="$TMP/corpus/home" \
  target/release/obelisk --build
node tests/golden/dump-index.mjs --home "$TMP/corpus/home" --out "$TMP/dump2.json"
cmp "$TMP/dump.json" "$TMP/dump2.json"   # must be identical

# search sanity (expect JSON results with hits)
env HOME="$TMP/corpus/home" USERPROFILE="$TMP/corpus/home" \
  target/release/obelisk --search "golden claude corpus"

cp "$TMP/dump.json" tests/golden/expected-dump.json
```

Environment notes (mirrors the E2E harness in `tests/e2e/harness.mjs`): the CLI
resolves all
provider roots from HOME, so `HOME`/`USERPROFILE` must point at the corpus home
and `DSH_HOME`, `KIMI_CODE_HOME`, `PI_CODING_AGENT_DIR`,
`PI_CODING_AGENT_SESSION_DIR` must be unset; the temp HOME must not contain a
pre-existing `~/.obelisk`. The CLI's cwd must not contain a `.pi/settings.json`
(pi reads project settings from the process cwd).

## Corpus layout (per scale unit)

```
home/.claude/projects/-home-synth-obelisk-golden-claude/
  claude-sess-NNNN-a-<hex>.jsonl                    main session A (ai-title)
  claude-sess-NNNN-a-<hex>/subagents/agent-77.jsonl + .meta.json
  claude-sess-NNNN-a-<hex>/subagents/workflows/<runId>/agent-11.jsonl + .meta.json
  claude-sess-NNNN-a-<hex>/workflows/<runId>.json   workflow run + workflowProgress
  claude-sess-NNNN-b-<hex>.jsonl                    session B (history.jsonl title)
home/.claude/history.jsonl
home/.codex/sessions/2026/06/DD/rollout-<ts>-<root>.jsonl
home/.codex/sessions/2026/06/DD/rollout-<ts>-<child>.jsonl   (thread_spawn subagent)
home/.codex/session_index.jsonl
home/.dsh/sessions/--home-synth-obelisk-golden-deepseek--/<root-id>/session.jsonl.zstd
home/.dsh/sessions/--home-synth-obelisk-golden-deepseek--/<child-id>/session.jsonl.zstd
home/.kimi-code/sessions/ws-golden-N/<session>/state.json
home/.kimi-code/sessions/ws-golden-N/<session>/agents/{main,agent-7}/wire.jsonl
home/.pi/agent/sessions/pi-NNNN-{a,b,c}/session.jsonl
```

## Dump normalization rules

The dump must be byte-identical across runs of the same corpus even though
file mtimes/inodes differ. `dump-index.mjs` therefore:

1. Emits tables in a fixed order — `sessions, messages, tool_calls,
   tool_results, subagents, workflows, workflow_agents, index_state,
   summaries, memories` — rows sorted by primary key, object keys in schema
   column order.
2. Relativizes home-prefixed path columns (`sessions.jsonl_path`,
   `index_state.jsonl_path`): the absolute corpus home is replaced with
   `<HOME>`, so the dump does not depend on the temp directory.
3. For `index_state` emits only `{jsonl_path, lines_processed}` per row:
   `mtime` (file mtime wall-clock) and `cursor` (embeds mtime + ctime + inode)
   are volatile and excluded; `lines_processed` is deterministic (line counts
   for claude/codex, zstd frame-count totals for deepseek, 0 for kimi/pi).
   System marker rows (`__last_build__`, `__app_heartbeat__`,
   `__fts_triggers_ready__`, `__project_path_backfill_v1__`, any other
   `__…__`) are excluded **except** the five provider index-version markers,
   which are kept: `__claude_canonical_transcript_v2__`,
   `__codex_canonical_transcript_v3__`,
   `__deepseek_canonical_transcript_v2__`, `__kimi_canonical_transcript_v6__`,
   `__pi_canonical_transcript_v9__`.

Everything else is emitted verbatim; the generator makes it deterministic.

## Idempotency finding

`obelisk --build` always runs `buildIndex({ force: true })` — every build is a
force full re-index (the 30 s `__last_build__` debounce is bypassed for force
builds). The second build above is therefore a genuine full re-parse, and the
dump is byte-identical to the first run (verified). `--search` also runs an
incremental pre-query refresh (`refreshQueryIndex`), which the 30 s debounce
turns into a no-op right after a build; the dump is unchanged after searches
too.

## Format quirks the Rust port must know

- **deepseek**: the artifact is a concatenation of **independent checksummed
  zstd frames** (one per append batch) — not one zstd stream. A whole root
  session **tree** (root + children, grouped by cwd-scoped ancestry) is ONE
  index unit with ONE `index_state` row keyed by the root file path;
  `lines_processed` is the summed frame count of all members. Session ids are
  `deepseek:<urlencoded-id>:<sha256-of-cwd>`; child messages fold into the
  root session as sidechain rows. The subagent parent link is recovered from
  the spawn tool-result text matching `started subagent <id>`.
- **kimi**: the index unit key is the **session directory**, not a file;
  `index_state` stores the manifest cursor (`kimi-manifest-v1` digest over
  member stat tuples). Titles/session metadata come from `state.json`; the
  provider re-parses the whole session (countMode `total`) and replays
  `context.undo`/`context.clear` to project the surviving messages. Subagent
  parent links come from tool-result text matching `agent_id: <id>`.
- **pi**: any `.jsonl` under `~/.pi/agent/sessions` (recursive). Message uuids
  are positional (`:entry:<ordinal>:message:block:<n>`, plus
  `:tail:<i>:block:<n>` for compaction retained tails), so an append that adds
  entries keeps earlier uuids stable. Session ids are
  `pi:<urlencoded-id>:<sha256-of-cwd>`. Visibility ('visible'/'inactive') is
  derived from the active branch (leaf target → parent chain → latest
  compaction checkpoint).
- **codex**: full-reparse provider (countMode `total`); `event_msg`
  user/agent messages are deduped against duplicate `response_item` messages;
  a child thread's session id folds into the parent thread's session
  (`codex:<parent-uuid>`), with the child's own id becoming the `agent_id`.
  Guardian threads emit `delete-session`.
- **claude**: line-incremental (countMode `delta`); cursors are
  `mtime:lines:size:ctime:inode` signatures. Workflow runs are separate index
  units keyed by the run `.json`; the parent link matches the run id inside
  the `Workflow` tool_result text (with an SQL-based heal at finalize).

## Machine context

`docs/history/ts-baseline.md` records Node version, CPU model, and platform.
Re-run the archived measure script on the target comparison machine before
judging the
Rust port; the corpus is deterministic, so only the machine differs.
