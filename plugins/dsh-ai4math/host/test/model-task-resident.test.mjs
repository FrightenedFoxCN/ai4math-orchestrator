import assert from 'node:assert/strict';
import { copyFile, mkdir, mkdtemp, readFile, realpath, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { runDeepSeekModelResident } from '../model-task-resident.mjs';

const key = 'fixture-resident-key-local';
const supported = ['darwin', 'linux'].includes(process.platform);
const local = (name, fn) => test(name, { skip: !supported }, fn);
const quote = (value) => "'" + value.replaceAll("'", "'\\''") + "'";
const empty = JSON.stringify({ pending: [], reconciled: 0 });
const isError = (code) => (err) => err.code === code && !Object.hasOwn(err, 'cause') && !err.message.includes('PRIVATE');

async function fixture(t, setup, { sdk = false } = {}) {
  const dir = await realpath(await mkdtemp(join(tmpdir(), 'ai4math-resident-')));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const root = join(dir, 'operator "C": root');
  const bareRepo = join(dir, 'managed "R": repo.git');
  const dshRoot = join(dir, 'sdk');
  const cliPath = join(dir, 'ai4math');
  await Promise.all([mkdir(root), mkdir(bareRepo), mkdir(dshRoot)]);
  const inputRoot = join(dir, 'root alias');
  const inputRepo = join(dir, 'repo alias');
  await Promise.all([symlink(root, inputRoot), symlink(bareRepo, inputRepo)]);
  const opts = { root: inputRoot, bareRepo: inputRepo, cliPath, dshRoot, apiKey: key, baseUrl: 'http://127.0.0.1:65535',
    model: 'fixture-math-model', scanLimit: 2, maxCycles: 3, maxLaunches: 2, pollIntervalMs: 1, scanTimeoutMs: 5_000 };
  const row = (task_id, event_seq = 1) => ({ task_id, claim_id: 'fixture-claim', role: 'prover_worker', repo: bareRepo,
    base_commit: 'a'.repeat(40), model: opts.model, max_wall_seconds: 8, max_model_calls: 1, state: 'approved', event_seq });
  const state = setup({ row });
  const source = join(dir, 'fixture.mjs');
  const log = join(dir, 'log');
  const scanPid = join(dir, 'scan-pid');
  const workerPid = join(dshRoot, 'worker-pid');
  await writeFile(log, '');
  await writeFile(cliPath, '#!/bin/sh\nELECTRON_RUN_AS_NODE=1 exec ' + quote(process.execPath) + ' ' + quote(source) + ' "$@"\n', { mode: 0o700 });
  await writeFile(source, String.raw`
    import assert from 'node:assert/strict';
    import { appendFileSync, readFileSync, writeFileSync } from 'node:fs';
    const { root, bareRepo, log, scanPid, model, state } = ${JSON.stringify({ root, bareRepo, log, scanPid, model: opts.model, state })};
    const args = process.argv.slice(2);
    const [command, actualRoot, actualRepo, ...rest] = args;
    assert.ok(!JSON.stringify(args).includes('fixture-resident' + '-key-local'));
    assert.equal(process.env.AI4MATH_RESIDENT_KEY, undefined);
    assert.equal(actualRoot, root);
    assert.equal(actualRepo, bareRepo);
    const before = readFileSync(log, 'utf8').trim().split('\n');
    const trace = (line) => appendFileSync(log, line + '\n');
    const stdin = async () => {
      const chunks = [];
      for await (const chunk of process.stdin) chunks.push(chunk);
      return Buffer.concat(chunks).toString('utf8');
    };
    if (command === 'model-dispatch-resident-scan') {
      assert.deepEqual(rest, [model, String(state.limit ?? 2)]);
      assert.equal(await stdin(), '');
      const count = before.filter((line) => line === 'scan').length;
      trace('scan');
      if (state.scanMode === 'hang') {
        writeFileSync(scanPid, String(process.pid));
        setInterval(() => {}, 1_000);
      } else if (state.scanMode === 'utf8') {
        process.stdout.write(Buffer.from([0xff]));
      } else if (state.scanMode === 'overlong') {
        process.stdout.write(' '.repeat(200 * 1024));
      } else if (state.scanMode === 'nonzero') {
        process.stderr.write('PRIVATE CLI diagnostics');
        process.stdout.write(state.scans[0]);
        process.exitCode = 1;
      } else {
        process.stderr.write('PRIVATE CLI diagnostics' + 'x'.repeat(256 * 1024));
        process.stdout.write(state.scans[Math.min(count, state.scans.length - 1)]);
      }
    } else {
      assert.equal(command, 'task-run-approved-model');
      const [task] = rest;
      const row = state.rows.find((candidate) => candidate.task_id === task);
      assert.ok(row);
      assert.deepEqual(rest, [row.task_id, row.claim_id, row.role, String(row.max_wall_seconds), row.model, String(row.max_model_calls)]);
      trace('launch ' + task);
      if (task === 'blocked') {
        process.stderr.write('PRIVATE blocked approval');
        process.exitCode = 1;
      } else {
        const { task_id, claim_id, role, base_commit, max_wall_seconds } = row;
        const binding = { task_id, claim_id, role, base_commit, module_path: 'Proof.lean', target_decl: 'goal' };
        process.stdout.write(JSON.stringify({ packet: { ...binding, max_wall_seconds, version: 1, engine: 'lean4',
          informal_statement: 'True', formal_header: 'theorem goal : True' } }) + '\n');
        const result = JSON.parse(await stdin());
        assert.deepEqual(result, { ...binding, suggestion: {kind: 'proof_body', proof_body: 'by trivial'}, proof_verified: false });
        trace('received ' + task);
        if (task !== 'lost-ack') process.stdout.write(JSON.stringify({ task_id, base_commit, role,
          source: 'dsh_model', suggestion_recorded: true, proof_verified: false }) + '\n');
      }
    }
  `);
  if (sdk) {
    await copyFile(new URL('./model-task-resident-sdk-fixture.cjs', import.meta.url), join(dshRoot, 'fixture.cjs'));
    for (const name of ['cordis', 'dsh-llm', 'dsh-tool-executor-native', 'cordis-plugin-loader', 'dsh-session',
      'dsh-session-projection', 'dsh-agent', 'dsh-system-prompt', 'dsh-tools', 'dsh-agent-loop',
      'dsh-agent-preset-registry', 'dsh-llm-deepseek']) {
      const pkg = join(dshRoot, 'node_modules', '@deepseek-ai', name);
      await mkdir(pkg, { recursive: true });
      await writeFile(join(pkg, 'index.js'), "module.exports = require('../../../fixture.cjs');\n");
    }
  }
  return { opts, scanPid, workerPid, log: () => readFile(log, 'utf8'), row };
}

const frame = (pending, reconciled = 0) => JSON.stringify({ pending, reconciled });
async function waitFor(read, accept) {
  const until = Date.now() + 4_000;
  while (Date.now() < until) {
    try { const value = await read(); if (accept(value)) return value; }
    catch (err) { if (err.code !== 'ENOENT') throw err; }
    await delay(10);
  }
  assert.fail('fixture did not become ready');
}
const waitPid = (path) => waitFor(() => readFile(path, 'utf8'), (value) => /^\d+$/u.test(value));

local('explicit path, route and finite budget validation happen before launch', async (t) => {
  const f = await fixture(t, () => ({ scans: [empty] }));
  for (const patch of [
    { cliPath: 'relative' }, { root: 'relative' }, { bareRepo: 'relative' }, { dshRoot: 'relative' },
    { maxCycles: undefined }, { maxCycles: 0 }, { maxCycles: 257 }, { maxCycles: Infinity },
    { maxLaunches: undefined }, { maxLaunches: 257 }, { maxLaunches: '2' },
    { pollIntervalMs: undefined }, { pollIntervalMs: 0 }, { pollIntervalMs: 60_001 },
    { scanTimeoutMs: undefined }, { scanTimeoutMs: 0 }, { scanTimeoutMs: 30_001 },
    { scanLimit: undefined }, { scanLimit: 0 }, { scanLimit: 17 }, { scanLimit: 1.5 },
    { model: 'bogus\n' }, { apiKey: 'ordinary-key-not-fixture' }, { allowRemote: true }, { signal: {} },
  ]) await assert.rejects(runDeepSeekModelResident({ ...f.opts, ...patch }), isError('invalid'));
  assert.deepEqual(await runDeepSeekModelResident({ ...f.opts, signal: AbortSignal.abort(key) }),
    { cycles: 0, attempted: 0, recorded: 0, unconfirmed: 0, reconciled: 0, reason: 'cancelled' });
  assert.equal(await f.log(), '');
});

local('empty scans are bounded and may reconcile without launching', async (t) => {
  const f = await fixture(t, () => ({ scans: [frame([], 2), empty] }));
  const stats = await runDeepSeekModelResident(f.opts);
  assert.deepEqual(stats, { cycles: 3, attempted: 0, recorded: 0, unconfirmed: 0, reconciled: 2, reason: 'cycle_budget' });
  assert.equal(await f.log(), 'scan\nscan\nscan\n');
});

local('atomic-gate rejection costs one launch and cannot replay on later scans', async (t) => {
  const f = await fixture(t, ({ row }) => {
    const blocked = row('blocked');
    return { rows: [blocked], scans: [frame([blocked])] };
  });
  assert.deepEqual(await runDeepSeekModelResident(f.opts),
    { cycles: 3, attempted: 1, recorded: 0, unconfirmed: 1, reconciled: 0, reason: 'cycle_budget' });
  assert.equal(await f.log(), 'scan\nlaunch blocked\nscan\nscan\n');
});

local('lost acknowledgement after result intake never replays, but another ID can proceed', async (t) => {
  const f = await fixture(t, ({ row }) => {
    const lost = row('lost-ack'), next = row('next', 2);
    return { rows: [lost, next], scans: [frame([lost]), frame([lost, next], 1)] };
  }, { sdk: true });
  process.env.AI4MATH_RESIDENT_KEY = key;
  t.after(() => delete process.env.AI4MATH_RESIDENT_KEY);
  assert.deepEqual(await runDeepSeekModelResident(f.opts),
    { cycles: 2, attempted: 2, recorded: 1, unconfirmed: 1, reconciled: 1, reason: 'launch_budget' });
  assert.equal(await f.log(), 'scan\nlaunch lost-ack\nreceived lost-ack\nscan\nlaunch next\nreceived next\n');
});

local('strict scan fails closed before valid siblings and does not retry', async (t) => {
  const cases = [
    ['extra frame', () => empty + empty], ['extra key', () => '{"pending":[],"reconciled":0,"extra":1}'],
    ['duplicate envelope member', () => '{"pending":null,"reconciled":0,"p\\u0065nding":[]}'],
    ['too many reconciliations', () => frame([], 3)], ['negative reconciliation', () => frame([], -1)],
    ['noninteger reconciliation', () => frame([], 0.1)], ['more than limit', (a, b) => frame([a, b, { ...b, task_id: 'c' }])],
    ['duplicate task', (a) => frame([a, a])], ['model mismatch', (a, b) => frame([a, { ...b, model: 'foreign' }])],
    ['repo mismatch', (a, b) => frame([a, { ...b, repo: '/other.git' }])], ['bad role', (a) => frame([{ ...a, role: 'explorer' }])],
    ['bad base', (a) => frame([{ ...a, base_commit: 'main' }])], ['mixed base', (a, b) => frame([a, { ...b, base_commit: 'b'.repeat(40) }])],
    ['not approved', (a) => frame([{ ...a, state: 'dispatched' }])], ['extra row key', (a) => frame([{ ...a, lease: 'x' }])],
    ['duplicate row key', (a) => frame([a]).replace('"state":', '"state":null,"state":')],
    ['bad task id', (a) => frame([{ ...a, task_id: 'not valid' }])], ['bad claim id', (a) => frame([{ ...a, claim_id: 'bad\n' }])],
    ['too many seconds', (a) => frame([{ ...a, max_wall_seconds: 61 }])], ['too many calls', (a) => frame([{ ...a, max_model_calls: 9 }])],
    ['unsafe event seq', (a) => frame([{ ...a, event_seq: 9_007_199_254_740_992 }])],
  ];
  for (const [name, payload] of cases) await t.test(name, async (sub) => {
    const f = await fixture(sub, ({ row }) => ({ scans: [payload(row('valid-first'), row('valid-second', 2))] }));
    await assert.rejects(runDeepSeekModelResident(f.opts), isError('protocol'));
    assert.equal(await f.log(), 'scan\n');
  });
  for (const [scanMode, code] of [['utf8', 'protocol'], ['overlong', 'protocol'], ['nonzero', 'child']]) {
    await t.test(scanMode, async (sub) => {
      const f = await fixture(sub, () => ({ scanMode, scans: [empty] }));
      await assert.rejects(runDeepSeekModelResident(f.opts), isError(code));
      assert.equal(await f.log(), 'scan\n');
    });
  }
});

local('blocked scan times out and is reaped; no extra scans', async (t) => {
  const f = await fixture(t, () => ({ scanMode: 'hang' }));
  await assert.rejects(runDeepSeekModelResident({ ...f.opts, scanTimeoutMs: 1000 }), isError('timeout'));
  const pid = Number(await waitPid(f.scanPid));
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
  assert.equal(await f.log(), 'scan\n');
});

local('cancellation interrupts scan and prevents overlapping residents', async (t) => {
  const f = await fixture(t, () => ({ scanMode: 'hang' }));
  const abort = new AbortController();
  abort.signal.addEventListener('abort', (event) => event.stopImmediatePropagation());
  const run = runDeepSeekModelResident({ ...f.opts, signal: abort.signal });
  const pid = Number(await waitPid(f.scanPid));
  await assert.rejects(runDeepSeekModelResident(f.opts), isError('busy'));
  abort.abort(key);
  assert.deepEqual(await run, { cycles: 1, attempted: 0, recorded: 0, unconfirmed: 0, reconciled: 0, reason: 'cancelled' });
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
  assert.equal(await f.log(), 'scan\n');
});

local('cancellation during polling sleep returns without reading again', async (t) => {
  const f = await fixture(t, () => ({ scans: [empty] }));
  const abort = new AbortController();
  abort.signal.addEventListener('abort', (event) => event.stopImmediatePropagation());
  const run = runDeepSeekModelResident({ ...f.opts, pollIntervalMs: 5_000, signal: abort.signal });
  await waitFor(f.log, (log) => log === 'scan\n');
  await delay(100);
  const started = Date.now();
  abort.abort(key);
  assert.equal((await run).reason, 'cancelled');
  assert.ok(Date.now() - started < 2_000);
  assert.equal(await f.log(), 'scan\n');
});

local('cancellation stops and joins a hung worker; no second assignment starts', async (t) => {
  const f = await fixture(t, ({ row }) => {
    const rows = [row('hung-worker'), row('next', 2)];
    return { rows, scans: [frame(rows)] };
  }, { sdk: true });
  const abort = new AbortController();
  const run = runDeepSeekModelResident({ ...f.opts, signal: abort.signal });
  const pid = Number(await waitPid(f.workerPid));
  abort.abort(key);
  assert.deepEqual(await run, { cycles: 1, attempted: 1, recorded: 0, unconfirmed: 1, reconciled: 0, reason: 'cancelled' });
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
  assert.equal(await f.log(), 'scan\nlaunch hung-worker\n');
});
