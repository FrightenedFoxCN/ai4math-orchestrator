import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { existsSync } from 'node:fs';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import {
  MODEL_TASK_REQUEST_MAX_BYTES, MODEL_TASK_RESPONSE_MAX_BYTES, runModelTaskProcess,
} from '../model-task-process.mjs';

const supported = ['linux', 'darwin'].includes(process.platform);
const fixture = new URL('./model-task-process-fixture.mjs', import.meta.url);
const secret = 'synthetic secret value';
const packet = { task_id: 'test', informal_statement: 'π', max_wall_seconds: 1 };
const run = (mode, { wall = 1, signal, entry = fixture, ...data } = {}) => runModelTaskProcess({
  packet: { ...packet, max_wall_seconds: wall }, signal, entry, workerData: { mode, ...data },
});
const rejects = (promise, code) => assert.rejects(promise, (error) => {
  assert.equal(error.code, code);
  assert.doesNotMatch(error.stack, /synthetic secret value/u);
  assert.equal(error.cause, undefined);
  return true;
});
const local = (name, fn) => test(name, { skip: !supported }, fn);

async function markerPath(t) {
  const dir = await mkdtemp(join(tmpdir(), 'ai4math-model-task-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  return join(dir, 'ready');
}
async function waitForPids(marker, count = 1) {
  const end = performance.now() + 4000;
  while (performance.now() < end) {
    let text;
    try { text = await readFile(marker, 'utf8'); } catch (error) { if (error.code !== 'ENOENT') throw error; }
    if (/^\d+( \d+)?$/u.test(text ?? '')) {
      const pids = text.split(' ').map(Number);
      if (pids.length === count) return pids;
    }
    await delay(10);
  }
  assert.fail('synthetic subprocess did not report ready');
}
const assertReaped = (pid) => assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });

local('worker config uses stdin, not argv or inherited environment; split UTF-8 works', async () => {
  const name = 'AI4MATH_TEST_SYNTHETIC_SECRET';
  const previous = process.env[name];
  process.env[name] = secret;
  try {
    assert.deepEqual(await run('echo', { secret }), { packet, stdinMatched: true, envCleared: true, electronFlag: true, argvClean: true });
  } finally {
    if (previous === undefined) delete process.env[name];
    else process.env[name] = previous;
  }
});

local('non-cooperative worker is killed on deadline and reaped before rejection', async (t) => {
  const marker = await markerPath(t);
  await rejects(run('hang', { marker }), 'MODEL_TASK_TIMEOUT');
  const [pid] = await waitForPids(marker);
  assertReaped(pid);
});

local('cancellation kills and reaps even if an earlier abort listener stops propagation', async (t) => {
  const marker = await markerPath(t);
  const controller = new AbortController();
  controller.signal.addEventListener('abort', (event) => event.stopImmediatePropagation());
  const rejection = rejects(run('hang', { marker, signal: controller.signal, wall: 8 }), 'MODEL_TASK_ABORTED');
  const [pid] = await waitForPids(marker);
  controller.abort(new Error(secret));
  await rejection;
  assertReaped(pid);
});

local('cancellation kills ordinary inherited same-group descendants too', async (t) => {
  const marker = await markerPath(t);
  const controller = new AbortController();
  const rejection = rejects(run('group', { marker, signal: controller.signal, wall: 8 }), 'MODEL_TASK_ABORTED');
  const [pid, descendant] = await waitForPids(marker, 2);
  controller.abort();
  await rejection;
  assertReaped(pid);
  const end = performance.now() + 2000;
  while (performance.now() < end) {
    try { process.kill(descendant, 0); } catch (error) { if (error.code === 'ESRCH') return; throw error; }
    if (process.platform === 'linux') {
      // A killed orphan awaiting the OS reaper is not a running descendant.
      try { if (/\) Z /u.test(await readFile(`/proc/${descendant}/stat`, 'utf8'))) return; }
      catch (error) { if (error.code === 'ENOENT') return; throw error; }
    }
    await delay(10);
  }
  assert.fail('same-group descendant continued running');
});

