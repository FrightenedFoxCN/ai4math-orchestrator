const { createElement: h, useCallback, useEffect, useRef, useState, useSyncExternalStore } = require('react')
const { call } = require('./remote.cjs')
const { REMOTE_METHODS, WIKI_SCOPES } = require('./contract')

const TRUST = ['L0', 'L1', 'L2a', 'L2b', 'L3', 'L4a', 'L4b', 'L5']
const PRIORITIES = ['P0', 'P1', 'P2']
const NA = 'n/a'
const text = (value, otherwise = NA) => typeof value === 'string' && value.length > 0 ? value : otherwise
const isCount = value => Number.isSafeInteger(value) && value >= 0
const count = value => isCount(value) ? String(value) : NA
const numeric = value => typeof value === 'number' && Number.isFinite(value) ? String(value) : NA
const rows = (value, max = 80) => Array.isArray(value)
  ? value.slice(0, max).filter(item => item && typeof item === 'object' && !Array.isArray(item)) : []
const pct = value => typeof value === 'number' && Number.isFinite(value) && value >= 0 && value <= 100
  ? Math.round(value) + '%' : NA
const badge = (label, tone) => h('span', { className: 'ai4m-badge', 'data-tone': tone }, label)
const labelValue = (label, value) => h('span', null, h('span', { className: 'ai4m-muted' }, label + ' '), value)
const button = (label, onClick, disabled) => h('button', { className: 'ai4m-button', type: 'button', onClick, disabled }, label)
const note = (value, role) => h('p', { className: 'ai4m-muted', role }, value)
const empty = (value) => note(value ?? 'No records returned.')
const panels = Object.freeze([
  { key: 'briefing', label: 'Briefing', description: 'Preview or retain a deterministic fact pack.' },
  { key: 'frontier', label: 'Frontier', description: 'Inspect active claims, dependency plans, and breaker signals.' },
  { key: 'inbox', label: 'Inbox', description: 'Review prioritized decisions awaiting human attention.' },
  { key: 'wiki', label: 'Wiki', description: 'Search CORE and inspect archaeology hints and provenance.' },
])

function trustTape(trust) {
  return h('span', { className: 'ai4m-tape', 'aria-label': 'Trust ledger' }, TRUST.map(level =>
    h('span', { key: level }, labelValue(level, count(trust?.[level])))))
}

function AlertCatchUp({ live, state }) {
  if (!state.alertHistoryIncomplete) return null
  return h('div', { className: 'ai4m-actions' },
    note(state.alertHistoryLoading ? 'Loading next P0 alert batch…' : state.phase === 'live'
      ? 'P0 alert catch-up incomplete.' : 'P0 alert catch-up incomplete. Waiting for the daemon to reconnect.', 'status'),
    button('Continue P0 alert catch-up', live.continueAlertCatchUp, state.phase !== 'live' || state.alertHistoryLoading))
}

function BreakerTotals({ snapshot }) {
  const tripped = snapshot?.tripped_breakers
  const halfOpen = snapshot?.half_open_breakers
  const archived = snapshot?.archived_breakers
  return h('span', { className: 'ai4m-meta', role: 'status' },
    badge('Tripped ' + count(tripped), isCount(tripped) && tripped > 0 ? 'danger' : undefined),
    isCount(halfOpen) && badge('HALF_OPEN ' + halfOpen + (halfOpen > 0 ? ' · Affected ordinary queues remain frozen' : ''),
      halfOpen > 0 ? 'warn' : undefined),
    isCount(archived) && archived > 0 && badge('ARCHIVED ' + archived + ' · Affected ordinary queues remain gated', 'warn'))
}

function InfraRfcTotals({ snapshot }) {
  const {
    event_seq: seq,
    infra_rfc_drafts: drafts,
    infra_rfc_approved: approved,
    infra_rfc_building_smoke: smoke,
    infra_rfc_frozen_proving: frozen,
    infra_rfc_completed: completed,
  } = snapshot ?? {}
  // Every recorded RFC has a submission event at or before this snapshot.
  // Reject absent, unsafe, or out-of-ledger totals without inferring zero.
  if (!isCount(seq) || !isCount(drafts) || !isCount(approved) || drafts > seq || approved > seq - drafts) return null
  const hasLifecycle = smoke !== undefined || frozen !== undefined || completed !== undefined
  if (!hasLifecycle) {
    return badge('Infra RFCs · DRAFT ' + drafts + ' · APPROVED ' + approved + ' · Review only; not ready to run')
  }
  if (!isCount(smoke) || !isCount(frozen) || !isCount(completed)) return null
  const total = drafts + approved + smoke + frozen + completed
  if (!Number.isSafeInteger(total) || total > seq) return null
  return badge('Infra RFCs · DRAFT ' + drafts + ' · APPROVED ' + approved
    + ' · SMOKE ' + smoke + ' · FROZEN ' + frozen + ' · COMPLETED ' + completed)
}

