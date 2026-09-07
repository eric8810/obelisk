# TS CLI performance baseline (Rust-migration M1.0)

Measured against the built TS CLI (`packages/cli/dist/cli/src/obelisk.js`, obelisk
v0.2.3) on a **deterministic synthetic corpus** — no real corpus exists on this
machine, so the original plan's "real corpus" measurement is approximated by
generated data of comparable shape (`tests/golden/generate-corpus.mjs`).

## Machine context

- Node: v26.7.0
- CPU: AMD Ryzen 7 H 255 w/ Radeon 780M Graphics (16 cores)
- Platform: linux x64, 27.2 GiB RAM

## Corpus scale

- Generator: `node tests/golden/generate-corpus.mjs --out <tmp> --scale 100 --big-messages 2000`
- Sessions: 801 total (claude: 201, codex: 100, deepseek: 100, kimi: 100, pi: 300)
- Messages: 42700 total (claude: 10500, codex: 6400, deepseek: 8300, kimi: 6600, pi: 10900)
- Tool calls: 14600, tool results: 14500
- Longest session: 2000 messages (claude `claude-sess-big-*`)

## Measurements

| Metric | Value |
| --- | --- |
| TS CLI cold start: `--version` (median of 5) | 35.9 ms |
| TS CLI cold start: `--version` (min … max) | 34.6 ms … 36.4 ms |
| `--search "golden"` (median of 5, 20 hits at limit 20) | 61.2 ms |
| `--search "golden"` (min … max) | 60.3 ms … 63.4 ms |
| Full build, fresh DB (single run) | 4.85 s |
| Force rebuild over existing DB (single run) | 5.51 s |
| Corpus generation (single run) | 251.9 ms |
| `obelisk.sqlite` size after build | 54.6 MiB (57,257,984 bytes) |
| Total corpus size on disk | 70.6 MiB (74,002,124 bytes) |

## TS install footprint (Rust binary-size target: < 15 MB)

| Path | Size |
| --- | --- |
| `node_modules` (repo, all workspaces) | 41.1 MiB |
| `packages/cli/dist` (compiled CLI itself) | 380.9 KiB |

## Notes

- `--search` timings were taken immediately after a build, so the CLI's
  30-second build debounce suppresses the pre-query incremental refresh
  (`refreshQueryIndex`); these numbers measure the hot query path (process
  start + provider registry + SQLite open + FTS query). A search run more
  than 30 s after the last build additionally pays one incremental
  discovery pass over all provider roots.
- `--build` always runs a force full re-index (`buildIndex({ force: true })`);
  there is no cheap incremental path exposed by the CLI, so both build
  measurements are full parses.
- `node_modules` is the shared dev dependency tree of the whole monorepo
  (includes the Electron app, ESLint, TypeScript). The Rust port replaces
  both the Node runtime requirement and this footprint with a single
  statically-linked binary (< 15 MB target).
- Total measurement wall time: 11.63 s.
