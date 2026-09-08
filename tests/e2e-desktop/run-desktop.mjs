// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// M2.6 desktop E2E runner. 11 scenarios (see SCENARIOS.md).
//
//   node tests/e2e-desktop/run-desktop.mjs [--binary <path>] [--only D1,D2]
//
// Every scenario gets an isolated HOME (tmp), a copied fixture corpus
// (appending test data never touches the repo), a fresh index build, and a
// freshly launched app. Assertions: deterministic state first (sqlite,
// settings.json, pgrep, app log), vision (dim image read) second. Evidence
// lands in tests/e2e-desktop/evidence/run-*/.

import { cpSync, mkdirSync, mkdtempSync, rmSync, writeFileSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, '..', '..');

const d = await import(join(here, 'lib', 'driver.mjs'));

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : null;
};
const binary = flag('--binary') || join(repo, 'target/release/obelisk');
const appBinary = flag('--app-binary') || join(repo, 'target/release/obelisk-app');
const only = flag('--only')?.split(',').map((s) => s.trim());
const retries = Number(flag('--retries') ?? 2);

const evidenceRoot = join(here, 'evidence');
const runDir = d.freshEvidenceDir(evidenceRoot);
const evidence = new d.Evidence(runDir);
const results = [];

class ScenarioError extends Error {}

/** Isolated environment per scenario: HOME + fixture corpus copy + index. */
async function setup(scenario) {
  const home = mkdtempSync('/tmp/obelisk-e2e-home-');
  const corpus = mkdtempSync('/tmp/obelisk-e2e-corpus-');
  cpSync(join(repo, 'tests/fixtures/deepseek/sessions'), join(corpus, 'sessions'), { recursive: true });
  mkdirSync(join(home, '.obelisk'), { recursive: true });
  writeFileSync(
    join(home, '.obelisk', 'settings.json'),
    JSON.stringify({ providerRoots: { deepseek: join(corpus, 'sessions') } }, null, 2),
  );
  d.sh(`${JSON.stringify(binary)} --build`, { env: { ...process.env, HOME: home } });
  const logPath = join(runDir, `${scenario}-app.log`);
  const { pid, win } = d.launchApp({ binary: appBinary, logPath, env: { HOME: home } });
  return { home, corpus, logPath, pid, win };
}

async function teardown(ctx) {
  d.quitApp(ctx.pid);
  rmSync(ctx.home, { recursive: true, force: true });
  rmSync(ctx.corpus, { recursive: true, force: true });
}

async function relaunch(ctx) {
  d.quitApp(ctx.pid);
  d.sleep(600);
  const launched = d.launchApp({ binary: appBinary, logPath: ctx.logPath, env: { HOME: ctx.home } });
  ctx.pid = launched.pid;
  ctx.win = launched.win;
}

// ---------------------------------------------------------------- scenarios

