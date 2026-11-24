import test from 'node:test';
import assert from 'node:assert/strict';
import { runDshModelTask } from '../model-task-host.mjs';

const base = {
  cliPath: '/not-executed-ai4math', root: '/project', bareRepo: '/repo.git', taskId: 'T', claimId: 'C',
  role: 'prover_worker', wallSeconds: 1, dshRoot: '/isolated-sdk', provider: 'fixture', model: 'check/test-model',
};

test('one-shot Host requires an explicit SDK, route and exactly one trusted adapter source', async () => {
  let provisioned = 0;
  const registerAdapter = () => { provisioned++; };
  for (const invalid of [
    { ...base },
    { ...base, registerAdapter, adapter: { prepareCall() {} } },
    { ...base, registerAdapter, dshRoot: 'relative' },
    { ...base, registerAdapter, provider: undefined },
    { ...base, registerAdapter, model: 'fixture\nother' },
    { ...base, registerAdapter, maxModelCalls: 0 },
    { ...base, registerAdapter, maxModelCalls: 9 },
    { ...base, registerAdapter: 'arbitrary-module' },
    { ...base, adapter: {} },
  ]) await assert.rejects(runDshModelTask(invalid), { code: 'invalid' });
  await assert.rejects(runDshModelTask({ ...base, registerAdapter, signal: AbortSignal.abort() }), { code: 'cancelled' });
  assert.equal(provisioned, 0);
});
