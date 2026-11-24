// Manual macOS acceptance from the repository root; requires operator-approved Seatbelt.
// env -i PATH=/usr/bin:/bin ELECTRON_RUN_AS_NODE=1 \
//   '/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness' \
//   plugins/dsh-ai4math/host/test/installed-t2-audit.mjs \
//   '/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh' \
//   /absolute/ai4mathd /absolute/ai4math /absolute/lean-toolchain/bin/lean [--independent|--heartbeats]
// Only the installed SDK libraries and a disposable ai4math project are used; no DSH profile or GUI.
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { constants, createReadStream } from 'node:fs';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs/promises';
import { createRequire, registerHooks } from 'node:module';
import { dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { NdjsonConnection, requireSocketPath } from '../connection.mjs';

const args = process.argv.slice(2);
const independent = args.at(-1) === '--independent';
const heartbeats = args.at(-1) === '--heartbeats';
if (independent || heartbeats) args.pop();
assert.ok(args.length === 4 && args.every(isAbsolute),
  'usage: installed-t2-audit.mjs DSH_SDK_ROOT AI4MATHD_BINARY AI4MATH_BINARY NATIVE_LEAN [--independent|--heartbeats]');
const [dshRoot, daemonPath, cliPath, leanPath] = args.map((arg) => resolve(arg));
const [daemonExe, cliExe, nativeLean] = await Promise.all([daemonPath, cliPath, leanPath].map((path) => fs.realpath(path)));
const lookup = createRequire(join(dshRoot, 'package.json'));
const externals = new Set([
  '@deepseek-ai/dsh-typert-protocol', '@deepseek-ai/dsh-typert-protocol/remote-info',
  '@deepseek-ai/dsh-scope', '@deepseek-ai/schemastery', 'zod',
]);
function sdkEntry(specifier) {
  const entry = lookup.resolve(specifier);
  const path = relative(dshRoot, entry);
  assert.ok(path && path !== '..' && !path.startsWith('..' + sep) && !isAbsolute(path),
    'SDK dependency resolved outside DSH_SDK_ROOT: ' + specifier);
  return entry;
}
const hostUrl = new URL('../', import.meta.url).href;
const hook = registerHooks({ resolve(specifier, ctx, next) {
  if (externals.has(specifier) && ctx.parentURL?.startsWith(hostUrl)) {
    return { url: pathToFileURL(sdkEntry(specifier)).href, shortCircuit: true };
  }
  return next(specifier, ctx);
}});

const AUDIT_TIMEOUT_MS = heartbeats ? 180_000 : independent ? 90_000 : 30_000;
const POLICY = 'ai4math-t2-release.json';
async function sha256(path) {
  const digest = createHash('sha256');
  for await (const bytes of createReadStream(path)) digest.update(bytes);
  return digest.digest('hex');
}
const RPC_TIMEOUT_MS = 1_000;
const MAIN = 'refs/heads/main';
const ANCHOR = 'Proof.lean';
const TOPIC = 'vital_signs.tick';

// A hard deadline even if an SDK operation fails to observe its cancellation signal.
async function untilAbort(signal, operation) {
  signal.throwIfAborted();
  let stop;
  const aborted = new Promise((_, reject) => {
    stop = () => reject(signal.reason);
    signal.addEventListener('abort', stop, { once: true });
  });
  try { return await Promise.race([aborted, Promise.resolve().then(operation)]); }
  finally { signal.removeEventListener('abort', stop); }
}

async function bounded(ms, label, operation) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new Error(label + ' timed out')), ms);
  try { return await untilAbort(controller.signal, operation); }
  finally { clearTimeout(timer); }
}

function startDaemon(root, configPath, env, controller) {
  const child = spawn(daemonExe, ['--project', root, '--tick-ms', '100', '--t2-audit', configPath],
    { cwd: root, env, stdio: ['ignore', 'ignore', 'inherit'] });
  child.once('error', (error) => controller.abort(error));
  const closed = new Promise((done) => child.once('close', (code, signal) => {
    controller.abort(new Error('ai4mathd exited unexpectedly: ' + (signal ?? code)));
    done();
  }));
  return async () => {
    // The daemon holds the lifeline to its native subprocess group. Collect the
    // daemon before removing the project, including on assertion/timeout paths.
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    try { await bounded(1_000, 'ai4mathd stop', () => closed); }
    catch {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      await bounded(2_000, 'ai4mathd collection', () => closed);
    }
  };
}

