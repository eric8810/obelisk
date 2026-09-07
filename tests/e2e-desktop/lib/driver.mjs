// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Desktop E2E driver layer (M2.6). Wraps:
//  - cua-driver (primary): background input via XSendEvent, window discovery,
//    screenshots. `call <tool> '<json>'` — JSON args piped via stdin `-`.
//  - XTest fallback (/tmp tools): xclick/xmove + setxfocus, for cases the
//    daemon rejects (e.g. click without pid).
//  - window capture: ImageMagick import -window <id>.
//  - vision assertions: `dim image read <path> --prompt '<q>'` (local model),
//    keyword-based; deterministic assertions live in the scenarios (sqlite3,
//    files, pgrep).
//
// Layout grid: the app renders deterministically at scale 1.6 on this
// machine (1250x749 window). Rows are addressed by window coordinates; the
// sidebar nav rows (Sessions/Memory/Activity/Recap/Settings) sit at
// y ≈ 121/191/261/331/401 with x≈120. The runner re-derives these from a
// probe screenshot when the layout shifts.

import { execFileSync, execSync, spawn } from 'node:child_process';
import { existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');

export const CUA = `${process.env.HOME}/.local/bin/cua-driver`;

export function sh(command, options = {}) {
  return execSync(command, { encoding: 'utf8', timeout: 30_000, ...options });
}

export function shQuiet(command, options = {}) {
  try {
    return { ok: true, stdout: execSync(command, { encoding: 'utf8', timeout: 30_000, ...options }) };
  } catch (error) {
    return { ok: false, stdout: String(error.stdout || ''), stderr: String(error.stderr || '') };
  }
}

export function sqlite(db, sql) {
  return sh(`sqlite3 ${JSON.stringify(db)} ${JSON.stringify(sql)}`).trim();
}

/** The Obelisk X11 window (id + bounds), or null. */
export function findObeliskWindow() {
  const listing = sh(`${CUA} call list_windows`);
  const parsed = JSON.parse(listing);
  return parsed.windows.find((w) => w.title === 'Obelisk') || null;
}

/** Capture the app window to `path` (PNG). */
export function captureWindow(windowId, path) {
  execFileSync('import', ['-window', `0x${windowId.toString(16)}`, path], {
    env: { ...process.env, DISPLAY: ':0' },
    timeout: 20_000,
  });
  return path;
}

/** Click at window-relative coordinates (root space conversion inside). */
export function clickAt(window, x, y) {
  // cua-driver rejects pixel clicks without pid on some versions; the XTest
  // tool is the stable fallback and delivers at the same coordinates.
  const rootX = window.x + x;
  const rootY = window.y + y;
  sh(`DISPLAY=:0 /tmp/xclick ${rootX} ${rootY}`);
}

/** Press a key by X keysym name (Page_Down, End, Escape, r, slash, …). */
export function pressKey(keysym, { focus = true, windowId = null } = {}) {
  if (focus && windowId != null) {
    sh(`DISPLAY=:0 /tmp/setxfocus 0x${windowId.toString(16)}`);
  }
  sh(`DISPLAY=:0 /tmp/xclick key ${keysym}`);
}

/** Type a text run. XSendEvent keysym injection; Unicode via cua-driver. */
export function typeText(text) {
  const payload = JSON.stringify({ text });
  execFileSync(CUA, ['call', 'type_text', '-'], { input: payload, timeout: 30_000 });
}

/** Vision assertion: ask the local model a yes/no-ish question about a
 * screenshot; pass when every keyword appears in the answer. */
export async function visionExpects(path, prompt, keywords) {
  const answer = sh(
    `dim image read ${JSON.stringify(path)} --prompt ${JSON.stringify(prompt)}`,
    { timeout: 120_000 },
  );
  const missing = keywords.filter((keyword) => !answer.toLowerCase().includes(keyword.toLowerCase()));
  if (missing.length) {
    throw new Error(
      `vision assertion failed: missing ${JSON.stringify(missing)} in answer:\n${answer.slice(0, 800)}`,
    );
  }
  return answer;
}

/** Launch the app with a display; resolves once the X11 window appears. */
export function launchApp({ binary, logPath, env = {} }) {
  const log = spawn('/bin/sh', ['-c', `exec ${JSON.stringify(binary)} > ${JSON.stringify(logPath)} 2>&1`], {
    detached: true,
    env: { ...process.env, DISPLAY: ':0', ...env },
    stdio: 'ignore',
  });
  log.unref();
  const deadline = Date.now() + 15_000;
  let pid = null;
  let win = null;
  while (Date.now() < deadline) {
    win = findObeliskWindow();
    if (win) {
      pid = win.pid;
      break;
    }
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 300);
  }
  if (!win) throw new Error('app window did not appear within 15s');
  // The X11 window exists before GPUI's first frame settles; give the
  // renderer a moment so evidence shots capture the real first paint.
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 1200);
  return { pid, win, logPath };
}

