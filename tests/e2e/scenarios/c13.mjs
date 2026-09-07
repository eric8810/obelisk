// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Scenario C13: real daemon arbitration (ADR-0013 M3.1). The GPUI app's
// resident daemon writes the __app_heartbeat__ marker; while it is fresh the
// CLI's mutations skip with daemon_active, searches stay available, and
// after the daemon exits the marker ages out and builds recover. This is
// C6's injected-marker scenario replayed against the real owner process.

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { makeCorpusHome, drive, parseJsonOutput } from '../harness.mjs';

export default [
  {
    id: 'C13',
    title: '真实 daemon 心跳仲裁（M3.1 写权移交）',
    async run(opts) {
      // The scenario drives the real GPUI daemon: it needs the release app
      // binary and a display. On machines without either, skip (mirrors
      // C12's compare-binary guard).
      const appBinary = join(repoRoot(), 'target', 'release', 'obelisk-app');
      if (!existsSync(appBinary) || !process.env.DISPLAY) {
        console.log('C13: skipped (no app binary or no display)');
        return;
      }

      const home = await makeCorpusHome(this, { scale: 1 });
      const run = (args, timeoutMs) =>
        drive(opts.binary, 'c13', args, { home, cwd: home, driver: 'direct', timeoutMs });
      const dbPath = join(home, '.obelisk', 'obelisk.sqlite');

      // The index must exist before the daemon can claim it.
      const first = await run(['--build'], 90000);
      assert.equal(first.exitCode, 0, first.stdout);

      // Start the app daemon on this isolated HOME.
      const app = spawn(appBinary, [], {
        env: { ...process.env, HOME: home },
        stdio: ['ignore', 'ignore', 'ignore'],
        detached: false,
      });
      try {
        // Startup writes the first heartbeat; give it a moment to settle.
        await sleep(4000);
        const marker = heartbeatAge(dbPath);
        assert.ok(marker !== null && marker < 60000, `daemon heartbeat is fresh (age ${marker}ms)`);

        // Mutations are daemon-owned while the marker is fresh…
        const owned = await run(['--build'], 90000);
        assert.equal(owned.exitCode, 0);
        assert.ok(
          parseJsonOutput(owned.stdout).error?.includes('daemon_active'),
          `expected daemon_active, got ${owned.stdout}`,
        );

        // …but read paths keep working.
        const search = await run(['--search', 'actor'], 90000);
        assert.equal(search.exitCode, 0, search.stdout);
        assert.ok(Array.isArray(parseJsonOutput(search.stdout)));

        // Daemon death does not free the marker instantly (TS parity: the
        // 60s freshness window ages it out)…
      } finally {
        process.kill(app.pid, 'SIGKILL');
      }
      const stillOwned = await run(['--build'], 90000);
      assert.ok(
        parseJsonOutput(stillOwned.stdout).error?.includes('daemon_active'),
        'fresh marker after daemon death still owns builds',
      );

      // …and once it ages out, CLI builds recover.
      const sqlite = spawnSync('sqlite3', [dbPath,
        "UPDATE index_state SET mtime = mtime - 120000 WHERE jsonl_path = '__app_heartbeat__'"]);
      assert.equal(sqlite.status, 0, sqlite.stderr?.toString());
      const recovered = await run(['--build'], 90000);
      assert.equal(recovered.exitCode, 0, recovered.stdout);
      assert.equal(parseJsonOutput(recovered.stdout).ok, true);
    },
  },
];

function heartbeatAge(dbPath) {
  const probe = spawnSync('sqlite3', [dbPath,
    "SELECT round((strftime('%s','now')*1000 - mtime)) FROM index_state WHERE jsonl_path='__app_heartbeat__'"]);
  if (probe.status !== 0) return null;
  const value = probe.stdout.toString().trim();
  return value === '' ? null : Number(value);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function repoRoot() {
  return join(new URL('.', import.meta.url).pathname, '..', '..', '..');
}