// Failure-only, opt-in local diagnostic. Use the same private CLI mode and a
// dedicated group/lifeline so no native process survives a diagnostic timeout.
async function diagnoseUnavailable(cli, root, bareRepo, reviewed, pins, signal) {
  const auditArgs = heartbeats
    ? ['t2-compare-managed-heartbeats', root, bareRepo, ANCHOR, pins.baseline_commit, reviewed, nativeLean,
      pins.compiler_sha256, String(pins.min_delta_heartbeats_raw), String(pins.min_increase_percent)]
    : [independent ? 't2-release-check' : 't2-audit', root, bareRepo, ANCHOR, reviewed, nativeLean,
      ...(independent ? [pins.policy_sha256, pins.compiler_sha256, pins.exporter_sha256, pins.checker_sha256] : [])];
  const child = spawn(cli, [...auditArgs, '--daemon-parent'], {
    cwd: root, env: { PATH: '/usr/bin:/bin' }, detached: true, stdio: ['pipe', 'ignore', 'pipe'],
  });
  const output = Buffer.alloc(8_192);
  let used = 0;
  let truncated = false;
  let spawnError;
  child.once('error', (error) => { spawnError = error; });
  child.stderr.on('data', (chunk) => {
    const copied = chunk.copy(output, used);
    used += copied;
    truncated ||= copied < chunk.length;
  });
  const closed = new Promise((done) => child.once('close', (code, bySignal) => done(bySignal ?? code)));
  try {
    const result = await bounded(10_000, 'private T2 diagnostic', () => untilAbort(signal, () => closed));
    if (spawnError) throw spawnError;
    console.error('private T2 diagnostic exit: ' + result + (used ? '\n' + output.toString('utf8', 0, used) : '') +
      (truncated ? '\n[diagnostic output truncated]' : ''));
  } finally {
    child.stdin.destroy(); // Closing the writer revokes the native subprocess group.
    try { await bounded(1_000, 'private T2 diagnostic stop', () => closed); }
    catch {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      await bounded(2_000, 'private T2 diagnostic collection', () => closed);
    }
  }
}

function assertObservation(status, reviewed, seq) {
  assert.equal(status.event_seq, seq, 'telemetry moved off the bound ledger generation');
  const audit = status.t2_audit;
  if (heartbeats) {
    assert.deepEqual(audit, {
      status: 'observation', attempts: 1, exhausted: true,
      observation: {
        schema: 'ai4math.t2.resident-heartbeats.v3', advisory_only: true, reviewed_commit: reviewed,
        ledger_event_seq: seq, regressed: true, max_command_regressed: true, total_commands_regressed: true,
      },
    });
    return audit;
  }
  const compileMs = audit?.observation?.compile_ms;
  assert.ok(Number.isSafeInteger(compileMs) && compileMs >= 0 && compileMs <= 120_000,
    'expected a bounded compile time in the T2 observation');
  // Exact equality of both objects rejects extra keys (paths, claims, source, logs, etc.).
  assert.deepEqual(audit, {
    status: 'observation', attempts: 1, exhausted: true,
    observation: {
      schema: 'ai4math.t2.resident-sample.v' + (independent ? '2' : '1'), advisory_only: true,
      reviewed_commit: reviewed, ledger_event_seq: seq, compile_ms: compileMs,
      module_count: 1, debt_findings: 0,
      ...(independent ? { independent_checks: 1 } : {}),
    },
  });
  return audit;
}

