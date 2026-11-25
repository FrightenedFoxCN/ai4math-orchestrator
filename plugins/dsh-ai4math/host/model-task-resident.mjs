// Explicit operator-only local loop. Not a plugin tool, RPC, daemon, or profile hook.
import { realpath } from 'node:fs/promises';
import { isAbsolute } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { validateDeepSeekRoute } from './model-task-deepseek-adapter.mjs';
import { runApprovedDeepSeekModelTask } from './model-task-deepseek.mjs';
import { readModelDispatchScan, scanFailure } from './model-task-resident-scan.mjs';

const fields = ['task_id', 'claim_id', 'role', 'repo', 'base_commit', 'model', 'max_wall_seconds', 'max_model_calls', 'state', 'event_seq'];
const id = /^[a-zA-Z0-9_-]{1,100}$/u;
const oid = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u;
const token = (s, pattern) => typeof s === 'string' && !/[\r\n]/u.test(s) && pattern.test(s);
const path = (s) => typeof s === 'string' && Buffer.byteLength(s) <= 4096 && isAbsolute(s) && !/[\u0000-\u001f\u007f-\u009f\ud800-\udfff]/u.test(s);
const between = (n, min, max) => Number.isSafeInteger(n) && n >= min && n <= max;
const exactKeys = (obj, keys) => obj !== null && typeof obj === 'object' && !Array.isArray(obj) &&
  Object.keys(obj).length === keys.length && keys.every((key) => Object.hasOwn(obj, key));
const invalid = () => Object.assign(new Error('Invalid local model resident options.'), { code: 'invalid' });
let active = false;

function validateScan({ value, members }, { bareRepo, model, scanLimit }) {
  if (!exactKeys(value, ['pending', 'reconciled']) || !between(value.reconciled, 0, scanLimit) ||
      !Array.isArray(value.pending) || value.pending.length > scanLimit || members !== 2 + fields.length * value.pending.length) {
    throw scanFailure('protocol');
  }
  const taskIds = new Set();
  let base;
  for (const row of value.pending) {
    if (!exactKeys(row, fields) || !token(row.task_id, id) || !token(row.claim_id, id) ||
        (row.role !== 'prover_worker' && row.role !== 'independent_reviewer') ||
        row.repo !== bareRepo || !token(row.base_commit, oid) || (base !== undefined && base !== row.base_commit) ||
        row.model !== model || row.state !== 'approved' || !between(row.max_wall_seconds, 1, 60) ||
        !between(row.max_model_calls, 1, 8) || !between(row.event_seq, 1, Number.MAX_SAFE_INTEGER) || taskIds.has(row.task_id)) {
      throw scanFailure('protocol');
    }
    taskIds.add(row.task_id);
    base = row.base_commit;
  }
  return value;
}

/** Run one explicitly bounded, process-singleton resident invocation.
 * Scan immediately, then wait pollIntervalMs between scans (including empty
 * scans). Scan failure is terminal. Every dispatch attempt costs one launch,
 * even if the atomic gate rejects it or the acknowledgement is unknown. Do
 * not retry that ID in this invocation; across executions the SQLite gate
 * remains authoritative. Cancellation joins scan/CLI/worker teardown.
 *
 * Required integer limits: maxCycles and maxLaunches 1..256, pollIntervalMs
 * 1..60_000, scanTimeoutMs 1..30_000, scanLimit 1..16. Per-worker wall and
 * call limits come only from the approval. No discovery/default credentials.
 * Linux/macOS only, matching the isolated assignment process.
 */
export async function runDeepSeekModelResident({ cliPath, dshRoot, root, bareRepo, model, apiKey, baseUrl,
  allowRemote = false, maxCycles, maxLaunches, pollIntervalMs, scanTimeoutMs, scanLimit, signal } = {}) {
  if (![cliPath, dshRoot, root, bareRepo].every(path) || !between(maxCycles, 1, 256) ||
      !between(maxLaunches, 1, 256) || !between(pollIntervalMs, 1, 60_000) ||
      !between(scanTimeoutMs, 1, 30_000) || !between(scanLimit, 1, 16) ||
      (signal !== undefined && !(signal instanceof AbortSignal))) throw invalid();
  if (process.platform !== 'darwin' && process.platform !== 'linux') throw invalid();
  const route = { dshRoot, model, apiKey, baseUrl, allowRemote };
  validateDeepSeekRoute(route);
  const stats = { cycles: 0, attempted: 0, recorded: 0, unconfirmed: 0, reconciled: 0 };
  const done = (reason) => Object.freeze({ ...stats, reason });
  if (signal?.aborted) return done('cancelled');
  if (active) throw Object.assign(new Error('A local model resident is already running in this process.'), { code: 'busy' });
  active = true;
  try {
    try { [root, bareRepo] = await Promise.all([realpath(root), realpath(bareRepo)]); }
    catch { throw invalid(); }
    if (![root, bareRepo].every(path)) throw invalid();
    const scan = { cliPath, root, bareRepo, model, scanLimit, scanTimeoutMs, signal };
    const attemptedIds = new Set();
    while (stats.cycles < maxCycles) {
      if (signal?.aborted) return done('cancelled');
      stats.cycles++;
      let batch;
      try { batch = validateScan(await readModelDispatchScan(scan), scan); }
      catch (err) {
        if (signal?.aborted) return done('cancelled');
        throw err;
      }
      stats.reconciled += batch.reconciled;
      for (const row of batch.pending) {
        if (signal?.aborted) return done('cancelled');
        if (attemptedIds.has(row.task_id)) continue;
        // Mark before touching the gate; never infer a safe retry from failure.
        attemptedIds.add(row.task_id);
        stats.attempted++;
        try {
          await runApprovedDeepSeekModelTask({ cliPath, root, bareRepo, signal, ...route,
            taskId: row.task_id, claimId: row.claim_id, role: row.role,
            wallSeconds: row.max_wall_seconds, maxModelCalls: row.max_model_calls });
          stats.recorded++;
        } catch { stats.unconfirmed++; }
        if (signal?.aborted) return done('cancelled');
        if (stats.attempted === maxLaunches) return done('launch_budget');
      }
      if (signal?.aborted) return done('cancelled');
      if (stats.cycles < maxCycles) {
        try { await delay(pollIntervalMs, undefined, { signal }); }
        catch (err) {
          if (signal?.aborted) return done('cancelled');
          throw err;
        }
      }
    }
    return done('cycle_budget');
  } finally { active = false; }
}
