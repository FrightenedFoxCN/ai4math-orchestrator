import assert from 'node:assert/strict';
import { once } from 'node:events';
import { test } from 'node:test';
import { createBridge } from '../../host/bridge.mjs';
import { fixture, push, reply } from '../../host/test/fixture.mjs';
import { browser, until } from './vm-fixture.mjs';

const ok = value => ({ ok: true, value });
const topics = ['vital_signs.tick', 'alert.p0', 'breaker.state_changed'];
const snapshot = (event_seq, overrides = {}) => ({
  event_seq,
  observed_at: '2026-01-10T00:00:00Z',
  trust: { L0: 4, L1: 2 },
  stale_claims: 2,
  pending_inbox: { P0: 1, P1: 3, P2: 2 },
  tripped_breakers: 0,
  last_alert: null,
  ...overrides,
});
const expectText = (view, pattern) => until(view.text, text => pattern.test(text), 'Expected ' + pattern);
const fromWire = value => JSON.parse(JSON.stringify(value));
const mountTab = (app, key) => app.mount('sidebar.right.pane.tab', 'dsh-ai4math/' + key);

// Stand in for the browser Remote proxy, not the Host bridge. Unary replies
// carry a result wrapper; stream consumers see the data frames themselves.
function streamHandle(frames, controller) {
  return { [Symbol.asyncIterator]: () => frames, dispose: () => controller.abort() };
}

test('HUD shows resident T3 as a read-only last sample and clears it when unavailable', async t => {
  const channel = new AbortController()
  let report;
  const gate = new Promise(resolve => { report = resolve; });
  const app = await browser(t, {
    subscribe(_params, signal) {
      return streamHandle((async function* () {
        yield { topic: 'ai4math.connection', data: { state: 'connected' } };
        await gate;
        yield { topic: 'vital_signs.tick', data: snapshot(13, { t3_discovery: { status: 'unavailable' } }) };
        const active = AbortSignal.any([signal, channel.signal]);
        if (!active.aborted) await once(active, 'abort');
      })(), channel);
    },
    async status() {
      return ok(snapshot(12, { t3_discovery: { status: 'structural_change', observed_head: 'b'.repeat(40),
        ledger_event_seq: 10, changed_module_count: 3, module_order_changed: true, advisory_only: true,
        baseline_scope: 'claim_t1', baseline_claim: 'PrivateClaimName', baseline_receipt_sha256: 'a'.repeat(64) } }));
    },
    async alertsList() { return ok({ items: [], truncated: false }); },
  });
  const hud = app.mount('shell.bottom');
  await expectText(hud, /Last T3 discovery Structure changed · 3 source change\(s\) · module list changed · Claim T1 baseline · advisory only · older ledger sample/);
  assert.doesNotMatch(hud.text(), /PrivateClaimName|a{64}/);
  assert.throws(() => hud.find('button', 'Apply T3'), /render did not contain/);
  report();
  await expectText(hud, /T3 discovery Unavailable/);
  assert.doesNotMatch(hud.text(), /Structure changed|older ledger sample|Claim T1 baseline/);
});

