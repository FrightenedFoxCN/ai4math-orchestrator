// Trusted worker-side adapter factory; no profile or ambient provider lookup.
import { randomUUID } from 'node:crypto';
import { createRequire } from 'node:module';
import { isAbsolute, join, relative, resolve, sep } from 'node:path';

const OFFICIAL = 'https://api.deepseek.com/anthropic';
const OFFLINE_FILES = Object.freeze({});
const safeToken = (value, pattern) => typeof value === 'string' && !/[\r\n]/u.test(value) && pattern.test(value);
const invalid = () => Object.assign(new Error('Explicit DeepSeek route, endpoint consent and matching API credential are required.'), { code: 'invalid' });

export function validateDeepSeekRoute({ dshRoot, model, apiKey, baseUrl, allowRemote, maxModelCalls }) {
  if (typeof dshRoot !== 'string' || Buffer.byteLength(dshRoot) > 4096 || !isAbsolute(dshRoot) ||
      /[\u0000-\u001f\u007f\ud800-\udfff]/u.test(dshRoot) ||
      !safeToken(model, /^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,95}$/u) ||
      !safeToken(apiKey, /^[A-Za-z0-9_-]{12,256}$/u) ||
      (maxModelCalls !== undefined && (!Number.isInteger(maxModelCalls) || maxModelCalls < 1 || maxModelCalls > 8))) throw invalid();
  const fixture = apiKey.startsWith('fixture-');
  if (baseUrl === undefined) {
    if (allowRemote !== true || fixture) throw invalid();
    return OFFICIAL;
  }
  // Only exact loopback and synthetic fixture credentials can use HTTP.
  const literal = typeof baseUrl === 'string' && baseUrl.length <= 24 && !/[\r\n]/u.test(baseUrl)
    && /^http:\/\/127\.0\.0\.1:([1-9][0-9]{0,4})$/u.exec(baseUrl);
  if (!literal || Number(literal[1]) > 65_535 || allowRemote !== false || !fixture) throw invalid();
  return baseUrl;
}

export function deepSeekAdapterFactory(route, wallSeconds) {
  const baseURL = validateDeepSeekRoute(route);
  const { dshRoot, model, apiKey } = route;
  return function registerAdapter() {
    const root = resolve(dshRoot);
    const lookup = createRequire(join(root, 'package.json'));
    const entry = lookup.resolve('@deepseek-ai/dsh-llm-deepseek');
    const owned = relative(root, entry);
    if (!owned || owned === '..' || owned.startsWith('..' + sep) || isAbsolute(owned)) throw invalid();
    const { DeepSeekAdapter, resolveAdapterOptions } = lookup(entry);
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
  };
}