function DiscoveryBadge({ value, currentSeq }) {
  if (!value || typeof value !== 'object') return null
  if (value.status === 'starting') return labelValue('T3 discovery', 'Waiting for first sample')
  if (value.status === 'unavailable') return labelValue('T3 discovery', 'Unavailable')
  const hash = value.observed_head
  const claimTier = value.baseline_scope === 'claim_t1' ? 'T1' : value.baseline_scope === 'claim_t3' ? 'T3' : null
  if (Object.hasOwn(value, 'baseline_scope') && !claimTier) return null
  if (value.advisory_only !== true || !isCount(value.ledger_event_seq)
    || typeof hash !== 'string' || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(hash)) return null
  let summary
  switch (value.status) {
    case 'unchanged': summary = 'No structural change'; break
    case 'diverged': summary = 'Main diverged · review baseline'; break
    case 'structural_change':
      if (!isCount(value.changed_module_count) || value.changed_module_count > 255
        || typeof value.module_order_changed !== 'boolean') return null
      summary = 'Structure changed · ' + value.changed_module_count + ' source change(s)'
        + (value.module_order_changed ? ' · module list changed' : '')
      break
    default: return null
  }
  const scopeNote = claimTier ? ' Baseline verified for one Claim, not for all of main.' : ''
  return h('span', { title: 'Observed Git commit: ' + hash + '. This sample does not approve or apply a T3 plan.' + scopeNote },
    labelValue('Last T3 discovery', summary + (claimTier ? ' · Claim ' + claimTier + ' baseline' : '') + ' · advisory only'
      + (isCount(currentSeq) && value.ledger_event_seq < currentSeq ? ' · older ledger sample' : '')))
}

const T2_AUDIT_STATUS_TEXT = {
  starting: 'Starting',
  running: 'Running',
  unavailable: 'Unavailable',
  waiting: 'Waiting',
  head_changed: 'Reviewed head changed',
}

function hasExactOwnKeys(value, ...keys) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    && Reflect.ownKeys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key))
}

function T2AuditBadge({ value, currentSeq }) {
  if (!isCount(currentSeq)) return null
  const hasObservation = hasExactOwnKeys(value, 'status', 'attempts', 'exhausted', 'observation')
  if (!hasObservation && !hasExactOwnKeys(value, 'status', 'attempts', 'exhausted')) return null
  const { status, attempts, exhausted } = value
  if (hasObservation !== (status === 'observation') || !isCount(attempts) || attempts > 64
    || typeof exhausted !== 'boolean' || (exhausted && attempts === 0)
    || (status === 'starting' && attempts !== 0)
    || (status === 'running' && (attempts === 0 || exhausted))) return null

  const attemptsText = attempts + (attempts === 1 ? ' attempt' : ' attempts')
  let summary, title
  if (hasObservation) {
    const sample = value.observation
    const fields = ['schema', 'advisory_only', 'reviewed_commit', 'ledger_event_seq']
    const schema = sample?.schema
    const heartbeats = schema === 'ai4math.t2.resident-heartbeats.v3'
    const independent = schema === 'ai4math.t2.resident-sample.v2'
    if (heartbeats) fields.push('regressed', 'max_command_regressed', 'total_commands_regressed')
    else fields.push('compile_ms', 'module_count', 'debt_findings', ...(independent ? ['independent_checks'] : []))
    if (attempts === 0 || !hasExactOwnKeys(sample, ...fields)) return null
    const { reviewed_commit: oid, ledger_event_seq: sampleSeq } = sample
    if (sample.advisory_only !== true || typeof oid !== 'string'
      || (oid.length !== 40 && oid.length !== 64) || /[^0-9a-f]/.test(oid) || !/[1-9a-f]/.test(oid)
      || !isCount(sampleSeq)) return null

    title = 'Reviewed Git commit: ' + oid + '. This sample carries no proof authority.'
    if (heartbeats) {
      const { regressed, max_command_regressed: max, total_commands_regressed: total } = sample
      if (typeof max !== 'boolean' || typeof total !== 'boolean' || regressed !== (max || total)) return null
      title += ' Heartbeat comparison covers one selected module.'
      summary = (regressed
        ? 'Heartbeat thresholds met (' + [max ? 'max command' : '', total ? 'total commands' : ''].filter(Boolean).join(', ') + ')'
        : 'Heartbeat thresholds not met') + ' · ' + attemptsText
    } else {
      const { compile_ms: ms, module_count: modules, debt_findings: debt, independent_checks: checks } = sample
      if ((!independent && schema !== 'ai4math.t2.resident-sample.v1')
        || (independent && (debt !== 0 || !isCount(checks) || checks < 1 || checks > 8))
        || !isCount(ms) || ms > 120000 || !isCount(modules) || modules < 1 || modules > 128
        || !isCount(debt) || debt > 3072) return null
      if (independent) title += ' Independent checks cover only the reviewed selection.'
      summary = ms + ' ms compile · ' + modules + (modules === 1 ? ' module' : ' modules')
        + ' · ' + debt + (debt === 1 ? ' debt finding' : ' debt findings')
        + (independent ? ' · ' + checks + (checks === 1 ? ' independent theorem check' : ' independent theorem checks') : '')
        + ' · ' + attemptsText
    }
    if (sampleSeq < currentSeq) summary = 'older ledger sample'
    else if (sampleSeq > currentSeq) summary = 'awaiting matching ledger'
  } else {
    if (typeof status !== 'string' || !Object.hasOwn(T2_AUDIT_STATUS_TEXT, status)) return null
    summary = T2_AUDIT_STATUS_TEXT[status] + ' · ' + attemptsText
  }
  return h('span', { className: 'ai4m-badge', title },
    'T2 Audit · advisory only · ' + summary + (exhausted ? ' · exhausted' : ''))
}

