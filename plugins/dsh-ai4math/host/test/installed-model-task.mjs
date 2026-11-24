// Run from the repository root without desktop/profile or provider credentials:
// env -i PATH=/usr/bin:/bin ELECTRON_RUN_AS_NODE=1 \
//   '/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness' \
//   plugins/dsh-ai4math/host/test/installed-model-task.mjs \
//   '/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh' [ABSOLUTE_BUILT_AI4MATH]
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';
import { runDshModelTask } from '../model-task-host.mjs';
import { runModelTaskAssignment } from '../model-task-runner.mjs';

const [dshRoot, cliPath, ...extraArgs] = process.argv.slice(2);
assert.ok(dshRoot && extraArgs.length === 0 && (cliPath === undefined || isAbsolute(cliPath)),
  'usage: installed-model-task.mjs DSH_SDK_ROOT [ABSOLUTE_BUILT_AI4MATH]');
const sessionId = 'ai4math-task';
const provider = 'fixture';
const model = 'deterministic';
const toolNames = ['ai4math_task_context', 'ai4math_task_submit'];
const inlinePacket = {
  version: 1, task_id: 'fixture-task', claim_id: 'fixture-claim',
  role: 'prover_worker', engine: 'lean4', module_path: 'Proof.lean', target_decl: 'goal',
  base_commit: 'a'.repeat(40), max_wall_seconds: 10,
  informal_statement: 'True', formal_header: 'theorem goal : True',
};

function fixtureSuggestion(role) {
  if (role === 'prover_worker') return { kind: 'proof_body', proof_body: 'by trivial' };
  assert.equal(role, 'independent_reviewer');
  return { kind: 'review', verdict: 'looks_consistent', rationale: 'The header matches the statement.' };
}

function* toolCalls(attempts) {
  for (const [index, [name, args]] of attempts.entries()) {
    yield { type: 'tool-call-delta', index, id: 'call-' + index, name, argumentsDelta: JSON.stringify(args) };
  }
  yield { type: 'finish', reason: { kind: 'tool-calls' } };
}

function offlineAdapter(stream) {
  return ({ LlmAdapter }) => new class extends LlmAdapter {
    async *stream(request) {
      assert.equal(request.sessionId, sessionId);
      assert.equal(request.provider, provider);
      assert.equal(request.model, model);
      assert.deepEqual(request.tools?.map((tool) => tool.name).sort(), toolNames);
      yield* stream(request);
    }
  }();
}

function successAdapter(packet, privateTokens = []) {
  const { kind, ...submission } = fixtureSuggestion(packet.role);
  let requests = 0;
  const registerAdapter = offlineAdapter(async function* (request) {
    requests++;
    const wire = JSON.stringify(request);
    for (const token of privateTokens) assert.ok(!wire.includes(token), 'private context reached the adapter');
    if (requests === 1) {
      yield* toolCalls([
        ['ai4math_task_context', {}],
        ['bash', {}],
        ['run_code', { code: 'return 42', description: 'Check blocked virtual transport' }],
        ['ai4math_task_submit', submission],
      ]);
      return;
    }
    assert.equal(requests, 2, 'unexpected additional model request');
    const results = new Map(request.messages.filter((message) => message.role === 'tool')
      .map((message) => [message.toolCallId, message]));
    assert.equal(results.size, 4);
    assert.equal(results.get('call-0').isError, false);
    assert.deepEqual(JSON.parse(results.get('call-0').content[0].text), packet);
    for (const callId of ['call-1', 'call-2']) {
      assert.equal(results.get(callId).isError, true);
      assert.match(results.get(callId).content[0].text, /not an assigned ai4math task tool/i);
    }
    assert.equal(results.get('call-3').isError, false);
    assert.deepEqual(JSON.parse(results.get('call-3').content[0].text), { accepted: true, verified: false });
    yield { type: 'text-delta', index: 0, text: 'OFFLINE_OK' };
    yield { type: 'finish', reason: { kind: 'stop' } };
  });
  return { registerAdapter, checkRequests: () => assert.equal(requests, 2) };
}

const sdkOptions = { dshRoot, provider, model };
const runAssignment = (registerAdapter, options = {}) => runModelTaskAssignment({
  ...sdkOptions, registerAdapter, packet: inlinePacket, signal: new AbortController().signal, ...options,
});