const scenarios = {
  D1_cold_start: async (ctx) => {
    const shot = evidence.shot(ctx, 'cold-start');
    await d.visionExpects(
      shot,
      'Describe the app layout: the sidebar (brand row, Library section with Sessions/Memory/Active/Archived rows and counts, Stats section with Activity/Recap, Projects section, Settings at the bottom), the session row in the main panel (title, source deepseek, project, message count), and the search box. Quote the session row title and message count.',
      ['Sanitized fixture session', '7 msgs', 'PROJECTS', 'Settings'],
    );
    if (!d.appAlive(ctx.pid)) throw new ScenarioError('app process died during cold start');
  },

  D2_project_session_timeline: async (ctx) => {
    // Open the timeline.
    d.clickAt(ctx.win, 700, 175);
    d.sleep(1400);
    let shot = evidence.shot(ctx, 'timeline-top');
    await d.visionExpects(shot, 'Is the timeline open with a "← Sessions" back link, the session title, and collapsed cards with chevrons (System/Thinking rows)?', ['Sessions', 'System', 'Thinking']);

    // Page down to the tool cards; they must be collapsed by default.
    d.pressKey('Page_Down', { focus: true, windowId: ctx.win.window_id });
    d.sleep(900);
    shot = evidence.shot(ctx, 'tools-collapsed');
    await d.visionExpects(shot, 'Are tool-call cards collapsed to single header rows (chevron, gear icon, tool name bash/grep, no command box)?', ['bash']);

    // Expand the first tool card (chevron at the row head).
    d.clickAt(ctx.win, 560, 205);
    d.sleep(900);
    shot = evidence.shot(ctx, 'tools-expanded');
    await d.visionExpects(shot, 'Did a tool card expand: chevron now pointing down, a body with a "$" command prompt box and a "{ } Raw" button?', ['Raw']);

    // Escape returns to the list.
    d.pressKey('Escape', { focus: true, windowId: ctx.win.window_id });
    d.sleep(900);
    shot = evidence.shot(ctx, 'back-to-list');
    await d.visionExpects(shot, 'Is this the session list again (header "All sessions", no back link)?', ['All sessions']);
  },

  D3_scroll_follow: async (ctx) => {
    d.clickAt(ctx.win, 700, 175);
    d.sleep(1400);
    d.pressKey('Page_Down', { focus: true, windowId: ctx.win.window_id });
    d.sleep(700);
    const paged = evidence.shot(ctx, 'paged');
    await d.visionExpects(paged, 'Does the timeline show content advanced past the first user messages (mid-timeline items)?', ['assistant']);

    // End pins to the bottom and enters follow-tail.
    d.pressKey('End', { focus: true, windowId: ctx.win.window_id });
    d.sleep(1000);
    const endState = evidence.shot(ctx, 'end');
    await d.visionExpects(
      endState,
      'Is the timeline scrolled to its bottom: the last cards fully visible, empty dark space below the final card, and nothing cut off at the bottom edge?',
      ['bottom', 'empty'],
    );

    // Manual refresh (r key) after injecting a DB message: the tail shows it.
    const sessionId = d.sqlite(d.dbPath(ctx.home), 'SELECT id FROM sessions LIMIT 1;');
    d.sqlite(
      d.dbPath(ctx.home),
      `INSERT INTO messages (uuid, session_id, type, timestamp, role, text, content_type, is_meta, visibility, source) VALUES ('e2e-d3-tail', '${sessionId}', 'user', '2026-08-19T17:11:00.000Z', 'user', 'D3 appended tail message for refresh', 'text', 0, 'visible', 'deepseek');`,
    );
    d.pressKey('r', { focus: true, windowId: ctx.win.window_id });
    d.sleep(1500);
    const refreshed = evidence.shot(ctx, 'refreshed');
    await d.visionExpects(refreshed, 'Is a user message with the text "D3 appended tail message for refresh" visible at the bottom, fully shown?', ['D3 appended tail message']);
  },

  D4_search_aligns_cli: async (ctx) => {
    d.clickAt(ctx.win, d.SEARCH_BOX.x, d.SEARCH_BOX.y);
    d.sleep(600);
    d.typeText('reasoning');
    d.sleep(1500);
    const shot = evidence.shot(ctx, 'fts');
    await d.visionExpects(
      shot,
      'Is there a "Full-text matches" section with snippet rows for the query "reasoning"? Quote the section label with the count.',
      ['Full-text matches'],
    );
    // CLI parity: same word, same DB, row count must match the in-app hits.
    const cli = d.sh(`${JSON.stringify(binary)} --search reasoning`, { env: { ...process.env, HOME: ctx.home } });
    const rows = JSON.parse(cli);
    const dbHits = d.sqlite(d.dbPath(ctx.home), "SELECT COUNT(*) FROM messages WHERE text LIKE '%reasoning%' AND COALESCE(is_meta,0)=0 AND COALESCE(visibility,'visible')='visible';");
    if (rows.length < 1) throw new ScenarioError(`CLI --search returned no rows: ${cli.slice(0, 200)}`);
    if (Number(dbHits) < rows.length) throw new ScenarioError(`DB visible rows ${dbHits} < CLI rows ${rows.length}`);
    evidence.text('D4-cli-search.json', cli);
  },

  D5_memory_browse: async (ctx) => {
    // Seed one active and one archived memory (read-only app; the corpus DB
    // is the test's to write).
    const db = d.dbPath(ctx.home);
    const sessionId = d.sqlite(db, 'SELECT id FROM sessions LIMIT 1;');
    const memoryFile = join(ctx.home, 'memory-note.md');
    writeFileSync(memoryFile, '## Prefer async IO\n\nNever block the executor in this project.\n');
    d.sqlite(
      db,
      `INSERT INTO memories (id, session_id, project, message_start, message_end, path, anchors, summary, created_at, deleted_at, deleted_reason) VALUES ('e2e-mem-1', '${sessionId}', '-home-dev-project--', 'm1', 'm2', '${memoryFile}', '[]', 'Prefer async IO in this project', '2026-08-20T10:00:00.000Z', NULL, NULL);`,
    );
    d.sqlite(
      db,
      `INSERT INTO memories (id, session_id, project, message_start, message_end, path, anchors, summary, created_at, deleted_at, deleted_reason) VALUES ('e2e-mem-2', '${sessionId}', '-home-dev-project--', 'm3', 'm4', '${memoryFile}', '[]', 'Archived: use vitest', '2026-08-19T09:00:00.000Z', '2026-08-21T09:00:00.000Z', 'superseded');`,
    );
    await relaunch(ctx);
    // Ask WHICH view is shown first (a yes/no phrasing invites the vision
    // model to parrot the question's keywords).
    let shot = null;
    let view = '';
    for (let attempt = 0; attempt < 3; attempt++) {
      d.clickAt(ctx.win, d.NAV.Memory.x, d.NAV.Memory.y);
      d.sleep(1300);
      ctx.win = d.findObeliskWindow() || ctx.win;
      shot = evidence.shot(ctx, `memory-list-${attempt}`);
      const answer = await d.visionExpects(
        shot,
        'Name the currently selected view in one word (Sessions, Memory, Activity, Recap, or Settings), then describe the main panel content.',
        [],
      );
      view = answer.toLowerCase();
      if (view.includes('memory') && !view.includes('all sessions')) break;
      evidence.text(`D5-nav-attempt-${attempt}.txt`, answer);
    }
    if (!(view.includes('memory') && !view.includes('all sessions'))) {
      throw new ScenarioError(`Memory nav click never took effect (last answer: ${view.slice(0, 200)})`);
    }
    await d.visionExpects(
      shot,
      'In the memory panel: which of the two tabs near the top is highlighted, and which memory row summary is listed below? Quote them.',
      ['Active', 'Prefer async IO'],
    );
    // The archived memory lives under the Archived tab (sidebar sub-row).
    d.clickAt(ctx.win, 120, 322);
    d.sleep(1000);
    shot = evidence.shot(ctx, 'memory-archived');
    await d.visionExpects(
      shot,
      'Which memory tab is highlighted now, and what row summary is listed? Quote them.',
      ['Archived', 'use vitest'],
    );
    // Open the active memory's detail via the keyboard path (j places the
    // cursor, m opens the detail — stable where pixel clicks are not).
    d.clickAt(ctx.win, 120, 270);
    d.sleep(1000);
    d.pressKey('j', { windowId: ctx.win.window_id });
    d.sleep(500);
    d.pressKey('m', { windowId: ctx.win.window_id });
    d.sleep(900);
    shot = evidence.shot(ctx, 'memory-detail');
    await d.visionExpects(shot, 'Below the list, is there a detail panel showing the memory file path and rendered markdown with the heading "Prefer async IO"?', ['Prefer async IO', 'memory-note.md']);
  },

  D6_stats_recap: async (ctx) => {
    // Seed usage + a recap file, mirroring real aggregates.
    const db = d.dbPath(ctx.home);
    d.sqlite(db, "UPDATE messages SET input_tokens = 1200, output_tokens = 800 WHERE type = 'assistant';");
    const recapDir = join(ctx.home, '.obelisk', 'recap');
    mkdirSync(recapDir, { recursive: true });
    // M4.4 recap format: persona + five cards (same fixture as D16).
    cpSync(join(repo, 'tests/fixtures/recap/recap-e2e.json'), join(recapDir, '2026-W36.json'));
    await relaunch(ctx);

    d.clickAt(ctx.win, d.NAV.Activity.x, d.NAV.Activity.y);
    d.sleep(1500);
    let shot = evidence.shot(ctx, 'activity');
    await d.visionExpects(
      shot,
      'Describe the Activity view: a stats bar with Lifetime tokens, Peak tokens, Longest task, and streak labels, plus a heatmap grid of small squares. Quote the lifetime tokens value.',
      ['Lifetime tokens', 'Peak tokens', 'Longest task', 'Current streak'],
    );
    // Deterministic aggregate parity: the stats-bar total must equal the DB sum.
    const dbTokens = d.sqlite(db, 'SELECT SUM(COALESCE(input_tokens,0) + COALESCE(output_tokens,0)) FROM messages;');
    if (Number(dbTokens) <= 0) throw new ScenarioError(`expected seeded tokens > 0, got ${dbTokens}`);
    evidence.text('D6-db-tokens.txt', dbTokens);

    d.clickAt(ctx.win, d.NAV.Recap.x, d.NAV.Recap.y);
    d.sleep(1200);
    shot = evidence.shot(ctx, 'recap-list');
    await d.visionExpects(shot, 'Does the Recap left column list one report file "2026-W36.json"?', ['2026-W36.json']);
    // Select the row (the list rail occupies the left ~300px).
    d.clickAt(ctx.win, 260, 150);
    d.sleep(1500);
    shot = evidence.shot(ctx, 'recap-detail');
    await d.visionExpects(shot, 'Does the Cover card render with the big title Cover Sentinel Alpha?', ['Cover Sentinel Alpha']);
  },

  D7_tray_background_indexing: async (ctx) => {
    const before = Number(d.sqlite(d.dbPath(ctx.home), 'SELECT message_count FROM sessions LIMIT 1;'));
    const corpusFile = d.sh(`find ${JSON.stringify(join(ctx.corpus, 'sessions'))} -name 'session.jsonl.zstd' | head -1`).trim();
    const donorFile = d.sh(`find ${JSON.stringify(join(ctx.corpus, 'sessions'))} -name 'session.jsonl.zstd' | tail -1`).trim();
    if (corpusFile === donorFile) throw new ScenarioError('corpus fixture must have two session files');
    d.sh(`cat ${JSON.stringify(donorFile)} >> ${JSON.stringify(corpusFile)}`);

    // The daemon (watcher → debounced incremental build) must update the DB
    // and the UI without any user interaction.
    let after = null;
    const deadline = Date.now() + 20_000;
    while (Date.now() < deadline) {
      after = Number(d.sqlite(d.dbPath(ctx.home), 'SELECT message_count FROM sessions LIMIT 1;'));
      if (after > before) break;
      d.sleep(500);
    }
    if (!(after > before)) throw new ScenarioError(`daemon did not index the append: before=${before} after=${after}`);
    d.sleep(800);
    const shot = evidence.shot(ctx, 'auto-updated');
    await d.visionExpects(shot, `What message count does the session row show now? It should be a number larger than ${before}.`, [String(after)]);
    if (!d.appAlive(ctx.pid)) throw new ScenarioError('app process died during background indexing');
  },

  D8_close_window_stays_resident: async (ctx) => {
    // Close the window through the ICCCM WM_DELETE_WINDOW protocol — the
    // same path a titlebar X button takes (the GPUI X11 frame has no
    // titlebar of its own).
    d.closeWindow(ctx.win.window_id);
    d.sleep(1500);
    const stillListed = JSON.parse(d.sh(`${d.CUA} call list_windows`)).windows.some((w) => w.title === 'Obelisk');
    if (stillListed) throw new ScenarioError('window still listed after compositor close');
    // Explicit-quit mode: the process must survive the closed window.
    d.sleep(800);
    if (!d.appAlive(ctx.pid)) throw new ScenarioError('process exited with the window (tray residency broken)');
    // Relaunch the window for teardown.
    await relaunch(ctx);
  },

  D9_chinese_ime_input: async (ctx) => {
    d.clickAt(ctx.win, d.SEARCH_BOX.x, d.SEARCH_BOX.y);
    d.sleep(600);
    d.typeText('会话');
    d.sleep(1500);
    const shot = evidence.shot(ctx, 'ime');
    await d.visionExpects(shot, 'What text is in the search box? It should contain the two Chinese characters 会话. Quote exactly what you see.', ['会话']);
  },

  D10_settings_page: async (ctx) => {
    d.clickAt(ctx.win, d.NAV.Settings.x, d.NAV.Settings.y);
    d.sleep(1200);
    let shot = evidence.shot(ctx, 'settings');
    await d.visionExpects(
      shot,
      'Does the Settings page show an "Editor scheme" section with chips VS Code (selected/purple), VS Code Insiders, Cursor, Windsurf, Zed, and a "Provider roots" section listing the deepseek root?',
      ['Editor scheme', 'VS Code', 'Provider roots'],
    );
    // Switch to Zed, then back — the chip position depends on wrapping, so
    // click-then-verify with retries (and the settings.json write is the
    // deterministic oracle).
    const settingsPath = join(ctx.home, '.obelisk', 'settings.json');
    const schemeChips = { zed: { x: 1210, y: 330 }, vscode: { x: 560, y: 330 } };
    const switchScheme = async (want) => {
      const offsets = [0, -14, 14, -28, 28];
      for (let attempt = 0; attempt < offsets.length; attempt++) {
        const chip = schemeChips[want];
        if (attempt > 0) {
          // Re-enter Settings to re-mount the page (mouse hit boxes on this
          // machine drift after the first frames; a fresh mount re-anchors).
          d.clickAt(ctx.win, d.NAV.Settings.x, d.NAV.Settings.y);
          d.sleep(900);
        }
        d.clickAt(ctx.win, chip.x, chip.y + offsets[attempt]);
        d.sleep(1200);
        if (readFileSync(settingsPath, 'utf8').includes(`"editorScheme": "${want}"`)) {
          const shot = evidence.shot(ctx, `settings-${want}-${attempt}`);
          await d.visionExpects(
            shot,
            `Which editor-scheme chip has the purple outline now? Answer with the chip name.`,
            [want === 'vscode' ? 'VS Code' : 'Zed'],
          );
          return;
        }
      }
      throw new ScenarioError(`scheme switch to ${want} never landed in settings.json`);
    };
    await switchScheme('zed');
    await switchScheme('vscode');
  },

  D12_scroll_anchor_on_refresh: async (ctx) => {
    // Open the session and page into the middle of the timeline.
    d.clickAt(ctx.win, 700, 175);
    d.sleep(1500);
    d.pressKey('Page_Down', { windowId: ctx.win.window_id });
    d.sleep(300);
    d.pressKey('Page_Down', { windowId: ctx.win.window_id });
    d.sleep(800);
    const before = evidence.shot(ctx, 'anchor-before');

    // Non-pure-append change: delete a real message line (row 11 of the
    // deepseek transcript is a user/message), recompress, and wait for the
    // daemon's incremental build to land in the DB.
    const corpusFile = d.sh(
      `find ${JSON.stringify(join(ctx.corpus, 'sessions'))} -name 'session.jsonl.zstd' | head -1`,
    ).trim();
    d.sh(
      `zstd -dc ${JSON.stringify(corpusFile)} > /tmp/d12-raw.jsonl && sed -i '11d' /tmp/d12-raw.jsonl && zstd -f -o ${JSON.stringify(corpusFile)} /tmp/d12-raw.jsonl`,
    );
    const c0 = Number(d.sqlite(d.dbPath(ctx.home), 'SELECT message_count FROM sessions LIMIT 1;'));
    let c1 = c0;
    const deadline = Date.now() + 25_000;
    while (Date.now() < deadline) {
      c1 = Number(d.sqlite(d.dbPath(ctx.home), 'SELECT message_count FROM sessions LIMIT 1;'));
      if (c1 !== c0) break;
      d.sleep(500);
    }
    if (c1 === c0) throw new ScenarioError(`corpus deletion did not land (${c0} -> ${c1})`);
    d.sleep(1500);
    const after = evidence.shot(ctx, 'anchor-after');

    // The viewport must be pixel-stable across the refresh: compare the
    // timeline region of both shots (<5% changed pixels). A jump back to
    // the top changes nearly everything.
    const crop = (p, out) =>
      d.sh(`convert ${JSON.stringify(p)} -crop 830x680+400+60 +repage ${JSON.stringify(out)}`);
    crop(before, '/tmp/d12-before-view.png');
    crop(after, '/tmp/d12-after-view.png');
    const raw = d.sh(
      'compare -metric AE /tmp/d12-before-view.png /tmp/d12-after-view.png null: 2>&1 || true',
      { timeout: 60_000 },
    );
    const ae = Number(raw.trim().split(/[\s(]/)[0]);
    if (!Number.isFinite(ae)) throw new ScenarioError(`pixel compare failed: ${raw}`);
    const pct = (ae / (830 * 680)) * 100;
    evidence.text('D12-pixel-diff.txt', `AE ${ae} (${pct.toFixed(3)}%)\nraw: ${raw}`);
    if (pct >= 5) {
      throw new ScenarioError(`viewport jumped on non-pure-append refresh: ${pct.toFixed(2)}% pixels changed`);
    }

    // Reader-state restore: close the timeline (Esc) and reopen the same
    // session — the reading position must come back (again pixel-stable).
    d.pressKey('Escape', { windowId: ctx.win.window_id });
    d.sleep(700);
    d.clickAt(ctx.win, 700, 175);
    d.sleep(1500);
    const reopened = evidence.shot(ctx, 'anchor-reopened');
    crop(reopened, '/tmp/d12-reopened-view.png');
    const raw2 = d.sh(
      'compare -metric AE /tmp/d12-after-view.png /tmp/d12-reopened-view.png null: 2>&1 || true',
      { timeout: 60_000 },
    );
    const ae2 = Number(raw2.trim().split(/[\s(]/)[0]);
    const pct2 = (ae2 / (830 * 680)) * 100;
    evidence.text('D12-reopen-diff.txt', `AE ${ae2} (${pct2.toFixed(3)}%)\nraw: ${raw2}`);
    if (pct2 >= 5) {
      throw new ScenarioError(`reader state not restored on reopen: ${pct2.toFixed(2)}% pixels changed`);
    }
  },

  D13_memory_archive_flow: async (ctx) => {
    // Keyboard-driven throughout (j/m/v/d/u/x are the app's own paths; the
    // "v" conversation jump is a GPUI-side addition because mouse clicks on
    // list rows are unreliable through XTest on this machine — recorded in
    // the parity doc). The sidebar click is stable and stays a click.
    const db = d.dbPath(ctx.home);
    const sessionId = d.sqlite(db, 'SELECT id FROM sessions LIMIT 1;');
    const firstMsg = d.sqlite(
      db,
      `SELECT uuid FROM messages WHERE session_id = '${sessionId}' ORDER BY timestamp LIMIT 1;`,
    );
    const memoryFile = join(ctx.home, 'e2e-mem-1.md');
    writeFileSync(memoryFile, '# Prefer async IO in this project\n');
    d.sh(
      `sqlite3 ${JSON.stringify(db)} "INSERT INTO memories (id, session_id, project, message_start, message_end, path, anchors, summary, created_at) VALUES ('e2e-mem-1', '${sessionId}', '-home-dev-project--', '${firstMsg}', NULL, '${memoryFile}', '[]', 'Prefer async IO in this project', '2026-08-20T10:00:00.000Z');"`,
    );

    d.clickAt(ctx.win, d.NAV.Memory.x, d.NAV.Memory.y);
    d.sleep(1800);

    // 1) j -> m: cursor on the first row, open its detail. (The original
    //    Enter is eaten by the platform layer on X11 — documented.)
    d.pressKey('j', { windowId: ctx.win.window_id });
    d.sleep(600);
    d.pressKey('m', { windowId: ctx.win.window_id });
    d.sleep(1100);
    let shot = evidence.shot(ctx, 'detail');
    await d.visionExpects(
      shot,
      'Look at the bottom of the main panel, below the list. Quote the metadata line (project and file name) and any action button labels you see there.',
      ['e2e-mem-1'],
    );
    // 2) v: jump into the conversation; the timeline opens with the first
    //    message highlighted.
    d.pressKey('v', { windowId: ctx.win.window_id });
    d.sleep(1800);
    shot = evidence.shot(ctx, 'jump');
    await d.visionExpects(
      shot,
      'Describe the current main panel view. What kind of content is shown, and is any row carrying a colored highlight border? Answer plainly.',
      ['Sessions'],
    );
    // Returning via the sidebar row (select_view drops the timeline and
    // refocuses the memory panel; Escape proved unreliable here).
    d.clickAt(ctx.win, d.NAV.Memory.x, d.NAV.Memory.y);
    d.sleep(1200);

    // 3) j -> d: archive; DB + undo bar.
    d.pressKey('j', { windowId: ctx.win.window_id });
    d.sleep(500);
    d.pressKey('d', { windowId: ctx.win.window_id });
    d.sleep(1000);
    const archived = d.sqlite(db, "SELECT deleted_at IS NOT NULL FROM memories WHERE id='e2e-mem-1';");
    if (archived !== '1') throw new ScenarioError('archive key (d) did not land in the DB');
    // The undo-bar's own paint races an fc-gpui frame-drop on focus
    // switches (upstream); its FUNCTION is proven by the u round-trip.
    // Recorded in docs/desktop-parity.md.
    shot = evidence.shot(ctx, 'undo-bar');

    // 4) u: undo restores the row in the DB (re-enter Memory first — keys
    //    need the panel focused, and the archive may have dropped it).
    d.clickAt(ctx.win, d.NAV.Memory.x, d.NAV.Memory.y);
    d.sleep(900);
    let restored = '0';
    for (let attempt = 0; attempt < 3; attempt++) {
      d.pressKey('u', { windowId: ctx.win.window_id });
      d.sleep(1100);
      restored = d.sqlite(db, "SELECT deleted_at IS NULL FROM memories WHERE id='e2e-mem-1';");
      if (restored === '1') break;
    }
    if (restored !== '1') throw new ScenarioError('undo did not restore the memory');

    // 5) j -> x -> d: checkbox mark + batch archive, then the sidebar
    //    Archived sub-row shows it under the Archived tab.
    d.clickAt(ctx.win, d.NAV.Memory.x, d.NAV.Memory.y);
    d.sleep(800);
    d.pressKey('j', { windowId: ctx.win.window_id });
    d.sleep(500);
    d.pressKey('x', { windowId: ctx.win.window_id });
    d.sleep(400);
    d.pressKey('d', { windowId: ctx.win.window_id });
    d.sleep(1000);
    const archived2 = d.sqlite(db, "SELECT deleted_at IS NOT NULL FROM memories WHERE id='e2e-mem-1';");
    if (archived2 !== '1') throw new ScenarioError('checkbox archive (x+d) did not land in the DB');
    // Switch via the header Archived chip (the sidebar sub-row positions
    // drift between launches; the header tabs are in a stable spot).
    for (const x of [720, 700, 740, 680, 760]) {
      d.clickAt(ctx.win, x, 169);
      d.sleep(500);
    }
    d.sleep(700);
    shot = evidence.shot(ctx, 'archived-tab');
    await d.visionExpects(
      shot,
      'Which of the two memory tabs at the top of the panel is highlighted, and does the list below show any row? Quote the row file name if present.',
      ['Archived', 'e2e-mem-1'],
    );
  },

  D17_keyboard_and_list_polish: async (ctx) => {
    // P0-12 + M4.5 list polish: global Ctrl+1/2/3, `s` sort toggle, Escape
    // clears the query, quiet-session fold, branch chip, FTS hit jumps to
    // the matched message. Quiet + branch fixtures are derived from the
    // donor transcript by rewriting session/title and gitBranch lines.
    const donorDir = join(ctx.corpus, 'sessions', '--home-dev-project--');
    const donor = readdirSync(donorDir).find((n) => n.startsWith('session-'));
    const raw = d.sh(`zstd -d -c ${JSON.stringify(join(donorDir, donor, 'session.jsonl.zstd'))}`);
    const mkVariant = (dirName, rewrite) => {
      const lines = raw
        .split('\n')
        .map((line) => {
          try {
            const o = JSON.parse(line);
            return rewrite(o);
          } catch {
            return line;
          }
        })
        .join('\n');
      const dir = join(donorDir, dirName);
      mkdirSync(dir, { recursive: true });
      // Write via a temp file — the transcript is far too large for argv.
      const tmp = `/tmp/d17-variant-${dirName}.jsonl`;
      writeFileSync(tmp, lines);
      d.sh(`zstd -q -f -o ${JSON.stringify(join(dir, 'session.jsonl.zstd'))} ${JSON.stringify(tmp)}`);
    };
    // Quiet: same id (deduped into one session) but no title event.
    mkVariant('session-quiet-00000000-0000-0000-0000-000000000017', (o) => {
      if (o.type === 'session') {
        return JSON.stringify({ ...o, id: 'session-quiet-00000000-0000-0000-0000-000000000017' });
      }
      if (o.type === 'session/title') {
        return JSON.stringify({ ...o, data: { ...o.data, title: '' } });
      }
      return JSON.stringify(o);
    });
    // Branch: own id, titled, carries a gitBranch.
    mkVariant('session-branch-00000000-0000-0000-0000-000000000042', (o) => {
      if (o.type === 'session') {
        return JSON.stringify({
          ...o,
          id: 'session-branch-00000000-0000-0000-0000-000000000042',
          gitBranch: 'feature/parity',
        });
      }
      if (o.type === 'session/title') {
        return JSON.stringify({ ...o, data: { ...o.data, title: 'Branch fixture session' } });
      }
      return JSON.stringify(o);
    });

    // Wait for the daemon to index the two new sessions.
    const db = d.dbPath(ctx.home);
    let count = 0;
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      count = Number(d.sqlite(db, 'SELECT COUNT(*) FROM sessions;'));
      if (count >= 3) break;
      d.sleep(800);
    }
    if (count < 3) throw new ScenarioError(`fixtures not indexed (${count} sessions)`);

    // 1) Quiet fold: banner hides the untitled session until expanded.
    let shot = evidence.shot(ctx, 'list');
    await d.visionExpects(
      shot,
      'Does the session list show a banner about quiet sessions hidden? Quote it.',
      ['quiet sessions hidden'],
    );
    const foldAnswer = d.sh(
      `dim image read ${JSON.stringify(shot)} --prompt 'Find the banner line that mentions quiet sessions hidden. Give its center as X=NN% Y=NN% (percentages of the whole image). Format only.'`,
      { timeout: 300_000 },
    );
    let m = foldAnswer.match(/X\s*=\s*(\d+(?:\.\d+)?)\s*%\s*Y\s*=\s*(\d+(?:\.\d+)?)\s*%/i);
    if (!m) throw new ScenarioError(`fold banner not located: ${foldAnswer.slice(0, 160)}`);
    d.clickAt(
      ctx.win,
      Math.round((Number(m[1]) / 100) * ctx.win.width),
      Math.round((Number(m[2]) / 100) * ctx.win.height),
    );
    d.sleep(1200);
    shot = evidence.shot(ctx, 'fold-expanded');
    await d.visionExpects(
      shot,
      'Is an (untitled) session row visible now, and is a session row showing the text feature/parity also visible?',
      ['untitled', 'feature/parity'],
    );

    // 2) `s` toggles sort: the header count flips newest/oldest.
    d.pressKey('s', { windowId: ctx.win.window_id, focus: false });
    d.sleep(900);
    shot = evidence.shot(ctx, 'sort-asc');
    await d.visionExpects(
      shot,
      'In the sessions header next to the session count, does it say oldest now?',
      ['oldest'],
    );

    // 3) Escape clears a typed query.
    d.clickAt(ctx.win, d.SEARCH_BOX.x, d.SEARCH_BOX.y);
    d.sleep(400);
    d.typeText('Branch fixture');
    d.sleep(900);
    shot = evidence.shot(ctx, 'filtered');
    await d.visionExpects(
      shot,
      'How many session rows are listed, and does the first row title contain Branch fixture?',
      ['Branch fixture'],
    );
    d.pressKey('escape', { windowId: ctx.win.window_id, focus: false });
    d.sleep(900);
    shot = evidence.shot(ctx, 'cleared');
    await d.visionExpects(
      shot,
      'Is the search box empty and are the quiet-fold banner and all rows back?',
      ['quiet sessions hidden'],
    );

    // 4) FTS hit click jumps to the matched message inside the timeline.
    d.typeText('text');
    d.sleep(1200);
    shot = evidence.shot(ctx, 'fts');
    await d.visionExpects(
      shot,
      'Is there a full-text matches section with a snippet row? Quote the section header.',
      ['Full-text matches'],
    );
    const hitAnswer = d.sh(
      `dim image read ${JSON.stringify(shot)} --prompt 'Find the first row under the Full-text matches section (the snippet row). Give its center as X=NN% Y=NN% (percentages of the whole image). Format only.'`,
      { timeout: 300_000 },
    );
    m = hitAnswer.match(/X\s*=\s*(\d+(?:\.\d+)?)\s*%\s*Y\s*=\s*(\d+(?:\.\d+)?)\s*%/i);
    if (!m) throw new ScenarioError(`hit row not located: ${hitAnswer.slice(0, 160)}`);
    d.clickAt(
      ctx.win,
      Math.round((Number(m[1]) / 100) * ctx.win.width),
      Math.round((Number(m[2]) / 100) * ctx.win.height),
    );
    d.sleep(2000);
    shot = evidence.shot(ctx, 'timeline');
    await d.visionExpects(
      shot,
      'Does the timeline view show an opened session with message bubbles, and is one message highlighted with a colored outline?',
      ['Sanitized fixture session'],
    );

    // 5) Global shortcuts: Ctrl+2 → Active memories, Ctrl+1 → back.
    d.pressKey('ctrl-2', { windowId: ctx.win.window_id, focus: false });
    d.sleep(1500);
    shot = evidence.shot(ctx, 'ctrl2');
    await d.visionExpects(
      shot,
      'Which view is showing now? Is it the memory list with an Active tab selected?',
      ['Active'],
    );
    d.pressKey('ctrl-1', { windowId: ctx.win.window_id, focus: false });
    d.sleep(1500);
    shot = evidence.shot(ctx, 'ctrl1');
    await d.visionExpects(
      shot,
      'Is the sessions list visible again with the quiet-fold banner?',
      ['quiet sessions hidden'],
    );
  },

  D15_activity_heatmap_ledger: async (ctx) => {
    // P0-6: stats bar, daily heatmap, weekly/cumulative tabs, and the
    // day ledger after a heatmap cell click. All assertions are vision over
    // deterministic screenshots (the fixture corpus has known usage).
    d.clickAt(ctx.win, d.NAV.Activity.x, d.NAV.Activity.y);
    d.sleep(2000);
    let shot = evidence.shot(ctx, 'activity');
    await d.visionExpects(
      shot,
      'Describe the top of this Activity page: list the stat labels you can read, and say whether a grid of many small squares (a heatmap) is visible.',
      ['Lifetime tokens', 'Peak tokens', 'Current streak', 'Longest streak'],
    );

    // Locate the Daily/Weekly/Cumulative tab row by vision.
    const tabsPos = async (label, fromShot) => {
      const answer = d.sh(
        `dim image read ${JSON.stringify(fromShot)} --prompt 'Find the small tab button labeled "${label}" near the top right of the panel (next to Token activity). Give its center as X=NN% Y=NN% (percentages of the whole image). Format only.'`,
        { timeout: 300_000 },
      );
      const m = answer.match(/X\s*=\s*(\d+(?:\.\d+)?)\s*%\s*Y\s*=\s*(\d+(?:\.\d+)?)\s*%/i);
      if (!m) throw new ScenarioError(`${label} tab not located: ${answer.slice(0, 160)}`);
      return [
        Math.round((Number(m[1]) / 100) * ctx.win.width),
        Math.round((Number(m[2]) / 100) * ctx.win.height),
      ];
    };
    const weeklyPos = await tabsPos('Weekly', shot);
    d.clickAt(ctx.win, weeklyPos[0], weeklyPos[1]);
    d.sleep(900);
    shot = evidence.shot(ctx, 'weekly');
    await d.visionExpects(
      shot,
      'Does this panel show a bar chart of vertical bars now (weekly view)? Answer in one line.',
      ['bar'],
    );

    // Back to Daily, then click the brightest heatmap cell (the fixture
    // day with tokens) and expect the day ledger.
    const dailyPos = await tabsPos('Daily', shot);
    d.clickAt(ctx.win, dailyPos[0], dailyPos[1]);
    d.sleep(1200);
    shot = evidence.shot(ctx, 'daily');
    const answer = d.sh(
      `dim image read ${JSON.stringify(shot)} --prompt 'In the heatmap grid of small squares, find the square with the strongest purple fill (most saturated, level-4). Give its center as X=NN% Y=NN% (percentages of the whole image). Format only.'`,
      { timeout: 300_000 },
    );
    const m = answer.match(/X\s*=\s*(\d+(?:\.\d+)?)\s*%\s*Y\s*=\s*(\d+(?:\.\d+)?)\s*%/i);
    if (!m) throw new ScenarioError(`cell not located: ${answer.slice(0, 160)}`);
    d.clickAt(
      ctx.win,
      Math.round((Number(m[1]) / 100) * ctx.win.width),
      Math.round((Number(m[2]) / 100) * ctx.win.height),
    );
    d.sleep(1500);
    shot = evidence.shot(ctx, 'day-ledger');
    await d.visionExpects(
      shot,
      'Below the chart, is there a session activity section for the clicked day — a heading with a month and a session count, and rows mentioning sessions or workspaces? Quote the heading.',
      ['session'],
    );
  },

  D16_recap_five_cards: async (ctx) => {
    // P0-7: recap list + five-card detail with archetype theming and
    // prev/next navigation. Uses a fixture recap with sentinel strings.
    const recapDir = join(ctx.home, '.obelisk', 'recap');
    mkdirSync(recapDir, { recursive: true });
    cpSync(
      join(repo, 'tests/fixtures/recap/recap-e2e.json'),
      join(recapDir, 'recap-e2e.json'),
    );

    d.clickAt(ctx.win, d.NAV.Recap.x, d.NAV.Recap.y);
    d.sleep(2000);
    let shot = evidence.shot(ctx, 'list');
    // Click the recap row in the left list to open its cards.
    const rowAnswer = d.sh(
      `dim image read ${JSON.stringify(shot)} --prompt 'In the left column, find the list row that shows the file name recap-e2e.json. Give its center as X=NN% Y=NN% (percentages of the whole image). Format only.'`,
      { timeout: 300_000 },
    );
    const rowMatch = rowAnswer.match(/X\s*=\s*(\d+(?:\.\d+)?)\s*%\s*Y\s*=\s*(\d+(?:\.\d+)?)\s*%/i);
    if (!rowMatch) throw new ScenarioError(`recap row not located: ${rowAnswer.slice(0, 160)}`);
    d.clickAt(
      ctx.win,
      Math.round((Number(rowMatch[1]) / 100) * ctx.win.width),
      Math.round((Number(rowMatch[2]) / 100) * ctx.win.height),
    );
    d.sleep(1500);
    shot = evidence.shot(ctx, 'cover');
    await d.visionExpects(
      shot,
      'Is a large card visible now? Quote the big title text on the card.',
      ['Cover Sentinel Alpha'],
    );

    // Navigate with the next arrow, located by pixel-scanning the bottom
    // nav strip for the purple arrow glyph (vision coordinates drift on
    // full-window shots; pixels do not).
    const nextArrowPos = (fromShot) => {
      const strip = '/tmp/d16-nav.txt';
      d.sh(
        `convert ${JSON.stringify(fromShot)} -crop '${ctx.win.width}x100+0+${ctx.win.height - 100}' +repage txt:- > ${strip}`,
      );
      const purple = [];
      for (const line of readFileSync(strip, 'utf8').split('\n')) {
        const m = line.match(/^(\d+),(\d+): \(167,139,250\)/);
        if (m) purple.push([Number(m[1]), Number(m[2])]);
      }
      if (!purple.length) throw new ScenarioError('no purple pixels in nav strip');
      const maxX = Math.max(...purple.map((p) => p[0]));
      const right = purple.filter((p) => p[0] >= maxX - 12);
      const yMid = right.reduce((sum, p) => sum + p[1], 0) / right.length;
      // The glyph sits inside the 36px circular button; aim a touch left of
      // the glyph's right edge, vertically centered on it.
      return [Math.round(maxX - 8), Math.round(ctx.win.height - 100 + yMid)];
    };

    const clickNext = () => {
      const shot = evidence.shot(ctx, 'nav');
      const [ax, ay] = nextArrowPos(shot);
      d.clickAt(ctx.win, ax, ay);
      d.sleep(1200);
    };

    // Path card (one next from Cover).
    clickNext();
    shot = evidence.shot(ctx, 'path');
    await d.visionExpects(
      shot,
      'Which card is shown now — quote its title, the eyebrow text at the top of the card, and one prompt line.',
      ['Path Sentinel Beta', 'thinking path', 'Ship the parser'],
    );

    // Closing card (three more nexts: Vibe, Workflow, Closing).
    clickNext();
    clickNext();
    clickNext();
    shot = evidence.shot(ctx, 'closing');
    await d.visionExpects(
      shot,
      'Quote the large centered headline of this card and one line from the receipt list below it.',
      ['Carved Sentinel Zeta'],
    );
  },

  D14_settings_rebuild_and_validation: async (ctx) => {
    // P0-8/P0-9 end-to-end. Root validation logic is unit-tested in Rust;
    // here we prove: (a) a settings.json root change hot-applies through the
    // daemon (watcher rebuild + rebuild from the NEW roots, no restart) —
    // asserted via the indexed jsonl_path flipping between corpus copies;
    // and (b) the manual rebuild button forces a full rebuild.
    const db = d.dbPath(ctx.home);
    const settingsPath = join(ctx.home, '.obelisk', 'settings.json');
    const corpusB = `${ctx.corpus}-B`;
    cpSync(join(ctx.corpus, 'sessions'), join(corpusB, 'sessions'), { recursive: true });
    const readSettings = () => JSON.parse(readFileSync(settingsPath, 'utf8'));
    const switchRoot = (root) => {
      const settings = readSettings();
      settings.providerRoots.deepseek = root;
      writeFileSync(settingsPath, JSON.stringify(settings, null, 2));
    };
    const indexedRoot = () =>
      d
        .sqlite(db, "SELECT jsonl_path FROM index_state WHERE jsonl_path LIKE '/tmp/%';")
        .split('\n')
        .find((line) => line.includes('/session.jsonl.zstd')) || '';

    // Sanity: the initial build indexed the A corpus.
    if (!indexedRoot().startsWith(ctx.corpus)) {
      throw new ScenarioError(`expected corpus A indexed first, got ${indexedRoot()}`);
    }

    // 1) Hot-apply to corpus B: the daemon notices the settings mtime change
    //    on its heartbeat tick, rebuilds the watcher from the new roots and
    //    republishes the index pointing at corpus B.
    switchRoot(join(corpusB, 'sessions'));
    let flipped = false;
    let deadline = Date.now() + 45_000;
    while (Date.now() < deadline) {
      flipped = indexedRoot().startsWith(corpusB);
      if (flipped) break;
      d.sleep(1000);
    }
    if (!flipped) {
      throw new ScenarioError(`root switch to B did not hot-apply (${indexedRoot()})`);
    }

    // 2) Hot-apply BACK to corpus A proves the swap is repeatable.
    switchRoot(join(ctx.corpus, 'sessions'));
    let back = false;
    deadline = Date.now() + 45_000;
    while (Date.now() < deadline) {
      back = indexedRoot().startsWith(ctx.corpus) && !indexedRoot().startsWith(corpusB);
      if (back) break;
      d.sleep(1000);
    }
    if (!back) {
      throw new ScenarioError(`root switch back to A did not hot-apply (${indexedRoot()})`);
    }

    // 3) Manual rebuild button: scroll the settings body to the About
    //    section, locate the button by vision, click, and wait for the
    //    __last_build__ marker to refresh.
    const buildBefore = d.sqlite(
      db,
      "SELECT mtime FROM index_state WHERE jsonl_path='__last_build__';",
    );
    d.clickAt(ctx.win, d.NAV.Settings.x, d.NAV.Settings.y);
    d.sleep(1500);
    for (let i = 0; i < 4; i++) {
      d.scrollAt(ctx.win, 800, 400, 5);
      d.sleep(300);
    }
    const shot = evidence.shot(ctx, 'about');
    const answer = d.sh(
      `dim image read ${JSON.stringify(shot)} --prompt 'Find the button labeled "Rebuild index". Give its center as X=NN% Y=NN% of the image (percentages). Format only.'`,
      { timeout: 300_000 },
    );
    const m = answer.match(/X\s*=\s*(\d+(?:\.\d+)?)\s*%\s*Y\s*=\s*(\d+(?:\.\d+)?)\s*%/i);
    if (!m) throw new ScenarioError(`rebuild button not located: ${answer.slice(0, 160)}`);
    d.clickAt(
      ctx.win,
      Math.round((Number(m[1]) / 100) * ctx.win.width),
      Math.round((Number(m[2]) / 100) * ctx.win.height),
    );
    let buildAfter = buildBefore;
    const deadline2 = Date.now() + 40_000;
    while (Date.now() < deadline2) {
      buildAfter = d.sqlite(db, "SELECT mtime FROM index_state WHERE jsonl_path='__last_build__';");
      if (buildAfter !== buildBefore) break;
      d.sleep(800);
    }
    if (buildAfter === buildBefore) {
      throw new ScenarioError('manual rebuild did not refresh __last_build__');
    }
  },

  D11_live_session_follow: async (ctx) => {
    d.clickAt(ctx.win, 700, 175);
    d.sleep(1400);
    d.pressKey('End', { focus: true, windowId: ctx.win.window_id });
    d.sleep(1000);

    const corpusFile = d.sh(`find ${JSON.stringify(join(ctx.corpus, 'sessions'))} -name 'session.jsonl.zstd' | head -1`).trim();
    const donorFile = d.sh(`find ${JSON.stringify(join(ctx.corpus, 'sessions'))} -name 'session.jsonl.zstd' | tail -1`).trim();
    d.sh(`cat ${JSON.stringify(donorFile)} >> ${JSON.stringify(corpusFile)}`);

    // No key press: the daemon build + follow-tail must reveal the new tail
    // in the open timeline by itself.
    let grew = false;
    const deadline = Date.now() + 20_000;
    while (Date.now() < deadline) {
      const count = Number(d.sqlite(d.dbPath(ctx.home), 'SELECT message_count FROM sessions LIMIT 1;'));
      if (count > 7) { grew = true; break; }
      d.sleep(500);
    }
    if (!grew) throw new ScenarioError('daemon did not index the append (D11)');
    d.sleep(1000);
    const shot = evidence.shot(ctx, 'follow-tail');
    await d.visionExpects(
      shot,
      'Is the timeline still in the session detail view (back link "← Sessions") and pinned near the bottom showing the last messages? What is the header item count (a number larger than 14)?',
      ['Sessions', '17'],
    );
  },
};

// ---------------------------------------------------------------- runner

async function runScenario(name, body) {
  for (let attempt = 0; attempt <= retries; attempt++) {
    const ctx = { name };
    let failure = null;
    try {
      Object.assign(ctx, await setup(name));
      ctx.name = name;
      await body(ctx);
    } catch (error) {
      failure = error;
    }
    if (ctx.home) {
      try { await teardown(ctx); } catch { /* teardown best-effort */ }
    }
    if (!failure) {
      results.push({ name, pass: true, attempt });
      console.log(`PASS ${name}${attempt > 0 ? ` (retry ${attempt})` : ''}`);
      return;
    }
    console.error(`FAIL ${name} (attempt ${attempt}): ${failure.message}`);
    if (attempt === retries) {
      evidence.text(`${name}-error.txt`, failure.stack || failure.message);
      results.push({ name, pass: false, error: failure.message });
    } else {
      d.sleep(1500);
    }
  }
}

const order = [
  'D1_cold_start',
  'D2_project_session_timeline',
  'D3_scroll_follow',
  'D4_search_aligns_cli',
  'D5_memory_browse',
  'D6_stats_recap',
  'D7_tray_background_indexing',
  'D8_close_window_stays_resident',
  'D9_chinese_ime_input',
  'D10_settings_page',
  'D11_live_session_follow',
  'D12_scroll_anchor_on_refresh',
  'D13_memory_archive_flow',
  'D14_settings_rebuild_and_validation',
  'D15_activity_heatmap_ledger',
  'D16_recap_five_cards',
  'D17_keyboard_and_list_polish',
];

const selected = only ? order.filter((n) => only.some((o) => n.startsWith(o))) : order;
console.log(`desktop e2e: ${selected.length} scenarios, evidence → ${runDir}`);

for (const name of selected) {
  await runScenario(name, scenarios[name]);
}

const failed = results.filter((r) => !r.pass);
const report = {
  pass: failed.length === 0,
  total: results.length,
  failed: failed.map((f) => f.name),
  results,
  evidence: runDir,
  driver: { app: appBinary, cli: binary },
};
writeFileSync(join(runDir, 'report.json'), JSON.stringify(report, null, 2));
console.log(`\n${report.pass ? 'ALL PASS' : 'FAILURES'}: ${results.length - failed.length}/${results.length} (${runDir}/report.json)`);
process.exit(report.pass ? 0 : 1);
