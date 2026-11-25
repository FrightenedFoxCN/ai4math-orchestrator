// Worker thread inside the dedicated model process. The process main thread
// watches the caller's lifetime and can exit even if the SDK event loop hangs.
import { parentPort, workerData } from 'node:worker_threads';
import { deepSeekAdapterFactory } from './model-task-deepseek-adapter.mjs';
import { MODEL_TASK_RESPONSE_MAX_BYTES } from './model-task-process.mjs';
import { runModelTaskAssignment } from './model-task-runner.mjs';

try {
  const { packet, route } = workerData;
  const result = await runModelTaskAssignment({
    packet, dshRoot: route.dshRoot, provider: 'deepseek', model: route.model,
    signal: new AbortController().signal, maxModelCalls: route.maxModelCalls,
    registerAdapter: deepSeekAdapterFactory(route, packet.max_wall_seconds),
  });
  const frame = JSON.stringify({ version: 1, result }) + '\n';
  if (Buffer.byteLength(frame) > MODEL_TASK_RESPONSE_MAX_BYTES) throw new Error('Model response exceeds its limit.');
  parentPort.postMessage(frame);
} catch {
  // Do not print exceptions (SDK errors may contain prompts or credentials).
  process.exitCode = 1;
}
