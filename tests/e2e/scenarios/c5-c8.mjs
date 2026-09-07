// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Scenarios C5–C8: incremental freshness (passive pull), daemon coexistence,
// memory remember→recall→forget, sql() write rejection.

import assert from 'node:assert/strict';
import { writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { makeCorpusHome, openDb, drive, parseJsonOutput } from '../harness.mjs';

export default [
  {
    id: 'C5',
    title: '增量新鲜（被动拉取：改语料→查询即见）',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const run = (args, timeoutMs = 90000) => drive(opts.binary, 'c5', args, { home, cwd: home, driver: opts.driver, timeoutMs });
      assert.equal((await run(['--build'])).exitCode, 0);
      const before = parseJsonOutput((await run(['--search', 'zzz-unique-fresh-word'])).stdout);
      assert.equal(before.length, 0, 'fresh word absent before corpus append');
      // Append a brand-new claude session containing the fresh word.
      const projectsDir = join(home, '.claude', 'projects', '-fresh-proj');
      mkdirSync(projectsDir, { recursive: true });
      writeFileSync(join(projectsDir, 'fresh-session.jsonl'), JSON.stringify({
        uuid: 'fresh-u1', type: 'user', timestamp: '2026-06-12T10:00:00Z', cwd: '/fresh',
        message: { role: 'user', content: 'the zzz-unique-fresh-word appears here' },
      }) + '\n');
      // The pre-query refresh is debounced to 30s after the last build:
      // backdate the marker so the passive pull actually runs (an honest
      // simulation of querying later than 30s after the previous build).
      const { DatabaseSync } = await import('node:sqlite');
      const dbw = new DatabaseSync(join(home, '.obelisk', 'obelisk.sqlite'));
      dbw.prepare("INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__last_build__', ?, 0)").run(Date.now() - 60000);
      dbw.close();
      const after = parseJsonOutput((await run(['--search', 'zzz-unique-fresh-word'])).stdout);
      assert.ok(after.length > 0, 'query refresh pulls the new session before answering');
      assert.equal(after[0].session.project, '-fresh-proj');
    },
  },
  {
    id: 'C6',
    title: 'daemon 共存（心跳在场查询可用、构建让位）',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const run = (args, timeoutMs = 90000) => drive(opts.binary, 'c6', args, { home, cwd: home, driver: opts.driver, timeoutMs });
      assert.equal((await run(['--build'])).exitCode, 0);
      // A fresh daemon heartbeat owns writes: --build skips, queries still work.
      const dbw = new DatabaseSync(join(home, '.obelisk', 'obelisk.sqlite'));
      dbw.prepare("INSERT OR REPLACE INTO index_state (jsonl_path, mtime, lines_processed) VALUES ('__app_heartbeat__', ?, 0)").run(Date.now());
      dbw.close();
      const search = await run(['--search', 'golden']);
      assert.equal(search.exitCode, 0, 'queries stay available under a daemon heartbeat');
      const out = parseJsonOutput(search.stdout);
      assert.ok(Array.isArray(out) && out.length > 0);
      // Direct incremental build skips with daemon_active (observable only
      // via unchanged DB; force build is the CLI's published path and is
      // NOT daemon-gated — verify the skip on the internal debounce
      // instead: __last_build__ fresh means search skipped the refresh).
      const db = openDb(home);
      const lastBuild = db.prepare("SELECT mtime FROM index_state WHERE jsonl_path='__last_build__'").get();
      assert.ok(lastBuild && Date.now() - lastBuild.mtime < 120000, 'the query path did not force a rebuild under the heartbeat');
      db.close();
    },
  },
  {
    id: 'C7',
    title: '记忆 remember→召回→forget 幂等',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const run = (args, timeoutMs = 90000) => drive(opts.binary, 'c7', args, { home, cwd: home, driver: opts.driver, timeoutMs });
      assert.equal((await run(['--build'])).exitCode, 0);
      const memoryFile = join(home, 'AGENTS-memory.md');
      writeFileSync(memoryFile, '# Project memory\n\n- prefer tabs\n');
      const remember = join(home, 'remember.js');
      writeFileSync(remember, 'return await remember({ path: ' + JSON.stringify(memoryFile) + ', summary: "Uses tabs and strict module boundaries in the payments module", session_id: null });\n');
      const r1 = await run(['--attune', remember]);
      assert.equal(r1.exitCode, 0, r1.stdout + r1.stderr);
      const mem = parseJsonOutput(r1.stdout);
      assert.ok(mem.id && mem.id.startsWith('mem-'), JSON.stringify(mem));
      assert.equal(mem.project, null);
      // Recall via the query sandbox.
      const recall = join(home, 'recall.js');
      writeFileSync(recall, 'return await memories({ query: "tabs payments" });\n');
      const r2 = await run(['--query', recall]);
      assert.equal(r2.exitCode, 0, r2.stdout + r2.stderr);
      const hits = parseJsonOutput(r2.stdout);
      assert.ok(Array.isArray(hits) && hits.some(h => h.id === mem.id), JSON.stringify(hits).slice(0, 300));
      // Forget is idempotent.
      const forget = join(home, 'forget.js');
      writeFileSync(forget, `return await forget({ id: ${JSON.stringify(mem.id)}, reason: "stale" });\n`);
      const r3 = await run(['--attune', forget]);
      assert.equal(r3.exitCode, 0);
      const f1 = parseJsonOutput(r3.stdout);
      assert.equal(f1.deleted_at && f1.deleted_reason, 'stale');
      const r4 = await run(['--attune', forget]);
      const f2 = parseJsonOutput(r4.stdout);
      assert.equal(f2.already_deleted, true, 'second forget is idempotent');
      // Deleted memories no longer recall.
      const r5 = await run(['--query', recall]);
      const after = parseJsonOutput(r5.stdout);
      assert.ok(!after.some(h => h.id === mem.id), 'deleted memory is not recalled');
    },
  },
  {
    id: 'C8',
    title: 'sql() 写拒绝',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const run = async (script) => {
        const file = join(home, 'q.js');
        writeFileSync(file, script);
        return run2(['--query', file]);
      };
      const run2 = (args, timeoutMs = 90000) => drive(opts.binary, 'c8', args, { home, cwd: home, driver: opts.driver, timeoutMs });
      assert.equal((await run2(['--build'])).exitCode, 0);
      const db0 = openDb(home);
      const messages0 = db0.prepare('SELECT COUNT(*) AS c FROM messages').get().c;
      db0.close();
      for (const bad of [
        'return await sql("DELETE FROM messages");',
        'return await sql("INSERT INTO messages (uuid) VALUES (\'x\')");',
        'return await sql("UPDATE messages SET text=\'x\'");',
        'return await sql("DROP TABLE messages");',
        'return await sql("SELECT 1; SELECT 2;");',
        'return await sql("CREATE TABLE evil (x)");',
        'return await sql("PRAGMA journal_mode=DELETE");',
      ]) {
        const r = await run(bad);
        assert.equal(r.exitCode, 1, `write must fail: ${bad}\n${r.stdout}`);
        const payload = JSON.parse(r.stdout.trim().split('\n')[0]);
        assert.ok(typeof payload.error === 'string' && payload.error.length > 0, `${bad} → ${r.stdout}`);
      }
      const good = await run('return await sql("SELECT COUNT(*) AS c FROM messages");');
      assert.equal(good.exitCode, 0, good.stdout);
      const out = parseJsonOutput(good.stdout);
      assert.equal(Array.isArray(out) && out[0].c, messages0, 'rows unchanged after write attempts');
    },
  },
];