test('lazy browser bundle mounts shell.bottom and four sidebar tabs with owned cleanup', async t => {
  const calls = [];
  const remote = Object.fromEntries(Object.entries({
    status: snapshot(7, { broker: { fast_busy: 2, fast_capacity: 8, heavy_running: 1, heavy_queued: 3 } }),
    alertsList: { items: [], truncated: false },
    briefingPeek: { fact_pack: { event_from_exclusive: 1, event_to_inclusive: 7 } },
    briefingPull: { retained_id: 'retained-42', fact_pack: { event_from_exclusive: 1, event_to_inclusive: 7 } },
    frontierList: { items: [{ claim_id: 'c7', title: 'A frontier claim' }] },
    inboxList: { items: [{ inbox_id: 'i7', summary: 'A pending decision' }] },
    wikiSearch: { hits: [{ id: 'w7', title: 'A verified result' }], shadow_hint: { sediment_count: 1, graveyard_count: 0 } },
  }).map(([method, value]) => [method, async (params, signal) => {
    assert.equal(signal.aborted, false);
    calls.push([method, fromWire(params)]);
    return ok(value);
  }]));
  let streamSignal;
  remote.subscribe = (params, signal) => {
    assert.equal(streamSignal, undefined, 'one plugin subscription, not one per surface');
    assert.deepEqual(fromWire(params), { topics });
    streamSignal = signal;
    const controller = new AbortController();
    return streamHandle((async function* () {
      const active = AbortSignal.any([signal, controller.signal]);
      yield { topic: 'ai4math.connection', data: { state: 'connected' } };
      if (!active.aborted) await once(active, 'abort');
    })(), controller);
  };

  const app = await browser(t, remote);
  const [registration] = app.registrations;
  assert.equal(app.registrations.length, 1);
  assert.equal(registration.package, 'dsh-ai4math');
  assert.deepEqual(Array.from(registration.descriptors, descriptor => descriptor.method).sort(),
    ['alertsList', 'briefingGet', 'briefingPeek', 'briefingPull', 'frontierList', 'inboxList', 'status', 'subscribe',
      'wikiCheckGraveyard', 'wikiProvenance', 'wikiSearch']);
  const subscription = registration.descriptors.find(d => d.method === 'subscribe');
  assert.equal(subscription.mode, 'stream');
  assert.equal(subscription.cancellation.parameter, 'signal');
  assert.equal(app.styles.size, 1);
  const [style] = app.styles;
  assert.equal(style.dataset.plugin, 'dsh-ai4math');
  assert.match(style.textContent, /\.ai4m-hud/);
  assert.equal(app.slots.size, 5);
  assert.equal(app.tabs.size, 4);

  const hud = app.mount('shell.bottom');
  assert.equal(hud.find('section').props['aria-label'], 'AI4Math Live HUD');
  await expectText(hud, /Live .*Daemon broker fast 2\/8 busy · heavy 1 running \/ 3 queued .*Seq 7/);
  assert.doesNotMatch(hud.text(), /HALF_OPEN|probe|Oracle-held/i, 'older daemons do not supply those counters');
  const element = {};
  hud.find('button', 'Briefing').props.onClick({ currentTarget: element });
  assert.deepEqual(app.opened, []);
  assert.match(hud.text(), /Open a DSH session/);

  const panels = [
    ['briefing', 'Briefing', 'Review briefing'],
    ['frontier', 'Frontier', 'Active frontier'],
    ['inbox', 'Inbox', 'Decision inbox'],
    ['wiki', 'Wiki', 'Math Wiki'],
  ];
  const target = { sessionId: 'active-test-session' };
  app.navigation.target = target;
  const views = panels.map(([key, title, heading]) => {
    const tab = app.tabs.get('dsh-ai4math/' + key);
    assert.equal(tab?.title(), title);
    assert.equal(tab.kind, 'ai4math-' + key);
    hud.find('button', title).props.onClick({ currentTarget: element });
    assert.equal(app.opened.at(-1).kind, tab.kind);
    assert.equal(app.opened.at(-1).target, target);
    const view = mountTab(app, key);
    view.find('h2', heading);
    return view;
  });
  assert.equal(app.opened.length, 4);
  assert.deepEqual(app.targetElements, Array(5).fill(element));
  assert.doesNotMatch(hud.text(), /Open a DSH session/);

  const [briefing, frontier, inbox, wiki] = views;
  briefing.find('button', 'Peek (preview only)').props.onClick();
  await expectText(briefing, /Preview .*Event window \(1, 7\]/);
  briefing.find('button', 'Pull and retain').props.onClick();
  await expectText(briefing, /Pull .*retained-42/);
  await expectText(frontier, /A frontier claim/);
  await expectText(inbox, /A pending decision/);
  wiki.find('input').props.onChange({ target: { value: '  test lemma  ' } });
  wiki.find('form').props.onSubmit({ preventDefault() {} });
  await expectText(wiki, /CORE .*A verified result/);
  assert.deepEqual(calls.filter(([key]) => key !== 'status' && key !== 'alertsList'), [
    ['frontierList', { limit: 32 }], ['inboxList', { limit: 32 }], ['briefingPeek', {}], ['briefingPull', {}],
    ['wikiSearch', { text: 'test lemma', scope: 'core', limit: 24 }],
  ]);

  app.dispose();
  assert.equal(streamSignal.aborted, true);
  assert.equal(app.styles.size, 0);
  assert.equal(app.slots.size, 0);
  assert.equal(app.tabs.size, 0);
  assert.equal(app.timers.size, 0);
});

test('Wiki provenance is fetched on demand and rendered as bounded text-only lineage', async t => {
  const calls = [];
  const metadata = { note: '<img src=x onerror=alert(1)>' };
  const app = await browser(t, {
    async *subscribe() {},
    async status() { return ok(snapshot(5)); },
    async alertsList() { return ok({ items: [], truncated: false }); },
    async wikiSearch(params) {
      calls.push(['search', fromWire(params)]);
      return ok({ hits: [{ id: 'COR-A', title: 'Archived relation' }], truncated: false });
    },
    async wikiProvenance(params) {
      calls.push(['provenance', fromWire(params)]);
      return ok({ edges: [{ src_id: 'SED-A', dst_id: 'COR-A', src_stratum: 'sediment', dst_stratum: 'core',
        edge_type: 'superseded_by', depth: 1, metadata }], truncated: true });
    },
  });
  const wiki = mountTab(app, 'wiki');
  wiki.find('input').props.onChange({ target: { value: 'lemma' } });
  wiki.find('form').props.onSubmit({ preventDefault() {} });
  await expectText(wiki, /Derived CORE coverage was not reported for this search. .*Archived relation/);
  assert.doesNotMatch(wiki.text(), /Static Wiki only/);
  assert.deepEqual(calls, [['search', { text: 'lemma', scope: 'core', limit: 24 }]]);
  wiki.find('button', 'Inspect provenance for COR-A').props.onClick();
  await expectText(wiki, /Provenance for COR-A .*SED-A sediment → COR-A core .*superseded_by .*bounded provenance slice/);
  assert.deepEqual(calls.at(-1), ['provenance', { id: 'COR-A', max_depth: 2, limit: 24 }]);
  assert.doesNotThrow(() => wiki.find('summary', 'Indexed edge metadata'));
  assert.match(wiki.text(), /Derived CORE coverage was not reported for this provenance response./);
  assert.throws(() => wiki.find('img'), /render did not contain img/);
  const methods = app.registrations[0].descriptors;
  const input = key => methods.find(d => d.method === key).parameters[0].codec.create();
  assert.equal(input('wikiSearch').safeParse({ text: 'x', include_shadow_hint: false }).success, true);
  assert.equal(input('wikiSearch').safeParse({ text: 'x', include_shadow_hint: 1 }).success, false);
  assert.equal(input('wikiSearch').safeParse({ text: 'x', managed_core: true }).success, false);
  assert.equal(input('wikiProvenance').safeParse({ id: 'α'.repeat(65) }).success, false);
  assert.equal(input('wikiProvenance').safeParse({ id: 'COR-A', scope: 'core' }).success, false);
  assert.equal(input('wikiProvenance').safeParse({ id: 'COR-A', managed_core: true }).success, false);
});

