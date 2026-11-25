// Explicit operator-only integration with the installed DeepSeek Messages adapter.
// No GUI mount, provider discovery, profile reads or ambient credentials.
import { runModelTaskCli } from './model-task-cli.mjs';
import { validateDeepSeekRoute } from './model-task-deepseek-adapter.mjs';
import { runModelTaskProcess } from './model-task-process.mjs';

/** Submit one deliberately selected task. Official HTTPS egress requires explicit
 * consent; a loopback port instead requires a synthetic fixture-* credential.
 * The credential is passed in memory over the private worker stdin, never argv. */
export async function runDeepSeekModelTask(options) {
  return runDeepSeek(options, false);
}

/** Consume one persistent operator approval, with no automatic redelivery. */
export async function runApprovedDeepSeekModelTask(options) {
  return runDeepSeek(options, true);
}

async function runDeepSeek({ cliPath, root, bareRepo, taskId, claimId, role, wallSeconds,
  dshRoot, model, apiKey, baseUrl, allowRemote = false, maxModelCalls, signal } = {}, approved) {
  const route = { dshRoot, model, apiKey, baseUrl, allowRemote, maxModelCalls };
  validateDeepSeekRoute(route);
  let worker;
  try {
    return await runModelTaskCli({
      cliPath, root, bareRepo, taskId, claimId, role, wallSeconds, signal,
      approval: approved ? { model, maxModelCalls } : undefined,
      runAssignment: (packet, lifetime) => (worker = runModelTaskProcess({
        packet, signal: lifetime,
        workerData: { adapter: 'deepseek', ...route, baseUrl: baseUrl ?? null, maxModelCalls: maxModelCalls ?? null },
      })),
    });
  } finally {
    // The CLI aborts its assignment lifetime before resolving or rejecting.
    // Join worker cleanup here so cancellation does not leave it in flight.
    await worker?.catch(() => {});
  }
}