function leanAdvisoryBadge(broker) {
  const { lean_advisory_busy: busy, lean_advisory_capacity: capacity, fast_busy: fastBusy, fast_capacity: fastCapacity } = broker ?? {}
  if (!isCount(busy) || !isCount(capacity) || capacity < 1 || capacity > 4
    || !isCount(fastCapacity) || capacity > fastCapacity
    || !isCount(fastBusy) || busy > fastBusy || busy > capacity) return null
  return badge('Lean advisory · ' + busy + ' in use / ' + capacity + ' nominal cap')
}

function LiveHud({ live, open }) {
  const [navigationError, setNavigationError] = useState(false)
  const state = useSyncExternalStore(live.subscribe, live.getSnapshot)
  const { phase, snapshot, recentAlerts, breaker } = state
  const latestBreaker = phase === 'live' && readBreakerEvent(breaker)
  const alert = recentAlerts[0]
  const alertText = text(alert?.headline, '')
  const memory = snapshot?.memory
  const broker = snapshot?.broker
  const advisoryBadge = phase === 'live' ? leanAdvisoryBadge(broker) : null
  const workers = snapshot?.workers
  const oracleHeld = snapshot?.task_scheduler?.exploration?.held_for_oracle
  const brokerText = !broker ? NA
    : 'fast ' + count(broker.fast_busy) + '/' + count(broker.fast_capacity) + ' busy · heavy '
      + count(broker.heavy_running) + ' running / ' + count(broker.heavy_queued) + ' queued'
  const phaseLabel = phase === 'live' ? 'Live' : phase === 'reconnecting'
    ? (snapshot ? 'Reconnecting · stale' : 'Reconnecting') : 'Connecting'
  return h('section', { className: 'ai4m ai4m-hud', 'aria-label': 'AI4Math Live HUD' },
    h('div', { className: 'ai4m-hud-line' },
      h('strong', null, 'AI4Math'),
      h('span', { role: 'status' }, badge(phaseLabel, phase === 'live' ? 'good' : 'warn')),
      labelValue('RAM', pct(memory?.used_percent) + (typeof memory?.pressure === 'string' ? ' · ' + memory.pressure : '')),
      h('span', { title: 'Daemon-managed work only; independent CLI processes are not counted.' },
        labelValue('Daemon broker', brokerText), advisoryBadge && ' ', advisoryBadge),
      labelValue('Workers', count(typeof workers === 'number' ? workers : workers?.active)),
      trustTape(snapshot?.trust),
      labelValue('Stale', count(snapshot?.stale_claims)),
      phase === 'live' && h(T2AuditBadge, { value: snapshot?.t2_audit, currentSeq: snapshot?.event_seq }),
      phase === 'live' && h(DiscoveryBadge, { value: snapshot?.t3_discovery, currentSeq: snapshot?.event_seq }),
      h(BreakerTotals, { snapshot }),
      phase === 'live' && h(InfraRfcTotals, { snapshot }),
      phase === 'live' && isCount(oracleHeld) && oracleHeld > 0 && h('span', {
        title: 'Temporary exploration scheduling hold; not a Claim refutation.',
      }, labelValue('Oracle-held exploration', count(oracleHeld))),
      ...PRIORITIES.map(priority => h('span', { key: priority }, badge(priority + ' ' + count(snapshot?.pending_inbox?.[priority]),
        snapshot?.pending_inbox?.[priority] > 0 ? priority === 'P0' ? 'danger' : priority === 'P1' ? 'warn' : undefined : undefined))),
      h('span', { title: text(snapshot?.observed_at, '') }, labelValue('Seq', count(snapshot?.event_seq))),
      h('nav', { className: 'ai4m-actions', 'aria-label': 'AI4Math panels' }, panels.map(panel =>
        h('button', { key: panel.key, type: 'button', className: 'ai4m-button', onClick: event => {
          setNavigationError(!open(panel.key, event.currentTarget))
        } }, panel.label)))),
    navigationError && note('Open a DSH session before opening an AI4Math tab.', 'status'),
    h(AlertCatchUp, { live, state }),
    h('div', { role: 'status', 'aria-atomic': true }, (latestBreaker?.probe || latestBreaker?.state === 'ARCHIVED') &&
      h('div', { className: 'ai4m-hud-alert' },
        'Latest breaker event · ', h('code', null, text(breaker.entity_id)), ' · #' + count(breaker.event_seq) + ' ',
        h(BreakerReading, { reading: latestBreaker }))),
    alertText && h('div', { className: 'ai4m-hud-alert' },
      badge('Last P0', 'danger'), ' ', alertText,
      alert.briefing_id && ' · ' + text(alert.briefing_id)))
}

// Each mounted tab owns its request. Manual actions do not retry writes, and
// stale completions cannot update an unmounted tab or replace a newer query.
function useQuery(remote) {
  const [state, setState] = useState({ phase: 'idle', value: null })
  const active = useRef(null)
  useEffect(() => () => { active.current?.abort(); active.current = null }, [])
  const run = useCallback(async (method, params, alertEventSeq) => {
    active.current?.abort()
    const owner = new AbortController()
    active.current = owner
    setState({ phase: 'loading', value: null })
    try {
      const value = await call(remote, method, params, owner.signal)
      if (active.current === owner) setState({ phase: 'done', method, value, alertEventSeq })
    } catch {
      if (active.current === owner && !owner.signal.aborted) setState({ phase: 'error', value: null })
    }
  }, [remote])
  return { ...state, run }
}

