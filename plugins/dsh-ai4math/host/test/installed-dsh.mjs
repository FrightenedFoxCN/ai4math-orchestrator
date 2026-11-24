// Run under installed Electron in Node mode; no desktop profile, server, or GUI is touched.
import assert from 'node:assert/strict';
import { createRequire, registerHooks } from 'node:module';
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { execFileSync, spawn } from 'node:child_process';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { once } from 'node:events';

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
const processHandle=spawn(daemon,['--project',dir,'--tick-ms','100'],{stdio:['ignore','ignore','inherit']});
const closed=once(processHandle,'exit');
try {
  const ctx=new Context();
  // Anonymous and non-owner peers cannot acquire app access in this fixture.
  // A separate call below verifies the business service itself rejects a non-owner.
  ctx.reflect.provide('remoteAccess',{decide:()=>({kind:'allow'})});
  new TypertRegistry(ctx);
  const gateway=new TypertGatewayService(ctx,{ websocketHeartbeatIntervalMs: 30000, streamInboxBytes: 1024 * 1024, websocketCompression: "none" });
  plugin.apply(ctx,{socketPath:socket,timeoutMs:1000});
  assert.ok(ctx.typert.local.get('ai4math/status'));
  const signal=new AbortController().signal;
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
  await assert.rejects(gateway.invoke(request('status',{role:'chief'})));
  await assert.rejects(gateway.invoke(request('status',{}, {kind:'guest'})), {code:'ai4math/denied'});
  const wire = await gateway.dispatchRpc('ai4math/status', { args: { params: {} } }, signal, {principal:{kind:'owner'},signal});
  assert.equal(wire.ok,true);
  assert.equal(wire.value.event_seq,0);
  assert.equal(ctx.typert.local.get('ai4math/proofSubmit'),undefined);
  for (const [method, params, key] of [
    ['wikiProvenance', {id:'COR-A'}, 'edges'],
    ['wikiCheckGraveyard', {informal_statement:'example'}, 'warnings'],
  ]) {
    assert.ok(ctx.typert.local.get('ai4math/' + method));
    assert.deepEqual(await gateway.invoke(request(method, params)), {[key]:[], truncated:false});
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
  }
} finally {
  hook.deregister();
  if (processHandle.exitCode===null && processHandle.signalCode===null) processHandle.kill();
  await closed;
  await rm(dir,{recursive:true,force:true});
}
