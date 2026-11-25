// Bounded, credential-blind local scan transport. Never mounted as a plugin tool.
import { spawn } from 'node:child_process';
import { addAbortListener } from 'node:events';
import { performance } from 'node:perf_hooks';
import { TextDecoder } from 'node:util';

const outputLimit = 160 * 1024; // 16 rows, including their escaped absolute paths
const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const messages = {
  cancelled: 'Local model dispatch scan was cancelled.',
  timeout: 'Local model dispatch scan exceeded its deadline.',
  protocol: 'Local model dispatch scan returned invalid output.',
  child: 'Local model dispatch scan did not finish successfully.',
};
export const scanFailure = (code) => Object.assign(new Error(messages[code]), { name: 'Ai4mathModelDispatchScanError', code });

// Internal use only; the operator API validates every argument before this call.
export function readModelDispatchScan({ cliPath, root, bareRepo, model, scanLimit, scanTimeoutMs, signal }) {
  if (signal?.aborted) throw scanFailure('cancelled');
  const deadline = performance.now() + scanTimeoutMs;
  const ownGroup = process.platform !== 'win32';
  let child;
  try {
    child = spawn(cliPath, ['model-dispatch-resident-scan', root, bareRepo, model, String(scanLimit)], {
      cwd: ownGroup ? '/' : undefined, env: { PATH: '/usr/bin:/bin' }, shell: false,
      detached: ownGroup, windowsHide: true, stdio: ['ignore', 'pipe', 'ignore'],
    });
  } catch { throw scanFailure('child'); }

  return new Promise((resolve, reject) => {
    const output = Buffer.allocUnsafe(outputLimit);
    let used = 0, exited = false, closed = false, settled = false, groupSignalled = false;
    let error, reapTimer, subscription;
    const timer = setTimeout(() => stop('timeout'), Math.max(0, Math.ceil(deadline - performance.now())));

    function finish(err, value) {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(reapTimer);
      subscription?.[Symbol.dispose]();
      if (err) reject(err);
      else resolve(value);
    }

    function killGroup() {
      if (groupSignalled) return;
      // Scan may have ordinary Git descendants. Do not signal the direct PID
      // after libuv reports it exited, or repeat a successful group signal.
      if (ownGroup && child.pid !== undefined) {
        try { process.kill(-child.pid, 'SIGKILL'); groupSignalled = true; return; }
        catch { /* group may have exited */ }
      }
      if (!exited) {
        try { child.kill('SIGKILL'); } catch { /* already exited */ }
      }
    }

    function stop(code) {
      if (error || settled) return;
      error = scanFailure(code);
      clearTimeout(timer);
      if (closed) return finish(error);
      killGroup();
      child.stdout.destroy();
      reapTimer = setTimeout(() => { child.unref(); finish(error); }, 500);
    }

    function withinDeadline() {
      if (error || settled) return false;
      if (signal?.aborted) { stop('cancelled'); return false; }
      if (performance.now() >= deadline) { stop('timeout'); return false; }
      return true;
    }

    child.on('error', () => stop('child'));
    child.stdout.on('error', () => stop('child'));
    child.stdout.on('data', (chunk) => {
      if (!withinDeadline()) return;
      if (used + chunk.length > outputLimit) return stop('protocol');
      chunk.copy(output, used);
      used += chunk.length;
    });
    child.once('exit', () => {
      exited = true;
      killGroup();
    });
    child.once('close', (code, childSignal) => {
      closed = true;
      if (error) return finish(error);
      if (!withinDeadline()) return;
      if (code !== 0 || childSignal !== null) return stop('child');
      let text, value;
      try {
        text = decoder.decode(output.subarray(0, used));
        value = JSON.parse(text);
      } catch { return stop('protocol'); }
      // Count property tokens, not colons inside strings. Combined with exact
      // parsed shapes this rejects duplicate JSON members instead of silently
      // accepting the last value. The regular expression sees valid JSON only.
      let members = 0;
      for (const match of text.matchAll(/"(?:[^"\\]|\\[\s\S])*"([\t\n\r ]*:)?/gu)) if (match[1]) members++;
      if (withinDeadline()) finish(null, { value, members });
    });
    if (signal) subscription = addAbortListener(signal, () => stop('cancelled'));
    if (signal?.aborted) stop('cancelled');
  });
}
