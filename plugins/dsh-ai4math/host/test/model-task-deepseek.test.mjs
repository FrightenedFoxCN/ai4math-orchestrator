import test from 'node:test';
import assert from 'node:assert/strict';
import { runDeepSeekModelTask } from '../model-task-deepseek.mjs';

const fixtureKey = 'fixture-not-a-real-key';
const publicShape = 'sk-unit-placeholder';
const stop = AbortSignal.abort();
const opts = {
  cliPath: '/this-cli-is-not-executed', dshRoot: '/this-sdk-is-not-loaded', root: '/workspace', bareRepo: '/bare.repo',
  taskId: 'T', claimId: 'C', role: 'prover_worker', wallSeconds: 5, model: 'deepseek-v4-pro', signal: stop,
};

test('DeepSeek tasks require explicit consent and do not exchange fixture and public keys', async () => {
  for (const patch of [
    {}, { apiKey: publicShape }, { apiKey: fixtureKey, allowRemote: true },
    { apiKey: fixtureKey, allowRemote: 'true' }, { apiKey: publicShape, baseUrl: 'http://127.0.0.1:15000' },
    { apiKey: fixtureKey, allowRemote: true, baseUrl: 'http://127.0.0.1:15000' },
    { apiKey: publicShape + '\r\nmalicious: value', allowRemote: true },
    { apiKey: publicShape, allowRemote: true, model: '../unknown' },
  ]) await assert.rejects(runDeepSeekModelTask({ ...opts, ...patch }), error =>
    error?.code === 'invalid' && !error.message.includes('placeholder') && !error.message.includes('malicious'));
  // Cancelled invocations pass preflight but never start the SDK/CLI or fetch.
  await assert.rejects(runDeepSeekModelTask({ ...opts, apiKey: publicShape, allowRemote: true }), { code: 'cancelled' });
  await assert.rejects(runDeepSeekModelTask({ ...opts, apiKey: fixtureKey, baseUrl: 'http://127.0.0.1:65535' }), { code: 'cancelled' });
});

test('fixture endpoints have no redirect-like alias, URL credentials, path or query', async () => {
  for (const baseUrl of [
    'http://[::1]:1234', 'http://localhost:1234', 'http://127.1:1234', 'http://2130706433:1234',
    'http://127.0.0.1:0', 'http://127.0.0.1:00', 'http://127.0.0.1:65536', 'http://127.0.0.1:01234',
    'http://127.0.0.1:1234/', 'http://127.0.0.1:1234/x', 'http://127.0.0.1:1234?a=1',
    'http://127.0.0.1:1234#anchor', 'http://127.0.0.1:1234@service.test', 'https://api.deepseek.com/anthropic',
    'http://127.0.0.1:1234\n',
  ]) await assert.rejects(runDeepSeekModelTask({ ...opts, apiKey: fixtureKey, baseUrl }), { code: 'invalid' });
});
