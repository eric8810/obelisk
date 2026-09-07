#!/usr/bin/env node
// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

// Deterministic synthetic five-provider corpus generator (Rust-migration M1.0).
//
//   node tests/golden/generate-corpus.mjs --out <dir> [--scale N]
//        [--big-messages M] [--seed S]
//
// Writes a synthetic transcript corpus into <dir> such that, with HOME=<dir>/home,
// the Obelisk CLI discovers all five providers:
//
//   claude    <home>/.claude/projects/-home-synth-obelisk-golden-claude/...
//   codex     <home>/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl (+ session_index.jsonl)
//   deepseek  <home>/.dsh/sessions/--home-synth-obelisk-golden-deepseek--/<id>/session.jsonl.zstd
//   kimi      <home>/.kimi-code/sessions/<workspace>/<session>/agents/{main,agent-7}/wire.jsonl
//   pi        <home>/.pi/agent/sessions/<dir>/session.jsonl
//
// Determinism contract: identical arguments produce byte-identical files. All
// content derives from a seeded mulberry32 PRNG, fixed counters, and a fixed
// epoch (2026-06-10T10:00:00Z); Date.now() is never used for generated content.
// File mtimes/inodes vary but the index dump (dump-index.mjs) normalizes them
// away, so the golden expected-dump.json stays byte-stable across runs.
//
// Scale semantics: --scale N replicates the scale-1 shape N times with distinct
// ids/timestamps, and grows per-session turn counts moderately
// (extraTurns = min(48, floor((N-1)/6))). --big-messages M (default 0) adds one
// extra claude session with M messages to stress long transcripts.
//
// Everything is synthetic (lorem-style coding filler). No real user data.

import { mkdirSync, rmSync, writeFileSync, existsSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { constants, zstdCompressSync } from 'node:zlib';

// ---- args ----

function parseArgs(argv) {
  const out = { scale: 1, bigMessages: 0, seed: 0x0be11ace };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--out') out.out = argv[++i];
    else if (arg === '--scale') out.scale = Number(argv[++i]);
    else if (arg === '--big-messages') out.bigMessages = Number(argv[++i]);
    else if (arg === '--seed') out.seed = Number(argv[++i]);
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (!out.out) throw new Error('usage: generate-corpus.mjs --out <dir> [--scale N] [--big-messages M] [--seed S]');
  if (!Number.isInteger(out.scale) || out.scale < 1) throw new Error('--scale must be a positive integer');
  if (!Number.isInteger(out.bigMessages) || out.bigMessages < 0) throw new Error('--big-messages must be >= 0');
  if (!Number.isInteger(out.seed)) throw new Error('--seed must be an integer');
  return out;
}

// ---- deterministic primitives ----

function mulberry32(a) {
  return function () {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const BASE_MS = Date.UTC(2026, 5, 10, 10, 0, 0); // 2026-06-10T10:00:00Z — fixed epoch
const iso = (ms) => new Date(ms).toISOString();
const pad = (n, w) => String(n).padStart(w, '0');

function uuidFromRng(rng) {
  const hex = (n) => {
    let out = '';
    for (let i = 0; i < n; i++) out += '0123456789abcdef'[Math.floor(rng() * 16)];
    return out;
  };
  return `${hex(8)}-${hex(4)}-4${hex(3)}-8${hex(3)}-${hex(12)}`;
}

const WORDS = [
  'golden', 'parser', 'session', 'index', 'cursor', 'refactor', 'module', 'probe',
  'fixture', 'transcript', 'sqlite', 'schema', 'token', 'prompt', 'agent', 'tool',
  'branch', 'compact', 'sentinel', 'retry', 'cache', 'vector', 'prompt-cache',
];
function synthSentence(rng, tag, n = 12) {
  const parts = [];
  for (let i = 0; i < n; i++) parts.push(WORDS[Math.floor(rng() * WORDS.length)]);
  return `${tag}: ${parts.join(' ')}.`;
}

function writeJsonl(path, records) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, records.map((r) => JSON.stringify(r)).join('\n') + '\n');
}

function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify(value));
}

// The deepseek artifact is a concatenation of independent checksummed zstd
// frames (one frame per append batch) — mirrors deepseek-tree.test.mjs mkFrame.
function zstdFrame(lines) {
  return zstdCompressSync(
    Buffer.from(lines.map((line) => JSON.stringify(line)).join('\n') + '\n'),
    { params: { [constants.ZSTD_c_checksumFlag]: 1 } },
  );
}

// ---- corpus bookkeeping ----

const stats = {
  providers: {
    claude: { sessions: 0, messages: 0, tool_calls: 0, files: [] },
    codex: { sessions: 0, messages: 0, tool_calls: 0, files: [] },
    deepseek: { sessions: 0, messages: 0, tool_calls: 0, files: [] },
    kimi: { sessions: 0, messages: 0, tool_calls: 0, files: [] },
    pi: { sessions: 0, messages: 0, tool_calls: 0, files: [] },
  },
};
function note(provider, fields) {
  const s = stats.providers[provider];
  s.sessions += fields.sessions ?? 0;
  s.messages += fields.messages ?? 0;
  s.tool_calls += fields.tool_calls ?? 0;
}

// ---- claude ----

