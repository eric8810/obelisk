// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// E2E-CLI harness (plan §6.1): pure black-box — drive the real binary
// through a real TTY (tmux) or a pipe (direct), assert only on observable
// output (stdout JSON, DB rows, files, exit codes). Never call internals.
//
// Usage:
//   node tests/e2e/run-e2e.mjs --binary <path> [--compare <path>] \
//        [--filter C1,C4] [--driver tmux|direct] [--keep]
//
// `--compare` runs every scenario on both binaries and diffs the
// normalized stdout JSON (see SCENARIOS.md for normalization rules).

import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DatabaseSync } from 'node:sqlite';

export const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const GENERATOR = join(repoRoot, 'tests', 'golden', 'generate-corpus.mjs');
const DUMPER = join(repoRoot, 'tests', 'golden', 'dump-index.mjs');

// ---- drivers ----

// tmux: run the binary on a REAL TTY via `tmux new-session <shellCommand>`
// (the command is executed, not typed, so nothing is echoed into the pane).
// stdout stays on the TTY; stderr is redirected to a file so warnings never
// interleave with JSON output under assertion.
async function driveTmux(name, command, args, { home, cwd, timeoutMs = 60000 }) {
  const session = `obelisk-e2e-${name}-${Date.now()}`;
  const sentinel = `__E2E_DONE_${Math.random().toString(36).slice(2, 10)}__`;
  const errFile = join(tmpdir(), `obelisk-e2e-${name}-${Date.now()}.err`);
  const shQuote = (s) => `'${String(s).replace(/'/g, `'\\''`)}'`;
  const shellCommand = [
    `HOME=${shQuote(home)}`,
    `USERPROFILE=${shQuote(home)}`,
    `DSH_HOME=`,
    `PATH=${shQuote(process.env.PATH)}`,
    `cd ${shQuote(cwd)}`,
    [shQuote(command), ...args.map(shQuote), `2>${shQuote(errFile)}`].join(' '),
    `echo ${sentinel}:$?`,
    `sleep 900`,
  ].join('; ');
  // The trailing sleep keeps the session (and pane history) alive after the
  // binary exits, so the sentinel can always be captured; cleanup kills it.
  const r = spawnSync('tmux', ['new-session', '-d', '-s', session, '-x', '220', '-y', '50', shellCommand], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`tmux new-session failed: ${r.stderr}`);
  try {
    const started = Date.now();
    while (Date.now() - started < timeoutMs) {
      const pane = spawnSync('tmux', ['capture-pane', '-p', '-J', '-t', session, '-S', '-5000'], { encoding: 'utf8' }).stdout ?? '';
      const m = pane.match(new RegExp(`${regexEscape(sentinel)}:(\\d+)`));
      if (m) {
        const exitCode = Number(m[1]);
        const before = pane.slice(0, pane.indexOf(`${sentinel}:`));
        let stderr = '';
        try { stderr = readFileSync(errFile, 'utf8'); } catch { /* no stderr */ }
        return { stdout: before.replace(/\n+$/, ''), stderr, exitCode, pane };
      }
      await sleep(100);
    }
    throw new Error(`tmux scenario ${name} timed out after ${timeoutMs}ms`);
  } finally {
    spawnSync('tmux', ['kill-session', '-t', session], { encoding: 'utf8' });
    try { rmSync(errFile, { force: true }); } catch { /* best effort */ }
  }
}

function regexEscape(value) {
  return value.replace(/[^A-Za-z0-9_]/g, (ch) => `\\${ch}`);
}

async function driveDirect(name, command, args, { home, cwd, timeoutMs = 60000 }) {
  const started = Date.now();
  const env = { ...process.env, HOME: home, USERPROFILE: home };
  delete env.DSH_HOME;
  const r = spawnSync(command, args, { encoding: 'utf8', cwd, env, timeout: timeoutMs });
  const elapsed = Date.now() - started;
  return {
    stdout: r.stdout ?? '',
    stderr: r.stderr ?? '',
    exitCode: r.status ?? (r.error ? 1 : 0),
    elapsed,
    failed: r.error !== undefined,
  };
}

function sleep(ms) { return new Promise(r => setTimeout(r, ms)); }

// ---- fixtures ----

const pendingCleanups = [];

export async function makeCorpusHome(_scenario, { scale = 1 } = {}) {
  const out = mkdtempSync(join(tmpdir(), 'obelisk-e2e-corpus-'));
  const home = join(out, 'home');
  pendingCleanups.push(() => {
    try { rmSync(out, { recursive: true, force: true }); } catch { /* keep evidence on failure */ }
  });
  const r = spawnSync(process.execPath, [GENERATOR, '--out', out, '--scale', String(scale)], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`corpus generation failed: ${r.stderr}`);
  return home;
}

export function runCleanups() {
  while (pendingCleanups.length > 0) {
    const cleanup = pendingCleanups.pop();
    try { cleanup(); } catch { /* best effort */ }
  }
}

export function openDb(home) {
  return new DatabaseSync(join(home, '.obelisk', 'obelisk.sqlite'), { readOnly: true });
}

export function dumpIndex(home, out) {
  const r = spawnSync(process.execPath, [DUMPER, '--home', home, ...(out ? ['--out', out] : [])], { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`dump failed: ${r.stderr}`);
  return out ? readFileSync(out, 'utf8') : r.stdout;
}

export function isTsBinary(binary) {
  return binary.endsWith('.js') || binary.endsWith('.mjs');
}

// Drive whichever binary: TS needs node to run it; paths are resolved
// absolute so temp cwd's cannot skew module resolution.
export async function drive(binary, name, args, opts) {
  const absolute = resolve(repoRoot, binary);
  if (isTsBinary(absolute)) {
    return driveDirectOrTmux(name, process.execPath, ['--disable-warning=ExperimentalWarning', absolute, ...args], opts);
  }
  return driveDirectOrTmux(name, absolute, args, opts);
}

async function driveDirectOrTmux(name, command, args, opts) {
  if (opts.driver === 'tmux') return driveTmux(name, command, args, opts);
  return driveDirect(name, command, args, opts);
}

// ---- normalization for dual-run comparison ----

export function normalizeOutput(text, home) {
  let normalized = text;
  if (home) normalized = normalized.split(home).join('<HOME>');
  // Stack traces differ per implementation.
  normalized = normalized.replace(/"stack":\s*(null|"[^"]*")/g, '"stack":null');
  return normalized;
}

export function parseJsonOutput(stdout) {
  const trimmed = stdout.trim();
  if (!trimmed) return null;
  try { return JSON.parse(trimmed); } catch { return { __unparsable: trimmed }; }
}
