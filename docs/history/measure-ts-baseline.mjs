#!/usr/bin/env node
// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// TS CLI performance baseline measurement (Rust-migration M1.0).
//
//   node tests/golden/measure-baseline.mjs [--scale N] [--big-messages M]
//        [--runs R] [--search-term T] [--out <file>]
//
// Generates a deterministic synthetic corpus (default scale 100 plus one
// 2000-message long session), then measures against the built TS CLI
// (packages/cli/dist/cli/src/obelisk.js):
//   (a) cold start: `--version` wall time, median of R runs (default 5);
//       `--search` wall time, median of R runs, measured right after a
//       warm-up build (the CLI's 30s build debounce makes the pre-query
//       incremental refresh a no-op, so this measures the hot query path);
//   (b) full build wall time on a fresh DB (single run) and a force rebuild
//       over the existing DB (single run);
//   (c) resulting obelisk.sqlite size and total corpus size;
//   (d) TS install footprint (node_modules, packages/cli/dist) for comparison
//       with the Rust binary-size target (<15 MB).
//
// Everything runs with HOME isolated to the temp corpus (USERPROFILE set,
// DSH_HOME/KIMI_CODE_HOME/PI_CODING_* cleared). No network, synthetic data
// only. Results are written as a markdown table to --out (default
// tests/golden/baseline.md).

import { spawnSync } from 'node:child_process';
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const cliEntry = join(repoRoot, 'packages', 'cli', 'dist', 'cli', 'src', 'obelisk.js');
const generateCorpus = join(repoRoot, 'tests', 'golden', 'generate-corpus.mjs');
const dumpIndex = join(repoRoot, 'tests', 'golden', 'dump-index.mjs');

function parseArgs(argv) {
  const out = { scale: 100, bigMessages: 2000, runs: 5, searchTerm: 'golden', out: join(repoRoot, 'tests', 'golden', 'baseline.md') };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--scale') out.scale = Number(argv[++i]);
    else if (arg === '--big-messages') out.bigMessages = Number(argv[++i]);
    else if (arg === '--runs') out.runs = Number(argv[++i]);
    else if (arg === '--search-term') out.searchTerm = argv[++i];
    else if (arg === '--out') out.out = resolve(argv[++i]);
    else throw new Error(`unknown argument: ${arg}`);
  }
  return out;
}

function cliEnv(home) {
  const env = { ...process.env, HOME: home, USERPROFILE: home };
  // Provider-root env overrides would escape the isolated HOME.
  delete env.DSH_HOME;
  delete env.KIMI_CODE_HOME;
  delete env.PI_CODING_AGENT_DIR;
  delete env.PI_CODING_AGENT_SESSION_DIR;
  return env;
}

function runCli(args, home) {
  const started = process.hrtime.bigint();
  const child = spawnSync(process.execPath, ['--disable-warning=ExperimentalWarning', cliEntry, ...args], {
    cwd: repoRoot,
    env: cliEnv(home),
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  });
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;
  if (child.status !== 0) {
    throw new Error(`obelisk ${args.join(' ')} failed (${child.status}): ${child.stderr || child.stdout}`);
  }
  return { stdout: child.stdout, stderr: child.stderr, elapsedMs };
}

function runNode(script, args) {
  const child = spawnSync(process.execPath, [script, ...args], { cwd: repoRoot, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 });
  if (child.status !== 0) throw new Error(`${script} failed: ${child.stderr || child.stdout}`);
  return child.stdout;
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}

function dirSizeBytes(path) {
  let total = 0;
  let entries;
  try {
    entries = readdirSync(path, { withFileTypes: true });
  } catch {
    return statSync(path).size;
  }
  for (const entry of entries) {
    const full = join(path, entry.name);
    total += entry.isDirectory() ? dirSizeBytes(full) : statSync(full).size;
  }
  return total;
}

function humanBytes(bytes) {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(2)} GiB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