function generateClaude(home, { scale, extraTurns, bigMessages }, rng) {
  const root = join(home, '.claude');
  const projectSlug = '-home-synth-obelisk-golden-claude';
  const projectDir = join(root, 'projects', projectSlug);
  const cwd = '/home/synth/obelisk-golden/claude';
  const historyEntries = [];
  mkdirSync(projectDir, { recursive: true });

  for (let copy = 1; copy <= scale; copy++) {
    const t0 = BASE_MS + (copy - 1) * 6 * 3600_000;
    const idA = `claude-sess-${pad(copy, 4)}-a-${uuidFromRng(rng).slice(0, 8)}`;
    const idB = `claude-sess-${pad(copy, 4)}-b-${uuidFromRng(rng).slice(0, 8)}`;
    const turns = 5 + extraTurns;
    const sid = (n) => `cl${copy}${n}`;

    // --- session A: the full-featured session (ai-title wins over history) ---
    const lines = [];
    const push = (o) => lines.push(o);
    const t = (sec) => t0 + sec * 1000;

    push({ type: 'ai-title', aiTitle: `Golden Claude Session ${pad(copy, 3)}` });
    push({
      uuid: `${sid('u0')}`, type: 'user', timestamp: iso(t(1)), cwd, gitBranch: 'golden-main', version: '1.0.21',
      message: { role: 'user', content: `golden claude corpus: ${synthSentence(rng, 'inspect')}` },
    });
    // Meta message with a <command-name> envelope (isMeta + envelope regex).
    push({
      uuid: `${sid('m1')}`, type: 'user', timestamp: iso(t(2)), cwd, gitBranch: 'golden-main', version: '1.0.21', isMeta: true,
      message: { role: 'user', content: '<command-name>/compact</command-name>\n<command-contents></command-contents>' },
    });
    // Skill-instructions meta message (content_type 'skill_instructions').
    push({
      uuid: `${sid('m2')}`, type: 'user', timestamp: iso(t(3)), cwd, gitBranch: 'golden-main', version: '1.0.21', isMeta: true,
      message: { role: 'user', content: `Base directory for this skill: /home/synth/skills/golden\n${synthSentence(rng, 'skill')}` },
    });

    let toolCallsA = 0;
    let messagesA = 3;
    const turnBlock = (sec, turn, { task, workflow } = {}) => {
      const asUuid = `${sid(`as${turn}`)}`;
      const content = [
        { type: 'thinking', thinking: synthSentence(rng, 'think', 8) },
        { type: 'text', text: `golden claude reply ${turn}: ${synthSentence(rng, 'answer', 10)}` },
      ];
      const resultLines = [];
      if (task) {
        content.push({ type: 'tool_use', id: `${sid(`tu-task`)}`, name: 'Task', input: { subagent_type: 'explorer', description: 'Explore the golden corpus structure', prompt: synthSentence(rng, 'task', 10) } });
        toolCallsA += 1;
        resultLines.push({ type: 'tool_result', tool_use_id: `${sid('tu-task')}`, content: `agent-77 explored the golden corpus` });
      } else if (workflow) {
        content.push({ type: 'tool_use', id: `${sid('tu-wf')}`, name: 'Workflow', input: { workflow: 'golden-review', input: synthSentence(rng, 'flow', 8) } });
        toolCallsA += 1;
        resultLines.push({ type: 'tool_result', tool_use_id: `${sid('tu-wf')}`, content: `Run ID: ${sid('wf-run')}\nSummary: golden workflow complete` });
      } else {
        content.push({ type: 'tool_use', id: `${sid(`tu-r${turn}`)}`, name: 'Read', input: { file_path: `${cwd}/src/module${turn}.ts` } });
        content.push({ type: 'tool_use', id: `${sid(`tu-s${turn}`)}`, name: 'Skill', input: { skill: 'golden-review', args: synthSentence(rng, 'args', 6) } });
        content.push({ type: 'tool_use', id: `${sid(`tu-e${turn}`)}`, name: 'Edit', input: { file_path: `${cwd}/src/module${turn}.ts`, old_string: 'before', new_string: 'after' } });
        toolCallsA += 3;
        resultLines.push({ type: 'tool_result', tool_use_id: `${sid(`tu-r${turn}`)}`, content: [{ type: 'text', text: `module ${turn} golden contents\n${synthSentence(rng, 'file', 8)}` }], is_error: turn === 2 });
        resultLines.push({ type: 'tool_result', tool_use_id: `${sid(`tu-s${turn}`)}`, content: 'golden skill ran fine' });
        resultLines.push({ type: 'tool_result', tool_use_id: `${sid(`tu-e${turn}`)}`, content: 'the file has been updated' });
      }
      push({
        uuid: asUuid, type: 'assistant', timestamp: iso(t(sec)), cwd, gitBranch: 'golden-main', version: '1.0.21',
        message: {
          role: 'assistant', model: 'claude-sonnet-4-6', content,
          usage: { input_tokens: 10 + turn, output_tokens: 5 + turn, cache_creation_input_tokens: 20 + turn, cache_read_input_tokens: 30 + turn },
        },
      });
      messagesA += 1;
      push({
        uuid: `${sid(`ur${turn}`)}`, type: 'user', timestamp: iso(t(sec + 1)), cwd, gitBranch: 'golden-main', version: '1.0.21',
        toolUseResult: { filePath: `${cwd}/src/module${turn}.ts` },
        message: { role: 'user', content: resultLines },
      });
      messagesA += 1;
      push({
        uuid: `${sid(`uf${turn}`)}`, type: 'user', timestamp: iso(t(sec + 2)), cwd, gitBranch: 'golden-main', version: '1.0.21',
        message: { role: 'user', content: `golden claude follow-up ${turn}: ${synthSentence(rng, 'next', 8)}` },
      });
      messagesA += 1;
      push({ type: 'system', subtype: 'turn_duration', parentUuid: asUuid, durationMs: 900 + turn * 37 });
    };

    for (let turn = 1; turn <= turns; turn++) {
      const sec = 10 + turn * 10;
      const isTaskTurn = turn === turns - 1;
      const isWorkflowTurn = turn === turns;
      turnBlock(sec, turn, { task: isTaskTurn, workflow: isWorkflowTurn });
    }
    // Away summary after the conversation.
    push({ type: 'system', subtype: 'away_summary', uuid: `${sid('away')}`, timestamp: iso(t(10 + (turns + 1) * 10)), content: `away summary: ${synthSentence(rng, 'away', 10)}` });

    writeJsonl(join(projectDir, `${idA}.jsonl`), lines);
    historyEntries.push({ sessionId: idA, title: `Golden Claude History ${pad(copy, 3)}`, display: 'user', project: cwd });

    // --- plain subagent transcript + meta (parent_tool_use_id from meta) ---
    const subagentDir = join(projectDir, idA, 'subagents');
    const subLines = [];
    for (let m = 0; m < 4; m++) {
      const role = m % 2 === 0 ? 'user' : 'assistant';
      subLines.push({
        uuid: `${sid(`sa${m}`)}`, type: role, timestamp: iso(t(600 + m * 10)), cwd,
        ...(role === 'assistant'
          ? { message: { role: 'assistant', model: 'claude-haiku-4-5', content: [{ type: 'text', text: `golden subagent step ${m}: ${synthSentence(rng, 'sub', 8)}` }], usage: { input_tokens: 7 + m, output_tokens: 3 + m, cache_creation_input_tokens: 11, cache_read_input_tokens: 13 } } }
          : { message: { role: 'user', content: `golden subagent prompt ${m}: ${synthSentence(rng, 'subq', 8)}` } }),
      });
    }
    writeJsonl(join(subagentDir, 'agent-77.jsonl'), subLines);
    writeJson(join(subagentDir, 'agent-77.meta.json'), {
      toolUseId: `${sid('tu-task')}`,
      agentType: 'explorer',
      description: 'Explore the golden corpus structure',
    });

    // --- workflow run + workflow-linked agent transcript ---
    const runId = `${sid('wf-run')}`;
    const workflowDir = join(projectDir, idA, 'workflows');
    writeJson(join(workflowDir, `${runId}.json`), {
      runId,
      workflowName: 'Golden Review',
      status: 'completed',
      taskId: `${sid('wf-task')}`,
      script: 'review the golden corpus',
      timestamp: iso(t(700)),
      durationMs: 45_000 + copy,
      totalTokens: 12_345 + copy,
      result: { summary: 'golden workflow result', findings: 3 },
      workflowProgress: [
        { type: 'workflow_agent', agentId: '11', phaseTitle: 'review', label: 'Reviewer', model: 'claude-sonnet-4-6', state: 'completed', durationMs: 1_000 + copy, tokens: 500 + copy, toolCalls: 3 },
        { type: 'workflow_agent', agentId: '12', phaseTitle: 'verify', label: 'Verifier', model: 'claude-haiku-4-5', state: 'completed', durationMs: 800 + copy, tokens: 300 + copy, toolCalls: 2 },
      ],
    });
    const wfAgentDir = join(projectDir, idA, 'subagents', 'workflows', runId);
    writeJsonl(join(wfAgentDir, 'agent-11.jsonl'), [
      { uuid: `${sid('wa0')}`, type: 'user', timestamp: iso(t(710)), cwd, message: { role: 'user', content: 'review the golden implementation' } },
      { uuid: `${sid('wa1')}`, type: 'assistant', timestamp: iso(t(720)), cwd, message: { role: 'assistant', content: [{ type: 'text', text: 'golden review complete, no findings' }] } },
    ]);
    writeJson(join(wfAgentDir, 'agent-11.meta.json'), { agentType: 'reviewer', description: 'Review the golden implementation' });

    // --- session B: no ai-title line, title comes from history.jsonl ---
    const linesB = [];
    const turnsB = 2 + Math.min(extraTurns, 4);
    linesB.push({ uuid: `${sid('bu0')}`, type: 'user', timestamp: iso(t(800)), cwd, gitBranch: 'golden-main', version: '1.0.21', message: { role: 'user', content: `golden claude corpus: ${synthSentence(rng, 'second')}` } });
    let toolCallsB = 0;
    for (let turn = 1; turn <= turnsB; turn++) {
      linesB.push({
        uuid: `${sid(`bas${turn}`)}`, type: 'assistant', timestamp: iso(t(810 + turn * 10)), cwd, gitBranch: 'golden-main', version: '1.0.21',
        message: {
          role: 'assistant', model: 'claude-sonnet-4-6',
          content: [
            { type: 'text', text: `golden claude b reply ${turn}: ${synthSentence(rng, 'answer', 8)}` },
            { type: 'tool_use', id: `${sid(`btu${turn}`)}`, name: 'Read', input: { file_path: `${cwd}/docs/note${turn}.md` } },
          ],
          usage: { input_tokens: 8 + turn, output_tokens: 4 + turn },
        },
      });
      linesB.push({
        uuid: `${sid(`bur${turn}`)}`, type: 'user', timestamp: iso(t(811 + turn * 10)), cwd, gitBranch: 'golden-main', version: '1.0.21',
        message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: `${sid(`btu${turn}`)}`, content: `golden note ${turn} body` }] },
      });
      toolCallsB += 1;
    }
    writeJsonl(join(projectDir, `${idB}.jsonl`), linesB);
    historyEntries.push({ sessionId: idB, title: `Golden Claude History B ${pad(copy, 3)}`, display: 'user', project: cwd });

    note('claude', {
      sessions: 2,
      messages: messagesA + 4 + 2 + linesB.filter((l) => l.uuid).length,
      tool_calls: toolCallsA + toolCallsB,
    });
  }

  // --- one very long session to stress long transcripts ---
  if (bigMessages > 0) {
    const count = bigMessages - (bigMessages % 2);
    const id = `claude-sess-big-${pad(count, 6)}`;
    const t0 = BASE_MS + scale * 6 * 3600_000;
    const lines = [];
    for (let m = 0; m < count; m++) {
      const role = m % 2 === 0 ? 'user' : 'assistant';
      lines.push({
        uuid: `clbig${pad(m, 6)}`, type: role, timestamp: iso(t0 + (m + 1) * 2000), cwd,
        gitBranch: 'golden-main', version: '1.0.21',
        message: {
          role,
          content: `golden claude long transcript ${m}: ${synthSentence(rng, 'long', 10)}`,
          ...(role === 'assistant' ? { model: 'claude-sonnet-4-6', usage: { input_tokens: 10, output_tokens: 5 } } : {}),
        },
      });
    }
    writeJsonl(join(projectDir, `${id}.jsonl`), lines);
    historyEntries.push({ sessionId: id, title: 'Golden Claude Long Session', display: 'user', project: '/home/synth/obelisk-golden/claude' });
    note('claude', { sessions: 1, messages: count });
  }

  writeJsonl(join(root, 'history.jsonl'), historyEntries);
}

