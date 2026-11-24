import assert from 'node:assert/strict';
import { test } from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { BridgeError, MAX_PENDING, NdjsonConnection, requireSocketPath } from '../connection.mjs';
import { createBridge, MAX_STREAMS, methodsForRole, parseParams } from '../bridge.mjs';
import { fixture, line, push, reply } from './fixture.mjs';

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