test('Wiki shows managed CORE as derived metadata alongside authored CORE and labels both lineage directions', async t => {
  const id = 'CORE-ADAPT-' + 'a'.repeat(64);
  const virtualPath = '.managed-core/' + id;
  const calls = [];
  let provenanceCalls = 0;
  const app = await browser(t, {
    async *subscribe() {},
    async status() { return ok(snapshot(5)); },
    async alertsList() { return ok({ items: [], truncated: false }); },
    async wikiSearch(params) {
      calls.push(['search', fromWire(params)]);
      return ok({ managed_core: true, hits: [
        { id: 'COR-A', title: 'Authored CORE theorem', engine: 'lean4', stratum: 'core', trust_level: 'L5',
          status: 'active', file_path: 'core/authored.md' },
        { id, title: 'Derived CORE theorem', engine: 'lean4', stratum: 'core', trust_level: 'L5',
          status: 'active', informal_gloss: 'An indexed adaptation', formal_signature: null, file_path: virtualPath,
          verified_at_commit: 'b'.repeat(40) },
      ] });
    },
    async wikiProvenance(params) {
      calls.push(['provenance', fromWire(params)]);
      if (provenanceCalls++ > 0) return ok({ managed_core: false, edges: [], truncated: false });
      return ok({ managed_core: true, edges: [
        { src_id: 'SED-A', src_stratum: 'sediment', dst_id: id, dst_stratum: 'core', edge_type: 'superseded_by', depth: 1 },
        { src_id: id, src_stratum: 'core', dst_id: 'Claim-A', dst_stratum: null, edge_type: 'born_from_claim', depth: 1 },
      ], truncated: false });
    },
  });
  const wiki = mountTab(app, 'wiki');
  wiki.find('input').props.onChange({ target: { value: 'theorem' } });
  wiki.find('form').props.onSubmit({ preventDefault() {} });
  await expectText(wiki, /Authored CORE theorem COR-A lean4 core L5 active .*Source path core\/authored\.md .*Derived CORE theorem CORE-ADAPT-[0-9a-f]+ lean4 Derived CORE Indexed trust L5 active .*Virtual index location/);
  assert.match(wiki.text(), /Verified at Git commit b{40}/);
  const location = wiki.find('code', virtualPath);
  assert.equal(location.props.onClick, undefined);
  assert.equal(location.props.href, undefined);
  assert.throws(() => wiki.find('a'), /render did not contain a/);
  assert.match(wiki.text(), /not an authored Wiki page or a proof source/);
  assert.match(wiki.text(), /Search again to check for changes/);
  assert.doesNotMatch(wiki.text(), /Static Wiki only|coverage was not reported/);
  assert.deepEqual(calls, [['search', { text: 'theorem', scope: 'core', limit: 24 }]]);

  wiki.find('button', 'Inspect provenance for ' + id).props.onClick();
  await expectText(wiki, /Provenance for CORE-ADAPT-[0-9a-f]+ .*not authored Wiki pages or proof sources[.] .*born_from_claim/);
  wiki.find('li', 'SED-A sediment → ' + id + ' Derived CORE · superseded_by · Hops 1');
  wiki.find('li', id + ' Derived CORE → Claim-A Outside index · born_from_claim · Hops 1');
  assert.doesNotMatch(wiki.text(), /Static Wiki lineage only|coverage was not reported/);
  assert.match(wiki.text(), /Inspect again to check for changes/);
  assert.deepEqual(calls.at(-1), ['provenance', { id, max_depth: 2, limit: 24 }]);
  assert.throws(() => wiki.find('a'), /render did not contain a/);

  // A later response can fail the generation gate independently of the search.
  wiki.find('button', 'Inspect provenance for ' + id).props.onClick();
  await expectText(wiki, /Static Wiki lineage only; derived CORE edges were not included[.] No edges returned/);
  assert.doesNotMatch(wiki.text(), /superseded_by|born_from_claim|Static Wiki only;/);
});