// ---- codex ----

function generateCodex(home, { scale, extraTurns }, rng) {
  const root = join(home, '.codex');
  const cwd = '/home/synth/obelisk-golden/codex';
  const sessionsDir = join(root, 'sessions');
  const sessionIndex = [];
  const turns = 4 + extraTurns;

  for (let copy = 1; copy <= scale; copy++) {
    const t0 = BASE_MS + (copy - 1) * 6 * 3600_000;
    const cid = (n) => `cx${copy}${n}`;
    const threadId = uuidFromRng(rng);
    const childThreadId = uuidFromRng(rng);
    const day = 10 + ((copy - 1) % 20);
    const dir = join(sessionsDir, '2026', '06', pad(day, 2));
    mkdirSync(dir, { recursive: true });
    const t = (sec) => iso(t0 + sec * 1000);

    // --- root session (a fresh session id namespace per copy) ---
    const lines = [];
    lines.push({ type: 'session_meta', timestamp: t(0), payload: { id: threadId, timestamp: t(0), cwd, cli_version: '0.42.0-golden', git: { branch: 'golden-main' }, originator: 'codex_cli' } });
    // Hidden-context envelope (response_item user message) — visibility 'hidden', is_meta 1.
    lines.push({
      type: 'response_item', timestamp: t(1), payload: {
        type: 'message', role: 'user',
        content: [{ type: 'input_text', text: `<environment_context>\nWorking directory: ${cwd}\nIs directory a git repo: true\nGit branch: golden-main\n</environment_context>` }],
      },
    });
    let messages = 1;
    let toolCalls = 0;
    let agentText;
    for (let turn = 1; turn <= turns; turn++) {
      lines.push({ type: 'turn_context', timestamp: t(turn * 10), payload: { cwd, model: 'gpt-5.3-codex' } });
      lines.push({ type: 'event_msg', timestamp: t(turn * 10 + 1), payload: { type: 'user_message', message: `golden codex corpus ${turn}: ${synthSentence(rng, 'inspect', 10)}` } });
      messages += 1;
      agentText = `golden codex reply ${turn}: ${synthSentence(rng, 'answer', 10)}`;
      lines.push({ type: 'event_msg', timestamp: t(turn * 10 + 2), payload: { type: 'agent_message', message: agentText } });
      messages += 1;
      // Duplicate response_item of the agent_message — exercises event↔item dedup.
      lines.push({ type: 'response_item', timestamp: t(turn * 10 + 2), payload: { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: agentText }] } });
      if (turn % 2 === 1) {
        lines.push({ type: 'response_item', timestamp: t(turn * 10 + 3), payload: { type: 'custom_tool_call', call_id: `${cid(`ct${turn}`)}`, name: 'shell', input: JSON.stringify({ cmd: `ls golden-${turn}` }) } });
        lines.push({ type: 'response_item', timestamp: t(turn * 10 + 4), payload: { type: 'custom_tool_call_output', call_id: `${cid(`ct${turn}`)}`, output: `golden file listing ${turn}\nmodule.ts` } });
      } else {
        lines.push({ type: 'response_item', timestamp: t(turn * 10 + 3), payload: { type: 'function_call', call_id: `${cid(`fc${turn}`)}`, name: 'read_file', arguments: JSON.stringify({ path: `${cwd}/src/mod${turn}.ts` }) } });
        lines.push({ type: 'response_item', timestamp: t(turn * 10 + 4), payload: { type: 'function_call_output', call_id: `${cid(`fc${turn}`)}`, output: `golden file body ${turn}: export const golden = ${turn};` } });
      }
      messages += 1; // the tool_use message
      toolCalls += 1;
      lines.push({ type: 'event_msg', timestamp: t(turn * 10 + 5), payload: { type: 'token_count', info: { last_token_usage: { input_tokens: 100 + turn, output_tokens: 40 + turn } } } });
      lines.push({ type: 'event_msg', timestamp: t(turn * 10 + 6), payload: { type: 'task_complete', duration_ms: 900 * turn + 17 } });
    }
    // Subagent spawn: an Agent tool call that started a child thread.
    lines.push({ type: 'event_msg', timestamp: t(turns * 10 + 10), payload: { type: 'collab_agent_spawn_end', call_id: `${cid('spawn')}`, new_thread_id: childThreadId, new_agent_role: 'explorer', new_agent_nickname: 'Golden Explorer', prompt: `explore the golden corpus ${synthSentence(rng, 'spawn', 6)}`, model: 'gpt-5.3-codex', reasoning_effort: 'medium' } });
    messages += 1;
    toolCalls += 1;
    lines.push({ type: 'event_msg', timestamp: t(turns * 10 + 11), payload: { type: 'thread_name_updated', thread_name: `Golden Codex Session ${pad(copy, 3)}` } });
    const rootPath = join(dir, `rollout-2026-06-10T10-00-00-${threadId}.jsonl`);
    writeJsonl(rootPath, lines);
    sessionIndex.push({ id: threadId, thread_name: `Golden Codex Session ${pad(copy, 3)}`, updated_at: t(turns * 10 + 12) });

    // --- child/subagent session (sidechain, folds into the parent session) ---
    const childLines = [];
    childLines.push({
      type: 'session_meta', timestamp: t(turns * 10 + 20), payload: {
        id: childThreadId, timestamp: t(turns * 10 + 20), cwd, cli_version: '0.42.0-golden',
        source: { subagent: { thread_spawn: { parent_thread_id: threadId, agent_role: 'explorer', agent_nickname: 'Golden Explorer' } } },
      },
    });
    childLines.push({ type: 'turn_context', timestamp: t(turns * 10 + 21), payload: { cwd, model: 'gpt-5.3-codex' } });
    childLines.push({ type: 'event_msg', timestamp: t(turns * 10 + 22), payload: { type: 'user_message', message: `golden codex subagent prompt: ${synthSentence(rng, 'subq', 8)}` } });
    childLines.push({ type: 'event_msg', timestamp: t(turns * 10 + 23), payload: { type: 'agent_message', message: `golden codex subagent reply: ${synthSentence(rng, 'suba', 8)}` } });
    childLines.push({ type: 'event_msg', timestamp: t(turns * 10 + 24), payload: { type: 'token_count', info: { last_token_usage: { input_tokens: 55, output_tokens: 21 } } } });
    childLines.push({ type: 'event_msg', timestamp: t(turns * 10 + 25), payload: { type: 'task_complete', duration_ms: 4_321 } });
    const childPath = join(dir, `rollout-2026-06-10T11-00-00-${childThreadId}.jsonl`);
    writeJsonl(childPath, childLines);

    note('codex', { sessions: 1, messages: messages + 2, tool_calls: toolCalls });
  }

  writeJsonl(join(root, 'session_index.jsonl'), sessionIndex);
}