function queryNote(query, fallback) {
  if (query.phase === 'loading') return note('Loading AI4Math data…', 'status')
  if (query.phase === 'error') return note('AI4Math data unavailable. Check the host daemon connection or retry.', 'alert')
  if (query.phase === 'idle') return note(fallback)
  return null
}

function Panel({ title, subtitle, actions, children }) {
  return h('section', { className: 'ai4m ai4m-panel' },
    h('header', null,
      h('h2', null, title), subtitle && note(subtitle),
      actions && h('div', { className: 'ai4m-actions' }, actions)),
    children)
}

function MathCard({ informal, formal, target }) {
  return h('div', { className: 'ai4m-math' },
    h('div', null, h('strong', null, 'Informal statement'), h('p', null, text(informal, 'Not supplied'))),
    h('div', null, h('strong', null, 'Formal signature'), h('pre', null, text(formal, 'Not supplied')),
      text(target, '') && h('p', null, labelValue('Formal target', h('code', null, target)))))
}

function ClaimCard({ claim }) {
  return h('article', { className: 'ai4m-card' },
    h('h4', null, text(claim.title, text(claim.claim_id, 'Unnamed claim'))),
    h('div', { className: 'ai4m-meta' },
      h('code', null, text(claim.claim_id)), badge(text(claim.engine)),
      badge('Effective ' + text(claim.effective_trust)), badge('Local ' + text(claim.local_trust)),
      claim.critical === true && badge('Critical'), claim.stale === true && badge('Stale', 'warn'),
      claim.oracle_debt === true && badge('Oracle debt', 'warn')),
    h(MathCard, { informal: claim.informal_statement, formal: claim.formal_signature, target: claim.formal_target }),
    typeof claim.wiki_id === 'string' && labelValue('Wiki reference', h('code', null, claim.wiki_id)))
}

function InboxCard({ item }) {
  return h('article', { className: 'ai4m-card' },
    h('h4', null, text(item.summary, 'No summary supplied')),
    h('div', { className: 'ai4m-meta' }, badge(text(item.priority), item.priority === 'P0' ? 'danger' : item.priority === 'P1' ? 'warn' : undefined),
      badge(text(item.category)), h('code', null, text(item.inbox_id)), labelValue('Leverage', numeric(item.leverage_score)),
      text(item.created_at)),
    item.payload != null && h(Evidence, { title: 'Recorded evidence / options', value: item.payload }),
    item.payload_truncated === true && note('Evidence was truncated by the daemon.'))
}

function Evidence({ title, value, open }) {
  // Always React text. Compile no HTML, Markdown, URLs, or formal snippets.
  return h('details', { open }, h('summary', null, title), h(EvidenceText, { value }))
}

function EvidenceText({ value }) {
  return h('pre', { className: 'ai4m-evidence' }, JSON.stringify(value, null, 2)?.slice(0, 8000))
}

function Section({ title, children }) {
  return h('section', { className: 'ai4m-section' }, h('h3', null, title), children)
}

function Cards({ page, itemKey, card, emptyText }) {
  const items = rows(page?.items)
  return h('div', { className: 'ai4m-stack' },
    items.length ? items.map((item, index) => card(item, text(item[itemKey], 'item') + ':' + index)) : empty(emptyText),
    page?.truncated === true && note('The daemon returned a bounded slice.'))
}

function PlanSlice({ page }) {
  const edges = rows(page?.items)
  return h(Section, { title: 'Dependency plan slice' }, edges.length > 0 ? h('ul', { className: 'ai4m-edges' }, edges.map((edge, index) =>
    h('li', { key: index }, h('code', null, text(edge.target_claim_id)), ' · ',
      h('code', null, text(edge.plan_id)), ' ', badge(text(edge.plan_status)),
      typeof edge.dep_claim_id === 'string' && h('span', null, ' → ', h('code', null, edge.dep_claim_id), ' ', text(edge.dep_kind, '')))))
    : empty(page ? 'No dependency edges in this slice.' : 'No dependency edge data supplied.'),
  page?.truncated === true && note('Dependencies outside the bounded slice are not shown.'))
}

function readBreaker(item) {
  const state = text(item?.state, text(item?.to_state))
  const status = item?.probe_status
  const passed = item?.probe_passed
  const required = item?.probe_required
  const countsAbsent = passed == null && required == null
  const validCounts = isCount(passed) && isCount(required) && required > 0 && passed <= required
  const halfOpen = state === 'HALF_OPEN'
  let probe, tone
  if (halfOpen || state === 'TRIPPED') {
    switch (status) {
      case 'AWAITING_REVIEW': probe = 'Blocked — awaiting human review'; break
      case 'NEEDS_PROBE': probe = 'Blocked — a probe is required'; break
      case 'FAILED': probe = 'Blocked — probe failed'; break
      case 'READY': if (halfOpen) probe = 'Waiting — probe ready'; break
      case 'RUNNING': if (halfOpen) probe = 'Waiting — probe attempt started'; break
    }
    if (probe) tone = probe.startsWith('Blocked') ? 'danger' : 'warn'
  }
  // Verification comes from an explicit daemon outcome, never from the counts
  // alone. Older payloads can omit both counts; contradictory counts cannot verify.
  if (status === 'PASSED' && state === 'CLOSED' &&
    (countsAbsent || (validCounts && passed === required))) {
    probe = 'Probe verified'
    tone = 'good'
  }
  const progress = probe && validCounts ? passed + '/' + required + ' probes passed' : null
  if (!probe && (halfOpen || (status === 'PASSED' && state !== 'ARCHIVED'))) {
    probe = 'Probe status unavailable'
    tone = 'warn'
  }
  return { state, probe, tone, halfOpen, progress }
}

