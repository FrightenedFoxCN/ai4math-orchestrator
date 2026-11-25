// Trusted operator composition, not a Cordis plugin, browser endpoint or agent tool.
import { isAbsolute } from 'node:path';
import { runModelTaskCli } from './model-task-cli.mjs';
import { runModelTaskAssignment } from './model-task-runner.mjs';

const routePart = (part) => typeof part === 'string' && part.length <= 200 &&
  /^[A-Za-z0-9][A-Za-z0-9._:/-]*$/u.test(part) && !/[\r\n]/u.test(part);
const absolute = (path) => typeof path === 'string' && path.length <= 4096 && isAbsolute(path) && !path.includes('\0');

/** In-process seam for SDK fixtures and trusted local experiments only. It
 * cannot hard-stop a stuck adapter; never use it for resident model dispatch.
 * The isolated operator path is runDeepSeekModelTask. */
export async function runDshModelTask({ cliPath, root, bareRepo, taskId, claimId, role, wallSeconds,
  dshRoot, provider, model, adapter, registerAdapter, maxModelCalls, signal } = {}) {
  if (!absolute(dshRoot) || !routePart(provider) || !routePart(model) ||
      (adapter !== undefined) === (registerAdapter !== undefined) ||
      (registerAdapter !== undefined && typeof registerAdapter !== 'function') ||
      (adapter !== undefined && (!adapter || typeof adapter.prepareCall !== 'function')) ||
      (maxModelCalls !== undefined && (!Number.isInteger(maxModelCalls) || maxModelCalls < 1 || maxModelCalls > 8))) {
    throw Object.assign(new Error('A trusted SDK path, explicit model route and one adapter source are required.'), { code: 'invalid' });
  }
  return runModelTaskCli({
    cliPath, root, bareRepo, taskId, claimId, role, wallSeconds, signal,
    runAssignment: (packet, lifetime) => runModelTaskAssignment({
      dshRoot, provider, model, adapter, registerAdapter, maxModelCalls, packet, signal: lifetime,
    }),
  });
}