// ---- deepseek ----

function generateDeepseek(home, { scale, extraTurns }, rng) {
  const root = join(home, '.dsh', 'sessions');
  const cwd = '/home/synth/obelisk-golden/deepseek';
  const projectDirName = '--home-synth-obelisk-golden-deepseek--';
  const turns = 4 + extraTurns;

  for (let copy = 1; copy <= scale; copy++) {
    const t0 = BASE_MS + (copy - 1) * 6 * 3600_000;
    const did = (n) => `dsh${copy}${n}`;
    const rootId = `dsh-root-${pad(copy, 4)}-${uuidFromRng(rng).slice(0, 8)}`;
    const childId = `dsh-child-${pad(copy, 4)}-${uuidFromRng(rng).slice(0, 8)}`;
    const t = (sec) => t0 + sec * 1000;

    const rootFrames = [];
    const subagentTurn = turns; // last turn spawns the subagent
    rootFrames.push([{ type: 'session', version: 0, id: rootId, createdAt: t(0), cwd, delegationDepth: 0, agentPreset: 'standard' }]);
    rootFrames.push([{ type: 'request/header', seq: 0, time: t(1), data: { header: { config: { provider: 'deepseek-official', model: 'deepseek-v4-flash' } }, reason: 'initial' } }]);
    rootFrames.push([{ type: 'user/message', seq: 1, time: t(2), data: { content: [{ type: 'text', text: `golden deepseek corpus: ${synthSentence(rng, 'inspect', 10)}` }], source: { kind: 'user' }, role: 'user', id: `${did('msg1')}` } }]);
    let seq = 2;
    let messages = 1;
    let toolCalls = 0;
    for (let turn = 1; turn <= turns; turn++) {
      const isSubagentTurn = turn === subagentTurn;
      const callId = `${did(`call${turn}`)}`;
      const content = isSubagentTurn
        ? [{ type: 'text', text: `golden deepseek reply ${turn}: ${synthSentence(rng, 'answer', 8)}` }, { type: 'tool-call', id: callId, name: 'subagent', arguments: JSON.stringify({ prompt: 'review the golden code' }) }]
        : [
            { type: 'reasoning', text: synthSentence(rng, 'reason', 6) },
            { type: 'text', text: `golden deepseek reply ${turn}: ${synthSentence(rng, 'answer', 8)}` },
            { type: 'tool-call', id: callId, name: 'read', arguments: JSON.stringify({ file_path: `${cwd}/src/a${turn}.ts` }) },
          ];
      rootFrames.push([{ type: 'assistant/message', seq: seq++, time: t(10 * turn), data: { turn, step: 1, message: { role: 'assistant', content, source: { kind: 'model', provider: 'deepseek-official', model: 'deepseek-v4-flash' }, id: `${did(`am${turn}`)}` }, usage: { inputTokens: 10 + turn, outputTokens: 4 + turn, cacheReadTokens: 3 + turn } } }]);
      rootFrames.push([{ type: 'tool/call', seq: seq++, time: t(10 * turn + 1), data: { turn, step: 1, callId, name: isSubagentTurn ? 'subagent' : 'read', arguments: isSubagentTurn ? JSON.stringify({ prompt: 'review the golden code' }) : JSON.stringify({ file_path: `${cwd}/src/a${turn}.ts` }) } }]);
      toolCalls += 1;
      const resultContent = isSubagentTurn
        ? [{ type: 'tool-result', toolCallId: callId, content: [{ type: 'text', text: `started subagent ${childId}` }] }]
        : [{ type: 'tool-result', toolCallId: callId, content: [{ type: 'text', text: `golden file body ${turn}: ${synthSentence(rng, 'file', 6)}` }] }];
      const resultData = { turn, step: 1, message: { source: { kind: 'tool', callId }, content: resultContent, role: 'user', id: `${did(`rm${turn}`)}` } };
      if (turn === 2) resultData.error = { message: 'golden tool failure' }; // exercises is_error
      rootFrames.push([{ type: 'tool/result', seq: seq++, time: t(10 * turn + 2), data: resultData }]);
      rootFrames.push([{ type: 'user/message', seq: seq++, time: t(10 * turn + 3), data: { content: [{ type: 'text', text: `golden deepseek follow-up ${turn}: ${synthSentence(rng, 'next', 8)}` }], source: { kind: 'user' }, role: 'user', id: `${did(`um${turn}`)}` } }]);
      messages += isSubagentTurn ? 3 : 4; // assistant text (+reasoning) + follow-up user
    }
    rootFrames.push([{ type: 'session/title', seq, time: t(turns * 10 + 90), data: { title: `Golden DeepSeek Session ${pad(copy, 3)}`, messageSeqs: [1], source: { kind: 'fallback' } } }]);
    mkdirSync(join(root, projectDirName, rootId), { recursive: true });
    writeFileSync(join(root, projectDirName, rootId, 'session.jsonl.zstd'), Buffer.concat(rootFrames.map(zstdFrame)));

    // --- child session of the tree (parentSession links it into the root unit) ---
    const childFrames = [];
    childFrames.push([{ type: 'session', version: 0, id: childId, createdAt: t(turns * 10 + 50), cwd, parentSession: rootId, origin: 'subagent', delegationDepth: 1 }]);
    childFrames.push([
      { type: 'subagent/descriptor', seq: 0, time: t(turns * 10 + 50), data: { version: 2, mode: 'continuable', provider: 'spawn', label: 'golden review helper', agentProvider: 'deepseek-official', agentModel: 'deepseek-v4-flash' } },
      { type: 'user/message', seq: 1, time: t(turns * 10 + 51), data: { content: [{ type: 'text', text: 'review the golden code' }], source: { kind: 'user' }, role: 'user', id: `${did('cm1')}` } },
    ]);
    childFrames.push([{ type: 'assistant/message', seq: 2, time: t(turns * 10 + 52), data: { turn: 1, step: 1, message: { role: 'assistant', content: [{ type: 'reasoning', text: synthSentence(rng, 'creason', 5) }, { type: 'text', text: 'golden child review complete' }], source: { kind: 'model', model: 'deepseek-v4-flash' }, id: `${did('cm2')}` }, usage: { inputTokens: 20, outputTokens: 5 } } }]);
    mkdirSync(join(root, projectDirName, childId), { recursive: true });
    writeFileSync(join(root, projectDirName, childId, 'session.jsonl.zstd'), Buffer.concat(childFrames.map(zstdFrame)));

    note('deepseek', { sessions: 1, messages: messages + 3, tool_calls: toolCalls });
  }
}

