// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// E2E-CLI runner: executes the scenario modules in tests/e2e/scenarios/
// against one binary (--binary) and optionally a second (--compare),
// diffing normalized outputs. See SCENARIOS.md for the traceability matrix.

import { readdirSync, writeFileSync, mkdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

function parseArgs(argv) {
  const out = { filter: null, driver: 'direct', compare: null, binary: null, keep: false };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--binary') out.binary = argv[++i];
    else if (argv[i] === '--compare') out.compare = argv[++i];
    else if (argv[i] === '--filter') out.filter = argv[++i].split(',');
    else if (argv[i] === '--driver') out.driver = argv[++i];
    else if (argv[i] === '--keep') out.keep = true;
  }
  return out;
}

const opts = parseArgs(process.argv.slice(2));
if (!opts.binary) {
  console.error('Usage: node tests/e2e/run-e2e.mjs --binary <path> [--compare <path>] [--filter C1,C4] [--driver tmux|direct]');
  process.exit(2);
}

const scenarioFiles = readdirSync(join(here, 'scenarios'))
  .filter(f => f.endsWith('.mjs'))
  .sort();

const results = [];
let failures = 0;

for (const file of scenarioFiles) {
  const mod = await import(pathToFileURL(join(here, 'scenarios', file)).href);
  const scenarios = Array.isArray(mod.default) ? mod.default : [mod.default];
  for (const scenario of scenarios) {
    if (opts.filter && !opts.filter.includes(scenario.id)) continue;
    const started = Date.now();
    let status = 'PASS';
    let detail = '';
    try {
      await runWithRetries(scenario, opts);
    } catch (error) {
      status = 'FAIL';
      detail = error.message;
      failures += 1;
    }
    const elapsed = Date.now() - started;
    results.push({ id: scenario.id, title: scenario.title, status, elapsed, detail });
    console.log(`${status === 'PASS' ? '✔' : '✖'} ${scenario.id} ${scenario.title} (${elapsed}ms)${detail ? `\n    ${detail.split('\n')[0]}` : ''}`);
  }
}

console.log(`\n${results.filter(r => r.status === 'PASS').length}/${results.length} passed`);
if (failures > 0) {
  mkdirSync(join(here, 'evidence'), { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  writeFileSync(join(here, 'evidence', `run-${stamp}.json`), JSON.stringify(results, null, 2));
  process.exit(1);
}

async function runWithRetries(scenario, options) {
  let lastError;
  for (let attempt = 1; attempt <= 3; attempt++) {
    try {
      await scenario.run.call(scenario, options);
      return;
    } catch (error) {
      lastError = error;
      if (attempt < 3) await new Promise(r => setTimeout(r, 200 * attempt));
    } finally {
      const { runCleanups } = await import(pathToFileURL(join(here, 'harness.mjs')).href);
      runCleanups();
    }
  }
  throw lastError;
}