function readBreakerEvent(event) {
  // Daemon events carry state and probe details in the event payload. A
  // truncated payload is evidence only and must not imply verification.
  if (!event?.payload || event.payload_truncated === true) return null
  const reading = readBreaker(event.payload)
  return reading.state === NA ? null : reading
}

function BreakerReading({ reading }) {
  const { state, probe, tone, halfOpen, progress } = reading
  return h('span', { className: 'ai4m-meta' },
    badge(state, state === 'TRIPPED' ? 'danger' : halfOpen ? 'warn' : undefined),
    probe && badge(probe, tone),
    progress && h('span', null, progress),
    halfOpen && h('span', null, 'Ordinary queue remains frozen'),
    state === 'ARCHIVED' && h('span', null, 'Ordinary queue remains gated'))
}

function BreakerCard({ item }) {
  return h('article', { className: 'ai4m-card' },
    h('h4', null, text(item.target_definition_id, text(item.breaker_id, 'Breaker'))),
    h('div', { className: 'ai4m-meta' }, h(BreakerReading, { reading: readBreaker(item) }),
      h('code', null, text(item.module_path)), text(item.engine), text(item.updated_at, '')))
}

function BreakerSignal({ event }) {
  const reading = readBreakerEvent(event)
  return h('article', { className: 'ai4m-card' },
    h('h4', null, 'Latest breaker event'),
    h('div', { className: 'ai4m-meta', role: 'status' }, badge(text(event.event_type), 'warn'),
      h('code', null, text(event.entity_id)), labelValue('Seq', count(event.event_seq)),
      reading && h(BreakerReading, { reading })),
    event.payload != null && h(Evidence, { title: 'Recorded transition evidence', value: event.payload }))
}

function schedulerBlockSummary(event) {
  if ((event.event_kind ?? event.event_type) !== 'ALL_TRACKS_BLOCKED') return ''
  const exploration = event.payload?.exploration_blocked
  const infrastructure = event.payload?.infrastructure_blocked
  if (!isCount(exploration) || !isCount(infrastructure) || exploration < 1 || infrastructure < 1
    || exploration + infrastructure > 4096) return ''
  return 'Exploration blocked: ' + exploration + '; infrastructure blocked: ' + infrastructure
}

function regressionAlertSummary(event) {
  if ((event.event_kind ?? event.event_type) !== 'BROKER_REGRESSION_MELTDOWN') return ''
  const { failed_module: module, distinct_candidates: candidates, window_seconds: seconds } = event.payload ?? {}
  if (typeof module !== 'string' || !/^[A-Z][A-Za-z0-9_]{0,250}\.lean$/.test(module)
    || candidates !== 3 || seconds !== 900) return ''
  return module + ': ' + candidates + ' distinct candidates failed full regression'
}

function criticalBreakerSummary(event) {
  if ((event.event_kind ?? event.event_type) !== 'CRITICAL_BREAKER_TRIPPED') return ''
  const { critical_claim_id: first, critical_claim_count: affected } = event.payload ?? {}
  if (typeof first !== 'string' || !first.length || first.length > 256 || /[\u0000-\u001f\u007f]/.test(first)
    || !isCount(affected) || affected < 1) return ''
  return 'No known route remains for ' + first + (affected > 1 ? ' (' + affected + ' critical claims affected)' : '')
}

function EventList({ page, openEventSeq, emptyText = 'No new events in this window.' }) {
  const events = rows(page?.items)
  return h('div', { className: 'ai4m-stack' },
    events.length ? events.map((event, index) => h(Evidence, { key: index, value: event,
      open: openEventSeq != null && event.event_seq === openEventSeq ? true : undefined,
      title: [text(event.event_kind, text(event.event_type, 'Event')), text(event.entity_id, ''), '#' + count(event.event_seq),
        schedulerBlockSummary(event), regressionAlertSummary(event), criticalBreakerSummary(event)]
        .filter(Boolean).join(' · ') })) : empty(emptyText),
    page?.truncated === true && note('The fact pack contains a bounded event window.'))
}