export function appAlive(pid) {
  const result = shQuiet(`pgrep -f 'obelisk-ap[p]$'`);
  return result.ok && result.stdout.split('\n').map(Number).includes(pid);
}

/** Compile the WM_DELETE_WINDOW tool once per machine (gcc present). */
function ensureXclose() {
  const bin = '/tmp/obelisk-e2e-xclose';
  if (!existsSync(bin)) {
    execFileSync('gcc', ['-O2', '-o', bin, join(repo, 'tests/e2e-desktop/bin/xclose.c'), '-lX11'], {
      env: { ...process.env, DISPLAY: ':0' },
      timeout: 60_000,
    });
  }
  return bin;
}

/** Close a window through the ICCCM delete protocol (titlebar-X semantics). */
export function closeWindow(windowId) {
  const bin = ensureXclose();
  execFileSync(bin, [`0x${windowId.toString(16)}`], {
    env: { ...process.env, DISPLAY: ':0' },
    timeout: 20_000,
  });
}

export function quitApp(pid) {
  shQuiet(`kill ${pid}`);
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline && appAlive(pid)) {
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 200);
  }
}

export function sleep(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/** Sidebar nav row centers (window coords), derived from the standard
 * layout; re-validated per run by the runner's probe. */
export const NAV = {
  Sessions: { x: 120, y: 121 },
  Memory: { x: 120, y: 191 },
  Activity: { x: 120, y: 261 },
  Recap: { x: 120, y: 331 },
  Settings: { x: 120, y: 401 },
};

/** Session-list search box center (window coords). */
export const SEARCH_BOX = { x: 800, y: 61 };

export class Evidence {
  constructor(runDir) {
    this.runDir = runDir;
    mkdirSync(runDir, { recursive: true });
  }

  shot(ctx, name) {
    // `ctx` is the scenario context object carrying `name`; the scenario
    // id prefixes the filename (this used to stringify to
    // "[object Object]" and once to "undefined").
    const scenario = typeof ctx === 'string' ? ctx : ctx.name;
    const path = join(this.runDir, `${scenario}-${name}.png`);
    captureWindow(ctx.win.window_id, path);
    return path;
  }

  text(name, content) {
    const path = join(this.runDir, `${name}.txt`);
    writeFileSync(path, content);
    return path;
  }
}

export function freshEvidenceDir(root) {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const dir = join(root, `run-${stamp}`);
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  return dir;
}

export function dbPath(home) {
  return join(home, '.obelisk', 'obelisk.sqlite');
}

export function buildFixtureDb(home, cwd, binary) {
  const settings = join(home, '.obelisk');
  mkdirSync(settings, { recursive: true });
  writeFileSync(
    join(settings, 'settings.json'),
    JSON.stringify({ providerRoots: { deepseek: join(cwd, 'tests/fixtures/deepseek/sessions') } }, null, 2),
  );
  sh(`${JSON.stringify(binary)} --build`, { env: { ...process.env, HOME: home } });
}