async function runNegativeFixtures() {
  // A completed turn is insufficient absent a valid tool submission.
  await assert.rejects(runAssignment(offlineAdapter(async function* () {
    yield { type: 'text-delta', index: 0, text: 'NO_SUBMISSION' };
    yield { type: 'finish', reason: { kind: 'stop' } };
  })), { code: 'assignment' });

  // A retryable SDK failure must not create a new model attempt. Exercise an
  // explicit adapter instance as well as the factory route used elsewhere.
  let prepareCalls = 0;
  await assert.rejects(runAssignment(undefined, { adapter: {
    async prepareCall(provider, model, signal) {
      assert.ok(signal instanceof AbortSignal);
      prepareCalls++;
      return { model: { provider, id: model, name: model }, async *stream() {
        yield { type: 'finish', reason: { kind: 'error', failure: { code: 'SERVER', message: 'Private fixture diagnostic' } } };
      } };
    },
  } }), { code: 'incomplete', message: 'The SDK model task turn did not complete.' });
  assert.equal(prepareCalls, 1, 'SDK must not retry a failed attempt');

  // A recorded submission is insufficient absent a completed turn, or when
  // returning from an adapter that omitted the terminal finish marker.
  for (const reason of [{ kind: 'max-tokens' }, null]) {
    let requests = 0;
    await assert.rejects(runAssignment(offlineAdapter(async function* () {
      if (++requests === 1) yield* toolCalls([['ai4math_task_submit', { proof_body: 'by trivial' }]]);
      else {
        yield { type: 'text-delta', index: 0, text: 'PARTIAL' };
        if (reason) yield { type: 'finish', reason };
      }
    })), { code: 'incomplete' });
    assert.equal(requests, 2);
  }

  let requests = 0;
  await assert.rejects(runAssignment(offlineAdapter(async function* () {
    requests++;
    yield* toolCalls([['ai4math_task_submit', { proof_body: 'by trivial' }]]);
  }), { maxModelCalls: 1 }), { code: 'budget' });
  assert.equal(requests, 1, 'model budget must prevent a second dispatch');
  await assert.rejects(runAssignment(offlineAdapter(async function* () {
    yield* toolCalls(Array.from({ length: 9 }, () => ['ai4math_task_context', {}]));
  })), { code: 'budget' });

  for (const expected of ['cancelled', 'timeout']) {
    const controller = new AbortController();
    const entered = Promise.withResolvers();
    let drained = false, cancelCause;
    const pending = runAssignment(offlineAdapter(async function* (request) {
      try {
        entered.resolve();
        await new Promise((resolve) => {
          if (request.signal.aborted) resolve();
          else request.signal.addEventListener('abort', resolve, { once: true });
        });
        cancelCause = request.signal.reason.kind;
      } finally { drained = request.signal.aborted; }
    }), { signal: controller.signal, packet: { ...inlinePacket, max_wall_seconds: expected === 'timeout' ? 1 : 10 } });
    if (expected === 'cancelled') {
      await Promise.race([entered.promise, pending]);
      controller.abort(new Error('caller-only diagnostic'));
    }
    await assert.rejects(pending, { code: expected });
    assert.equal(drained, true, 'runner returned before SDK adapter iterator cleanup');
    assert.equal(cancelCause, 'parent');
  }
  console.log('Installed SDK negative cases: no result, incomplete turn, no retry, call budgets, cancel/timeout and drain');
}

