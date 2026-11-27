import assert from 'node:assert/strict';
import { test } from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { BridgeError, MAX_PENDING, NdjsonConnection, requireSocketPath } from '../connection.mjs';
import { AGENT_ROLES, createBridge, MAX_STREAMS, methodsForRole, parseParams } from '../bridge.mjs';
import { fixture, line, push, reply } from './fixture.mjs';
import { REMOTE_METHODS } from '../remote-contract.mjs';
import { readFileSync } from 'node:fs';

const event = 'vital_signs.tick';
const topics = [event];
const abort = () => new AbortController();
const fails = (code) => (error) => error instanceof BridgeError && error.code === code;

test('reassembles split UTF-8 + CRLF and matches out-of-order replies', async (t) => {
  let first;
  const socketPath = await fixture(t, (request, socket) => {
    if (!first) { first = request; return; }
    const second = line({ id: request.id, result: { text: '证✅' } });
    const out = Buffer.from(second.replace(/\n$/, '\r\n') + line({ id: first.id, result: { first: true } }));
    const split = out.indexOf(Buffer.from('证')) + 1;
    socket.write(out.subarray(0, split));
    setImmediate(() => socket.write(out.subarray(split)));
  });
  const conn = new NdjsonConnection(socketPath);
  t.after(() => conn.close());
  assert.deepEqual(await Promise.all([conn.request('a', {}), conn.request('b', {})]), [{ first: true }, { text: '证✅' }]);
});

test('rejects oversized, malformed UTF-8, corrupt envelopes and excessive JSON depth; next calls recover', async (t) => {
  const socketPath = await fixture(t, ({ id }, socket, generation) => {
    if (generation === 1) return socket.write(Buffer.alloc(150, 65));
    if (generation === 2) return socket.write(Buffer.from([0xc0, 0x80, 10]));
    if (generation === 3) return socket.write(line({ id, result: {}, error: { code: -1, message: 'extra' } }));
    if (generation === 4) return socket.write('{"jsonrpc":"2.0","id":"' + id + '","result":1e700}\n');
    if (generation === 5) return socket.write('{"jsonrpc":"2.0","id":"' + id + '","result":' + '['.repeat(70) + '0' + ']'.repeat(70) + '}\n');
    reply(socket, id, { generation });
  });
  const conn = new NdjsonConnection(socketPath, { maxFrameBytes: 128, timeoutMs: 1000 });
  t.after(() => conn.close());
  for (let i = 0; i < 5; i++) await assert.rejects(conn.request('a', {}), fails('protocol'));
  assert.deepEqual(await conn.request('a', {}), { generation: 6 });
});

test('deadline, cancellation, late replies, and errors do not replay or disclose internals', async (t) => {
  const calls = [];
  const socketPath = await fixture(t, ({ id, method }, socket) => {
    calls.push(method);
    if (method === 'slow') return setTimeout(() => reply(socket, id, { late: true }), 130);
    if (method === 'failure') return socket.write(line({ id, error: { code: -32017, message: '/host/private/info', data: { secret: 'x' } } }));
    reply(socket, id, { ok: true });
  });
  const conn = new NdjsonConnection(socketPath, { timeoutMs: 75 });
  t.after(() => conn.close());
  await assert.rejects(conn.request('slow', {}), fails('timeout'));
  const ac = abort();
  ac.abort();
  await assert.rejects(conn.request('never-sent', {}, ac.signal), fails('cancelled'));
  await assert.rejects(conn.request('failure', {}), (error) => fails('daemon')(error) && error.rpcCode === -32017 && !error.message.includes('/host/'));
  await delay(85);
  assert.deepEqual(await conn.request('status', {}), { ok: true });
  assert.deepEqual(calls, ['slow', 'failure', 'status']);
});

test('bounds pending calls and closes all with cancellation', async (t) => {
  const socketPath = await fixture(t, () => {});
  const conn = new NdjsonConnection(socketPath);
  t.after(() => conn.close());
  const pending = Array.from({ length: MAX_PENDING }, () => assert.rejects(conn.request('held', {}), fails('cancelled')));
  await assert.rejects(conn.request('extra', {}), fails('busy'));
  conn.close();
  await Promise.all(pending);
});

