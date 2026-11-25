import assert from 'node:assert/strict';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { createBridge } from '../bridge.mjs';
import { checkSocketPathLength, createWorkbench } from '../workbench.mjs';

function makeTempDir(t, prefix = 'a4w-') {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function writeExecutable(filePath, script) {
  writeFileSync(filePath, script, { mode: 0o755, encoding: 'utf8' });
  return filePath;
}

test('workbench initializes, persists, hot-switches projects, and supervises managed daemons', async (t) => {
  const scratch = makeTempDir(t, 'a4w-');
  const projA = join(scratch, 'a');
  const projB = join(scratch, 'b');
  mkdirSync(projA);
  mkdirSync(projB);
  const registryPath = join(scratch, 'workbench.json');

  // Fake ai4math CLI: creates .runtime/state.sqlite and .runtime/primary_engine
  const fakeCli = writeExecutable(join(scratch, 'fake-cli.mjs'), `#!/usr/bin/env node
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
const [cmd, root, engine] = process.argv.slice(2);
if (cmd !== 'init') process.exit(2);
mkdirSync(join(root, '.runtime'), { recursive: true, mode: 0o700 });
writeFileSync(join(root, '.runtime', 'state.sqlite'), 'stub');
writeFileSync(join(root, '.runtime', 'primary_engine'), engine + '\\n');
`);

  // Fake ai4mathd daemon: serves NDJSON status on <root>/.runtime/ai4mathd.sock
  const fakeDaemon = writeExecutable(join(scratch, 'fake-daemon.mjs'), `#!/usr/bin/env node
import { createServer } from 'node:net';
import { basename, join } from 'node:path';
const idx = process.argv.indexOf('--project');
const root = process.argv[idx + 1];
const sock = join(root, '.runtime', 'ai4mathd.sock');
const server = createServer((socket) => {
  let buf = '';
  socket.on('data', (chunk) => {
    buf += chunk.toString('utf8');
    let nl;
    while ((nl = buf.indexOf('\\n')) >= 0) {
      const msg = JSON.parse(buf.slice(0, nl));
      buf = buf.slice(nl + 1);
      if (msg.method === 'status') {
        socket.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result: { project: basename(root), event_seq: 1 } }) + '\\n');
      } else if (msg.method === 'subscribe') {
        socket.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result: { topics: msg.params.topics } }) + '\\n');
      }
    }
  });
});
server.listen(sock);
process.on('SIGTERM', () => server.close(() => process.exit(0)));
`);

  const bridge = createBridge({ reconnectMinMs: 20, reconnectMaxMs: 200, timeoutMs: 2000 });
  const wb = createWorkbench({
    bridge,
    cliPath: fakeCli,
    daemonPath: fakeDaemon,
    registryPath,
    rolePresets: { chief: 'ai4math-chief' },
    proofSlots: { prover: ['slot-1'] },
  });
  t.after(async () => { await wb.close(); bridge.close(); });

  // 1. Initialize project A with auto_start
  const initA = await wb.projectInit({ root: projA, engine: 'lean4', name: 'Alpha', auto_start: true });
  assert.equal(initA.ok, true);
  assert.equal(initA.initialized, true);
  assert.equal(initA.project.engine, 'lean4');
  assert.equal(initA.state.active_project_id, initA.project.id);
  assert.equal(initA.state.chief_preset, 'ai4math-chief');
  assert.equal(initA.state.chief_bound, true);
  assert.equal(initA.state.projects[0].daemon_status, 'managed');
  assert.deepEqual(await bridge.bind('owner').call('status', {}), { project: 'a', event_seq: 1 });

  // 2. Re-init same directory adopts existing project without clobbering engine
  const adoptA = await wb.projectInit({ root: projA, engine: 'agda', name: 'Alpha Renamed', auto_start: true });
  assert.equal(adoptA.ok, true);
  assert.equal(adoptA.initialized, false);
  assert.equal(adoptA.project.engine, 'lean4');
  assert.equal(adoptA.state.projects.length, 1);

  // 3. Initialize project B and verify managed daemon switches cleanly
  const initB = await wb.projectInit({ root: projB, engine: 'rocq', name: 'Beta', auto_start: true });
  assert.equal(initB.ok, true);
  assert.equal(initB.project.engine, 'rocq');
  assert.deepEqual(await bridge.bind('owner').call('status', {}), { project: 'b', event_seq: 1 });
  assert.equal(initB.state.projects.find((p) => p.id === initA.project.id).daemon_status, 'stopped');
  assert.equal(initB.state.projects.find((p) => p.id === initB.project.id).daemon_status, 'managed');

  // 4. Stop managed daemon explicitly
  const stopped = await wb.daemonStop({});
  assert.equal(stopped.ok, true);
  assert.equal(stopped.state.managed_daemon, null);
  assert.equal(stopped.state.projects.find((p) => p.id === initB.project.id).daemon_status, 'stopped');

  // 5. Select project A again and start its daemon
  const switched = await wb.projectSelect({ project_id: initA.project.id, auto_start: true });
  assert.equal(switched.ok, true);
  assert.deepEqual(await bridge.bind('owner').call('status', {}), { project: 'a', event_seq: 1 });

  // 6. Remove project A from Workbench list: stops managed daemon, keeps disk files intact
  const removed = await wb.projectRemove({ project_id: initA.project.id });
  assert.equal(removed.ok, true);
  assert.equal(removed.state.projects.length, 1);
  assert.equal(existsSync(join(projA, '.runtime', 'state.sqlite')), true, 'never deletes project data on disk');
});

test('workbench detects external daemons, never kills them, and rejects overlong socket paths', async (t) => {
  const scratch = makeTempDir(t, 'a4e-');
  const proj = join(scratch, 'ext');
  mkdirSync(join(proj, '.runtime'), { recursive: true });
  writeFileSync(join(proj, '.runtime', 'state.sqlite'), 'stub');
  writeFileSync(join(proj, '.runtime', 'primary_engine'), 'lean4\n');
  const sock = join(proj, '.runtime', 'ai4mathd.sock');
  const sockets = new Set();
  const server = createServer((socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
    socket.on('data', (chunk) => {
      const msg = JSON.parse(chunk.toString('utf8').trim());
      socket.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result: { external: true } }) + '\n');
    });
  });
  await new Promise((resolve) => server.listen(sock, resolve));
  t.after(() => new Promise((resolve) => {
    for (const s of sockets) s.destroy();
    server.close(resolve);
  }));

  const bridge = createBridge({ timeoutMs: 2000 });
  const wb = createWorkbench({ bridge, registryPath: join(scratch, 'wb.json') });
  t.after(async () => { await wb.close(); bridge.close(); });

  const adopted = await wb.projectInit({ root: proj, engine: 'lean4', auto_start: true });
  assert.equal(adopted.ok, true);
  assert.equal(adopted.state.projects[0].daemon_status, 'external');
  assert.equal(adopted.state.managed_daemon, null);
  assert.deepEqual(await bridge.bind('owner').call('status', {}), { external: true });

  // daemonStop refuses to touch external daemons
  const stopRes = await wb.daemonStop({});
  assert.equal(stopRes.ok, false);
  assert.equal(stopRes.code, 'external_or_stopped');
  assert.deepEqual(await bridge.bind('owner').call('status', {}), { external: true });

  // Overlong path check
  const deep = join(scratch, 'x'.repeat(50), 'y'.repeat(50));
  mkdirSync(deep, { recursive: true });
  assert.equal(checkSocketPathLength(deep).ok, false);
  const tooLong = await wb.projectInit({ root: deep, engine: 'lean4', auto_start: false });
  assert.equal(tooLong.ok, false);
  assert.equal(tooLong.code, 'socket_path_too_long');
});