const commandEnvironment = { PATH: '/usr/bin:/bin', GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' };
function run(exe, args, cwd, stage) {
  try {
    return execFileSync(exe, args, {
      cwd, env: commandEnvironment, timeout: 15_000, maxBuffer: 64 * 1024,
      stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8',
    }).trim();
  } catch {
    assert.fail('Offline setup or readback command failed: ' + stage);
  }
}

async function runCliFixture() {
  const dir = await mkdtemp(join(tmpdir(), 'ai4math-installed-task-'));
  const root = join(dir, 'project');
  const source = join(dir, 'source');
  const bareRepo = join(dir, 'repo.git');
  const git = (cwd, ...args) => run('/usr/bin/git', [
    '-c', 'user.name=Fixture', '-c', 'user.email=fixture@localhost', '-c', 'commit.gpgSign=false',
    '-c', 'core.hooksPath=/dev/null', ...args,
  ], cwd, 'git ' + args[0]);
  const ai = (command, ...args) => run(cliPath, [command, root, ...args], dir, command);
  const privateMarker = 'WRITER_PRIVATE_FIXTURE_CONTEXT';
  const { claim_id: claimId, module_path: module, target_decl: decl, max_wall_seconds: wallSeconds } = inlinePacket;
  try {
    await mkdir(source);
    git(source, 'init', '--initial-branch=main', '--quiet');
    await writeFile(join(source, module), '-- ' + privateMarker + '\ndef privateFixture : Nat := 0\n' +
      inlinePacket.formal_header + ' := {{PROOF_BODY}}\n');
    git(source, 'add', '--', module);
    git(source, 'commit', '--quiet', '-m', 'Freeze offline proof slot');
    git(dir, 'clone', '--local', '--bare', '--quiet', source, bareRepo);
    const base = git(bareRepo, 'rev-parse', 'refs/heads/main');
    ai('init', 'lean4');
    ai('declare-claim', claimId, module, decl, 'Truth fixture', inlinePacket.informal_statement);
    ai('task-bind', bareRepo, module, base);
    for (const [taskId, role] of [['fixture-prover', 'prover_worker'], ['fixture-reviewer', 'independent_reviewer']]) {
      ai('task-register', bareRepo, taskId, 'infrastructure', module, decl);
      if (role === 'prover_worker') {
        let attempts = 0;
        await assert.rejects(runDshModelTask({
          cliPath, root, bareRepo, taskId, claimId, role, wallSeconds, ...sdkOptions,
          registerAdapter: offlineAdapter(async function* () {
            attempts++;
            yield { type: 'text-delta', index: 0, text: 'NO_SUBMISSION' };
            yield { type: 'finish', reason: { kind: 'stop' } };
          }),
        }), { code: 'assignment' });
        assert.equal(attempts, 1);
        assert.throws(() => ai('task-result', taskId), /Offline setup or readback command failed: task-result/);
        // A separate, explicit invocation of this same job below must now
        // succeed, proving the unsuccessful run did not strand the lease.
      }
      const packet = { ...inlinePacket, task_id: taskId, role, base_commit: base };
      const { registerAdapter, checkRequests } = successAdapter(packet,
        [dir, cliPath, dshRoot, privateMarker, 'privateFixture', 'state.sqlite']);
      const summary = await runDshModelTask({
        cliPath, root, bareRepo, taskId, claimId, role, wallSeconds, ...sdkOptions, registerAdapter,
      });
      checkRequests();
      assert.deepEqual(summary, {
        task_id: taskId, base_commit: base, role, source: 'dsh_model', suggestion_recorded: true, proof_verified: false,
      });
      const { candidate, proof_verified } = JSON.parse(ai('task-result', taskId));
      assert.equal(proof_verified, false);
      assert.equal(candidate.task_id, taskId);
      assert.equal(candidate.claim_id, claimId);
      assert.equal(candidate.base_commit, base);
      assert.equal(candidate.role, role);
      assert.equal(candidate.source, 'dsh_model');
      assert.ok(!Object.hasOwn(candidate, 'executable_sha256'));
      assert.deepEqual(JSON.parse(candidate.result_json), fixtureSuggestion(role));
      assert.equal(git(bareRepo, 'rev-parse', 'refs/heads/main'), base);
    }
    console.log('Installed Cordis + offline adapter + built CLI: both roles recorded unverified suggestions; durable task-result readback; main unchanged');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

if (cliPath) await runCliFixture();
else {
  const { registerAdapter, checkRequests } = successAdapter(inlinePacket);
  assert.deepEqual(await runAssignment(registerAdapter), {
    task_id: inlinePacket.task_id, claim_id: inlinePacket.claim_id, base_commit: inlinePacket.base_commit,
    role: inlinePacket.role, module_path: inlinePacket.module_path, target_decl: inlinePacket.target_decl,
    suggestion: fixtureSuggestion(inlinePacket.role), proof_verified: false,
  });
  checkRequests();
  console.log('Installed SDK: exactly two task tools; bash/run_code denied; bound suggestion captured');
}
await runNegativeFixtures();