test('allows only individually bound role operations and rejects role/command fields', async (t) => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => { seen.push({ method, params }); reply(socket, id, {}); });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const invalid of [null, '../socket', '127.0.0.1:3200', '/a\0b']) assert.throws(() => requireSocketPath(invalid), fails('unconfigured'));
  assert.deepEqual(methodsForRole('toString'), []);
  await assert.rejects(bridge.bind('ghost').call('status', {}), fails('denied'));
  await assert.rejects(bridge.bind('chief').call('command.run', {}), fails('denied'));
  await assert.rejects(bridge.bind('chief').call('alertsList', {}), fails('denied'));
  await assert.rejects(bridge.bind('reviewer').call('briefingPeek', {}), fails('denied'));
  await assert.rejects(bridge.bind('prover').call('frontierList', {}), fails('denied'));
  await assert.rejects(bridge.bind('prover').call('wikiSearch', { text: 'lemma', scope: 'sediment' }), fails('denied'));
  await assert.rejects(bridge.bind('owner').call('status', { role: 'chief' }), fails('invalid'));
  await assert.rejects(bridge.bind('owner').call('frontierList', { limit: 1000 }), fails('invalid'));
  await assert.rejects(bridge.bind('owner').call('wikiSearch', { text: 'math', path: '/root' }), fails('invalid'));
  await assert.rejects(bridge.bind('owner').call('wikiSearch', { text: 'x'.repeat(513) }), fails('invalid'));
  await assert.rejects(bridge.bind('owner').call('wikiSearch', { text: '   ' }), fails('invalid'));
  assert.throws(() => parseParams('subscribe', { topics: [event, event] }), fails('invalid'));
  assert.throws(() => parseParams('status', Object.create({ role: 'owner' })), fails('invalid'));
  await assert.rejects(bridge.bind('chief').call('briefingPull', {}), fails('denied'));
  const proof = { target_id: 'slot-A', proof_body: 'by rfl' };
  await assert.rejects(bridge.bind('prover').call('proofSubmit', proof), fails('denied'));
  await assert.rejects(bridge.bind('owner', { proofSlots: ['slot-A'] }).call('proofSubmit', proof), fails('denied'));
  await assert.rejects(bridge.bind('prover', { proofSlots: ['other'] }).call('proofSubmit', proof), fails('denied'));
  await assert.rejects(bridge.bind('prover', { proofSlots: ['slot-A'] }).call('proofSubmit', { ...proof, executable: '/bin/sh' }), fails('invalid'));
  await bridge.bind('prover', { proofSlots: ['slot-A'] }).call('proofSubmit', proof);
  await bridge.bind('owner').call('briefingPull', {});
  await bridge.bind('prover').call('wikiSearch', { text: 'lemma' });
  await bridge.bind('reviewer').call('wikiSearch', { text: 'lemma', scope: 'graveyard' });
  await bridge.bind('owner').call('alertsList', { after: 0 });
  assert.deepEqual(seen, [
    { method: 'proof.submit', params: proof }, { method: 'briefing.pull', params: {} }, { method: 'wiki.search', params: { text: 'lemma' } },
    { method: 'wiki.search', params: { text: 'lemma', scope: 'graveyard' } }, { method: 'alerts.list', params: { after: 0 } },
  ]);
});



test('Owner Remote descriptors and handlers match, while role tools stay agent-only', () => {
  const source = readFileSync(new URL('../index.mjs', import.meta.url), 'utf8');
  for (const [key, route] of Object.entries(REMOTE_METHODS)) {
    assert.equal(Object.hasOwn(REMOTE_METHODS, 'session.bind'), false);
    if (route.agentOnly) {
      assert.equal(methodsForRole('owner').includes(key), false, key + ' must not be an Owner Remote method');
    } else {
      assert.match(source, new RegExp('\\b' + key + '\\s*\\('), key + ' needs an Owner Remote handler');
    }
  }
});