local('fd 3 guardian observes EOF when an isolated supervisor parent is SIGKILLed', async (t) => {
  const marker = await markerPath(t);
  const supervisor = new URL('../model-task-process.mjs', import.meta.url);
  const source = `
    const [moduleUrl, fixtureUrl, marker] = process.argv.slice(1);
    const { runModelTaskProcess } = await import(moduleUrl);
    await runModelTaskProcess({
      packet: { max_wall_seconds: 10 }, entry: new URL(fixtureUrl), workerData: { mode: 'lifeline', marker },
    });
  `;
  const driver = spawn(process.execPath, ['--input-type=module', '--eval', source, supervisor.href, fixture.href, marker], {
    env: process.versions.electron ? { ELECTRON_RUN_AS_NODE: '1' } : {}, stdio: 'ignore',
  });
  const driverClosed = once(driver, 'close');
  let childPid;
  try {
    [childPid] = await waitForPids(marker);
    assert.equal(existsSync(`${marker}.eof`), false);
    assert.equal(driver.kill('SIGKILL'), true);
    assert.deepEqual(await driverClosed, [null, 'SIGKILL']);
    const end = performance.now() + 2000;
    while (!existsSync(`${marker}.eof`) && performance.now() < end) await delay(10);
    assert.equal(existsSync(`${marker}.eof`), true);
  } finally {
    if (driver.exitCode === null && driver.signalCode === null) driver.kill('SIGKILL');
    await driverClosed;
    if (childPid && !existsSync(`${marker}.eof`)) {
      try { process.kill(-childPid, 'SIGKILL'); } catch (error) { if (error.code !== 'ESRCH') throw error; }
    }
  }
});

local('a complete response without prompt process exit cannot succeed', async (t) => {
  const marker = await markerPath(t);
  await rejects(run('valid-then-hang', { marker }), 'MODEL_TASK_TIMEOUT');
  const [pid] = await waitForPids(marker);
  assertReaped(pid);
});

local('a response queued during a parent stall cannot succeed after the monotonic deadline', async (t) => {
  const marker = await markerPath(t);
  const rejection = rejects(run('ready-then-reply', { marker, wall: 2 }), 'MODEL_TASK_TIMEOUT');
  const [pid] = await waitForPids(marker);
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 2100);
  await rejection;
  assert.equal(existsSync(`${marker}.sent`), true);
  assertReaped(pid);
});

local('a nonzero exit discards a valid-looking frame and redacts stderr', async (t) => {
  const marker = await markerPath(t);
  await rejects(run('crash', { marker, secret }), 'MODEL_TASK_PROCESS');
  const [pid] = await waitForPids(marker);
  assertReaped(pid);
});

local('bounded transport counts response bytes including LF and limits discarded stderr', async () => {
  const result = await run('response-at-limit');
  assert.equal(Buffer.byteLength(JSON.stringify({ version: 1, result }) + '\n'), MODEL_TASK_RESPONSE_MAX_BYTES);
  await rejects(run('oversize'), 'MODEL_TASK_PROTOCOL');
  await rejects(run('oversize-stderr'), 'MODEL_TASK_PROCESS');
});

local('rejects truncated, multiple, malformed or ambiguous response frames', async (t) => {
  const valid = '{"version":1,"result":null}\n';
  const cases = {
    missingLF: valid.slice(0, -1), trailingData: valid + 'x', twoFrames: valid.repeat(2), crlf: valid.replace('\n', '\r\n'),
    bom: '\ufeff' + valid, malformed: '{\n', duplicateKey: '{"version":0,"version":1,"result":null}\n',
    badVersion: '{"version":2,"result":null}\n', extraKey: '{"version":1,"result":null,"packet":{}}\n',
    invalidUtf8: Buffer.from([0xc0, 0xaf, 0x0a]),
  };
  for (const [name, frame] of Object.entries(cases)) {
    await t.test(name, () => rejects(run('raw', { frame: Buffer.from(frame).toString('base64') }), 'MODEL_TASK_PROTOCOL'));
  }
});

local('rejects oversized request and pre-abort before executing the worker', async (t) => {
  const marker = await markerPath(t);
  await rejects(run('hang', { marker, pad: 'π'.repeat(MODEL_TASK_REQUEST_MAX_BYTES / 2) }), 'MODEL_TASK_REQUEST');
  const controller = new AbortController();
  controller.abort(new Error(secret));
  await rejects(run('hang', { marker, signal: controller.signal }), 'MODEL_TASK_ABORTED');
  assert.equal(existsSync(marker), false);
  await rejects(run('echo', { entry: 'relative-worker.mjs' }), 'MODEL_TASK_REQUEST');
  await rejects(run('echo', { entry: new URL('./not-present-model-task.mjs', import.meta.url) }), 'MODEL_TASK_PROCESS');
});

test('fails closed on platforms without this process-group implementation', async () => {
  const previous = Object.getOwnPropertyDescriptor(process, 'platform');
  Object.defineProperty(process, 'platform', { ...previous, value: 'win32' });
  try { await rejects(run('echo', { secret }), 'MODEL_TASK_UNSUPPORTED'); }
  finally { Object.defineProperty(process, 'platform', previous); }
});
