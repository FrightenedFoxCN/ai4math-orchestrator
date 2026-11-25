import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { fileURLToPath } from 'node:url';

const entry = fileURLToPath(new URL('../model-task-process-entry.mjs', import.meta.url));

test('worker guardian exits when the parent lifeline reaches EOF, even with stdin still open', {
  skip: !['linux', 'darwin'].includes(process.platform), timeout: 5000,
}, async (t) => {
  const child = spawn(process.execPath, [entry], {
    shell: false, env: { PATH: '/usr/bin:/bin', ...(process.versions.electron ? { ELECTRON_RUN_AS_NODE: '1' } : {}) },
    stdio: ['pipe', 'pipe', 'pipe', 'pipe'],
  });
  t.after(() => { if (child.exitCode === null) child.kill('SIGKILL'); });
  const done = once(child, 'close');
  const stdout = [], stderr = [];
  child.stdout.on('data', (chunk) => stdout.push(chunk));
  child.stderr.on('data', (chunk) => stderr.push(chunk));
  child.stdio[3].destroy();
  const [code, signal] = await done;
  assert.equal(code, 1);
  assert.equal(signal, null);
  assert.equal(Buffer.concat(stdout).length + Buffer.concat(stderr).length, 0);
});
