// Run under installed Electron in Node mode; no desktop profile, server, or GUI is touched.
import assert from 'node:assert/strict';
import { createRequire, registerHooks } from 'node:module';
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { execFileSync, spawn } from 'node:child_process';
import { lstat, mkdir, mkdtemp, rm, unlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';

const [dshRoot, daemon, cli] = process.argv.slice(2);
assert.ok(dshRoot && daemon, 'usage: installed-dsh.mjs DSH_SDK_ROOT AI4MATHD_BINARY [AI4MATH_BINARY]');
const lookup = createRequire(join(dshRoot,'package.json'));
const external = new Set(['@deepseek-ai/dsh-typert-protocol','@deepseek-ai/dsh-typert-protocol/remote-info','@deepseek-ai/dsh-scope','@deepseek-ai/schemastery','zod']);
const hook=registerHooks({resolve(specifier,ctx,next){
  if (external.has(specifier) && ctx.parentURL?.includes('/plugins/dsh-ai4math/')) {
    return { url:pathToFileURL(lookup.resolve(specifier)).href, shortCircuit:true };
  }
  return next(specifier,ctx);
}});
const plugin=await import('../index.mjs');
const {Context}=lookup('@deepseek-ai/cordis');
const {TypertRegistry}=lookup('@deepseek-ai/dsh-typert-registry');
const {TypertGatewayService}=lookup('@deepseek-ai/dsh-api-gateway');
const dir=await mkdtemp(join(tmpdir(),'ai4-'));
const socket=join(dir,'.runtime','ai4mathd.sock');
const daemons = new Set();
function startDaemon() {
  const child = spawn(daemon, ['--project', dir, '--tick-ms', '100'], { stdio: ['ignore', 'ignore', 'inherit'] });
  let error;
  child.once('error', (err) => { error = err; });
  const closed = new Promise((resolve) => child.once('close', (code, signal) => resolve({ code, signal, error })));
  const run = { child, closed };
  daemons.add(run);
  return run;
}
async function bounded(promise, label, ms = 5000) {
  const stop = new AbortController();
  try {
    return await Promise.race([promise, delay(ms, undefined, { signal: stop.signal }).then(() => { throw new Error(label + ' timed out'); })]);
  } finally { stop.abort(); }
}
async function stopDaemon(run) {
  if (run.child.exitCode === null && run.child.signalCode === null) run.child.kill();
  let result;
  try { result = await bounded(run.closed, 'daemon shutdown', 1000); }
  catch {
    run.child.kill('SIGKILL');
    result = await bounded(run.closed, 'daemon reaping', 2000);
  }
  daemons.delete(run);
  if (result.error) throw result.error;
}
const initialDaemon = startDaemon();
let fiber;
try {
  const ctx=new Context();
  // Exercise the installed dispatcher with synthetic peer identities. Admission
  // is deliberately permissive here to exercise the plugin's own role check;
  // this is not a browser login, credential or live-profile assertion.
  ctx.reflect.provide('remoteAccess',{decide:()=>({kind:'allow'})});
  new TypertRegistry(ctx);
  const gateway=new TypertGatewayService(ctx,{ websocketHeartbeatIntervalMs: 30000, streamInboxBytes: 1024 * 1024, websocketCompression: "none" });
  fiber = ctx.plugin((scoped) => plugin.apply(scoped,{socketPath:socket,timeoutMs:1000}));
  await bounded(fiber, 'plugin mount');
  assert.ok(ctx.typert.local.get('ai4math/status'));
  const signal=AbortSignal.timeout(20_000);
  const request=(method,params={},principal={kind:'owner'})=>({namespace:'ai4math',method,args:{params},peer:{principal,signal},signal});
  let result;
  for (let attempt=0;attempt<30;attempt++) {
    try { result=await gateway.invoke(request('status')); break; }
    catch (error) {
      if (attempt===29 || !/unavailable|disconnected/i.test(error.message)) throw error;
      await delay(50);
    }
  }
  assert.equal(result.event_seq,0);
  assert.equal(result.archived_breakers,0);
  assert.equal(ctx.typert.local.get('ai4math/pivotArchivePreview'),undefined);
  assert.equal(ctx.typert.local.get('ai4math/pivotArchiveApply'),undefined);
  await assert.rejects(gateway.invoke(request('status',{role:'chief'})));
  await assert.rejects(gateway.invoke(request('status',{}, {kind:'guest'})), {code:'ai4math/denied'});
  const wire = await gateway.dispatchRpc('ai4math/status', { args: { params: {} } }, signal, {principal:{kind:'owner'},signal});
  assert.equal(wire.ok,true);
  assert.equal(wire.value.event_seq,0);
  assert.equal(ctx.typert.local.get('ai4math/proofSubmit'),undefined);
  for (const [method, params, expected] of [
    ['wikiProvenance', {id:'COR-A'}, {edges:[], truncated:false, managed_core:false}],
    ['wikiCheckGraveyard', {informal_statement:'example'}, {warnings:[], truncated:false}],
  ]) {
    assert.ok(ctx.typert.local.get('ai4math/' + method));
    assert.deepEqual(await gateway.invoke(request(method, params)), expected);
    await assert.rejects(gateway.invoke(request(method, {...params, role:'owner'})));
    await assert.rejects(gateway.invoke(request(method, params, {kind:'guest'})), {code:'ai4math/denied'});
  }
  const stream=await gateway.stream(request('subscribe',{topics:['vital_signs.tick']}));
  const iterator=stream[Symbol.asyncIterator]();
  const connected=await iterator.next();
  assert.equal(connected.value.topic,'ai4math.connection');
  assert.equal(connected.value.data.state,'connected');
  const first=await iterator.next();
  assert.equal(first.value.topic,'vital_signs.tick');
  assert.equal(first.value.data.event_seq,0);
  await iterator.return();
  console.log('Installed Cordis + Typert Gateway + Rust UDS: owner, deny and stream passed');
  if (cli) {
    const fixture = join(dir, 'fixture');
    const repo = join(dir, 'repo.git');
    const run = (file,args,cwd=dir) => execFileSync(file,args,{ cwd, encoding:'utf8', timeout:5000 }).trim();
    const git = (...args) => run('/usr/bin/git', [
      '-c','user.name=Fixture','-c','user.email=fixture@localhost',
      '-c','commit.gpgSign=false','-c','core.hooksPath=/dev/null',...args,
    ], fixture);
    await mkdir(fixture);
    git('init','--quiet','--initial-branch=main');
    await writeFile(join(fixture,'Proof.lean'), 'theorem goal : True := by trivial\n');
    await writeFile(join(fixture,'Support.lean'), 'def helper : Nat := 0\n');
    await writeFile(join(fixture,'ai4math-modules.txt'), 'Proof.lean\nSupport.lean\n');
    git('add','.');
    git('commit','--quiet','-m','scheduler fixture');
    const head=git('rev-parse','HEAD');
    git('clone','--quiet','--bare',fixture,repo);
    const deadline=AbortSignal.timeout(10_000);
    const req=request('subscribe',{topics:['alert.p0']});
    const events=await gateway.stream({...req,signal:deadline,peer:{...req.peer,signal:deadline}});
    const alerts=events[Symbol.asyncIterator]();
    try {
      assert.equal((await alerts.next()).value.topic,'ai4math.connection');
      run(cli,['init',dir,'lean4']);
      run(cli,['task-bind',dir,repo,'Proof.lean',head]);
      run(cli,['task-register',dir,repo,'explore','exploration','Proof.lean','goal']);
      run(cli,['task-register',dir,repo,'infra','infrastructure','Support.lean','helper','explore']);
      run(cli,['trip-breaker',dir,repo,'fixture-breaker','Proof.lean','goal','deliberately blocked fixture']);
      const next=await alerts.next();
      assert.equal(next.value?.topic,'alert.p0');
      const alert=next.value.data;
      assert.equal(alert.reason_code,'ALL_TRACKS_BLOCKED');
      const kept=await gateway.invoke(request('briefingGet',{id:alert.briefing_id}));
      assert.equal(kept.fact_pack.alert_sources[0].event_seq,alert.event_seq);
      assert.equal(kept.fact_pack.alert_sources[0].payload.exploration_blocked,1);
      assert.equal(kept.fact_pack.alert_sources[0].payload.infrastructure_blocked,1);
      console.log('Installed Typert: real scheduler P0 push and retained Fact Pack passed');
    } finally {
      await alerts.return();
    }
    const selected=await gateway.invoke(request('briefingPull'));
    const reason='retain this archived queue gate';
    const args=[dir,repo,selected.retained_id,'fixture-breaker',reason];
    const plan=JSON.parse(run(cli,['pivot-archive-preview',...args]));
    assert.equal(plan.action,'ARCHIVE_BREAKER');
    assert.equal(plan.to_state,'ARCHIVED');
    assert.equal(git('--git-dir='+repo,'rev-parse','refs/heads/main'),head);
    const reqArchive=request('subscribe',{topics:['breaker.state_changed','vital_signs.tick']});
    const deadlineArchive=AbortSignal.timeout(10_000);
    const archiveStream=await gateway.stream({...reqArchive,signal:deadlineArchive,
      peer:{...reqArchive.peer,signal:deadlineArchive}});
    const observations=archiveStream[Symbol.asyncIterator]();
    try {
      assert.equal((await observations.next()).value.topic,'ai4math.connection');
      const applied=JSON.parse(run(cli,['pivot-archive-apply',...args,plan.adr_id]));
      assert.equal(applied.breaker_state,'ARCHIVED');
      assert.equal(applied.probes_started,false);
      let transition, tick;
      while (!transition || !tick) {
        const next=await observations.next();
        assert.ok(!next.done,'archive event and tick must arrive before deadline');
        const {topic,data}=next.value;
        if (topic==='breaker.state_changed' && data.event_type==='BREAKER_ARCHIVED') transition=data;
        if (topic==='vital_signs.tick' && data.archived_breakers===1) tick=data;
      }
      assert.equal(transition.payload.adr_id,plan.adr_id);
      assert.equal(transition.payload.state,'ARCHIVED');
      assert.equal(transition.git_commit,applied.git_commit);
      assert.equal(tick.half_open_breakers,0);
      assert.equal(tick.task_scheduler.exploration.breaker_blocked,1);
      const status=await gateway.invoke(request('status'));
      assert.equal(status.archived_breakers,1);
      assert.equal(status.event_seq,transition.event_seq);
      console.log('Installed Typert: exact approved archive event and retained queue gate passed');
    } finally {
      await observations.return();
    }
  }
  const reconnectSignal = AbortSignal.timeout(10_000);
  const reqTick = request('subscribe', {topics: ['vital_signs.tick']});
  const reconnectStream = await gateway.stream({...reqTick,signal:reconnectSignal,peer:{...reqTick.peer,signal:reconnectSignal}});
  const follow = reconnectStream[Symbol.asyncIterator]();
  async function nextMatching(predicate) {
    for (;;) {
      const item = await bounded(follow.next(), 'installed stream event');
      assert.ok(!item.done, 'stream expired before the expected lifecycle event');
      if (predicate(item.value)) return item.value;
    }
  }
  const transition = (expected) => (e) => e.topic === 'ai4math.connection' && e.data.state === expected;
  try {
    await nextMatching(transition('connected'));
    const before = await nextMatching((e) => e.topic === 'vital_signs.tick');
    const inode = await lstat(socket);
    assert.ok(inode.isSocket() && inode.uid === process.getuid());
    await stopDaemon(initialDaemon);
    await nextMatching(transition('reconnecting'));
    // The actual daemon refuses stale socket names. Only retire our unique
    // fixture socket after its process is collected and inode is confirmed.
    const stale = await lstat(socket).catch((err) => { if (err.code !== 'ENOENT') throw err; });
    if (stale) {
      assert.ok(stale.isSocket() && stale.dev === inode.dev && stale.ino === inode.ino);
      await unlink(socket);
    }
    startDaemon();
    await nextMatching(transition('connected'));
    const after = await nextMatching((e) => e.topic === 'vital_signs.tick');
    assert.ok(after.data.event_seq >= before.data.event_seq);

    await bounded(fiber.dispose(), 'plugin unload');
    fiber = undefined;
    for (;;) {
      if ((await bounded(follow.next(), 'disposed stream termination')).done) break;
    }
    assert.equal(ctx.typert.local.get('ai4math/status'), undefined);
    await assert.rejects(gateway.invoke(request('status')));
    console.log('Installed Cordis + Typert: reconnect after actual daemon restart and scoped unload passed');
  } finally { await bounded(follow.return(), 'close installed stream'); }
} finally {
  try { if (fiber) await bounded(fiber.dispose(), 'fixture unload'); }
  finally {
    hook.deregister();
    for (const run of daemons) await stopDaemon(run);
    await rm(dir,{recursive:true,force:true});
  }
}
