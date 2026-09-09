// Manual play-test: the exact user gesture that crashed v0.4.0 on macOS.
//
// Scenario: open a session, then page down repeatedly to the bottom WITHOUT
// ever pressing End (End arms follow-tail explicitly, which short-circuits
// the buggy handler; the crash path is scrolling to the bottom while NOT
// following). The E2E suite never exercised this sequence — D3 used End —
// which is why the RefCell re-borrow panic shipped in v0.4.0.
//
// Usage: node playtest-scroll-bottom.mjs <binary> <label>

import { execSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import * as d from './lib/driver.mjs';

const [binary, label] = process.argv.slice(2);
if (!binary || !label) {
  console.error('usage: node playtest-scroll-bottom.mjs <binary> <label>');
  process.exit(2);
}

const repo = join(import.meta.dirname, '..', '..');
const home = mkdtempSync(join(tmpdir(), `obelisk-play-${label}-`));
const evidenceRoot = join(repo, 'tests/e2e-desktop/evidence');
const runDir = join(evidenceRoot, `play-${label}-${new Date().toISOString().replace(/[:.]/g, '-')}`);
import { mkdirSync } from 'node:fs';
mkdirSync(runDir, { recursive: true });
try {
  // Same fixture as the E2E suite: deepseek sessions indexed into a fresh HOME.
  d.buildFixtureDb(home, repo, join(repo, 'target/release/obelisk'));

  const { pid, win } = d.launchApp({ binary, logPath: join(runDir, 'app.log'), env: { HOME: home } });
  console.log(`[${label}] launched pid=${pid}`);

  // 1. Open the first session from the list.
  d.clickAt(win, 700, 175);
  d.sleep(1500);
  const top = join(runDir, '1-timeline-top.png');
  d.captureWindow(win.window_id, top);
  console.log(`[${label}] opened session; shot=${top}`);

  // 2. The user gesture: page down all the way to the bottom, never End.
  //    A real trackpad flick lands here the same way.
  for (let i = 0; i < 8; i++) {
    d.pressKey('Page_Down', { focus: true, windowId: win.window_id });
    d.sleep(350);
    if (!d.appAlive(pid)) {
      console.log(`[${label}] CRASHED after ${i + 1} Page_Down presses`);
      process.exit(1);
    }
  }

  // 3. Alive at the bottom? Follow-tail should have auto-armed near the
  //    bottom (deferred), so one more page keeps us pinned there.
  d.sleep(800);
  const bottom = join(runDir, '2-timeline-bottom.png');
  d.captureWindow(win.window_id, bottom);
  d.pressKey('Page_Down', { focus: true, windowId: win.window_id });
  d.sleep(800);
  const pinned = join(runDir, '3-after-extra-page.png');
  d.captureWindow(win.window_id, pinned);
  console.log(`[${label}] survived 8 page-downs; bottom=${bottom} pinned=${pinned}`);

  // 4. Wheel scroll at the bottom too (trackpad flick equivalent on X11).
  d.scrollAt(win, 800, 400, 5);
  d.sleep(800);
  if (!d.appAlive(pid)) {
    console.log(`[${label}] CRASHED on wheel scroll at bottom`);
    process.exit(1);
  }
  const wheeled = join(runDir, '4-after-wheel.png');
  d.captureWindow(win.window_id, wheeled);
  console.log(`[${label}] survived wheel scroll; shot=${wheeled}`);

  d.quitApp(pid);
  console.log(`[${label}] PASS — no crash, evidence in ${runDir}`);
  process.exit(0);
} finally {
  rmSync(home, { recursive: true, force: true });
}
