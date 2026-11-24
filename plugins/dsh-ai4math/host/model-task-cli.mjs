// Private Host transport. Nothing here is mounted as an agent tool or plugin.
import { spawn } from 'node:child_process';
import { isAbsolute } from 'node:path';
import { performance } from 'node:perf_hooks';
import { TextDecoder } from 'node:util';

const id = /^[A-Za-z0-9_-]{1,100}$/u;
const oid = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u;
const moduleName = /^[A-Z][A-Za-z0-9_]{0,250}\.lean$/u;
const declaration = /^[A-Za-z_][A-Za-z0-9_']{0,127}$/u;
const roles = new Set(['prover_worker', 'independent_reviewer']);
const verdicts = new Set(['looks_consistent', 'needs_changes', 'insufficient_context']);
const binding = ['task_id', 'claim_id', 'base_commit', 'role', 'module_path', 'target_decl'];
const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const packetLimit = 64 * 1024;
const summaryLimit = 512;
const resultLimit = 32 * 1024;
const stderrLimit = 64 * 1024;
const messages = {
  invalid: 'Invalid local model task options.',
  cancelled: 'Local model task was cancelled.',
  timeout: 'Local model task exceeded its deadline.',
  protocol: 'Local model task transport received an invalid frame.',
  assignment: 'The assigned model did not supply an admissible suggestion.',
  child: 'Local model task CLI did not finish successfully.',
};
const failure = (code) => Object.assign(new Error(messages[code]), { name: 'Ai4mathModelTaskError', code });

function record(value, keys) {
  if (!value || typeof value !== 'object' || ![Object.prototype, null].includes(Object.getPrototypeOf(value)) ||
      Reflect.ownKeys(value).length !== keys.length) return null;
  const copy = Object.create(null);
  for (const key of keys) {
    const prop = Object.getOwnPropertyDescriptor(value, key);
    if (!prop || !Object.hasOwn(prop, 'value')) return null;
    copy[key] = prop.value;
  }
  return copy;
}
const token = (value, pattern) => typeof value === 'string' && !/[\r\n]/u.test(value) && pattern.test(value);
const bounded = (value, n) => typeof value === 'string' && value.length <= n && Buffer.byteLength(value) <= n;
const text = (value, n) => bounded(value, n) && value.trim().length > 0 &&
  !/[\u0000-\u0008\u000b-\u001f\u007f-\u009f\ud800-\udfff]/u.test(value);
const path = (value) => bounded(value, 4096) && isAbsolute(value) && !/[\u0000-\u001f\u007f-\u009f\ud800-\udfff]/u.test(value);

function readPacket(value, subject) {
  const envelope = record(value, ['packet']);
  const packet = envelope && record(envelope.packet, [...binding, 'version', 'engine', 'max_wall_seconds', 'informal_statement', 'formal_header']);
  if (!packet || packet.version !== 1 || packet.engine !== 'lean4' || packet.task_id !== subject.taskId ||
      packet.claim_id !== subject.claimId || packet.role !== subject.role || packet.max_wall_seconds !== subject.wallSeconds ||
      !token(packet.base_commit, oid) || !token(packet.module_path, moduleName) || !token(packet.target_decl, declaration) ||
      !text(packet.informal_statement, 8192) || !text(packet.formal_header, 16_384)) throw failure('protocol');
  return Object.freeze({ ...packet });
}

function encodeResult(value, packet) {
  const result = record(value, [...binding, 'suggestion', 'proof_verified']);
  if (!result || result.proof_verified !== false || !binding.every((key) => result[key] === packet[key])) throw failure('assignment');
  const proving = packet.role === 'prover_worker';
  const suggestion = record(result.suggestion, proving ? ['kind', 'proof_body'] : ['kind', 'verdict', 'rationale']);
  if (!suggestion || (proving ? suggestion.kind !== 'proof_body' || !text(suggestion.proof_body, 8192)
      : suggestion.kind !== 'review' || !verdicts.has(suggestion.verdict) || !text(suggestion.rationale, 8192))) throw failure('assignment');
  result.suggestion = suggestion;
  const encoded = JSON.stringify(result) + '\n';
  if (Buffer.byteLength(encoded) > resultLimit) throw failure('assignment');
  return encoded;
}

function readSummary(value, packet) {
  const summary = record(value, ['task_id', 'base_commit', 'role', 'source', 'suggestion_recorded', 'proof_verified']);
  if (!summary || summary.task_id !== packet.task_id || summary.base_commit !== packet.base_commit || summary.role !== packet.role ||
      summary.source !== 'dsh_model' || summary.suggestion_recorded !== true || summary.proof_verified !== false) throw failure('protocol');
  return Object.freeze({ ...summary });
}

/** A trusted operator supplies all spawn arguments explicitly. The trusted,
 * abort-aware runAssignment gets only the approved packet + a lifetime signal;
 * it must provision and dispose a fresh restricted Agent and return that
 * assignment's takeResult(). Never retry automatically, including if the
 * result was sent but the acknowledgement could not be read. */
export async function runModelTaskCli({ cliPath, root, bareRepo, taskId, claimId, role, wallSeconds, runAssignment, signal } = {}) {
  if (![cliPath, root, bareRepo].every(path) || !token(taskId, id) || !token(claimId, id) || !roles.has(role) ||
      !Number.isInteger(wallSeconds) || wallSeconds < 1 || wallSeconds > 60 || typeof runAssignment !== 'function' ||
      (signal !== undefined && !(signal instanceof AbortSignal))) throw failure('invalid');
  if (signal?.aborted) throw failure('cancelled');

  const lifetime = new AbortController();
  const start = performance.now();
  const deadline = start + wallSeconds * 1000 + 4000;
  const reapDeadline = deadline + 1000;
  const ownGroup = process.platform !== 'win32';
  let child;
  try {
    child = spawn(cliPath, ['task-run-model', root, bareRepo, taskId, claimId, role, String(wallSeconds)], {
      cwd: process.platform === 'win32' ? undefined : '/', env: { PATH: '/usr/bin:/bin' }, shell: false,
      detached: ownGroup, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'],
    });
  } catch {
    throw failure('child');
  }

  return new Promise((resolve, reject) => {
    const frame = Buffer.allocUnsafe(packetLimit);
    let used = 0, stderrBytes = 0;
    let packet, summary, wrote = false, closed = false, settled = false, error, releaseTimer, reapTimer;
    const logicalTimer = setTimeout(() => stop('timeout'), Math.max(0, deadline - performance.now()));
    const onAbort = () => stop('cancelled');

    function finish(err) {
      if (settled) return;
      settled = true;
      signal?.removeEventListener('abort', onAbort);
      clearTimeout(logicalTimer);
      clearTimeout(releaseTimer);
      clearTimeout(reapTimer);
      lifetime.abort();
      if (err) reject(err);
      else resolve(summary);
    }

    function stop(code) {
      if (settled || error) return;
      error = failure(code);
      clearTimeout(logicalTimer);
      lifetime.abort();
      if (code === 'assignment' && packet && !wrote && !closed) {
        // Empty EOF lets the coordinator record an ordinary failed run and
        // release the live lease. If it doesn't exit promptly, keep the same
        // fail-closed process cleanup used for transport failures.
        child.stdin.end();
        releaseTimer = setTimeout(terminate, Math.max(0, Math.min(500, deadline - performance.now())));
      } else terminate();
    }

    function terminate() {
      if (!closed) {
        // Do not keep the operator waiting for an inherited pipe or an
        // unresponsive child. Group kill covers the CLI's Git descendants.
        try {
          if (ownGroup && child.pid !== undefined) process.kill(-child.pid, 'SIGKILL');
          else child.kill('SIGKILL');
        } catch {
          // Fall back to the direct child if the group no longer exists.
          try { child.kill('SIGKILL'); } catch { /* It may already have exited. */ }
        }
        child.stdin.destroy();
        child.stdout.destroy();
        child.stderr.destroy();
        reapTimer = setTimeout(() => { child.unref(); finish(error); }, Math.max(0, Math.min(1000, reapDeadline - performance.now())));
      } else finish(error);
    }

    function withinDeadline() {
      if (error || settled) return false;
      if (performance.now() >= deadline) { stop('timeout'); return false; }
      return true;
    }

    child.on('error', () => stop('child'));
    for (const pipe of [child.stdin, child.stdout, child.stderr]) pipe.on('error', () => stop('child'));
    child.stderr.on('data', (chunk) => {
      // Discard raw diagnostics; only a bounded byte counter is kept.
      if (!withinDeadline()) return;
      stderrBytes += chunk.length;
      if (stderrBytes > stderrLimit) stop('child');
    });
    child.stdout.on('data', (chunk) => {
      if (!withinDeadline()) return;
      let start = 0;
      try {
        while (start < chunk.length) {
          if (packet && (!wrote || summary)) return stop('protocol');
          const lf = chunk.indexOf(0x0a, start);
          const end = lf === -1 ? chunk.length : lf;
          const count = end - start;
          if (used + count > (packet ? summaryLimit : packetLimit)) return stop('protocol');
          chunk.copy(frame, used, start, end);
          used += count;
          if (lf === -1) return;
          const value = JSON.parse(decoder.decode(frame.subarray(0, used)));
          used = 0;
          start = end + 1;
          if (packet) {
            summary = readSummary(value, packet);
          } else {
            packet = readPacket(value, { taskId, claimId, role, wallSeconds });
            void Promise.resolve().then(async () => {
              if (!withinDeadline()) return;
              const result = await runAssignment(packet, lifetime.signal);
              if (!withinDeadline()) return;
              const line = encodeResult(result, packet);
              if (!withinDeadline()) return;
              wrote = true;
              // Rust requires EOF before admitting the single result frame.
              child.stdin.end(line);
            }).catch(() => stop('assignment'));
          }
        }
      } catch { stop('protocol'); }
    });
    child.once('close', (code, childSignal) => {
      closed = true;
      if (error) return finish(error);
      if (!withinDeadline()) return;
      if (code !== 0 || childSignal !== null) return stop('child');
      if (!packet || !wrote || !summary || used !== 0) return stop('protocol');
      finish();
    });
    signal?.addEventListener('abort', onAbort, { once: true });
    if (signal?.aborted) onAbort();
  });
}