test('role graph and inbox tools are host-bound, schema-checked, and audited', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  const writeRoles = ['chief'];
  const readRoles = ['chief', 'regional', 'explorer', 'builder', 'reviewer'];
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    for (const [key, roles] of [
      ['claimCreate', writeRoles], ['planCreate', writeRoles], ['inboxPush', writeRoles],
      ['claimRead', readRoles], ['planRead', readRoles],
    ]) assert.equal(methodsForRole(role).includes(key), roles.includes(role), role + '/' + key);
  }
  const claim = { claim_id:'C1', title:'Claim', informal_statement:'Statement', module_path:'Demo.lean', target_decl:'demo' };
  await assert.rejects(bridge.bind('explorer').call('claimCreate', claim), fails('denied'));
  await assert.rejects(bridge.bind('chief').call('claimCreate', { ...claim, role:'owner' }), fails('invalid'));
  await bridge.bind('chief').call('claimCreate', claim);
  const plan = { plan_id:'P1', target_claim_id:'C1', name:'Induction', track:'infrastructure',
    dependencies:[{claim_id:'C2',kind:'lemma'}] };
  await bridge.bind('chief').call('planCreate', plan);
  const inbox = { inbox_id:'DEC-1', priority:'P1', category:'hard_bottleneck_escalation', summary:'Choose route',
    leverage_score:0.5, related_claims:['C1'], evidence_refs:['event:1'],
    options:[{label:'Continue',tradeoff:'More budget',recommended:true},{label:'Pivot',tradeoff:'Archive route',recommended:false}], details:{reason:'two routes'} };
  await bridge.bind('chief').call('inboxPush', inbox);
  for (const invalid of [
    { ...inbox, leverage_score:Number.NaN },
    { ...inbox, leverage_score:1.1 },
    { ...inbox, details:[] },
    { ...inbox, options:[{label:'Continue'}] },
    { ...inbox, options:[] },
    { ...plan, dependencies:[{claim_id:'C2',kind:'guess'}] },
    { ...plan, dependencies:[{claim_id:'C2',kind:'lemma'},{claim_id:'C2',kind:'definition'}] },
  ]) await assert.rejects(bridge.bind('chief').call(invalid.plan_id ? 'planCreate' : 'inboxPush', invalid), fails('invalid'));
  assert.deepEqual(seen, [
    ['session.bind', { role:'chief' }],
    ['claim.create', claim],
    ['plan.create', plan],
    ['inbox.push', inbox],
  ]);
});



test('RFC role tools are strict and use the reviewed managed-Git schema', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    assert.equal(methodsForRole(role).includes('rfcSubmit'), role === 'explorer', role + '/rfcSubmit');
    assert.equal(methodsForRole(role).includes('rfcApprove'), role === 'chief', role + '/rfcApprove');
    assert.equal(methodsForRole(role).includes('rfcReject'), role === 'chief', role + '/rfcReject');
    assert.equal(methodsForRole(role).includes('rfcSupersede'), role === 'chief', role + '/rfcSupersede');
    assert.equal(methodsForRole(role).includes('rfcSmokeStart'), role === 'builder', role + '/rfcSmokeStart');
    assert.equal(methodsForRole(role).includes('rfcInspect'),
      ['chief','regional','explorer','builder','reviewer'].includes(role), role + '/rfcInspect');
  }
  const draft = { rfc_id:'RFC-1', requested_by_claim:'C1', engine:'lean4', motivation:'Need a definition',
    proposed_definitions:['def demo : Nat := 0'], required_api_lemmas:[],
    consumer_usage_sketch:'example : demo = 0 := rfl', smoke_test_criteria:['nonempty'], status:'DRAFT' };
  const submit = { module_path:'Demo.lean', reviewed_commit:'a'.repeat(40), draft };
  await bridge.bind('explorer').call('rfcSubmit', submit);
  await bridge.bind('chief').call('rfcApprove', { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64) });
  await bridge.bind('chief').call('rfcInspect', { rfc_id:'RFC-1' });
  await bridge.bind('chief').call('rfcReject', { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64), reason:'not needed' });
  await bridge.bind('chief').call('rfcSupersede', { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64), replacement_rfc_id:'RFC-2', reason:'replacement' });
  await bridge.bind('builder').call('rfcSmokeStart', { rfc_id:'RFC-2', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64) });
  for (const invalid of [
    { ...submit, reviewed_commit:'A'.repeat(40) },
    { ...submit, draft:{ ...draft, status:'APPROVED' } },
    { ...submit, draft:{ ...draft, proposed_definitions:[] } },
    { ...submit, draft:{ ...draft, required_api_lemmas:Array.from({length:17}, (_, i) => 'L' + i) } },
  ]) await assert.rejects(bridge.bind('explorer').call('rfcSubmit', invalid), fails('invalid'));
  await assert.rejects(bridge.bind('chief').call('rfcReject', {
    rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64), reason:'bad\nreason',
  }), fails('invalid'));
  assert.deepEqual(seen, [
    ['session.bind', { role:'explorer' }], ['rfc.submit', submit],
    ['session.bind', { role:'chief' }],
    ['rfc.approve', { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64) }],
    ['rfc.inspect', { rfc_id:'RFC-1' }],
    ['rfc.reject', { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64), reason:'not needed' }],
    ['rfc.supersede', { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64), replacement_rfc_id:'RFC-2', reason:'replacement' }],
    ['session.bind', { role:'builder' }],
    ['rfc.smoke_start', { rfc_id:'RFC-2', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64) }],
  ]);
});


