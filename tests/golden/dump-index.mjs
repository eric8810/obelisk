#!/usr/bin/env node
// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Canonical index dump for the golden corpus (Rust-migration M1.0).
//
//   node tests/golden/dump-index.mjs --home <dir> [--out file]
//
// Opens <dir>/.obelisk/obelisk.sqlite strictly READ-ONLY (node:sqlite
// DatabaseSync with readOnly) and prints — or writes with --out — a canonical
// JSON dump of every table. This is the TS-oracle golden: the Rust CLI must
// produce a byte-identical dump for the same corpus.
//
// Normalization rules (the dump MUST be byte-identical across runs of the same
// corpus even though file mtimes/inodes differ):
//
// 1. Tables are emitted in a fixed order: sessions, messages, tool_calls,
//    tool_results, subagents, workflows, workflow_agents, index_state,
//    summaries, memories. Rows are sorted by primary key; object keys use the
//    schema column order.
// 2. Path columns whose values live under the corpus HOME (sessions.jsonl_path,
//    index_state.jsonl_path) are relativized: the absolute home prefix is
//    replaced with '<HOME>' so the dump does not depend on the temp directory.
// 3. index_state: volatile fields are dropped. For every row only
//    {jsonl_path, lines_processed} is emitted. mtime (file mtime wall-clock)
//    and cursor (embeds mtime + inode + ctime) are excluded; lines_processed
//    is deterministic (line/frame counts or 0). System marker rows —
//    __last_build__, __app_heartbeat__, __fts_triggers_ready__,
//    __project_path_backfill_v1__ and any other __…__ marker — are excluded,
//    EXCEPT the five provider index-version markers, which are kept:
//    __claude_canonical_transcript_v2__, __codex_canonical_transcript_v3__,
//    __deepseek_canonical_transcript_v2__, __kimi_canonical_transcript_v6__,
//    __pi_canonical_transcript_v9__.
//
// Everything else (uuids, ids, timestamps, token counts, cwd/file paths inside
// the synthetic corpus, project slugs, visibility) is emitted verbatim: the
// corpus generator makes it fully deterministic.

import { DatabaseSync } from 'node:sqlite';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';

const HOME_PLACEHOLDER = '<HOME>';

// Provider index-version markers to KEEP in index_state (from
// packages/core/src/providers/*.ts indexVersionMarker). All other __…__ rows
// are volatile system markers (build debounce, app heartbeat, FTS readiness,
// backfill bookkeeping) and are excluded.
const PROVIDER_INDEX_MARKERS = new Set([
  '__claude_canonical_transcript_v2__',
  '__codex_canonical_transcript_v3__',
  '__deepseek_canonical_transcript_v2__',
  '__kimi_canonical_transcript_v6__',
  '__pi_canonical_transcript_v9__',
]);

// table → { pk, columns (schema order), homeRelative: columns to relativize }
const TABLES = [
  { name: 'sessions', pk: 'id', columns: ['id', 'title', 'project', 'project_path', 'started_at', 'ended_at', 'git_branch', 'version', 'message_count', 'jsonl_path', 'source'], homeRelative: new Set(['jsonl_path']) },
  { name: 'messages', pk: 'uuid', columns: ['uuid', 'session_id', 'type', 'parent_uuid', 'timestamp', 'role', 'text', 'content_type', 'is_meta', 'visibility', 'model', 'is_sidechain', 'agent_id', 'input_tokens', 'output_tokens', 'cwd', 'skill', 'turn_duration_ms', 'source'], homeRelative: new Set() },
  { name: 'tool_calls', pk: 'id', columns: ['id', 'message_uuid', 'session_id', 'name', 'presentation', 'input_json', 'file_path'], homeRelative: new Set() },
  { name: 'tool_results', pk: 'tool_use_id', columns: ['tool_use_id', 'message_uuid', 'session_id', 'content', 'file_path', 'is_error'], homeRelative: new Set() },
  { name: 'subagents', pk: 'agent_id', columns: ['agent_id', 'session_id', 'parent_tool_use_id', 'agent_type', 'description', 'duration_ms', 'total_tokens'], homeRelative: new Set() },
  { name: 'workflows', pk: 'run_id', columns: ['run_id', 'session_id', 'parent_tool_use_id', 'task_id', 'script', 'result_json', 'timestamp', 'agent_count', 'duration_ms', 'total_tokens', 'status', 'workflow_name'], homeRelative: new Set() },
  { name: 'workflow_agents', pk: 'agent_id', columns: ['agent_id', 'run_id', 'session_id', 'agent_type', 'description', 'phase', 'label', 'model', 'state', 'duration_ms', 'tokens', 'tool_calls'], homeRelative: new Set() },
  { name: 'index_state', pk: 'jsonl_path', columns: ['jsonl_path', 'lines_processed'], homeRelative: new Set(['jsonl_path']) },
  { name: 'summaries', pk: 'id', columns: ['id', 'session_id', 'timestamp', 'source', 'content', 'visibility', 'input_tokens', 'output_tokens'], homeRelative: new Set() },
  { name: 'memories', pk: 'id', columns: ['id', 'session_id', 'project', 'message_start', 'message_end', 'path', 'anchors', 'summary', 'created_at', 'deleted_at', 'deleted_reason'], homeRelative: new Set() },
];

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--home') out.home = argv[++i];
    else if (arg === '--out') out.out = argv[++i];
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (!out.home) throw new Error('usage: dump-index.mjs --home <dir> [--out file]');
  return out;
}

function relativize(value, home) {
  if (typeof value !== 'string') return value;
  if (value === home) return HOME_PLACEHOLDER;
  if (value.startsWith(home + '/')) return HOME_PLACEHOLDER + value.slice(home.length);
  return value;
}

function dump(home) {
  const dbPath = join(home, '.obelisk', 'obelisk.sqlite');
  const db = new DatabaseSync(dbPath, { readOnly: true });
  try {
    const result = {};
    for (const table of TABLES) {
      const columns = table.columns.join(', ');
      const rows = db.prepare(`SELECT ${columns} FROM ${table.name} ORDER BY ${table.pk}`).all();
      if (table.name === 'index_state') {
        result[table.name] = rows
          .filter((row) => !row.jsonl_path.startsWith('__') || PROVIDER_INDEX_MARKERS.has(row.jsonl_path))
          .map((row) => ({
            jsonl_path: relativize(row.jsonl_path, home),
            lines_processed: row.lines_processed,
          }));
      } else {
        result[table.name] = rows.map((row) => {
          const record = {};
          for (const column of table.columns) {
            record[column] = table.homeRelative.has(column)
              ? relativize(row[column], home)
              : row[column];
          }
          return record;
        });
      }
    }
    return result;
  } finally {
    db.close();
  }
}

const { home, out } = parseArgs(process.argv.slice(2));
const json = JSON.stringify(dump(home), null, 2) + '\n';
if (out) {
  writeFileSync(out, json);
} else {
  process.stdout.write(json);
}