test('Wiki reports explicit static-only responses without implying a daemon failure or a cold search gap', async t => {
  const app = await browser(t, {
    async *subscribe() {},
    async status() { return ok(snapshot(5)); },
    async alertsList() { return ok({ items: [], truncated: false }); },
    async wikiSearch({ scope }) {
      return ok({ managed_core: false, hits: scope === 'core'
        ? [{ id: 'COR-B', title: 'Static result', stratum: 'core', file_path: 'core/static.md' }]
        : [] });
    },
    async wikiProvenance() { return ok({ managed_core: false, edges: [] }); },
  });
  const wiki = mountTab(app, 'wiki');
  wiki.find('input').props.onChange({ target: { value: 'lemma' } });
  wiki.find('form').props.onSubmit({ preventDefault() {} });
  await expectText(wiki, /Static Wiki only; derived CORE entries were not included. .*Static result/);
  wiki.find('button', 'Inspect provenance for COR-B').props.onClick();
  await expectText(wiki, /Static Wiki lineage only; derived CORE edges were not included. .*No edges returned/);
  assert.doesNotMatch(wiki.text(), /unavailable|connection|coverage was not reported/i);

  wiki.find('select').props.onChange({ target: { value: 'graveyard' } });
  wiki.find('form').props.onSubmit({ preventDefault() {} });
  await expectText(wiki, /· GRAVEYARD Graveyard matches are historical warnings, not verified evidence[.] No matching records returned/);
  assert.doesNotMatch(wiki.text(), /Static Wiki|Derived CORE|Provenance for/);
});