const controller = new AbortController();
const { signal } = controller;
const interrupt = (name) => controller.abort(new Error('acceptance interrupted by ' + name));
process.once('SIGINT', interrupt);
process.once('SIGTERM', interrupt);
let root, stopDaemon, fiber, iterator, timer;
const errors = [];
try {
  const plugin = await import('../index.mjs');
  const sdk = (name) => lookup(sdkEntry('@deepseek-ai/' + name));
  const { Context } = sdk('cordis');
  const { TypertRegistry } = sdk('dsh-typert-registry');
  const { TypertGatewayService } = sdk('dsh-api-gateway');
  // Avoid the long macOS TMPDIR and the /tmp alias; use a canonical, short UDS path.
  root = await fs.mkdtemp(join(await fs.realpath('/tmp'), 'a4t2-'));
  const socket = requireSocketPath(join(root, '.runtime', 'ai4mathd.sock'));
  const work = join(root, 'work');
  const bareRepo = join(root, 'repo.git');
  const cli = join(root, 'ai4math');
  const env = { PATH: '/usr/bin:/bin', HOME: root, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' };
  const run = (file, argv, cwd = root) => execFileSync(file, argv, {
    cwd, env, encoding: 'utf8', timeout: 5_000, killSignal: 'SIGKILL', stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
  const git = (cwd, ...argv) => run('/usr/bin/git', [
    '-c', 'user.name=T2 Audit Fixture', '-c', 'user.email=t2-fixture@example.invalid',
    '-c', 'commit.gpgSign=false', '-c', 'core.hooksPath=/dev/null', ...argv,
  ], cwd);
  const ai = (verb, ...argv) => run(cli, [verb, root, ...argv]);
  const ledgerSeq = () => JSON.parse(ai('status')).state.last_event_seq;
  const mainRef = () => git(bareRepo, 'for-each-ref', '--format=%(refname) %(objectname) %(objecttype) %(symref)', MAIN);

  await fs.mkdir(work, { mode: 0o700 });
  git(work, 'init', '--quiet', '--initial-branch=main', '--template=');
  await fs.writeFile(join(work, ANCHOR), independent
    ? 'prelude\nnamespace Fixture\ntheorem identity {P : Prop} : P → P := fun h => h\nend Fixture\n'
    : 'theorem fixtureProof : True := True.intro\n');
  const pins = {};
  if (independent) {
    await fs.writeFile(join(work, POLICY), JSON.stringify([{ target_module: ANCHOR, declaration: 'Fixture.identity' }]) + '\n');
    [pins.policy_sha256, pins.compiler_sha256, pins.exporter_sha256, pins.checker_sha256] = await Promise.all(
      [join(work, POLICY), nativeLean, join(dirname(nativeLean), 'leanexport'), join(dirname(nativeLean), 'nanoda_bin')].map(sha256));
  }
  git(work, 'add', '--', ANCHOR, ...(independent ? [POLICY] : []));
  git(work, 'commit', '--quiet', '-m', 'reviewed T2 fixture');
  if (heartbeats) {
    pins.baseline_commit = git(work, 'rev-parse', '--verify', MAIN);
    pins.compiler_sha256 = await sha256(nativeLean);
    pins.min_delta_heartbeats_raw = 1000;
    pins.min_increase_percent = 100;
    await fs.appendFile(join(work, ANCHOR), '\nset_option maxRecDepth 10000 in\n'
      + 'theorem fixtureHeavy : (List.range 1000).length = 1000 := by decide\n');
    git(work, 'add', '--', ANCHOR);
    git(work, 'commit', '--quiet', '-m', 'increase measured heartbeat workload');
  }
  const reviewed = git(work, 'rev-parse', '--verify', MAIN);
  git(root, 'clone', '--quiet', '--bare', '--no-hardlinks', '--template=', work, bareRepo);
  assert.equal(mainRef(), MAIN + ' ' + reviewed + ' commit');
  // Cargo can hardlink target/debug/ai4math. The daemon insists on a single-link executable.
  await fs.copyFile(cliExe, cli, constants.COPYFILE_EXCL);
  await fs.chmod(cli, 0o700);
  assert.equal((await fs.stat(cli)).nlink, 1);
  ai('init', 'lean4');
  assert.deepEqual(JSON.parse(ai('task-bind', bareRepo, ANCHOR, reviewed)), { bound: true, base_commit: reviewed });
  const seq = ledgerSeq();
  assert.ok(Number.isSafeInteger(seq) && seq > 0, 'expected a nonempty bound ledger');

  const configPath = join(root, 't2.json');
  await fs.writeFile(configPath, JSON.stringify({
    version: heartbeats ? 3 : independent ? 2 : 1, cli_executable: cli, bare_repo: bareRepo, anchor_module: ANCHOR,
    reviewed_main: reviewed, native_executable: nativeLean,
    interval_ms: 1_000, audit_timeout_ms: AUDIT_TIMEOUT_MS, max_runs: 1, ...pins,
  }), { flag: 'wx', mode: 0o600 });
  await fs.chmod(configPath, 0o600);

  const ctx = new Context();
  // Allow access to the gateway fixture, so guest rejection checks the actual Host policy.
  ctx.reflect.provide('remoteAccess', { decide: () => ({ kind: 'allow' }) });
  new TypertRegistry(ctx);
  const gateway = new TypertGatewayService(ctx, {
    websocketHeartbeatIntervalMs: 30_000, streamInboxBytes: 1024 * 1024, websocketCompression: 'none',
  });
  const peer = (kind = 'owner') => ({ principal: { kind }, signal });
  const request = (method, params = {}, kind = 'owner') => ({ namespace: 'ai4math', method, args: { params }, peer: peer(kind), signal });
  const dispatch = (method) => gateway.dispatchRpc('ai4math/' + method, { args: { params: {} } }, signal, peer());
  const wait = (operation) => untilAbort(signal, operation);
  const topics = { topics: [TOPIC] };

  timer = setTimeout(() => controller.abort(new Error('installed T2 audit deadline expired')), AUDIT_TIMEOUT_MS + 10_000);
  fiber = ctx.plugin(plugin.apply, { socketPath: socket, timeoutMs: RPC_TIMEOUT_MS });
  await wait(() => fiber);
  assert.ok(ctx.typert.local.get('ai4math/status'));
  assert.ok(ctx.typert.local.get('ai4math/subscribe'));
  stopDaemon = startDaemon(root, configPath, env, controller);
  // The first connection frame can be reconnecting if the daemon is still binding.
  const stream = await wait(() => gateway.stream(request('subscribe', topics)));
  iterator = stream[Symbol.asyncIterator]();
  let connected = false;
  let pushed;
  for (;;) {
    const { done, value } = await wait(() => iterator.next());
    assert.equal(done, false, 'owner stream ended before delivering a T2 observation');
    if (value.topic === 'ai4math.connection') {
      assert.ok(['connected', 'reconnecting'].includes(value.data.state));
      connected = value.data.state === 'connected';
      continue;
    }
    assert.ok(connected, 'received telemetry without a connected gateway stream');
    assert.equal(value.topic, TOPIC);
    assert.equal(value.data.event_seq, seq);
    const audit = value.data.t2_audit;
    if (audit?.status === 'observation') {
      pushed = assertObservation(value.data, reviewed, seq);
      break;
    }
    if (audit?.status === 'unavailable') {
      if (process.env.AI4MATH_T2_DIAG === '1') {
        try { await diagnoseUnavailable(cli, root, bareRepo, reviewed, pins, signal); }
        catch (error) { errors.push(error); }
      } else {
        console.error('Set AI4MATH_T2_DIAG=1 for a bounded local-only diagnostic on resident unavailability.');
      }
    }
    assert.ok(audit && !audit.exhausted && ['starting', 'running'].includes(audit.status),
      'resident audit failed to produce an observation: ' + JSON.stringify(audit));
  }

  // Compare observation objects, not whole status snapshots (other vitals can vary per read).
  const status = await wait(() => gateway.invoke(request('status')));
  assert.deepEqual(assertObservation(status, reviewed, seq), pushed);
  const wire = await wait(() => dispatch('status'));
  assert.equal(wire.ok, true);
  assert.deepEqual(assertObservation(wire.value, reviewed, seq), pushed);
  await assert.rejects(wait(() => gateway.invoke(request('status', {}, 'guest'))), { code: 'ai4math/denied' });
  await assert.rejects(wait(async () => {
    const guest = await gateway.stream(request('subscribe', topics, 'guest'));
    const guestIterator = guest[Symbol.asyncIterator]();
    try { await guestIterator.next(); }
    finally { await guestIterator.return?.(); }
  }), { code: 'ai4math/denied' });

  // The observation must already exist before making any launch-shaped requests.
  const uds = new NdjsonConnection(socket, { timeoutMs: RPC_TIMEOUT_MS });
  try {
    for (const [remote, local] of [['t2Audit', 't2.audit'], ['t2AuditRun', 't2.audit.run'], ['t2ReleaseCheck', 't2.releaseCheck'], ['t2CompareManagedHeartbeats', 't2.compareManagedHeartbeats']]) {
      assert.equal(ctx.typert.local.get('ai4math/' + remote), undefined);
      assert.equal((await wait(() => dispatch(remote))).ok, false);
      await assert.rejects(uds.request(local, {}, signal), { code: 'daemon', rpcCode: -32601 });
    }
  } finally { uds.close(); }

  assert.equal(mainRef(), MAIN + ' ' + reviewed + ' commit', 'audit changed managed main');
  assert.equal(ledgerSeq(), seq, 'audit appended a ledger event');
  signal.throwIfAborted();
} catch (error) {
  errors.push(error);
} finally {
  clearTimeout(timer);
  controller.abort();
  const cleanup = async (operation) => {
    try { await operation(); }
    catch (error) { errors.push(error); }
  };
  await cleanup(() => bounded(2_000, 'owner subscription cleanup', () => iterator?.return?.()));
  await cleanup(() => bounded(2_000, 'Host disposal', () => fiber?.dispose()));
  await cleanup(async () => {
    await stopDaemon?.();
    if (root) await fs.rm(root, { recursive: true, force: true });
  });
  process.removeListener('SIGINT', interrupt);
  process.removeListener('SIGTERM', interrupt);
  hook.deregister();
}
if (errors.length === 1) throw errors[0];
if (errors.length > 1) throw new AggregateError(errors, 'installed T2 audit or fixture cleanup failed');
console.log('Installed Typert + resident T2 ' + (heartbeats ? 'v3 heartbeats' : independent ? 'v2 independent' : 'v1 clean build')
  + ': matching advisory status/tick, guest denied, no launch RPC, Git and ledger unchanged');