test('task registration is Chief-only and never accepts repository or dispatch fields', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    assert.equal(methodsForRole(role).includes('taskRegister'), role === 'chief', role + '/taskRegister');
    assert.equal(methodsForRole(role).includes('taskStatus'), ['chief','regional'].includes(role), role + '/taskStatus');
  }
  const task = { task_id:'T1', track:'infrastructure', module_path:'Demo.lean', target_decl:'demo', dependencies:[] };
  await bridge.bind('chief').call('taskRegister', task);
  await bridge.bind('chief').call('taskStatus', {});
  for (const invalid of [
    { ...task, repo:'/tmp/repo.git' }, { ...task, reviewed_commit:'a'.repeat(40) },
    { ...task, dependencies:['T1'] }, { ...task, task_id:'x'.repeat(101) },
  ]) await assert.rejects(bridge.bind('chief').call('taskRegister', invalid), fails('invalid'));
  assert.deepEqual(seen, [
    ['session.bind', { role:'chief' }], ['task.register', task], ['task.status', {}],
  ]);
});


test('worker tools are role-bound and reject cross-role suggestion shapes', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    for (const key of ['workerAcquire','workerSubmit','workerCancel','workerStatus']) {
      assert.equal(methodsForRole(role).includes(key), ['prover','reviewer'].includes(role), role + '/' + key);
    }
  }
  const acquire = { request_id:'REQ-1', wall_seconds:60 };
  const submit = { attempt_id:'wa-1', suggestion:{kind:'proof_body',proof_body:'by rfl'} };
  await bridge.bind('prover').call('workerAcquire', acquire);
  await bridge.bind('prover').call('workerSubmit', submit);
  await bridge.bind('prover').call('workerStatus', { attempt_id:'wa-1' });
  await bridge.bind('reviewer').call('workerSubmit', { attempt_id:'wa-2', suggestion:{kind:'review',verdict:'needs_changes',rationale:'missing step'} });
  await assert.rejects(bridge.bind('prover').call('workerSubmit', { attempt_id:'wa-1', suggestion:{kind:'review',verdict:'needs_changes',rationale:'x'} }), fails('invalid'));
  await assert.rejects(bridge.bind('reviewer').call('workerSubmit', submit), fails('invalid'));
  await assert.rejects(bridge.bind('prover').call('workerAcquire', { ...acquire, wall_seconds:5 }), fails('invalid'));
  assert.deepEqual(seen, [
    ['session.bind', { role:'prover' }], ['worker.acquire', acquire], ['worker.submit', submit],
    ['worker.status', { attempt_id:'wa-1' }], ['session.bind', { role:'reviewer' }],
    ['worker.submit', { attempt_id:'wa-2', suggestion:{kind:'review',verdict:'needs_changes',rationale:'missing step'} }],
  ]);
});





test('native RFC tools are Builder-only and accept no executable or repository fields', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    assert.equal(methodsForRole(role).includes('rfcFreeze'), role === 'builder', role + '/rfcFreeze');
    assert.equal(methodsForRole(role).includes('rfcComplete'), role === 'builder', role + '/rfcComplete');
  }
  const freeze = { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64), smoke_lemmas:['demo_eq'] };
  const complete = { rfc_id:'RFC-1', reviewed_commit:'a'.repeat(40), draft_sha256:'b'.repeat(64) };
  await bridge.bind('builder').call('rfcFreeze', freeze);
  await bridge.bind('builder').call('rfcComplete', complete);
  for (const invalid of [
    { ...freeze, smoke_lemmas:[] },
    { ...freeze, smoke_lemmas:Array.from({length:17}, (_, i) => 'L' + i) },
    { ...freeze, executable:'/tmp/lean' },
    { ...complete, repo:'/tmp/repo.git' },
  ]) await assert.rejects(bridge.bind('builder').call(invalid.smoke_lemmas ? 'rfcFreeze' : 'rfcComplete', invalid), fails('invalid'));
  assert.deepEqual(seen, [
    ['session.bind', { role:'builder' }], ['rfc.freeze', freeze], ['rfc.complete', complete],
  ]);
});