function Briefing({ remote, live }) {
  const state = useSyncExternalStore(live.subscribe, live.getSnapshot)
  const { recentAlerts, alertHistoryIncomplete } = state
  const query = useQuery(remote)
  const pack = query.value?.fact_pack
  const checkpoint = pack?.checkpoint
  const hasCheckpoint = checkpoint != null && typeof checkpoint === 'object' && !Array.isArray(checkpoint)
  const busy = query.phase === 'loading'
  return h(Panel, { title: 'Review briefing', subtitle: 'Deterministic fact packs; neither action advances the human checkpoint.',
    actions: [h('span', { key: 'peek' }, button('Peek (preview only)', () => void query.run('briefingPeek', {}), busy)),
      h('span', { key: 'pull' }, button('Pull and retain', () => void query.run('briefingPull', {}), busy))] },
    queryNote(query, 'Choose Peek or Pull to request a briefing.'),
    query.phase === 'done' && !pack && note('No fact pack supplied.'),
    (recentAlerts.length > 0 || alertHistoryIncomplete) && h(Section, { title: 'Recent P0 alerts' },
      h(AlertCatchUp, { live, state }),
      recentAlerts.length > 0 && h('ul', { className: 'ai4m-edges' }, recentAlerts.map(alert =>
        h('li', { key: alert.event_seq }, '#' + count(alert.event_seq) + ' · ' + alert.headline,
          typeof alert.briefing_id === 'string' && /^[0-9a-f]{64}$/.test(alert.briefing_id)
            && button('Open retained facts', () => void query.run('briefingGet', { id: alert.briefing_id }, alert.event_seq), busy))))),
    pack && h('div', { className: 'ai4m-stack' },
      h('div', { className: 'ai4m-meta' }, badge(query.method === 'briefingPeek' ? 'Preview' : query.method === 'briefingGet' ? 'Retained alert' : 'Pull'),
        labelValue('Event window', '(' + count(pack.event_from_exclusive) + ', ' + count(pack.event_to_inclusive) + ']'),
        query.value.retained_id != null && labelValue('Retained', h('code', null, text(query.value.retained_id))),
        hasCheckpoint && labelValue('Retained human checkpoint event_seq', count(checkpoint.event_seq)),
        hasCheckpoint && labelValue('Checkpoint ADR', h('code', null, text(checkpoint.last_adr_id))),
        labelValue('Fact SHA-256', h('code', null, text(pack.fact_sha256)))),
      query.method === 'briefingGet' && h(Section, { title: 'P0 alert sources' },
        h(EventList, { page: { items: pack.alert_sources }, openEventSeq: query.alertEventSeq,
          emptyText: 'No P0 alert source events in this retained fact pack.' })),
      h(Section, { title: 'Trust ledger' }, trustTape(pack.trust), ' · Stale ', count(pack.stale_claims),
        pack.risk_radar && ' · Oracle debt ' + count(pack.risk_radar.oracle_debts) + ' · Cross-engine debt ' + count(pack.risk_radar.cross_engine_debts)),
      h(Section, { title: 'Milestones / Refutations' }, ...[['Milestones', pack.milestones], ['Refutations', pack.refutations]].map(([title, page]) =>
        h('div', { key: title }, h('h4', null, title), h(Cards, { page, itemKey: 'event_seq', card: (item, key) =>
          h('article', { key, className: 'ai4m-card' }, h('h4', null, text(item.claim?.title ?? item.claim_id)),
            badge(item.new_local_trust), ' · Event #' + count(item.event_seq),
            h(MathCard, { informal: item.claim?.informal_statement, formal: item.claim?.formal_signature,
              target: item.claim?.formal_target }),
            item.graveyard_id && labelValue('Graveyard witness', text(item.graveyard_id))) })))),
      h(Section, { title: 'Frontier claims' }, h(Cards, { page: pack.frontier, itemKey: 'claim_id',
        card: (claim, key) => h(ClaimCard, { claim, key }) })),
      h(PlanSlice, { page: pack.frontier?.plans }),
      h(Section, { title: 'Recent events / expand evidence' }, h(EventList, { page: pack.events })),
      h(Section, { title: 'Circuit breakers' }, h(Cards, { page: pack.breakers, itemKey: 'breaker_id',
        card: (item, key) => h(BreakerCard, { item, key }) })),
      h(Section, { title: 'Decision inbox' }, h(Cards, { page: pack.inbox, itemKey: 'inbox_id',
        card: (item, key) => h(InboxCard, { item, key }) }))))
}

function Frontier({ remote, live }) {
  const query = useQuery(remote)
  const { phase, snapshot, breaker } = useSyncExternalStore(live.subscribe, live.getSnapshot)
  const refresh = useCallback(() => void query.run('frontierList', { limit: 32 }), [query.run])
  useEffect(refresh, [refresh])
  return h(Panel, { title: 'Active frontier', subtitle: 'A bounded claim and dependency slice, not the full proof DAG.',
    actions: button('Refresh', refresh, query.phase === 'loading') },
    h(Section, { title: 'Circuit breakers' },
      h('p', null, h(BreakerTotals, { snapshot }),
        phase !== 'live' && ' · Live status unavailable or stale'),
      phase === 'live' && breaker && h(BreakerSignal, { event: breaker }),
      note('Read-only. To review stored breaker records, use Briefing → Peek.')),
    queryNote(query, 'Waiting for frontier…'),
    query.value && h(Section, { title: 'Claims' }, h(Cards, { page: query.value, itemKey: 'claim_id',
      card: (claim, key) => h(ClaimCard, { claim, key }) })),
    query.value && h(PlanSlice, { page: query.value.plans }))
}

