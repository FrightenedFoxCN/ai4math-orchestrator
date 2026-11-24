import assert from 'node:assert/strict';
import { test } from 'node:test';
import { BridgeError } from '../connection.mjs';
import { AGENT_ROLES, createBridge, methodsForRole, parseParams } from '../bridge.mjs';
import { fixture, reply } from './fixture.mjs';

const fails = code => error => error instanceof BridgeError && error.code === code;

test('Wiki provenance and warnings are exposed only to their bound roles', async t => {
  const seen = [];
  const socketPath = await fixture(t, ({ id, method, params }, socket) => {
    seen.push([method, params]);
    reply(socket, id, { truncated: false });
  });
  const bridge = createBridge({ socketPath });
  t.after(() => bridge.close());
  const warningRoles = ['owner', 'chief', 'regional', 'explorer', 'builder', 'reviewer'];
  const lineageRoles = ['owner', 'chief', 'explorer'];
  const warning = { informal_statement: 'claim', formal_signature: 'theorem demo : True', limit: 8 };
  const provenance = { id: 'COR-A', max_depth: 3, limit: 16 };
  const expected = [];
  for (const role of ['owner', ...AGENT_ROLES, 'unmapped']) {
    for (const [key, route, roles, args] of [
      ['wikiCheckGraveyard', 'wiki.check_graveyard', warningRoles, warning],
      ['wikiProvenance', 'wiki.provenance', lineageRoles, provenance],
    ]) {
      const method = bridge.bind(role);
      assert.equal(methodsForRole(role).includes(key), roles.includes(role), role + '/' + key);
      if (roles.includes(role)) {
        await method.call(key, args);
        expected.push([route, args]);
      } else {
        await assert.rejects(method.call(key, { ...args, role: 'owner' }), fails('denied'));
      }
    }
  }
  for (const role of ['regional', 'builder', 'prover', 'automation']) {
    for (const scope of ['sediment', 'graveyard']) {
      await assert.rejects(bridge.bind(role).call('wikiSearch', { text: 'claim', scope }), fails('denied'));
    }
  }
  assert.deepEqual(seen, expected);
});

test('Wiki requests accept only their declared types and bounded keys', () => {
  const search = { text: 'lemma', scope: 'core', engine: 'lean4', conclusion_head: 'LE.le',
    hypothesis_heads: ['Integrable'], typeclasses: [], include_shadow_hint: false, limit: 24 };
  assert.deepEqual(parseParams('wikiSearch', search), search);
  for (const args of [
    { text: '🐢'.repeat(129) }, { text: '\u0085' }, { text: '\ud800' }, { text: 'x\0y' },
    { text: 'x', engine: 'python' }, { text: 'x', scope: null }, { text: 'x', include_shadow_hint: 1 },
    { text: 'x', hypothesis_heads: ['A', 'A'] }, { text: 'x', hypothesis_heads: Array(1) },
    { text: 'x', typeclasses: Array.from({ length: 9 }, (_, n) => String(n)) },
    { text: 'x', typeclasses: ['α'.repeat(65)] }, { text: 'x', conclusion_head: '' },
  ]) assert.throws(() => parseParams('wikiSearch', args), fails('invalid'));
  for (const args of [
    { id: 'A', role: 'chief' }, { id: 'A', path: '/tmp/wiki' }, { id: 'A', max_depth: 17 },
    { id: 'A', max_depth: 0 }, { id: 'A', max_depth: null }, { id: 'A', limit: 65 }, { id: 'π'.repeat(65) }, {},
  ]) assert.throws(() => parseParams('wikiProvenance', args), fails('invalid'));
  for (const args of [{ text: 'x' }, { informal_statement: 'x', scope: 'core' },
    { informal_statement: 'x', formal_signature: null }, { informal_statement: 'x', limit: 17 }]) {
    assert.throws(() => parseParams('wikiCheckGraveyard', args), fails('invalid'));
  }
});