test('typed Oracle trust tools are Chief-only and reject caller-selected levels', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    assert.equal(methodsForRole(role).includes('claimApplyOracle'), role === 'chief', role + '/claimApplyOracle');
  }
  const args = { claim_id:'C1', observation_id:'a'.repeat(64) };
  await bridge.bind('chief').call('claimApplyOracle', args);
  await assert.rejects(bridge.bind('chief').call('claimApplyOracle', { ...args, trust_level:'L5' }), fails('invalid'));
  assert.deepEqual(seen, [
    ['session.bind', { role:'chief' }], ['claim.apply_oracle', args],
  ]);
});

test('promotion tools are Chief-only redacted reads with no execution surface', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    assert.equal(methodsForRole(role).includes('promotionPreview'), role === 'chief', role + '/promotionPreview');
    assert.equal(methodsForRole(role).includes('promotionStatus'), role === 'chief', role + '/promotionStatus');
  }
  await bridge.bind('chief').call('promotionPreview', { task_id:'T1' });
  await bridge.bind('chief').call('promotionStatus', {});
  await assert.rejects(bridge.bind('chief').call('promotionPreview', { task_id:'T1', proof_body:'secret' }), fails('invalid'));
  assert.deepEqual(seen, [
    ['session.bind', { role:'chief' }], ['promotion.preview', { task_id:'T1' }], ['promotion.status', {}],
  ]);
});

test('same-role Agent bindings use separate authenticated write connections', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  await bridge.bind('prover', { bindingId:'prover-a' }).call('workerAcquire', { request_id:'A', wall_seconds:60 });
  await bridge.bind('prover', { bindingId:'prover-b' }).call('workerAcquire', { request_id:'B', wall_seconds:60 });
  assert.deepEqual(seen, [
    ['session.bind', { role:'prover', binding_id:'prover-a' }],
    ['worker.acquire', { request_id:'A', wall_seconds:60 }],
    ['session.bind', { role:'prover', binding_id:'prover-b' }],
    ['worker.acquire', { request_id:'B', wall_seconds:60 }],
  ]);
});

test('first role writes share one bind and send immutable validated snapshots', async t => {
  const calls = [];
  let bindCount = 0;
  let releaseBind;
  let signalBind;
  const bindSeen = new Promise(resolve => { signalBind = resolve; });
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    if (method === 'session.bind') {
      bindCount++;
      releaseBind = () => reply(socket, id, { principal:params.role });
      signalBind();
      return;
    }
    calls.push([method, params]);
    reply(socket, id, {});
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  const inbox = { inbox_id:'DEC-1', priority:'P1', category:'hard_bottleneck_escalation', summary:'Choose route',
    options:[{label:'Continue',tradeoff:'More budget'},{label:'Pivot',tradeoff:'Archive route'}], details:{reason:'original'} };
  const claim = { claim_id:'C1', title:'Claim', informal_statement:'Statement', module_path:'Demo.lean', target_decl:'demo' };
  const first = bridge.bind('chief').call('inboxPush', inbox);
  const second = bridge.bind('chief').call('claimCreate', claim);
  await bindSeen;
  inbox.details.reason = 'mutated';
  inbox.options[0].label = 'mutated';
  releaseBind();
  await Promise.all([first, second]);
  assert.equal(bindCount, 1);
  const sentInbox = calls.find(([method]) => method === 'inbox.push')[1];
  assert.equal(sentInbox.details.reason, 'original');
  assert.equal(sentInbox.options[0].label, 'Continue');
});

test('in-flight proof traffic never head-of-line blocks owner RPCs', async (t) => {
  let proofSocket;
  let signalProof;
  const observed = new Promise(resolve => { signalProof = resolve; });
  const socketPath = await fixture(t, ({ id, method }, socket) => {
    if (method === 'proof.submit') { proofSocket = socket; signalProof(); return; }
    assert.notEqual(socket, proofSocket);
    reply(socket, id, { ready: true });
  });
  const bridge=createBridge({ socketPath, timeoutMs: 500 });
  t.after(() => bridge.close());
  const cancel = abort();
  const worker = bridge.bind('prover',{proofSlots:['slot']});
  const pending = worker.call('proofSubmit',{target_id:'slot',proof_body:'True.intro'},cancel.signal);
  await observed;
  assert.deepEqual(await bridge.bind('owner').call('status',{}),{ready:true});
  cancel.abort();
  await assert.rejects(pending,fails('cancelled'));
});

