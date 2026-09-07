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

import { cpSync, mkdirSync, mkdtempSync, rmSync, writeFileSync, readFileSync } from 'node:fs';
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
      'In the Memory view: quote the Active section row summary and the Archived section row (its red tag and reason).',
      ['Prefer async IO', 'superseded'],
    );
    // Click the active memory row: the detail shows the markdown file content.
    // Re-resolve the window right before clicking: a relaunch can move it,
    // and stale bounds send the click to the wrong surface.
    ctx.win = d.findObeliskWindow() || ctx.win;
    evidence.text('D5-window-bounds.txt', JSON.stringify(ctx.win, null, 2));
    d.clickAt(ctx.win, 700, 190);
    d.sleep(1000);
    shot = evidence.shot(ctx, 'memory-detail');
    await d.visionExpects(shot, 'Below the list, is there a detail panel showing the memory file path and rendered markdown with the heading "Prefer async IO"?', ['Prefer async IO', 'memory-note.md']);
  },

  D6_stats_recap: async (ctx) => {
    // Seed usage + a recap file, mirroring real aggregates.
    const db = d.dbPath(ctx.home);
    d.sqlite(db, "UPDATE messages SET input_tokens = 1200, output_tokens = 800 WHERE type = 'assistant';");
    const recapDir = join(ctx.home, '.obelisk', 'recap');
    mkdirSync(recapDir, { recursive: true });
    writeFileSync(join(recapDir, '2026-W36.json'), JSON.stringify({
      week: '2026-W36', sessions: 1, highlights: ['E2E recap card'], top_project: '-home-dev-project--',
    }, null, 2));
    await relaunch(ctx);

    d.clickAt(ctx.win, d.NAV.Activity.x, d.NAV.Activity.y);
    d.sleep(1200);
    let shot = evidence.shot(ctx, 'activity');
    await d.visionExpects(
      shot,
      'Describe the Activity view: stat cards for Total tokens, Peak day, and Longest turn with values, and a "Daily token usage" bar chart. Quote the total tokens value.',
      ['Total tokens', 'Peak day', 'Longest turn', 'Daily token usage'],
    );
    // Deterministic aggregate parity: the card total must equal the DB sum.
    const dbTokens = d.sqlite(db, 'SELECT SUM(COALESCE(input_tokens,0) + COALESCE(output_tokens,0)) FROM messages;');
    if (Number(dbTokens) <= 0) throw new ScenarioError(`expected seeded tokens > 0, got ${dbTokens}`);
    evidence.text('D6-db-tokens.txt', dbTokens);

    d.clickAt(ctx.win, d.NAV.Recap.x, d.NAV.Recap.y);
    d.sleep(1000);
    shot = evidence.shot(ctx, 'recap-list');
    await d.visionExpects(shot, 'Does the Recap view list one report file "2026-W36.json"?', ['2026-W36.json']);
    d.clickAt(ctx.win, 700, 150);
    d.sleep(1000);
    shot = evidence.shot(ctx, 'recap-detail');
    await d.visionExpects(shot, 'Is the parsed recap JSON shown below the list, including "week": "2026-W36" and the highlight "E2E recap card"?', ['2026-W36', 'E2E recap card']);
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
    const schemeChips = { zed: { x: 540, y: 415 }, vscode: { x: 575, y: 333 } };
    const switchScheme = async (want) => {
      for (let attempt = 0; attempt < 3; attempt++) {
        const chip = schemeChips[want];
        d.clickAt(ctx.win, chip.x, chip.y);
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