function Inbox({ remote }) {
  const query = useQuery(remote)
  const refresh = useCallback(() => void query.run('inboxList', { limit: 32 }), [query.run])
  useEffect(refresh, [refresh])
  return h(Panel, { title: 'Decision inbox', subtitle: 'Pending decisions only. This panel does not submit decisions.',
    actions: button('Refresh', refresh, query.phase === 'loading') },
    queryNote(query, 'Waiting for inbox…'),
    query.value && h(Cards, { page: query.value, itemKey: 'inbox_id', emptyText: 'No pending decisions.',
      card: (item, key) => h(InboxCard, { item, key }) }))
}

const provenanceIdSchema = REMOTE_METHODS.wikiProvenance.params.properties.id
const validProvenanceId = id => typeof id === 'string'
  && new TextEncoder().encode(id).byteLength <= provenanceIdSchema['x-ai4math-maxUtf8Bytes']
  && new RegExp(provenanceIdSchema.pattern, 'u').test(id)

// Identity comes from the reserved index namespace, not from L5 or the
// response-wide generation flag. Provenance endpoints have no file_path.
const managedCorePath = '.managed-core/'
const isDerivedCore = (id, stratum) => stratum === 'core'
  && typeof id === 'string' && /^CORE-ADAPT-[0-9a-f]{64}$/.test(id)

function wikiCoverageNote(value, surface) {
  if (value.managed_core === true) return note(surface === 'search'
    ? 'Derived CORE was checked for this search. Search again to check for changes.'
    : 'Derived CORE lineage was checked for this response. Inspect again to check for changes.')
  if (value.managed_core !== false) return note('Derived CORE coverage was not reported for this ' + surface + '.')
  return note(surface === 'search'
    ? 'Static Wiki only; derived CORE entries were not included.'
    : 'Static Wiki lineage only; derived CORE edges were not included.')
}

function WikiCard({ hit, inspectProvenance }) {
  const hasPath = typeof hit.file_path === 'string'
  const virtual = hasPath && hit.file_path.startsWith(managedCorePath)
  const derived = virtual && isDerivedCore(hit.id, hit.stratum) && hit.file_path === managedCorePath + hit.id
  return h('article', { className: 'ai4m-card' },
    h('h4', null, text(hit.title, text(hit.id, 'Untitled Wiki entry'))),
    h('div', { className: 'ai4m-meta' }, h('code', null, text(hit.id)), badge(text(hit.engine)),
      badge(derived ? 'Derived CORE' : text(hit.stratum)),
      badge((derived ? 'Indexed trust ' : '') + text(hit.trust_level)), badge(text(hit.status))),
    h(MathCard, { informal: hit.informal_gloss, formal: hit.formal_signature }),
    hasPath && labelValue(virtual ? 'Virtual index location' : 'Source path', h('code', null, hit.file_path)),
    derived && typeof hit.verified_at_commit === 'string' && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(hit.verified_at_commit)
      && labelValue('Verified at Git commit', h('code', null, hit.verified_at_commit)),
    derived && note('This derived index record is not an authored Wiki page or a proof source.'),
    validProvenanceId(hit.id) && h('div', { className: 'ai4m-actions' },
      button('Inspect provenance for ' + hit.id, () => inspectProvenance(hit.id, derived))))
}

function WikiProvenance({ target, query }) {
  const edges = rows(query.value?.edges, 24)
  const hasDerived = target.derived || edges.some(edge => isDerivedCore(edge.src_id, edge.src_stratum)
    || isDerivedCore(edge.dst_id, edge.dst_stratum))
  const endpoint = (value, stratum) => h('span', null, h('code', null, text(value)), ' ',
    badge(isDerivedCore(value, stratum) ? 'Derived CORE' : text(stratum, 'Outside index')))
  return h(Section, { title: 'Provenance for ' + target.id },
    note('Read-only lineage across strata; recorded edges do not confer proof status.'),
    hasDerived && note('Derived CORE nodes are index records, not authored Wiki pages or proof sources.'),
    queryNote(query),
    query.value && wikiCoverageNote(query.value, 'provenance response'),
    query.phase === 'done' && (edges.length ? h('ol', { className: 'ai4m-edges' }, edges.map((edge, index) =>
      h('li', { key: index }, endpoint(edge.src_id, edge.src_stratum), ' → ', endpoint(edge.dst_id, edge.dst_stratum),
        ' · ', text(edge.edge_type), ' · ', labelValue('Hops', count(edge.depth)),
        edge.metadata && Object.keys(edge.metadata).length > 0 && h(Evidence, { title: 'Indexed edge metadata', value: edge.metadata }))))
      : empty('No edges returned for this slice.')),
    query.value?.truncated === true && note('Only a bounded provenance slice is shown.'))
}

function validRouteLesson(record) {
  if (!hasExactOwnKeys(record, 'lesson_id', 'kind', 'trust_level', 'advisory_only', 'adr_id',
    'breaker_id', 'title', 'lesson', 'plan_ids', 'event_seq')) return false
  const bounded = (value, max) => typeof value === 'string' && value.length > 0
    && value.trim() === value && !/[\x00-\x1f\x7f-\x9f]/u.test(value)
    && new TextEncoder().encode(value).byteLength <= max
  return record.kind === 'operator_route_lesson' && record.advisory_only === true
    && record.trust_level === 'L0' && typeof record.adr_id === 'string'
    && /^ADR-PIVOT-[0-9a-f]{64}$/.test(record.adr_id) && record.lesson_id === 'GRV-PIVOT-' + record.adr_id.slice(10)
    && bounded(record.breaker_id, 100) && bounded(record.title, 120) && bounded(record.lesson, 512)
    && isCount(record.event_seq) && record.event_seq > 0
    && Array.isArray(record.plan_ids) && record.plan_ids.length >= 1 && record.plan_ids.length <= 8
    && Array.from(record.plan_ids).every(id => bounded(id, 256)) && new Set(record.plan_ids).size === record.plan_ids.length
}

