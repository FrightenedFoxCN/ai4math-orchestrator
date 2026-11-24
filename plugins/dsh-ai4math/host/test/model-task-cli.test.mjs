import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { runModelTaskCli } from '../model-task-cli.mjs';

const packet = {
  version: 1, task_id: 'fixture-task', claim_id: 'fixture-claim', role: 'prover_worker', engine: 'lean4',
  module_path: 'Proof.lean', target_decl: 'goal', base_commit: 'a'.repeat(40), max_wall_seconds: 1,
  informal_statement: 'True', formal_header: 'theorem goal : True',
};
const expected = {
  task_id: packet.task_id, base_commit: packet.base_commit, role: packet.role, source: 'dsh_model',
  suggestion_recorded: true, proof_verified: false,
};
const result = {
  task_id: packet.task_id, claim_id: packet.claim_id, base_commit: packet.base_commit, role: packet.role,
  module_path: packet.module_path, target_decl: packet.target_decl, suggestion: { kind: 'proof_body', proof_body: 'by trivial' },
  proof_verified: false,
};
const quote = (word) => "'" + word.replaceAll("'", "'\\''") + "'";

async function fakeCli(t, mode) {
  const dir = await mkdtemp(join(tmpdir(), 'ai4math fake cli-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const cliPath = join(dir, 'ai4math');
  const source = join(dir, 'fake.mjs');
  const root = join(dir, 'operator root');
  const bareRepo = join(dir, 'managed repo.git');
  await writeFile(cliPath, '#!/bin/sh\nexec ' + quote(process.execPath) + ' ' + quote(source) + ' "$@"\n', { mode: 0o700 });
  await writeFile(source, String.raw`
    import assert from 'node:assert/strict';
    import { writeFileSync } from 'node:fs';
    const mode = ${JSON.stringify(mode)};
    const root = ${JSON.stringify(root)};
    const packet = ${JSON.stringify(packet)};
    const summary = ${JSON.stringify(expected)};
    const result = ${JSON.stringify(result)};
    assert.equal(process.env.AI4MATH_TRANSPORT_TEST_SECRET, undefined);
    assert.deepEqual(process.argv.slice(2), ['task-run-model', root, ${JSON.stringify(bareRepo)},
      packet.task_id, packet.claim_id, packet.role, String(packet.max_wall_seconds)]);
    const line = (value) => JSON.stringify(value) + '\n';
    if (mode === 'stderr') {
      process.stderr.write('PRIVATE CLI diagnostic\n' + 'x'.repeat(64 * 1024));
    } else if (mode === 'overlong') {
      process.stdout.write('x'.repeat(64 * 1024 + 1));
    } else if (mode === 'bad-utf8') {
      process.stdout.write(Buffer.from([0xff, 0x0a]));
    } else if (mode === 'fence') {
      process.stdout.write(line({ packet: { ...packet, fence: 'private fence' } }));
    } else if (mode === 'wrong-packet') {
      process.stdout.write(line({ packet: { ...packet, claim_id: 'not-approved' } }));
    } else if (mode === 'early-summary') {
      process.stdout.write(line({ packet }) + line(summary));
    } else if (mode === 'empty-eof') {
      process.stdout.write(line({ packet }));
      const chunks = [];
      for await (const chunk of process.stdin) chunks.push(chunk);
      assert.equal(Buffer.concat(chunks).length, 0);
      writeFileSync(root, 'coordinator observed EOF');
      process.exitCode = 1;
    } else if (mode === 'hang') {
      writeFileSync(root, String(process.pid));
      process.stdout.write(line({ packet }));
      setInterval(() => {}, 3000);
    } else {
      const frame = line({ packet });
      process.stdout.write(frame.slice(0, 9));
      await new Promise(setImmediate);
      process.stdout.write(frame.slice(9));
      const chunks = [];
      for await (const chunk of process.stdin) chunks.push(chunk);
      const sent = Buffer.concat(chunks).toString('utf8');
      assert.equal(sent.split('\n').length, 2);
      assert.ok(sent.endsWith('\n'));
      assert.deepEqual(JSON.parse(sent), result);
      if (mode === 'wrong-summary') summary.base_commit = 'b'.repeat(40);
      if (mode === 'forged-summary') summary.proof_verified = true;
      if (mode === 'no-final-lf') process.stdout.write(JSON.stringify(summary));
      else process.stdout.write(line(summary));
      if (mode === 'extra-line') process.stdout.write('\n');
      if (mode === 'nonzero') process.exitCode = 1;
    }
  `);
  return { cliPath, root, bareRepo, taskId: packet.task_id, claimId: packet.claim_id, role: packet.role, wallSeconds: 1 };
}

const reason = (code) => (err) => err.code === code && !Object.hasOwn(err, 'cause') && !err.message.includes('PRIVATE');

test('the helper runs one approved callback, closes stdin and validates the bound summary', async (t) => {
  const options = await fakeCli(t, 'good');
  let calls = 0, lifetime;
  const key = 'AI4MATH_TRANSPORT_TEST_SECRET';
  const old = process.env[key];
  process.env[key] = 'PRIVATE transport parent marker';
  try {
    const output = await runModelTaskCli({ ...options, async runAssignment(value, signal) {
      calls++;
      assert.equal(signal.aborted, false);
      lifetime = signal;
      assert.ok(Object.isFrozen(value));
      assert.deepEqual(value, packet);
      return result;
    } });
    assert.deepEqual(output, expected);
    assert.equal(calls, 1);
    assert.ok(lifetime.aborted);
  } finally {
    if (old === undefined) delete process.env[key];
    else process.env[key] = old;
  }
});

test('invalid options and cancellation before spawn do not run the callback', async (t) => {
  const options = { ...await fakeCli(t, 'good'), runAssignment: () => assert.fail('should not call') };
  for (const change of [
    { cliPath: 'ai4math' }, { root: '/tmp/o\nb' }, { bareRepo: '/' + 'x'.repeat(4096) },
    { taskId: 'fixture-task\n' }, { claimId: 'x;ls' }, { role: 'explorer' }, { wallSeconds: 61 },
  ]) await assert.rejects(runModelTaskCli({ ...options, ...change }), reason('invalid'));
  await assert.rejects(runModelTaskCli({ ...options, signal: AbortSignal.abort() }), reason('cancelled'));
});

test('unexpected packet data, malformed output and premature acknowledgement fail closed', async (t) => {
  for (const mode of ['fence', 'wrong-packet', 'bad-utf8', 'overlong', 'early-summary']) {
    await t.test(mode, async (t) => {
      const options = await fakeCli(t, mode);
      let calls = 0;
      await assert.rejects(runModelTaskCli({ ...options, runAssignment: async (_packet, signal) => {
        calls++;
        await new Promise((resolve) => signal.addEventListener('abort', resolve, { once: true }));
      } }), reason('protocol'));
      if (mode !== 'early-summary') assert.equal(calls, 0);
    });
  }
});

test('only the exact advisory acknowledgement from a successful CLI is accepted', async (t) => {
  for (const mode of ['wrong-summary', 'forged-summary', 'extra-line', 'no-final-lf', 'nonzero']) {
    await t.test(mode, async (t) => {
      const options = await fakeCli(t, mode);
      await assert.rejects(runModelTaskCli({ ...options, runAssignment: async () => result }), reason(mode === 'nonzero' ? 'child' : 'protocol'));
    });
  }
});

test('stderr and assignment exceptions are bounded and never copied to errors', async (t) => {
  await assert.rejects(runModelTaskCli({ ...await fakeCli(t, 'stderr'), runAssignment: () => assert.fail('should not call') }), reason('child'));
  const options = await fakeCli(t, 'good');
  await assert.rejects(runModelTaskCli({ ...options, runAssignment: async () => { throw new Error('PRIVATE model output'); } }), reason('assignment'));
  await assert.rejects(runModelTaskCli({ ...options, runAssignment: async () => ({ ...result, proof_verified: true }) }), reason('assignment'));
  await assert.rejects(runModelTaskCli({ ...options, runAssignment: async () => null }), reason('assignment'));
});

test('a missing suggestion sends empty EOF so a live lease can be released', async (t) => {
  const options = await fakeCli(t, 'empty-eof');
  await assert.rejects(runModelTaskCli({ ...options, runAssignment: async () => null }), reason('assignment'));
  assert.equal(await readFile(options.root, 'utf8'), 'coordinator observed EOF');
});

test('an unresponsive coordinator is reaped after the empty-EOF grace', async (t) => {
  const options = await fakeCli(t, 'hang');
  await assert.rejects(runModelTaskCli({ ...options, runAssignment: async () => null }), reason('assignment'));
  const pid = Number(await readFile(options.root, 'utf8'));
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});

test('cancellation aborts the assignment and reaps the CLI without waiting for the callback', async (t) => {
  const options = await fakeCli(t, 'hang');
  const stop = new AbortController();
  let lifetime;
  await assert.rejects(runModelTaskCli({ ...options, signal: stop.signal, runAssignment: (_packet, signal) => {
    lifetime = signal;
    stop.abort(new Error('PRIVATE cancellation reason'));
    return new Promise(() => {});
  } }), reason('cancelled'));
  assert.ok(lifetime.aborted);
  const pid = Number(await readFile(options.root, 'utf8'));
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});

test('an uncooperative callback cannot extend the total deadline', async (t) => {
  const options = await fakeCli(t, 'hang');
  let lifetime;
  await assert.rejects(runModelTaskCli({ ...options, runAssignment: (_packet, signal) => {
    lifetime = signal;
    return new Promise(() => {});
  } }), reason('timeout'));
  assert.ok(lifetime.aborted);
  const pid = Number(await readFile(options.root, 'utf8'));
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' });
});