test('resubscribes after daemon drop and aborts a pending event read', async (t) => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket, generation) => {
    seen.push({ method, params });
    reply(socket, id, { topics });
    setTimeout(() => push(socket, event, { generation }), 10);
    if (generation === 1) setTimeout(() => socket.destroy(), 30);
  });
  const bridge = createBridge({ socketPath, reconnectMinMs: 5, reconnectMaxMs: 15 });
  t.after(() => bridge.close());
  const ac = abort();
  const stream = bridge.bind('owner').subscribe({ topics }, ac.signal);
  const state = (state) => ({ topic: 'ai4math.connection', data: { state } });
  assert.deepEqual((await stream.next()).value, state('connected'));
  assert.deepEqual((await stream.next()).value, { topic: event, data: { generation: 1 } });
  assert.deepEqual((await stream.next()).value, state('reconnecting'));
  assert.deepEqual((await stream.next()).value, state('connected'));
  assert.deepEqual((await stream.next()).value, { topic: event, data: { generation: 2 } });
  const waiting = stream.next();
  ac.abort();
  assert.equal((await waiting).done, true);
  assert.deepEqual(seen, Array.from({ length: 2 }, () => ({ method: 'subscribe', params: { topics } })));
});

test('fails closed on invalid pushes and bounded stream overflow', async (t) => {
  const socketPath = await fixture(t, ({ id }, socket, generation) => {
    reply(socket, id, { topics });
    setImmediate(() => {
      if (generation === 1) return push(socket, 'unknown.topic', {});
      for (let i = 0; i < 65; i++) push(socket, event, { i });
    });
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  for (const code of ['protocol', 'overflow']) {
    const stream = bridge.bind('owner').subscribe({ topics });
    assert.equal((await stream.next()).value.data.state, 'connected');
    await delay(30);
    await assert.rejects(stream.next(), fails(code));
  }
});

test('refuses additional streams beyond the host-wide limit', async (t) => {
  const socketPath = await fixture(t, ({ id }, socket) => reply(socket, id, { topics }));
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  const owner = bridge.bind('owner');
  const streams = Array.from({ length: MAX_STREAMS }, () => owner.subscribe({ topics }));
  await Promise.all(streams.map((stream) => stream.next()));
  await assert.rejects(owner.subscribe({ topics }).next(), fails('busy'));
  await Promise.all(streams.map((stream) => stream.return()));
});

test('retarget switches unary calls and live subscriptions to a new daemon socket immediately', async (t) => {
  const sockA = await fixture(t, ({ id, method }, socket) => {
    if (method === 'subscribe') { reply(socket, id, { topics }); setTimeout(() => push(socket, event, { project: 'A' }), 5); }
    else reply(socket, id, { project: 'A' });
  });
  const sockB = await fixture(t, ({ id, method }, socket) => {
    if (method === 'subscribe') { reply(socket, id, { topics }); setTimeout(() => push(socket, event, { project: 'B' }), 5); }
    else reply(socket, id, { project: 'B' });
  });
  const bridge = createBridge({ socketPath: sockA, reconnectMinMs: 20, reconnectMaxMs: 5000 });
  t.after(() => bridge.close());
  const owner = bridge.bind('owner');
  assert.deepEqual(await owner.call('status', {}), { project: 'A' });
  const ac = abort();
  const stream = owner.subscribe({ topics }, ac.signal);
  assert.equal((await stream.next()).value.data.state, 'connected');
  assert.deepEqual((await stream.next()).value, { topic: event, data: { project: 'A' } });

  assert.equal(bridge.retarget(sockB), true);
  assert.equal(bridge.socketPath(), sockB);
  assert.equal((await stream.next()).value.data.state, 'reconnecting');
  assert.equal((await stream.next()).value.data.state, 'connected');
  assert.deepEqual((await stream.next()).value, { topic: event, data: { project: 'B' } });
  assert.deepEqual(await owner.call('status', {}), { project: 'B' });
  ac.abort();
  await stream.return();
});