// ---- kimi ----

function generateKimi(home, { scale, extraTurns }, rng) {
  const root = join(home, '.kimi-code');
  const cwd = '/home/synth/obelisk-golden/kimi';
  const turns = 4 + extraTurns;

  for (let copy = 1; copy <= scale; copy++) {
    const t0 = BASE_MS + (copy - 1) * 6 * 3600_000;
    const kid = (n) => `km${copy}${n}`;
    const sessionName = `kimi-session-${pad(copy, 4)}-${uuidFromRng(rng).slice(0, 8)}`;
    const workspace = `ws-golden-${1 + ((copy - 1) % 3)}`;
    const sessionDir = join(root, 'sessions', workspace, sessionName);
    const mainDir = join(sessionDir, 'agents', 'main');
    const childDir = join(sessionDir, 'agents', 'agent-7');
    mkdirSync(mainDir, { recursive: true });
    mkdirSync(childDir, { recursive: true });
    const t = (sec) => t0 + sec * 1000;
    const title = `Golden Kimi Session ${pad(copy, 3)}`;

    writeJson(join(sessionDir, 'state.json'), {
      id: sessionName,
      version: 2,
      title,
      cwd,
      createdAt: t(0),
      updatedAt: t(900),
      archived: false,
      custom: {},
      isCustomTitle: false,
      agents: {
        main: { homedir: `${cwd}/agents/main`, type: 'main' },
        'agent-7': { type: 'sub', parentAgentId: 'main', labels: { profile: 'explore' }, swarmItem: 'explore the golden corpus' },
      },
    });

    // --- main agent wire ---
    const main = [];
    main.push({ type: 'metadata', protocol_version: '1.5', created_at: t(0) });
    main.push({ type: 'config.update', time: t(1), modelAlias: 'kimi-k2-0905-preview' });
    main.push({ type: 'context.append_message', time: t(2), message: { role: 'user', id: `${kid('p1')}`, content: [{ type: 'text', text: `golden kimi corpus: ${synthSentence(rng, 'inspect', 10)}` }], toolCalls: [], origin: { kind: 'user' } } });
    let messages = 1;
    let toolCalls = 0;
    for (let turn = 1; turn <= turns; turn++) {
      const step = `${kid(`step${turn}`)}`;
      const toolCallId = `${kid(`call${turn}`)}`;
      main.push({ type: 'context.append_loop_event', time: t(10 * turn), event: { type: 'step.begin', uuid: step, turnId: String(turn) } });
      main.push({ type: 'context.append_loop_event', time: t(10 * turn + 1), event: { type: 'content.part', uuid: `${kid(`think${turn}`)}`, stepUuid: step, part: { type: 'thinking', thinking: synthSentence(rng, 'think', 6) } } });
      main.push({ type: 'context.append_loop_event', time: t(10 * turn + 2), event: { type: 'tool.call', uuid: `${kid(`te${turn}`)}`, stepUuid: step, toolCallId, name: 'Read', args: { file_path: `${cwd}/src/k${turn}.ts` } } });
      main.push({ type: 'context.append_loop_event', time: t(10 * turn + 3), event: { type: 'tool.result', parentUuid: `${kid(`tr${turn}`)}`, toolCallId, result: { output: `agent_id: agent-7\ngolden kimi file body ${turn}`, isError: turn === 3 } } });
      main.push({ type: 'context.append_loop_event', time: t(10 * turn + 4), event: { type: 'content.part', uuid: `${kid(`text${turn}`)}`, stepUuid: step, part: { type: 'text', text: `golden kimi reply ${turn}: ${synthSentence(rng, 'answer', 8)}` } } });
      main.push({ type: 'context.append_loop_event', time: t(10 * turn + 5), event: { type: 'step.end', uuid: step, usage: { inputOther: 7, inputCacheRead: 3, inputCacheCreation: 2, output: 3 } } });
      messages += 3;
      toolCalls += 1;
      if (turn === 1) {
        // Undo/clear replay: an undone prompt + answer, then undo(1) removes both.
        main.push({ type: 'context.append_message', time: t(10 * turn + 6), message: { role: 'user', id: `${kid('pundo')}`, content: [{ type: 'text', text: 'undone golden prompt' }], toolCalls: [], origin: { kind: 'user' } } });
        main.push({ type: 'context.append_loop_event', time: t(10 * turn + 7), event: { type: 'content.part', uuid: `${kid('undoa')}`, stepUuid: step, part: { type: 'text', text: 'undone golden answer' } } });
        main.push({ type: 'context.undo', time: t(10 * turn + 8), count: 1 });
      }
      if (turn === 2) {
        // A clear resets the undo floor; later undos cannot reach earlier turns.
        main.push({ type: 'context.clear', time: t(10 * turn + 6) });
      }
    }
    // Legacy protocol-1.0 embedded tool calls/results on plain messages.
    main.push({ type: 'context.append_message', time: t(800), message: { role: 'assistant', content: [], toolCalls: [{ type: 'function', id: `${kid('legacy')}`, function: { name: 'Read', arguments: JSON.stringify({ file_path: `${cwd}/src/legacy.ts` }) } }] } });
    main.push({ type: 'context.append_message', time: t(801), message: { role: 'tool', content: [{ type: 'text', text: 'golden legacy result' }], toolCalls: [], toolCallId: `${kid('legacy')}` } });
    messages += 2;
    toolCalls += 1;
    main.push({ type: 'context.apply_compaction', time: t(810), summary: 'Golden kimi compaction summary', compactedCount: 2 });
    main.push({ type: 'context.append_message', time: t(820), message: { role: 'user', id: `${kid('pfinal')}`, content: [{ type: 'text', text: `golden kimi final prompt: ${synthSentence(rng, 'final', 8)}` }], toolCalls: [], origin: { kind: 'user' } } });
    messages += 1;
    writeJsonl(join(mainDir, 'wire.jsonl'), main);

    // --- subagent wire (sidechain messages, meta system_trigger prompt) ---
    const child = [];
    child.push({ type: 'metadata', protocol_version: '1.5', created_at: t(30) });
    child.push({ type: 'context.append_message', time: t(31), message: { role: 'user', content: [{ type: 'text', text: `golden kimi subagent prompt: ${synthSentence(rng, 'subq', 8)}` }], toolCalls: [], origin: { kind: 'system_trigger', name: 'subagent' } } });
    child.push({ type: 'context.append_loop_event', time: t(32), event: { type: 'step.begin', uuid: `${kid('cstep')}`, turnId: '1' } });
    child.push({ type: 'context.append_loop_event', time: t(33), event: { type: 'content.part', uuid: `${kid('ctext')}`, stepUuid: `${kid('cstep')}`, part: { type: 'text', text: 'golden kimi subagent reply' } } });
    child.push({ type: 'context.append_loop_event', time: t(34), event: { type: 'step.end', uuid: `${kid('cstep')}`, usage: { inputOther: 5, inputCacheRead: 1, inputCacheCreation: 1, output: 2 } } });
    writeJsonl(join(childDir, 'wire.jsonl'), child);

    note('kimi', { sessions: 1, messages: messages + 2, tool_calls: toolCalls });
  }
}

