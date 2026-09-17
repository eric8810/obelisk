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

/** Scroll the wheel: `steps` clicks downward (xclick's own sign is
 * positive=up, so we negate). */
export function scrollAt(window, x, y, steps) {
  const rootX = window.x + x;
  const rootY = window.y + y;
  sh(`DISPLAY=:0 /tmp/xmove ${rootX} ${rootY} && sleep 0.1 && DISPLAY=:0 /tmp/xclick scroll ${-steps}`);
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

/** Sidebar nav row centers (window coords). The defaults below were
 * calibrated by click-probing the ported sidebar (brand 36px + Library/
 * Stats/Projects sections with 28px rows; see the M4 visual-parity rework)
 * on a 1250×749 window at scale 1.6. Any other WM policy, screen scale, or
 * window size shifts the rows, so the runner vision-calibrates them from a
 * probe screenshot once per run (calibrateNav below) — the defaults are
 * only the fallback when the vision pass cannot parse a row. */
export const NAV = {
  Sessions: { x: 120, y: 164 },
  Memory: { x: 120, y: 217 },
  Active: { x: 120, y: 270 },
  Archived: { x: 120, y: 322 },
  Activity: { x: 120, y: 457 },
  Recap: { x: 120, y: 508 },
  Settings: { x: 120, y: 735 },
};

/** First project row in the sidebar's Projects section. */
export const NAV_PROJECT = { x: 120, y: 645 };

/** Session-list search box center (window coords); re-derived by
 * calibrateNav (its position follows the header, which resizes with the
 * window). */
export const SEARCH_BOX = { x: 800, y: 61 };

/** Vision-locate `what` in a screenshot, returning window-relative pixel
 *  coordinates. Pass `crop` ({x,y,w,h}, window coords) to restrict the
 *  search: percentage answers on a small crop keep the absolute error
 *  small, while full-window shots drift (recorded in D16's history).
 *  `fallback` ({x,y}, optional) is returned when the model answer cannot
 *  be parsed; without one a parse failure throws. */
export function locateInWindow(win, shotPath, what, fallback = null, crop = null) {
  const region = crop || { x: 0, y: 0, w: win.width, h: win.height };
  let target = shotPath;
  if (crop) {
    target = `/tmp/obelisk-locate-${Date.now()}.png`;
    sh(`convert ${JSON.stringify(shotPath)} -crop ${crop.w}x${crop.h}+${crop.x}+${crop.y} +repage ${JSON.stringify(target)}`);
  }
  const answer = sh(
    `dim image read ${JSON.stringify(target)} --prompt 'Find ${what}. Give its center as X=NN% Y=NN% (percentages of THIS image). Format only, no other text.'`,
    { timeout: 300_000 },
  );
  const m = answer.match(/X\s*=\s*(\d+(?:\.\d+)?)\s*%?\s*Y\s*=\s*(\d+(?:\.\d+)?)\s*%?/i);
  if (!m) {
    if (fallback) return fallback;
    throw new Error(`vision locate failed for "${what}": ${answer.slice(0, 160)}`);
  }
  return {
    x: region.x + Math.round((Number(m[1]) / 100) * region.w),
    y: region.y + Math.round((Number(m[2]) / 100) * region.h),
  };
}

let navCalibrated = false;

/** Has calibrateNav already succeeded this run? */
export function navReady() {
  return navCalibrated;
}

/** Re-derive the sidebar nav rows and the list search box from a probe
 *  screenshot of the sessions view, once per run. Mutates the exported
 *  NAV/SEARCH_BOX objects in place so every scenario click follows the
 *  actual geometry. Rows the vision pass cannot parse keep their
 *  hardcoded default. */
export function calibrateNav(win, probeShotPath) {
  if (navCalibrated) return;
  const railW = Math.round(win.width * 0.25);
  const answer = sh(
    `dim image read ${JSON.stringify(probeShotPath)} --prompt 'This is an app window: a left sidebar rail and a main panel. Locate these SIDEBAR rows: Sessions, Memory, Active, Archived, Activity, Recap, Settings. Output one line per row, exactly: NAME X=NN% Y=NN% (percentages of THIS image). Format only.'`,
    { timeout: 300_000 },
  );
  let found = 0;
  for (const line of answer.split('\n')) {
    const nm = line.match(/(sessions|memory|active|archived|activity|recap|settings)/i);
    if (!nm) continue;
    const xs = line.match(/x\s*[=:]\s*(\d+(?:\.\d+)?)\s*%/i);
    const ys = line.match(/y\s*[=:]\s*(\d+(?:\.\d+)?)\s*%/i);
    if (!xs || !ys) continue;
    const name = nm[1][0].toUpperCase() + nm[1].slice(1).toLowerCase();
    if (!NAV[name]) continue;
    NAV[name] = {
      x: Math.round((Number(xs[1]) / 100) * win.width),
      y: Math.round((Number(ys[1]) / 100) * win.height),
    };
    found += 1;
  }
  // The search box lives in the main-panel header, right of the sidebar.
  const sb = locateInWindow(
    win,
    probeShotPath,
    'the search input box in the panel header (top right, placeholder text "Search sessions")',
    null,
    { x: railW, y: 0, w: win.width - railW, h: Math.round(win.height * 0.15) },
  );
  if (sb) {
    SEARCH_BOX.x = sb.x;
    SEARCH_BOX.y = sb.y;
  }
  navCalibrated = true;
  console.log(
    `nav calibrated: ${found}/7 rows vision-derived at ${win.width}x${win.height}` +
      (sb ? ', search box too' : '; search box kept default'),
  );
}

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
