// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Scenarios C9–C12: runaway-script 30s kill (sync + post-await), five
// provider mixed index, error paths, TS→Rust cross-implementation handover.

import assert from 'node:assert/strict';
import { writeFileSync, readFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { makeCorpusHome, openDb, dumpIndex, drive, parseJsonOutput, isTsBinary } from '../harness.mjs';

export default [
  {
    id: 'C9',
    title: '失控脚本 30s（同步+await 后死循环）',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const run = async (script, timeoutMs) => {
        const file = join(home, 'loop.js');
        writeFileSync(file, script);
        return drive(opts.binary, 'c9', ['--query', file], { home, cwd: home, driver: 'direct', timeoutMs });
      };
      const build = await drive(opts.binary, 'c9', ['--build'], { home, cwd: home, driver: 'direct' });
      assert.equal(build.exitCode, 0);

      // Layer 1: synchronous runaway loop — both engines must kill at ~30s.
      const sync = await run('while (true) {}', 60000);
      assert.equal(sync.exitCode, 1, 'sync loop is killed, not hung');
      const payload = JSON.parse(sync.stdout.trim().split('\n')[0]);
      assert.ok(/timed out/i.test(payload.error), payload.error);
      assert.ok(sync.elapsed >= 28000 && sync.elapsed <= 42000, `sync kill wall time ${sync.elapsed}ms`);

      // Layer 2: a loop after an await — no VM ticks exist while suspended.
      // ADR-0013: the host timer closes this hole. Rust: killed at ~30s.
      // TS (node:vm timeout covers only the pre-await sync phase): known
      // gap — the process hangs; documented in ADR-0013, mirrored fix
      // pending. Assert the Rust behavior; skip for the TS oracle build.
      if (isTsBinary(opts.binary)) {
        const async = await run('await new Promise(r => setTimeout(r, 50)); while (true) {}', 8000);
        if (async.failed || async.elapsed >= 8000) {
          console.log('    (TS oracle: post-await runaway hangs — known gap, ADR-0013)');
          return;
        }
        assert.fail('TS oracle unexpectedly killed the post-await loop');
      }
      const async = await run('await new Promise(r => setTimeout(r, 50)); while (true) {}', 60000);
      assert.equal(async.exitCode, 1, 'post-await loop is killed by the host timer');
      const payload2 = JSON.parse(async.stdout.trim().split('\n')[0]);
      assert.ok(/timed out/i.test(payload2.error), payload2.error);
      assert.ok(async.elapsed >= 28000 && async.elapsed <= 42000, `async kill wall time ${async.elapsed}ms`);
    },
  },
  {
    id: 'C10',
    title: '五 provider 混合索引对照黄金清单',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const build = await drive(opts.binary, 'c10', ['--build'], { home, cwd: home, driver: opts.driver });
      assert.equal(build.exitCode, 0);
      const manifest = JSON.parse(readFileSync(join(repoRoot(), 'tests', 'golden', 'corpus-manifest.json'), 'utf8'));
      const db = openDb(home);
      for (const [source, expected] of Object.entries(manifest.providers)) {
        const row = db.prepare('SELECT COUNT(*) AS c FROM sessions WHERE source = ?').get(source);
        assert.equal(row.c, expected.sessions, `${source} session count`);
        const mrow = db.prepare('SELECT COUNT(*) AS c FROM messages WHERE source = ?').get(source);
        assert.equal(mrow.c, expected.messages, `${source} message count`);
        const trow = db.prepare('SELECT COUNT(*) AS c FROM tool_calls tc JOIN messages m ON m.uuid = tc.message_uuid WHERE m.source = ?').get(source);
        assert.equal(trow.c, expected.tool_calls, `${source} tool_call count`);
      }
      // Cross-source FTS hit.
      const search = await drive(opts.binary, 'c10', ['--search', 'golden'], { home, cwd: home, driver: opts.driver, timeoutMs: 90000 });
      const out = parseJsonOutput(search.stdout);
      assert.ok(Array.isArray(out) && out.length > 0);
      const sources = new Set(out.map(h => h.message.source));
      assert.ok(sources.size >= 3, `cross-source hits: ${[...sources].join(',')}`);
      db.close();
    },
  },
  {
    id: 'C11',
    title: '错误路径（坏旗标/空库/坏文件/坏查询文件）',
    async run(opts) {
      const home = await makeCorpusHome(this, { scale: 1 });
      const run = (args, timeoutMs = 30000) => drive(opts.binary, 'c11', args, { home, cwd: home, driver: opts.driver, timeoutMs });
      const usage = await run(['--nonsense']);
      assert.equal(usage.exitCode, 1);
      assert.ok(usage.stderr.includes('Usage:'), usage.stderr);
      const searchNoText = await run(['--search']);
      assert.equal(searchNoText.exitCode, 1, 'missing search text → usage');
      // Empty DB (no build): search answers an empty array.
      const empty = await run(['--search', 'anything'], 60000);
      assert.equal(empty.exitCode, 0, `empty search: ${empty.stdout}${empty.stderr}`);
      const emptyOut = parseJsonOutput(empty.stdout);
      assert.deepEqual(emptyOut, []);
      // Malformed JSONL lines do not break the build; good lines persist.
      const projectsDir = join(home, '.claude', 'projects', '-bad-proj');
      mkdirSync(projectsDir, { recursive: true });
      writeFileSync(join(projectsDir, 'bad.jsonl'), 'not-json\n' + JSON.stringify({
        uuid: 'good-u1', type: 'user', timestamp: '2026-06-12T11:00:00Z', cwd: '/bad',
        message: { role: 'user', content: 'still indexable' },
      }) + '\n');
      const build = await run(['--build'], 90000);
      assert.equal(build.exitCode, 0, `build tolerates malformed lines: ${build.stdout}`);
      const db = openDb(home);
      const good = db.prepare("SELECT COUNT(*) AS c FROM messages WHERE uuid='good-u1'").get().c;
      assert.equal(good, 1, 'the good line after garbage is indexed');
      db.close();
      // Nonexistent query file: honest error JSON, exit 1.
      const missing = await run(['--query', join(home, 'does-not-exist.js')]);
      assert.equal(missing.exitCode, 1);
      const payload = JSON.parse(missing.stdout.trim().split('\n')[0]);
      assert.ok(typeof payload.error === 'string' && payload.error.includes('does-not-exist'), payload.error);
    },
  },
  {
    id: 'C12',
    title: 'TS 建库→Rust 接续（跨实现兼容）',
    async run(opts) {
      if (!opts.compare) return; // requires both binaries
      const tsBinary = isTsBinary(opts.binary) ? opts.binary : opts.compare;
      const rustBinary = isTsBinary(opts.binary) ? opts.compare : opts.binary;
      const home = await makeCorpusHome(this, { scale: 1 });
      // Phase 1: TS builds the index and one memory.
      const build1 = await drive(tsBinary, 'c12', ['--build'], { home, cwd: home, driver: 'direct', timeoutMs: 90000 });
      assert.equal(build1.exitCode, 0, build1.stdout);
      const memoryFile = join(home, 'handover-memory.md');
      writeFileSync(memoryFile, 'handover memory body\n');
      const remember = join(home, 'remember.js');
      writeFileSync(remember, 'return await remember({ path: ' + JSON.stringify(memoryFile) + ', summary: "handover memory for cross-implementation handover" });\n');
      const r1 = await drive(tsBinary, 'c12', ['--attune', remember], { home, cwd: home, driver: 'direct', timeoutMs: 90000 });
      assert.equal(r1.exitCode, 0, r1.stdout + r1.stderr);
      const dumpTs = dumpIndex(home);
      // Phase 2: Rust continues on the same HOME: build (incremental), query,
      // attune — the dump must stay row-stable.
      const build2 = await drive(rustBinary, 'c12', ['--build'], { home, cwd: home, driver: 'direct', timeoutMs: 90000 });
      assert.equal(build2.exitCode, 0, build2.stdout);
      const recall = join(home, 'recall.js');
      writeFileSync(recall, 'return await memories({ query: "handover cross-implementation" });\n');
      const r2 = await drive(rustBinary, 'c12', ['--query', recall], { home, cwd: home, driver: 'direct', timeoutMs: 90000 });
      assert.equal(r2.exitCode, 0, r2.stdout + r2.stderr);
      const hits = parseJsonOutput(r2.stdout);
      assert.ok(Array.isArray(hits) && hits.length > 0, 'Rust recalls the TS-written memory');
      const dumpRust = dumpIndex(home);
      assert.equal(dumpRust, dumpTs, 'cross-implementation dump is row-stable');
    },
  },
];

function repoRoot() {
  return join(new URL('.', import.meta.url).pathname, '..', '..', '..');
}
