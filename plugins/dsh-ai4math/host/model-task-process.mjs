// Private assignment transport, not a browser endpoint, plugin or agent tool.
import { spawn } from 'node:child_process';
import { addAbortListener } from 'node:events';
import { isAbsolute } from 'node:path';
import { performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';
import { TextDecoder } from 'node:util';

export const MODEL_TASK_REQUEST_MAX_BYTES = 1024 * 1024;
export const MODEL_TASK_RESPONSE_MAX_BYTES = 32 * 1024 + 256;
const stderrLimit = 64 * 1024;
const defaultEntry = fileURLToPath(new URL('./model-task-process-entry.mjs', import.meta.url));
const utf8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const messages = {
  MODEL_TASK_ABORTED: 'Local assignment was cancelled.',
  MODEL_TASK_TIMEOUT: 'Local assignment exceeded its deadline.',
  MODEL_TASK_PROTOCOL: 'Local assignment returned an invalid frame.',
  MODEL_TASK_PROCESS: 'Local assignment process did not finish successfully.',
  MODEL_TASK_UNSUPPORTED: 'Local assignment isolation is not supported on this platform.',
  MODEL_TASK_REQUEST: 'Invalid local assignment request.',
};
const failure = (code) => Object.assign(new Error(messages[code]), { name: 'Ai4mathModelTaskProcessError', code });
const record = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const absolute = (value) => typeof value === 'string' && isAbsolute(value) && !value.includes('\0');

function killGroup(child, direct) {
  if (child.pid === undefined) return false;
  try {
    process.kill(-child.pid, 'SIGKILL');
    return true;
  } catch {
    // The group may have exited already. Do not signal a direct PID after its exit.
    if (direct) {
      try { child.kill('SIGKILL'); } catch { /* Node still owns the reap. */ }
    }
    return false;
  }
}

const active = new Set();
function onParentExit() {
  for (const child of active) killGroup(child, true);
}
function track(child) {
  if (active.size === 0) process.once('exit', onParentExit);
  active.add(child);
}
function untrack(child) {
  active.delete(child);
  if (active.size === 0) process.removeListener('exit', onParentExit);
}

/** Launch a fresh Node assignment with an already approved packet and JSON
 * workerData. `entry` is a trusted executable-code path (mainly a test seam);
 * never select it from packet content or from any worker response. Results are
 * still untrusted; the caller must check their schema and binding to the packet.
 *
 * A private process group contains ordinary inherited subprocesses, not a
 * malicious SDK or anything that leaves the group. The hard timer needs a
 * responsive parent event loop. Orderly Node exit attempts a synchronous group
 * kill only. An unwritten fd 3 gives the production entry an EOF lifeline on
 * parent death (including SIGKILL); custom entries need not monitor it. This is
 * not an OS watchdog or a guarantee of group cleanup after parent death.
 * While the parent lives, failures wait
 * for the direct child to be reaped: pathological kernel/OS failures can delay
 * this, and grandchildren are reaped by their own parent or the OS, not us.
 */
export async function runModelTaskProcess({ packet, signal, workerData, entry = defaultEntry } = {}) {
  const started = performance.now();
  // Fail closed rather than pretend an unsupported platform has group cleanup.
  if (process.platform !== 'darwin' && process.platform !== 'linux') throw failure('MODEL_TASK_UNSUPPORTED');
  if (signal !== undefined && !(signal instanceof AbortSignal)) throw failure('MODEL_TASK_REQUEST');
  if (signal?.aborted) throw failure('MODEL_TASK_ABORTED');

  let request, deadline;
  try {
    if (entry instanceof URL) entry = fileURLToPath(entry);
    if (!absolute(entry) || !absolute(process.execPath) || !record(packet) || !record(workerData)) throw new Error();
    const line = JSON.stringify({ version: 1, packet, workerData }) + '\n';
    if (Buffer.byteLength(line) > MODEL_TASK_REQUEST_MAX_BYTES) throw new Error();
    // Use the actual serialized packet, not a separately mutable deadline field.
    const sent = JSON.parse(line);
    const seconds = sent.packet?.max_wall_seconds;
    if (!record(sent.packet) || !record(sent.workerData) || !Number.isInteger(seconds) || seconds < 1 || seconds > 60) throw new Error();
    deadline = started + seconds * 1000;
    request = Buffer.from(line);
  } catch {
    throw failure('MODEL_TASK_REQUEST');
  }
  if (signal?.aborted) throw failure('MODEL_TASK_ABORTED');
  if (performance.now() >= deadline) throw failure('MODEL_TASK_TIMEOUT');

  let child;
  try {
    // Do not inherit PATH, NODE_OPTIONS, proxy settings or credentials. Node and
    // Electron launch through the host runtime's absolute executable path.
    const env = { ELECTRON_RUN_AS_NODE: '1' }; // Ordinary Node ignores this fixed flag.
    // The parent keeps fd 3 open and unwritten for the fixed entry's guardian.
    child = spawn(process.execPath, [entry], { env, cwd: '/', shell: false, detached: true, stdio: ['pipe', 'pipe', 'pipe', 'pipe'] });
  } catch {
    throw failure('MODEL_TASK_PROCESS');
  }
  track(child);

  return new Promise((resolve, reject) => {
    const frame = Buffer.alloc(MODEL_TASK_RESPONSE_MAX_BYTES - 1);
    let used = 0, stderrBytes = 0;
    let frameEnded = false, stdinFinished = false, exited = false, closed = false;
    let groupSignaled = false, error, timer, abortSubscription;

    function stop(code) {
      if (error) return;
      error = failure(code);
      clearTimeout(timer);
      if (closed) return;
      if (!exited) groupSignaled = killGroup(child, true);
      // Descendants that left the group may still hold pipes. Do not wait for
      // their EOF on failure, but do wait for libuv's direct child close event.
      child.stdin.destroy();
      child.stdout.destroy();
      child.stderr.destroy();
      child.stdio[3]?.destroy();
    }
    function current() {
      if (error) return false;
      if (signal?.aborted) stop('MODEL_TASK_ABORTED');
      else if (performance.now() >= deadline) stop('MODEL_TASK_TIMEOUT');
      return !error;
    }
    function arm() {
      if (current()) timer = setTimeout(arm, Math.ceil(deadline - performance.now()));
    }
    function processFailure() {
      if (current()) stop('MODEL_TASK_PROCESS');
    }
    child.on('error', processFailure);
    for (const pipe of child.stdio) pipe?.on('error', processFailure);
    child.stdin.once('finish', () => { stdinFinished = true; });
    child.stdout.on('data', (chunk) => {
      if (!current()) return;
      const lf = chunk.indexOf(0x0a);
      const count = lf === -1 ? chunk.length : lf;
      if (frameEnded || used + count > frame.length || (lf !== -1 && lf !== chunk.length - 1)) {
        stop('MODEL_TASK_PROTOCOL');
        return;
      }
      chunk.copy(frame, used, 0, count);
      used += count;
      frameEnded = lf !== -1;
    });
    child.stderr.on('data', (chunk) => {
      if (!current()) return;
      stderrBytes += chunk.length; // Keep no raw diagnostics.
      if (stderrBytes > stderrLimit) stop('MODEL_TASK_PROCESS');
    });
    child.once('exit', (code, bySignal) => {
      exited = true;
      // Best-effort cleanup of ordinary same-group descendants, including any
      // keeping stdio open. Never signal an exited leader directly or repeat
      // a successful group signal. POSIX provides no stable group handle.
      if (!groupSignaled) killGroup(child, false);
      child.stdio[3]?.destroy();
      untrack(child);
      if (code !== 0 || bySignal !== null) processFailure();
    });
    child.once('close', (code, bySignal) => {
      closed = true;
      untrack(child);
      clearTimeout(timer);
      abortSubscription?.[Symbol.dispose]();
      if (current()) {
        if (code !== 0 || bySignal !== null || !stdinFinished) stop('MODEL_TASK_PROCESS');
        else if (!frameEnded) stop('MODEL_TASK_PROTOCOL');
      }
      if (error) return reject(error);
      let response;
      try {
        const line = utf8.decode(frame.subarray(0, used));
        response = JSON.parse(line);
        // Compact JSON.stringify form rejects alternate framing, duplicate
        // keys and trailing data; concrete assignment validation is upstream.
        if (!record(response) || Object.keys(response).length !== 2 || response.version !== 1 ||
            !Object.hasOwn(response, 'result') || line !== JSON.stringify(response)) throw new Error();
      } catch {
        stop('MODEL_TASK_PROTOCOL');
      }
      if (!current()) return reject(error);
      resolve(response.result);
    });
    if (signal) abortSubscription = addAbortListener(signal, () => stop('MODEL_TASK_ABORTED'));
    arm();
    if (current()) child.stdin.end(request);
  });
}
