// Explicit operator-only integration with the installed DeepSeek Messages adapter.
// No plugin mount, provider discovery, profile reads, account flows or defaults
// for credentials/routing. The model only sees the task runner's two tools.
import { randomUUID } from 'node:crypto';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { runDshModelTask } from './model-task-host.mjs';

const OFFICIAL = 'https://api.deepseek.com/anthropic';
const OFFLINE_FILES = Object.freeze({}); // Only text; no SDK upload index or host file service.
const provider = 'deepseek';
const safeToken = (value, pattern) => typeof value === 'string' && !/[\r\n]/u.test(value) && pattern.test(value);
const invalid = () => Object.assign(new Error('Explicit DeepSeek route, endpoint consent and matching API credential are required.'), { code: 'invalid' });

function endpoint(baseUrl, allowRemote, apiKey) {
  if (!safeToken(apiKey, /^[A-Za-z0-9_-]{12,256}$/u)) throw invalid();
  const fixture = apiKey.startsWith('fixture-');
  if (baseUrl === undefined) {
    if (allowRemote !== true || fixture) throw invalid();
    return OFFICIAL;
  }
  // A literal loopback socket and a fake credential are required together.
  // No URL normalization or custom host accepted here; the installed SDK
  // uses fetch with redirect:error. Ambient networking remains operator-owned.
  const literal = typeof baseUrl === 'string' && baseUrl.length <= 24 && !/[\r\n]/u.test(baseUrl)
    && /^http:\/\/127\.0\.0\.1:([1-9][0-9]{0,4})$/u.exec(baseUrl);
  if (!literal || Number(literal[1]) > 65_535 || allowRemote !== false || !fixture) throw invalid();
  return baseUrl;
}

/** Submit one deliberately selected task. Real egress requires explicit
 * allowRemote:true and uses only the official HTTPS Messages endpoint.
 * An explicit loopback port instead requires a synthetic fixture-* credential.
 * Supply credentials in memory from an operator-controlled source, not argv. */
export async function runDeepSeekModelTask({ cliPath, root, bareRepo, taskId, claimId, role, wallSeconds,
  dshRoot, model, apiKey, baseUrl, allowRemote = false, maxModelCalls, signal } = {}) {
  if (!safeToken(model, /^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,95}$/u)) throw invalid();
  const baseURL = endpoint(baseUrl, allowRemote, apiKey);
  return runDshModelTask({
    cliPath, root, bareRepo, taskId, claimId, role, wallSeconds, dshRoot, provider, model, maxModelCalls, signal,
    registerAdapter() {
      const lookup = createRequire(join(dshRoot, 'package.json'));
      const { DeepSeekAdapter, resolveAdapterOptions } = lookup('@deepseek-ai/dsh-llm-deepseek');
      const connection = resolveAdapterOptions({
        baseURL, models: [{ id: model, inputModalities: ['text'] }], thinking: 'disabled', reasoningEffort: 'off',
        maxTokens: 4096, streamIdleTimeoutMs: wallSeconds * 1000, retryPolicy: { mode: 'normal', maxRetries: 0 },
      });
      const userId = randomUUID();
      return new DeepSeekAdapter({
        options: () => connection,
        resolveAuth: () => ({ headers: { 'x-api-key': apiKey } }),
        resolveUserId: () => userId,
        resolveFiles: () => OFFLINE_FILES,
        prepareExtensions: async () => ({ fields: {}, async accept() {} }),
      });
    },
  });
}