function fmtMs(ms) {
  return ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${ms.toFixed(1)} ms`;
}

const options = parseArgs(process.argv.slice(2));
const workDir = mkdtempSync(join(os.tmpdir(), 'obelisk-baseline-'));
const corpusOut = join(workDir, 'corpus');
const home = join(corpusOut, 'home');
const dbPath = join(home, '.obelisk', 'obelisk.sqlite');
const startedAt = process.hrtime.bigint();

try {
  // 1. Generate the corpus.
  const genStarted = process.hrtime.bigint();
  const genSummary = JSON.parse(runNode(generateCorpus, ['--out', corpusOut, '--scale', String(options.scale), '--big-messages', String(options.bigMessages)]));
  const genMs = Number(process.hrtime.bigint() - genStarted) / 1e6;
  const missingProviders = Object.entries(genSummary.providers).filter(([, v]) => v.sessions === 0).map(([k]) => k);
  if (missingProviders.length > 0) throw new Error(`generator produced no sessions for: ${missingProviders.join(', ')}`);

  // 2. Full build on a fresh DB (single run) — the warm-up build for search.
  const build1 = runCli(['--build'], home);

  // 3. Cold start: --version, median of R runs.
  const versionRuns = [];
  for (let i = 0; i < options.runs; i++) versionRuns.push(runCli(['--version'], home).elapsedMs);

  // 4. --search against the built index, median of R runs (measured right
  //    after the build, so the pre-query incremental refresh is debounced).
  const searchRuns = [];
  let searchHits = 0;
  for (let i = 0; i < options.runs; i++) {
    const result = runCli(['--search', options.searchTerm], home);
    searchRuns.push(result.elapsedMs);
    if (i === 0) {
      const parsed = JSON.parse(result.stdout);
      searchHits = Array.isArray(parsed) ? parsed.length : -1;
    }
  }

  // 5. Force rebuild over the existing DB (single run).
  const build2 = runCli(['--build'], home);

  // 6. Corpus scale params from the canonical dump.
  const dump = JSON.parse(runNode(dumpIndex, ['--home', home]));
  const sessionsBySource = {};
  for (const session of dump.sessions) sessionsBySource[session.source] = (sessionsBySource[session.source] ?? 0) + 1;
  const messagesBySource = {};
  for (const message of dump.messages) messagesBySource[message.source] = (messagesBySource[message.source] ?? 0) + 1;

  // 7. Sizes.
  const dbBytes = statSync(dbPath).size;
  const corpusBytes = dirSizeBytes(home);
  const nodeModulesBytes = dirSizeBytes(join(repoRoot, 'node_modules'));
  const cliDistBytes = dirSizeBytes(join(repoRoot, 'packages', 'cli', 'dist'));

  const cpus = os.cpus();
  const cliVersion = JSON.parse(readFileSync(join(repoRoot, 'packages', 'cli', 'package.json'), 'utf8')).version;
  const wallMs = Number(process.hrtime.bigint() - startedAt) / 1e6;

  const lines = [];
  lines.push('# TS CLI performance baseline (Rust-migration M1.0)');
  lines.push('');
  lines.push(`Measured against the built TS CLI (\`packages/cli/dist/cli/src/obelisk.js\`, obelisk`);
  lines.push(`v${cliVersion}) on a **deterministic synthetic corpus** — no real corpus exists on this`);
  lines.push('machine, so the original plan\'s "real corpus" measurement is approximated by');
  lines.push('generated data of comparable shape (`tests/golden/generate-corpus.mjs`).');
  lines.push('');
  lines.push('## Machine context');
  lines.push('');
  lines.push(`- Node: ${process.version}`);
  lines.push(`- CPU: ${cpus[0]?.model ?? 'unknown'} (${cpus.length} cores)`);
  lines.push(`- Platform: ${os.platform()} ${os.arch()}, ${(os.totalmem() / 1024 ** 3).toFixed(1)} GiB RAM`);
  lines.push('');
  lines.push('## Corpus scale');
  lines.push('');
  lines.push(`- Generator: \`node tests/golden/generate-corpus.mjs --out <tmp> --scale ${options.scale} --big-messages ${options.bigMessages}\``);
  lines.push(`- Sessions: ${dump.sessions.length} total (${Object.entries(sessionsBySource).map(([k, v]) => `${k}: ${v}`).join(', ')})`);
  lines.push(`- Messages: ${dump.messages.length} total (${Object.entries(messagesBySource).map(([k, v]) => `${k}: ${v}`).join(', ')})`);
  lines.push(`- Tool calls: ${dump.tool_calls.length}, tool results: ${dump.tool_results.length}`);
  lines.push(`- Longest session: ${options.bigMessages} messages (claude \`claude-sess-big-*\`)`);
  lines.push('');
  lines.push('## Measurements');
  lines.push('');
  lines.push('| Metric | Value |');
  lines.push('| --- | --- |');
  lines.push(`| TS CLI cold start: \`--version\` (median of ${options.runs}) | ${fmtMs(median(versionRuns))} |`);
  lines.push(`| TS CLI cold start: \`--version\` (min … max) | ${fmtMs(Math.min(...versionRuns))} … ${fmtMs(Math.max(...versionRuns))} |`);
  lines.push(`| \`--search "${options.searchTerm}"\` (median of ${options.runs}, ${searchHits} hits at limit 20) | ${fmtMs(median(searchRuns))} |`);
  lines.push(`| \`--search "${options.searchTerm}"\` (min … max) | ${fmtMs(Math.min(...searchRuns))} … ${fmtMs(Math.max(...searchRuns))} |`);
  lines.push(`| Full build, fresh DB (single run) | ${fmtMs(build1.elapsedMs)} |`);
  lines.push(`| Force rebuild over existing DB (single run) | ${fmtMs(build2.elapsedMs)} |`);
  lines.push(`| Corpus generation (single run) | ${fmtMs(genMs)} |`);
  lines.push(`| \`obelisk.sqlite\` size after build | ${humanBytes(dbBytes)} (${dbBytes.toLocaleString('en-US')} bytes) |`);
  lines.push(`| Total corpus size on disk | ${humanBytes(corpusBytes)} (${corpusBytes.toLocaleString('en-US')} bytes) |`);
  lines.push('');
  lines.push('## TS install footprint (Rust binary-size target: < 15 MB)');
  lines.push('');
  lines.push('| Path | Size |');
  lines.push('| --- | --- |');
  lines.push(`| \`node_modules\` (repo, all workspaces) | ${humanBytes(nodeModulesBytes)} |`);
  lines.push(`| \`packages/cli/dist\` (compiled CLI itself) | ${humanBytes(cliDistBytes)} |`);
  lines.push('');
  lines.push('## Notes');
  lines.push('');
  lines.push('- `--search` timings were taken immediately after a build, so the CLI\'s');
  lines.push('  30-second build debounce suppresses the pre-query incremental refresh');
  lines.push('  (`refreshQueryIndex`); these numbers measure the hot query path (process');
  lines.push('  start + provider registry + SQLite open + FTS query). A search run more');
  lines.push('  than 30 s after the last build additionally pays one incremental');
  lines.push('  discovery pass over all provider roots.');
  lines.push('- `--build` always runs a force full re-index (`buildIndex({ force: true })`);');
  lines.push('  there is no cheap incremental path exposed by the CLI, so both build');
  lines.push('  measurements are full parses.');
  lines.push('- `node_modules` is the shared dev dependency tree of the whole monorepo');
  lines.push('  (includes the Electron app, ESLint, TypeScript). The Rust port replaces');
  lines.push('  both the Node runtime requirement and this footprint with a single');
  lines.push('  statically-linked binary (< 15 MB target).');
  lines.push(`- Total measurement wall time: ${fmtMs(wallMs)}.`);
  lines.push('');

  writeFileSync(options.out, lines.join('\n'));
  process.stdout.write(lines.join('\n'));
} finally {
  rmSync(workDir, { recursive: true, force: true });
}