test('opening a retained P0 pack shows its source events outside the review window', async t => {
  const retained = 'a'.repeat(64);
  const requested = [];
  const alert = { event_seq: 31, headline: 'A retained critical refutation', briefing_id: retained };
  const sourcePayload = { claim_itself_refuted: true, recorded: '<img src=x onerror=alert(1)>' };
  const pack = {
    event_from_exclusive: 35, event_to_inclusive: 70, fact_sha256: retained, checkpoint: { event_seq: 35 },
    alert_sources: [
      { event_seq: 31, event_type: 'CRITICAL_CLAIM_REFUTED', entity_type: 'claim', entity_id: 'c31', payload: sourcePayload },
      { event_seq: 32, event_type: 'ALL_TRACKS_BLOCKED', entity_id: 'c32', payload: {} },
      { event_seq: 33, event_kind: 'ALL_TRACKS_BLOCKED', entity_id: 'scheduler',
        payload: { exploration_blocked: 2, infrastructure_blocked: 3 } },
      { event_seq: 34, event_kind: 'ALL_TRACKS_BLOCKED', entity_id: 'bad-counts',
        payload: { exploration_blocked: '<img>', infrastructure_blocked: -1 } },
      { event_seq: 35, event_type: 'BROKER_REGRESSION_MELTDOWN', entity_id: 'candidate',
        payload: { failed_module: 'Downstream.lean', distinct_candidates: 3, window_seconds: 900 } },
      { event_seq: 36, event_type: 'BROKER_REGRESSION_MELTDOWN', entity_id: 'legacy', payload: {} },
      { event_seq: 37, event_type: 'CRITICAL_BREAKER_TRIPPED', entity_id: 'breaker',
        payload: { critical_claim_id: 'GoalA', critical_claim_count: 2 } },
      { event_seq: 38, event_type: 'CRITICAL_BREAKER_TRIPPED', entity_id: 'old-breaker', payload: {} },
    ],
    events: { items: [{ event_seq: 39, event_type: 'CLAIM_TRUST_CHANGED', entity_id: 'c39' }], truncated: false },
    milestones: { items: [{ event_seq: 36, claim: { title: 'A promoted claim', informal_statement: 'A proof exists.',
      formal_signature: 'theorem promoted : True', formal_target: 'Promoted.lean#promoted' } }] },
    refutations: { items: [{ event_seq: 37, claim: { title: 'A claim with path only', informal_statement: 'A candidate proof.',
      formal_signature: null, formal_target: 'PathOnly.lean#target' } }] },
  };
  const app = await browser(t, {
    async *subscribe(_params, signal) {
      yield { topic: 'ai4math.connection', data: { state: 'connected' } };
      if (!signal.aborted) await once(signal, 'abort');
    },
    async status() { return ok(snapshot(70, { last_alert: alert })); },
    async alertsList() { return ok({ items: [alert], truncated: false }); },
    async briefingGet(params) {
      requested.push(fromWire(params));
      return ok({ retained_id: retained, fact_pack: pack });
    },
  });
  const briefing = mountTab(app, 'briefing');
  await expectText(briefing, /Recent P0 alerts .*A retained critical refutation/);
  assert.doesNotMatch(briefing.text(), /P0 alert sources/);
  briefing.find('button', 'Open retained facts').props.onClick();
  await expectText(briefing, /Retained alert .*Event window \(35, 70\] .*P0 alert sources/);
  assert.deepEqual(requested, [{ id: retained }]);
  const selected = briefing.find('details');
  assert.equal(selected.props.open, true, 'opens the selected alert’s source evidence');
  assert.equal(selected.children[1].type, 'pre');
  assert.deepEqual(JSON.parse(selected.children[1].children[0]).payload, sourcePayload);
  briefing.find('summary', 'CRITICAL_CLAIM_REFUTED · c31 · #31');
  briefing.find('summary', 'ALL_TRACKS_BLOCKED · c32 · #32');
  briefing.find('summary', 'ALL_TRACKS_BLOCKED · scheduler · #33 · Exploration blocked: 2; infrastructure blocked: 3');
  briefing.find('summary', 'ALL_TRACKS_BLOCKED · bad-counts · #34');
  briefing.find('summary', 'BROKER_REGRESSION_MELTDOWN · candidate · #35 · Downstream.lean: 3 distinct candidates failed full regression');
  briefing.find('summary', 'BROKER_REGRESSION_MELTDOWN · legacy · #36');
  briefing.find('summary', 'CRITICAL_BREAKER_TRIPPED · breaker · #37 · No known route remains for GoalA (2 critical claims affected)');
  briefing.find('summary', 'CRITICAL_BREAKER_TRIPPED · old-breaker · #38');
  briefing.find('summary', 'CLAIM_TRUST_CHANGED · c39 · #39');
  assert.throws(() => briefing.find('img'), /render did not contain img/);
  assert.match(briefing.text(), /Formal signature theorem promoted : True Formal target Promoted\.lean#promoted/);
  assert.match(briefing.text(), /Formal signature Not supplied Formal target PathOnly\.lean#target/);
});

test('briefing breaker cards show frozen, waiting, blocked and verified evidence safely', async t => {
  const scenarios = [
    ['legacy', { state: 'HALF_OPEN' }, 'HALF_OPEN Probe status unavailable Ordinary queue remains frozen'],
    ['nullable', { state: 'HALF_OPEN', probe_status: null, probe_passed: null, probe_required: null },
      'HALF_OPEN Probe status unavailable Ordinary queue remains frozen'],
    ['unknown', { state: 'HALF_OPEN', probe_status: '<img src=x>', probe_passed: 2, probe_required: 2 },
      'HALF_OPEN Probe status unavailable Ordinary queue remains frozen'],
    ['review', { state: 'TRIPPED', probe_status: 'AWAITING_REVIEW' }, 'TRIPPED Blocked — awaiting human review'],
    ['ready', { state: 'HALF_OPEN', probe_status: 'READY', probe_passed: 0, probe_required: 2 },
      'HALF_OPEN Waiting — probe ready 0/2 probes passed Ordinary queue remains frozen'],
    ['partial', { state: 'HALF_OPEN', probe_status: 'RUNNING', probe_passed: 1, probe_required: 2 },
      'HALF_OPEN Waiting — probe attempt started 1/2 probes passed Ordinary queue remains frozen'],
    ['blocked', { state: 'HALF_OPEN', probe_status: 'NEEDS_PROBE' },
      'HALF_OPEN Blocked — a probe is required Ordinary queue remains frozen'],
    ['failed', { state: 'HALF_OPEN', probe_status: 'FAILED', probe_passed: '100', probe_required: 2 },
      'HALF_OPEN Blocked — probe failed Ordinary queue remains frozen'],
    ['unconfirmed', { state: 'HALF_OPEN', probe_status: 'PASSED', probe_passed: 2, probe_required: 2 },
      'HALF_OPEN Probe status unavailable Ordinary queue remains frozen'],
    ['bad-progress', { state: 'CLOSED', probe_status: 'PASSED', probe_passed: 1, probe_required: 2 },
      'CLOSED Probe status unavailable'],
    ['missing-progress', { state: 'CLOSED', probe_status: 'PASSED', probe_passed: 2, probe_required: null },
      'CLOSED Probe status unavailable'],
    ['verified', { state: 'CLOSED', probe_status: 'PASSED', probe_passed: 2, probe_required: 2 },
      'CLOSED Probe verified 2/2 probes passed'],
    ['verified-no-counts', { state: 'CLOSED', probe_status: 'PASSED' }, 'CLOSED Probe verified'],
  ];
  const app = await browser(t, {
    async *subscribe() {},
    async briefingPeek() {
      return ok({ fact_pack: { checkpoint: { event_seq: 0, last_adr_id: 'ADR-42' }, breakers: {
        items: scenarios.map(([breaker_id, fields]) => ({ breaker_id, ...fields })), truncated: false,
      } } });
    },
  });
  const briefing = mountTab(app, 'briefing');
  briefing.find('button', 'Peek (preview only)').props.onClick();
  await expectText(briefing, /Retained human checkpoint event_seq 0 Checkpoint ADR ADR-42/);
  for (const [name, , expected] of scenarios) briefing.find('article', name + ' ' + expected + ' n/a n/a');
  assert.doesNotMatch(briefing.text(), /<img|unfrozen|resumed|100\/2/i);
});

test('live HUD shows valid Oracle scheduling counts and clears them on later telemetry', async t => {
  let sendNext;
  const next = new Promise(resolve => { sendNext = resolve; });
  const app = await browser(t, {
    async status() { return ok(snapshot(8, { task_scheduler: { exploration: { held_for_oracle: 2 } } })); },
    async *subscribe() {
      yield { topic: 'ai4math.connection', data: { state: 'connected' } };
      await next;
      yield { topic: 'vital_signs.tick', data: snapshot(8, {observed_at:'2026-01-10T00:00:01Z',
        task_scheduler: { exploration: { held_for_oracle: 0 } } }) };
    },
  });
  const hud = app.mount('shell.bottom');
  await expectText(hud, /Live .*Oracle-held exploration 2/);
  sendNext();
  await until(hud.text, text => !/Oracle-held/.test(text), 'Expired count should disappear');
});

test('real Host bridge events render, reconnect, handle failure, and cancel', { timeout: 10000 }, async t => {
  const subscriptions = [];
  const handles = [];
  const history = [];
  const historySignals = [];
  let status = snapshot(2);
  const socketPath = await fixture(t, ({ method, id, params }, socket) => {
    if (method === 'subscribe') {
      subscriptions.push({ socket, id, params });
    } else if (method === 'status') {
      reply(socket, id, status);
    } else if (method === 'alerts.list') {
      history.push({ socket, id, params });
    } else if (method === 'frontier.list') {
      reply(socket, id, { items: [] });
    }
  });
  const host = createBridge({ socketPath, reconnectMinMs: 5, reconnectMaxMs: 10, timeoutMs: 3000 });
  t.after(() => host.close());
  const owner = host.bind('owner');
  const app = await browser(t, {
    async status(params, signal) { return ok(await owner.call('status', fromWire(params), signal)); },
    async alertsList(params, signal) {
      historySignals.push(signal);
      return ok(await owner.call('alertsList', fromWire(params), signal));
    },
    async frontierList(params, signal) { return ok(await owner.call('frontierList', fromWire(params), signal)); },
    subscribe(params, signal) {
      const controller = new AbortController();
      handles.push(controller);
      return streamHandle(owner.subscribe(fromWire(params), AbortSignal.any([signal, controller.signal])), controller);
    },
  });
  const hud = app.mount('shell.bottom');
  const frontier = mountTab(app, 'frontier');
  const briefing = mountTab(app, 'briefing');
  assert.match(hud.text(), /Connecting/);
  const pageRequest = async (index, after) => {
    const request = await until(() => history[index], Boolean, 'Expected alert history request ' + (index + 1));
    assert.deepEqual(request.params, { after, limit: 64 });
    return request;
  };
  const nthSubscription = index => until(() => subscriptions[index], Boolean, 'Expected daemon subscription ' + (index + 1));
  const ack = subscription => {
    assert.deepEqual(subscription.params, { topics });
    reply(subscription.socket, subscription.id, { topics });
  };
  const first = await nthSubscription(0);
  ack(first);
  await expectText(hud, /Live .*Seq 2/);
  const firstPage = await pageRequest(0, 0);

  push(first.socket, 'vital_signs.tick', snapshot(38, { tripped_breakers: 1 }));
  push(first.socket, 'alert.p0', { event_seq: 38, headline: 'A fresh P0 alert', briefing_id: 'br-38' });
  push(first.socket, 'breaker.state_changed', { event_seq: 38, event_type: 'breaker.tripped', entity_id: 'breaker-38' });
  await expectText(hud, /Live .*Tripped 1 .*P0 1 .*Seq 38 .*A fresh P0 alert .*br-38/);
  reply(firstPage.socket, firstPage.id, { items: [{ event_seq: 1, headline: 'Previously persisted alert' }], truncated: false });
  await expectText(briefing, /Recent P0 alerts .*A fresh P0 alert .*Previously persisted alert/);
  await expectText(frontier, /Latest breaker event .*breaker\.tripped .*breaker-38/);
  // A queued older tick and unary status cannot overwrite sequence 38.
  push(first.socket, 'vital_signs.tick', snapshot(30));
  push(first.socket, 'alert.p0', { event_seq: 37, headline: 'Old P0 alert' });
  push(first.socket, 'breaker.state_changed', { event_seq: 39, event_type: 'BREAKER_STATE_CHANGED', entity_id: 'breaker-marker',
    payload: { state: 'HALF_OPEN', probe_status: 'RUNNING', probe_passed: 0, probe_required: 2 } });
  await expectText(frontier, /breaker-marker .*HALF_OPEN Waiting — probe attempt started 0\/2 probes passed Ordinary queue remains frozen/);
  const eventText = 'Latest breaker event · breaker-marker · #39 HALF_OPEN Waiting — probe attempt started 0/2 probes passed Ordinary queue remains frozen';
  await expectText(hud, /Latest breaker event · breaker-marker/);
  assert.equal(hud.find('div', eventText).props.role, 'status');
  assert.match(hud.text(), /Seq 38 .*A fresh P0 alert/);
  assert.doesNotMatch(hud.text(), /Old P0 alert/);

  push(first.socket, 'breaker.state_changed', { event_seq: 38, event_type: 'BREAKER_STATE_CHANGED', entity_id: 'older-breaker',
    payload: { state: 'CLOSED', probe_status: 'PASSED' } });
  push(first.socket, 'vital_signs.tick', snapshot(39, { half_open_breakers: 1 }));
  await expectText(hud, /Tripped 0 HALF_OPEN 1 · Affected ordinary queues remain frozen/);
  assert.doesNotMatch(hud.text(), /older-breaker|Probe verified/);
  await expectText(frontier, /Tripped 0 HALF_OPEN 1 · Affected ordinary queues remain frozen/);

  push(first.socket, 'breaker.state_changed', { event_seq: 40, event_type: 'BREAKER_STATE_CHANGED', entity_id: 'breaker-marker',
    payload: { state: 'HALF_OPEN', probe_status: 'NEEDS_PROBE' } });
  await expectText(hud, /#40 HALF_OPEN Blocked — a probe is required Ordinary queue remains frozen/);
  push(first.socket, 'breaker.state_changed', { event_seq: 41, event_type: 'BREAKER_STATE_CHANGED', entity_id: 'breaker-marker',
    payload: { state: 'CLOSED', probe_status: 'PASSED', probe_passed: 2, probe_required: 2 } });
  await expectText(hud, /#41 CLOSED Probe verified 2\/2 probes passed/);

  first.socket.destroy();
  await expectText(hud, /Reconnecting · stale .*Seq 39/);
  assert.doesNotMatch(hud.text(), /Latest breaker event|Probe verified/);
  const second = await nthSubscription(1);
  assert.equal(handles.length, 1, 'Host reconnect retains the browser stream');
  status = snapshot(42, { last_alert: { event_seq: 41, headline: 'Latest from status' } });
  ack(second);
  await expectText(hud, /Live .*Seq 42 .*Latest from status/);
  const secondPage = await pageRequest(1, 1);
  reply(secondPage.socket, secondPage.id, { items: [
    { event_seq: 38, headline: 'Duplicate persisted alert' },
    { event_seq: 40, headline: 'Missed during disconnect' },
  ], truncated: false });
  await expectText(briefing, /Latest from status .*Missed during disconnect .*A fresh P0 alert/);
  assert.doesNotMatch(briefing.text(), /Duplicate persisted alert/);
  assert.doesNotMatch(frontier.text(), /Latest breaker event/);

  // A non-retryable Host stream error crosses the async iterable, but the
  // browser retains stale data and schedules its own retry without exposing it.
  push(second.socket, 'unknown.topic', { secret: 'do-not-render' });
  await expectText(hud, /Reconnecting · stale .*Seq 42\b/);
  const retry = await until(() => app.timers.entries().next().value, Boolean, 'Expected browser retry timer');
  assert.equal(handles[0].signal.aborted, true);
  assert.doesNotMatch(hud.text(), /unknown\.topic|do-not-render/);
  app.timers.delete(retry[0]);
  retry[1].fn();
  const third = await nthSubscription(2);
  status = snapshot(1);
  ack(third);
  await expectText(hud, /Live .*Seq 1\b/);
  await pageRequest(2, 0);
  assert.doesNotMatch(briefing.text(), /Recent P0 alerts|Missed during disconnect/);
  assert.equal(handles.length, 2);
  const closed = once(third.socket, 'close');
  app.dispose();
  assert.equal(handles[1].signal.aborted, true);
  assert.equal(historySignals[2].aborted, true);
  await closed;
  assert.equal(app.timers.size, 0);
});

const continueLabel = 'Continue P0 alert catch-up';
const alertRequests = (...cursors) => cursors.map(after => ({ after, limit: 64 }));
const transition = state => ({ topic: 'ai4math.connection', data: { state } });
const historyPage = (after, size = 64, truncated = true) => ok({
  items: Array.from({ length: size }, (_, index) => ({
    event_seq: after + index + 1, headline: 'Persisted P0 ' + (after + index + 1),
  })),
  truncated,
});

test('each healthy-connection P0 continuation is bounded and does not skip unvalidated gaps', async t => {
  const calls = [];
  const firstManualPage = Promise.withResolvers();
  const pushGate = Promise.withResolvers();
  const app = await browser(t, {
    async *subscribe(_params, signal) {
      yield transition('connected');
      if (signal.aborted) return;
      const aborted = once(signal, 'abort');
      await Promise.race([pushGate.promise, aborted]);
      if (!signal.aborted) {
        yield { topic: 'alert.p0', data: { event_seq: 999, headline: 'New live alert' } };
        await aborted;
      }
    },
    async status() { return ok(snapshot(1000)); },
    async alertsList({ after, limit }) {
      calls.push({ after, limit });
      if (calls.length === 4) return firstManualPage.promise;
      if (calls.length === 5) return ok({ items: [
        { event_seq: 257, headline: 'Do not merge malformed page' },
        { event_seq: 257, headline: 'Duplicate within page' },
      ], truncated: true });
      return after < 448 ? historyPage(after) : historyPage(after, 1, false);
    },
  });
  const hud = app.mount('shell.bottom');
  const briefing = mountTab(app, 'briefing');
  await expectText(hud, /P0 alert catch-up incomplete.*Last P0 Persisted P0 192/);
  assert.deepEqual(calls, alertRequests(0, 64, 128));
  assert.equal(briefing.find('ul').children.length, 16);
  assert.match(briefing.text(), /#192 .*#177\b/);
  assert.doesNotMatch(briefing.text(), /#176\b/);

  pushGate.resolve();
  await expectText(hud, /Last P0 New live alert/);
  assert.deepEqual(calls, alertRequests(0, 64, 128), 'live pushes do not trigger polling');
  const hudContinue = hud.find('button', continueLabel).props;
  const briefingContinue = briefing.find('button', continueLabel).props;
  assert.equal(hudContinue.disabled, false);
  briefingContinue.onClick();
  assert.equal(hud.find('button', continueLabel).props.disabled, true);
  assert.equal(briefing.find('button', continueLabel).props.disabled, true);
  hudContinue.onClick(); // Simulate clicking another surface before React commits the disabled state.
  assert.deepEqual(calls, alertRequests(0, 64, 128, 192));

  firstManualPage.resolve(historyPage(192));
  await expectText(briefing, /catch-up incomplete.*#999 .*#256\b/);
  assert.doesNotMatch(briefing.text(), /#257\b|malformed page/);
  assert.deepEqual(calls, alertRequests(0, 64, 128, 192, 256));

  hudContinue.onClick();
  await expectText(briefing, /catch-up incomplete.*#999 .*#448\b/);
  assert.deepEqual(calls, alertRequests(0, 64, 128, 192, 256, 256, 320, 384),
    'retry resumes at the last valid cursor and stops at the next manual cap');
  assert.equal(app.timers.size, 0, 'healthy connection does not schedule a retry loop');
  briefing.find('button', continueLabel).props.onClick();
  await expectText(briefing, /#999 .*#449\b/);
  assert.doesNotMatch(hud.text(), /catch-up incomplete|Continue P0 alert catch-up/);
  hudContinue.onClick();
  assert.deepEqual(calls, alertRequests(0, 64, 128, 192, 256, 256, 320, 384, 448));
});

test('reconnect resumes the last confirmed P0 cursor and discards a late manual page', async t => {
  const calls = [];
  const disconnect = Promise.withResolvers();
  const reconnect = Promise.withResolvers();
  const late = Promise.withResolvers();
  let manualSignal;
  const app = await browser(t, {
    async *subscribe(_params, signal) {
      yield transition('connected');
      if (signal.aborted) return;
      const aborted = once(signal, 'abort');
      await Promise.race([disconnect.promise, aborted]);
      if (!signal.aborted) yield transition('reconnecting');
      await Promise.race([reconnect.promise, aborted]);
      if (!signal.aborted) {
        yield transition('connected');
        await aborted;
      }
    },
    async status() { return ok(snapshot(400)); },
    async alertsList({ after, limit }, signal) {
      calls.push({ after, limit });
      if (calls.length === 4) {
        manualSignal = signal;
        return late.promise;
      }
      return after < 192 ? historyPage(after) : historyPage(after, 1, false);
    },
  });
  const hud = app.mount('shell.bottom');
  await expectText(hud, /P0 alert catch-up incomplete.*Last P0 Persisted P0 192/);
  const click = hud.find('button', continueLabel).props.onClick;
  click();
  assert.equal(manualSignal.aborted, false);
  disconnect.resolve();
  await expectText(hud, /Reconnecting · stale/);
  assert.equal(manualSignal.aborted, true);
  assert.equal(hud.find('button', continueLabel).props.disabled, true);
  click();
  assert.deepEqual(calls, alertRequests(0, 64, 128, 192));

  reconnect.resolve();
  await expectText(hud, /Last P0 Persisted P0 193/);
  assert.doesNotMatch(hud.text(), /catch-up incomplete/);
  late.resolve(historyPage(192));
  await new Promise(resolve => setImmediate(resolve));
  assert.match(hud.text(), /Last P0 Persisted P0 193/);
  assert.deepEqual(calls, alertRequests(0, 64, 128, 192, 192));
});

test('tab errors use a fixed message and unmount aborts a pending request', async t => {
  const requests = [];
  const app = await browser(t, {
    subscribe() { return (async function* () {})(); },
    async status() { throw new Error('host path /private/local.sock'); },
    inboxList(_params, signal) {
      return new Promise((resolve, reject) => requests.push({ resolve, reject, signal }));
    },
  });
  const inbox = mountTab(app, 'inbox');
  inbox.text(); // commit its initial query effect
  assert.equal(requests.length, 1);
  requests[0].reject(new Error('exception from /private/local.sock'));
  await expectText(inbox, /AI4Math data unavailable/);
  assert.doesNotMatch(inbox.text(), /private|exception/);
  assert.equal(requests.length, 1, 'read does not replay automatically');
  inbox.find('button', 'Refresh').props.onClick();
  assert.equal(requests.length, 2);
  assert.equal(requests[1].signal.aborted, false);
  inbox.close();
  assert.equal(requests[1].signal.aborted, true);
  requests[1].resolve(ok({ items: [] }));
  await until(() => app.timers.size, count => count === 1, 'Expected pending stream retry');
  app.dispose();
  assert.equal(app.timers.size, 0, 'unmount cancels pending stream retry');
});
