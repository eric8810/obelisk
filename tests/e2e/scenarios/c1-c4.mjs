// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Scenarios C1–C4: fresh install/first build, skill install, agent first
// query (mktemp/heredoc), search + nonce attribution.

import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import {
  makeCorpusHome, openDb, drive, parseJsonOutput, isTsBinary, repoRoot,
} from '../harness.mjs';

export default [
  {
    id: 'C1',
    title: '全新安装→首建',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const run = async (args, o = {}) => drive(opts.binary, 'c1', args, { home, cwd: home, driver: opts.driver, timeoutMs: o.timeoutMs });
      const r = await run(['--build']);
      assert.equal(r.exitCode, 0, `build exit: ${r.stderr}\n${r.stdout}`);
      const payload = JSON.parse(r.stdout.trim().split('\n').pop());
      assert.equal(payload.ok, true);
      assert.ok(payload.db.endsWith('.obelisk/obelisk.sqlite'), JSON.stringify(payload));
      const db = openDb(home);
      const tables = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('sessions','messages','tool_calls','tool_results','subagents','workflows','workflow_agents','index_state','summaries','memories') ORDER BY name").all().map(r => r.name);
      assert.equal(tables.length, 10, `schema tables: ${tables.join(',')}`);
      const sessions = db.prepare('SELECT COUNT(*) AS c FROM sessions').get().c;
      assert.ok(sessions >= 8, `expected ≥8 sessions, got ${sessions}`);
      db.close();
      // Second build (force) is idempotent for row content.
      const r2 = await run(['--build']);
      assert.equal(r2.exitCode, 0);
      const db2 = openDb(home);
      const sessions2 = db2.prepare('SELECT COUNT(*) AS c FROM sessions').get().c;
      assert.equal(sessions2, sessions);
      db2.close();
    },
  },
  {
    id: 'C2',
    title: 'skill 安装（install 子命令）',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      // Fake npx on PATH: records its argv, exits 0.
      const shimDir = mkdtempSync(join(tmpdir(), 'obelisk-e2e-npx-'));
      const recorder = join(shimDir, 'argv.json');
      const script = `#!/bin/sh\necho "$@" > '${recorder}'\nexit 0\n`;
      writeFileSync(join(shimDir, 'npx'), script, { mode: 0o755 });
      const env = { ...process.env, HOME: home, USERPROFILE: home, PATH: `${shimDir}:${process.env.PATH}` };
      delete env.DSH_HOME;
      const absoluteBinary = resolve(repoRoot, opts.binary);
      const command = isTsBinary(absoluteBinary)
        ? [process.execPath, ['--disable-warning=ExperimentalWarning', absoluteBinary, 'install']]
        : [absoluteBinary, ['install']];
      const r = spawnSync(command[0], command[1], { encoding: 'utf8', env, cwd: home });
      assert.equal(r.status, 0, `install exit: ${r.stderr}`);
      const argv = readFileSync(recorder, 'utf8').trim().split('\n');
      assert.equal(argv[0], '--yes skills add tommy0103/obelisk-skill');
      // Without npx on PATH the CLI reports honestly and exits 1.
      const envNoNpx = { ...env, PATH: '/nonexistent' };
      const r2 = spawnSync(command[0], command[1], { encoding: 'utf8', env: envNoNpx, cwd: home });
      assert.equal(r2.status, 1);
      assert.ok(r2.stderr.includes('Unable to run the skills installer'), r2.stderr);
    },
  },
  {
    id: 'C3',
    title: 'agent 首查（mktemp/heredoc 流程）',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const build = await drive(opts.binary, 'c3', ['--build'], { home, cwd: home, driver: opts.driver });
      assert.equal(build.exitCode, 0);
      // The documented skill flow: mktemp → heredoc → --query.
      const queryFile = join(home, 'first-query.js');
      writeFileSync(queryFile, 'return await overview();\n');
      const r = await drive(opts.binary, 'c3', ['--query', queryFile], { home, cwd: home, driver: opts.driver, timeoutMs: 90000 });
      assert.equal(r.exitCode, 0, r.stdout + r.stderr);
      const out = parseJsonOutput(r.stdout);
      assert.ok(out && typeof out === 'object', `overview JSON: ${r.stdout.slice(0, 200)}`);
      assert.ok('current' in out, `overview shape: ${Object.keys(out)}`);
      assert.ok('projects' in out && Array.isArray(out.projects));
      assert.ok('totals' in out && out.totals.sessions >= 8);
      assert.ok(typeof out.current.cwd === 'string');
    },
  },
  {
    id: 'C4',
    title: '--search + nonce 归属',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const build = await drive(opts.binary, 'c4', ['--build'], { home, cwd: home, driver: opts.driver });
      assert.equal(build.exitCode, 0);
      const nonce = `e2e-nonce-${Math.random().toString(36).slice(2, 14)}`;
      // Plant the nonce in a RECENT message of a claude session that the
      // search text also hits (the resolution legs are bounded to the last
      // 15 minutes, and the corpus uses fixed historical timestamps).
      const db = openDb(home);
      const sessionId = db.prepare(
        "SELECT m.session_id AS id FROM messages m JOIN sessions s ON s.id = m.session_id WHERE s.source='claude' AND m.text LIKE '%golden%' ORDER BY m.timestamp LIMIT 1",
      ).get().id;
      db.close();
      const { DatabaseSync } = await import('node:sqlite');
      const dbw = new DatabaseSync(join(home, '.obelisk', 'obelisk.sqlite'));
      const now = new Date().toISOString();
      dbw.prepare('INSERT INTO messages (uuid, session_id, type, timestamp, role, text, is_meta, visibility, source) VALUES (?, ?, ?, ?, ?, ?, 0, \'visible\', \'claude\')')
        .run(`nonce-carrier-${nonce}`, sessionId, 'user', now, 'user', ` the agent ran obelisk --search golden --nonce ${nonce} in this golden session `);
      dbw.prepare("INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__last_build__', ?, 0)").run(Date.now() - 60000);
      dbw.close();
      const r = await drive(opts.binary, 'c4', ['--search', 'golden', '--nonce', nonce], { home, cwd: home, driver: opts.driver, timeoutMs: 90000 });
      assert.equal(r.exitCode, 0, r.stdout + r.stderr);
      const out = parseJsonOutput(r.stdout);
      assert.ok(Array.isArray(out), `search array: ${r.stdout.slice(0, 200)}`);
      assert.ok(out.length > 0, 'search finds corpus text');
      const invoking = out.filter(h => h.session && h.session.is_invoking);
      assert.equal(invoking.length > 0, true, 'the nonce session is marked is_invoking');
      assert.equal(invoking[0].session.id, sessionId);
    },
  },
];