function RouteLessons({ entries, truncated }) {
  if (entries.length === 0 && !truncated) return null
  return h(Section, { title: 'Operator route lessons' },
    note('Historical warnings only. These lessons are not proofs or mathematical refutations.'),
    ...entries.map(item => h('article', { key: item.lesson_id, className: 'ai4m-card' },
      h('h4', null, item.title), badge('L0 · advisory only'), h('p', null, item.lesson),
      labelValue('Lesson', h('code', null, item.lesson_id)), labelValue('Approved ADR', h('code', null, item.adr_id)),
      labelValue('Archived routes', item.plan_ids.join(', ')))),
    truncated && note('Only a bounded route lesson slice is shown.'))
}

function Wiki({ remote }) {
  const query = useQuery(remote)
  const provenance = useQuery(remote)
  const [provenanceTarget, setProvenanceTarget] = useState(null)
  const [input, setInput] = useState('')
  const [scope, setScope] = useState('core')
  const [searched, setSearched] = useState(null)
  const busy = query.phase === 'loading'
  const term = input.trim()
  const invalid = new TextEncoder().encode(term).byteLength > 512 || term.includes('\0')
  const shadow = query.value?.shadow_hint
  const hits = rows(query.value?.hits, 32)
  const lessonRows = searched?.stratum === 'graveyard' ? rows(query.value?.route_lessons, 16).filter(validRouteLesson) : []
  const routeLessonsTruncated = searched?.stratum === 'graveyard' && query.value?.route_lessons_truncated === true
  const submit = (term, stratum) => {
    if (!term || busy || new TextEncoder().encode(term).byteLength > 512 || term.includes('\0')) return
    setSearched({ term, stratum })
    setProvenanceTarget(null)
    void query.run('wikiSearch', { text: term, scope: stratum, limit: 24 })
  }
  const inspectProvenance = (id, derived) => {
    setProvenanceTarget({ id, derived })
    void provenance.run('wikiProvenance', { id, max_depth: 2, limit: 24 })
  }
  return h(Panel, { title: 'Math Wiki', subtitle: 'Search CORE first; inspect SEDIMENT and GRAVEYARD as separate strata.' },
    h('form', { className: 'ai4m-actions', onSubmit: event => { event.preventDefault(); submit(term, scope) } },
      h('label', null, 'Query ', h('input', { type: 'search', value: input, maxLength: 512,
        onChange: event => setInput(event.target.value), required: true })),
      h('label', null, 'Scope ', h('select', { value: scope, onChange: event => setScope(event.target.value) },
        WIKI_SCOPES.map(value => h('option', { key: value, value }, value.toUpperCase())))),
      h('button', { className: 'ai4m-button', type: 'submit', disabled: busy || !term || invalid }, 'Search')),
    invalid && note('Queries must be at most 512 UTF-8 bytes and cannot contain a NUL character.', 'status'),
    queryNote(query, 'Enter a title, mathematical phrase, or identifier.'),
    query.value && searched && note('Results for “' + searched.term + '” · ' + searched.stratum.toUpperCase()),
    query.value && searched?.stratum === 'core' && wikiCoverageNote(query.value, 'search'),
    query.value && searched?.stratum === 'graveyard' && note('Graveyard matches are historical warnings, not verified evidence.'),
    provenanceTarget && h(WikiProvenance, { target: provenanceTarget, query: provenance }),
    query.value && (hits.length > 0 ? h('div', { className: 'ai4m-stack' },
      hits.map((hit, index) => h(WikiCard, { key: text(hit.id, 'hit') + ':' + index, hit, inspectProvenance })))
      : (lessonRows.length === 0 && !routeLessonsTruncated && empty('No matching records returned.'))),
    h(RouteLessons, { entries: lessonRows, truncated: routeLessonsTruncated }),
    query.value?.truncated === true && note('Only a bounded search slice is shown.'),
    shadow && searched && h('details', null,
      h('summary', null, 'Shadow hint · SEDIMENT ' + count(shadow.sediment_count) + ' / GRAVEYARD ' + count(shadow.graveyard_count)),
      shadow.top_sediment && note('Closest sediment lead: ' + text(shadow.top_sediment.title, text(shadow.top_sediment.id))),
      shadow.top_graveyard_id && note('Graveyard warning: ' + text(shadow.top_graveyard_id) + ' is historical, not verified evidence.'),
      h('div', { className: 'ai4m-actions' },
        ...['sediment', 'graveyard'].map(stratum => h('span', { key: stratum }, button('Search ' + stratum.toUpperCase(), () => {
          setInput(searched.term); setScope(stratum); submit(searched.term, stratum)
        }, busy || !(shadow[stratum + '_count'] > 0)))))))
}

module.exports = { LiveHud, panels, views: { briefing: Briefing, frontier: Frontier, inbox: Inbox, wiki: Wiki },
  MathCard, ClaimCard, InboxCard, WikiCard, EvidenceText }