// ---- pi ----

function generatePi(home, { scale, extraTurns }, rng) {
  const root = join(home, '.pi', 'agent', 'sessions');
  const cwd = '/home/synth/obelisk-golden/pi';
  const turns = 3 + extraTurns;

  for (let copy = 1; copy <= scale; copy++) {
    const t0 = BASE_MS + (copy - 1) * 6 * 3600_000;
    const pid = (n) => `pi${copy}${n}`;
    const t = (sec) => t0 + sec * 1000;
    const ts = (sec) => iso(t(sec));

    // --- session A: tool calls + inactive branch + legacy compaction + leaf ---
    const a = [];
    a.push({ type: 'session', version: 3, id: `pi-golden-${pad(copy, 4)}-a`, timestamp: ts(0), cwd, metadata: { purpose: 'obelisk golden corpus' } });
    let messagesA = 0;
    let toolCallsA = 0;
    let prevId = null;
    const entry = (record) => { a.push(record); prevId = record.id; };
    entry({ type: 'message', id: `${pid('u0')}`, parentId: null, timestamp: ts(1), message: { role: 'user', content: `golden pi corpus: ${synthSentence(rng, 'inspect', 10)}`, timestamp: t(1) } });
    messagesA += 1;
    const firstTurnAssistantId = `${pid('as1')}`;
    let mainChainLastId = prevId;
    for (let turn = 1; turn <= turns; turn++) {
      const asId = `${pid(`as${turn}`)}`;
      entry({
        type: 'message', id: asId, parentId: prevId, timestamp: ts(10 * turn), message: {
          role: 'assistant',
          content: [
            { type: 'thinking', thinking: synthSentence(rng, 'think', 5) },
            { type: 'text', text: `golden pi reply ${turn}: ${synthSentence(rng, 'answer', 8)}` },
            { type: 'toolCall', id: `${pid(`call${turn}`)}`, name: 'read', arguments: { path: `golden-${turn}.ts` } },
          ],
          api: 'openai-responses', provider: 'obelisk-golden', model: 'golden-model',
          usage: { input: 14 + turn, output: 5 + turn, cacheRead: 3, cacheWrite: 0, reasoning: 2, totalTokens: 24 + turn, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
          stopReason: 'toolUse', timestamp: t(10 * turn),
        },
      });
      messagesA += 3;
      toolCallsA += 1;
      entry({
        type: 'message', id: `${pid(`tr${turn}`)}`, parentId: asId, timestamp: ts(10 * turn + 1), message: {
          role: 'toolResult', toolCallId: `${pid(`call${turn}`)}`, toolName: 'read',
          content: [{ type: 'text', text: `golden pi read result ${turn}\n` }], isError: turn === 2, timestamp: t(10 * turn + 1),
        },
      });
      messagesA += 1;
      entry({ type: 'message', id: `${pid(`uf${turn}`)}`, parentId: `${pid(`tr${turn}`)}`, timestamp: ts(10 * turn + 2), message: { role: 'user', content: `golden pi follow-up ${turn}: ${synthSentence(rng, 'next', 6)}`, timestamp: t(10 * turn + 2) } });
      messagesA += 1;
      mainChainLastId = `${pid(`uf${turn}`)}`;
    }
    // Inactive branch diverging from the first assistant answer.
    entry({ type: 'message', id: `${pid('br1')}`, parentId: firstTurnAssistantId, timestamp: ts(500), message: { role: 'user', content: 'inactive golden branch prompt', timestamp: t(500) } });
    entry({ type: 'message', id: `${pid('br2')}`, parentId: `${pid('br1')}`, timestamp: ts(501), message: { role: 'assistant', content: [{ type: 'text', text: 'inactive golden branch reply' }], api: 'openai-responses', provider: 'obelisk-golden', model: 'golden-model', stopReason: 'stop', timestamp: t(501) } });
    messagesA += 2;
    entry({ type: 'branch_summary', id: `${pid('brsum')}`, parentId: `${pid('br2')}`, timestamp: ts(502), summary: 'Golden pi branch summary' });
    // Legacy compaction (firstKeptEntryId, no retainedTail): the whole main chain stays active.
    entry({ type: 'compaction', id: `${pid('cmp')}`, parentId: mainChainLastId, timestamp: ts(510), summary: 'Golden pi compaction checkpoint', firstKeptEntryId: `${pid('u0')}`, tokensBefore: 9001 });
    entry({ type: 'message', id: `${pid('pu')}`, parentId: `${pid('cmp')}`, timestamp: ts(511), message: { role: 'user', content: 'golden pi post-compaction user turn', timestamp: t(511) } });
    entry({ type: 'message', id: `${pid('pas')}`, parentId: `${pid('pu')}`, timestamp: ts(512), message: { role: 'assistant', content: [{ type: 'text', text: 'golden pi post-compaction assistant turn' }], api: 'openai-responses', provider: 'obelisk-golden', model: 'golden-model', usage: { input: 9, output: 2, cacheRead: 1, cacheWrite: 0, reasoning: 0, totalTokens: 12, cost: { input: 0, output: 0, total: 0 } }, stopReason: 'stop', timestamp: t(512) } });
    messagesA += 2;
    entry({ type: 'session_info', id: `${pid('sinfo')}`, parentId: `${pid('pas')}`, timestamp: ts(113), name: `Golden Pi Session A ${pad(copy, 3)}` });
    entry({ type: 'leaf', id: `${pid('leaf')}`, parentId: `${pid('sinfo')}`, timestamp: ts(114), targetId: `${pid('pas')}` });
    mkdirSync(join(root, `pi-${pad(copy, 4)}-a`), { recursive: true });
    writeJsonl(join(root, `pi-${pad(copy, 4)}-a`, 'session.jsonl'), a);

    // --- session B: orphan chain (missing parent) + an inactive root ---
    const b = [];
    b.push({ type: 'session', version: 3, id: `pi-golden-${pad(copy, 4)}-b`, timestamp: ts(200), cwd });
    b.push({ type: 'message', id: `${pid('broot')}`, parentId: null, timestamp: ts(201), message: { role: 'user', content: 'inactive golden root', timestamp: t(201) } });
    b.push({ type: 'message', id: `${pid('bo1')}`, parentId: `${pid('b-missing-parent')}`, timestamp: ts(202), message: { role: 'user', content: 'golden pi orphan root', timestamp: t(202) } });
    b.push({ type: 'message', id: `${pid('bo2')}`, parentId: `${pid('bo1')}`, timestamp: ts(203), message: { role: 'assistant', content: [{ type: 'text', text: 'golden pi orphan child reply' }], api: 'openai-responses', provider: 'obelisk-golden', model: 'golden-model', stopReason: 'stop', timestamp: t(203) } });
    b.push({ type: 'session_info', id: `${pid('bsinfo')}`, parentId: `${pid('bo2')}`, timestamp: ts(204), name: `Golden Pi Session B ${pad(copy, 3)}` });
    b.push({ type: 'leaf', id: `${pid('bleaf')}`, parentId: `${pid('bsinfo')}`, timestamp: ts(205), targetId: `${pid('bo2')}` });
    mkdirSync(join(root, `pi-${pad(copy, 4)}-b`), { recursive: true });
    writeJsonl(join(root, `pi-${pad(copy, 4)}-b`, 'session.jsonl'), b);

    // --- session C: checkpoint compaction with a retained tail ---
    const c = [];
    c.push({ type: 'session', version: 3, id: `pi-golden-${pad(copy, 4)}-c`, timestamp: ts(300), cwd });
    c.push({ type: 'message', id: `${pid('cu1')}`, parentId: null, timestamp: ts(301), message: { role: 'user', content: 'golden pi checkpoint pre-compaction user', timestamp: t(301) } });
    c.push({ type: 'message', id: `${pid('cas1')}`, parentId: `${pid('cu1')}`, timestamp: ts(302), message: { role: 'assistant', content: [{ type: 'text', text: 'golden pi checkpoint pre-compaction answer' }], api: 'openai-responses', provider: 'obelisk-golden', model: 'golden-model', stopReason: 'stop', timestamp: t(302) } });
    c.push({
      type: 'compaction', id: `${pid('ccmp')}`, parentId: `${pid('cas1')}`, timestamp: ts(303),
      summary: 'Golden pi checkpoint compaction', firstKeptEntryId: `${pid('cu1')}`, tokensBefore: 12_000,
      retainedTail: [
        { role: 'user', content: 'golden pi retained user turn', timestamp: t(303) },
        { role: 'assistant', content: [{ type: 'text', text: 'golden pi retained assistant turn' }], api: 'openai-responses', provider: 'obelisk-golden', model: 'golden-model', usage: { input: 11, output: 3, cacheRead: 2, cacheWrite: 0, reasoning: 0, totalTokens: 16, cost: { input: 0, output: 0, total: 0 } }, stopReason: 'stop', timestamp: t(304) },
      ],
    });
    c.push({ type: 'message', id: `${pid('cpu')}`, parentId: `${pid('ccmp')}`, timestamp: ts(305), message: { role: 'user', content: 'golden pi checkpoint post-compaction user', timestamp: t(305) } });
    c.push({ type: 'message', id: `${pid('cpas')}`, parentId: `${pid('cpu')}`, timestamp: ts(306), message: { role: 'assistant', content: [{ type: 'text', text: 'golden pi checkpoint post-compaction answer' }], api: 'openai-responses', provider: 'obelisk-golden', model: 'golden-model', stopReason: 'stop', timestamp: t(306) } });
    c.push({ type: 'session_info', id: `${pid('csinfo')}`, parentId: `${pid('cpas')}`, timestamp: ts(307), name: `Golden Pi Session C ${pad(copy, 3)}` });
    c.push({ type: 'leaf', id: `${pid('cleaf')}`, parentId: `${pid('csinfo')}`, timestamp: ts(308), targetId: `${pid('cpas')}` });
    mkdirSync(join(root, `pi-${pad(copy, 4)}-c`), { recursive: true });
    writeJsonl(join(root, `pi-${pad(copy, 4)}-c`, 'session.jsonl'), c);

    note('pi', { sessions: 3, messages: messagesA + 3 + 6, tool_calls: toolCallsA });
  }
}

// ---- main ----

const options = parseArgs(process.argv.slice(2));
const { out, scale } = options;
const extraTurns = Math.min(48, Math.floor((scale - 1) / 6));
const MARKER = '.obelisk-golden-corpus';

if (existsSync(out)) {
  const entries = readdirSync(out);
  if (entries.length > 0 && !entries.includes(MARKER)) {
    throw new Error(`refusing to write into non-empty directory without a ${MARKER} marker: ${out}`);
  }
  rmSync(out, { recursive: true, force: true });
}
const home = join(out, 'home');
mkdirSync(home, { recursive: true });
writeFileSync(join(out, MARKER), `scale=${scale} seed=${options.seed} bigMessages=${options.bigMessages}\n`);

const rng = mulberry32(options.seed);
const ctx = { scale, extraTurns, bigMessages: options.bigMessages };
generateClaude(home, ctx, rng);
generateCodex(home, ctx, rng);
generateDeepseek(home, ctx, rng);
generateKimi(home, ctx, rng);
generatePi(home, ctx, rng);

process.stdout.write(JSON.stringify({
  out,
  home,
  scale,
  extraTurns,
  bigMessages: options.bigMessages,
  seed: options.seed,
  providers: stats.providers,
}, null, 2) + '\n');
