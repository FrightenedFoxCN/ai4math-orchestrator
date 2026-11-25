const { createElement: h, useCallback, useEffect, useRef, useState, useSyncExternalStore } = require('react')
const { call } = require('./remote.cjs')
const { REMOTE_METHODS, WIKI_SCOPES } = require('./contract')
const { DEFAULT_I18N, useI18n } = require('./i18n.cjs')
const { buildBreakerCommands, buildInboxCommands, CliHelper } = require('./cli-helper.cjs')
const { resolveNoticeText } = require('./workbench-store.cjs')

const EMPTY_STORE = Object.freeze({
  subscribe: () => () => {},
  getSnapshot: () => null,
  activeProject: () => null,
  mismatchedProject: () => null,
})

const TRUST = ['L0', 'L1', 'L2a', 'L2b', 'L3', 'L4a', 'L4b', 'L5']
const PRIORITIES = ['P0', 'P1', 'P2']
const text = (value, otherwise) => typeof value === 'string' && value.length > 0 ? value : otherwise
const isCount = value => Number.isSafeInteger(value) && value >= 0
const countOf = (value, na) => isCount(value) ? String(value) : na
const numericOf = (value, na) => typeof value === 'number' && Number.isFinite(value) ? String(value) : na
const rows = (value, max = 80) => Array.isArray(value)
  ? value.slice(0, max).filter(item => item && typeof item === 'object' && !Array.isArray(item)) : []
const pctOf = (value, na) => typeof value === 'number' && Number.isFinite(value) && value >= 0 && value <= 100
  ? Math.round(value) + '%' : na
const trustTone = level => level === 'L5' ? 'good' : level === 'L2b' ? 'danger' : (level === 'L4a' || level === 'L4b') ? 'brand' : undefined
const badge = (label, tone) => h('span', { className: 'ai4m-badge', 'data-tone': tone }, label)
const labelValue = (label, value, na = 'n/a') => h('span', { className: 'ai4m-kv' },
  h('span', { className: 'ai4m-muted' }, label + ' '),
  value === na ? h('span', { className: 'ai4m-muted' }, value) : value)
const button = (label, onClick, disabled, variant) => h('button', {
  className: 'ai4m-button', type: 'button', onClick, disabled, ...(variant ? { 'data-variant': variant } : {}),
}, label)
const note = (value, role) => h('p', { className: 'ai4m-muted', role }, value)
const banner = (value, tone, role) => h('p', { className: 'ai4m-banner', ...(tone ? { 'data-tone': tone } : {}), role }, value)
const empty = (value, t = DEFAULT_I18N.t) => h('p', { className: 'ai4m-empty' }, value ?? t('common.noRecords'))
const panels = Object.freeze([
  { key: 'briefing', labelKey: 'panel.briefing.label', descKey: 'panel.briefing.desc', label: 'Briefing', description: 'Preview or retain a deterministic fact pack.' },
  { key: 'frontier', labelKey: 'panel.frontier.label', descKey: 'panel.frontier.desc', label: 'Frontier', description: 'Inspect active claims, dependency plans, and breaker signals.' },
  { key: 'inbox', labelKey: 'panel.inbox.label', descKey: 'panel.inbox.desc', label: 'Inbox', description: 'Review prioritized decisions awaiting human attention.' },
  { key: 'wiki', labelKey: 'panel.wiki.label', descKey: 'panel.wiki.desc', label: 'Wiki', description: 'Search CORE and inspect archaeology hints and provenance.' },
])

function trustTape(trust, t = DEFAULT_I18N.t) {
  const na = t('common.na')
  return h('span', { className: 'ai4m-tape', 'aria-label': t('hud.trustAria') }, TRUST.map(level => {
    const value = trust?.[level]
    const nonzero = isCount(value) && value > 0
    return h('span', { key: level, className: 'ai4m-tier', 'data-level': level, 'data-nonzero': nonzero ? 'true' : 'false' },
      labelValue(level, countOf(value, na), na))
  }))
}

function AlertCatchUp({ live, state, t = DEFAULT_I18N.t }) {
  if (!state.alertHistoryIncomplete) return null
  return h('div', { className: 'ai4m-hud-catchup' },
    note(state.alertHistoryLoading ? t('catchup.loading') : state.phase === 'live'
      ? t('catchup.incomplete') : t('catchup.waiting'), 'status'),
    button(t('catchup.button'), live.continueAlertCatchUp, state.phase !== 'live' || state.alertHistoryLoading))
}

function BreakerTotals({ snapshot, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const tripped = snapshot?.tripped_breakers
  const halfOpen = snapshot?.half_open_breakers
  const archived = snapshot?.archived_breakers
  return h('span', { className: 'ai4m-meta', role: 'status' },
    badge(t('breaker.tripped', { n: countOf(tripped, na) }), isCount(tripped) && tripped > 0 ? 'danger' : 'muted'),
    isCount(halfOpen) && badge(halfOpen > 0 ? t('breaker.halfOpenFrozen', { n: halfOpen }) : t('breaker.halfOpen', { n: halfOpen }),
      halfOpen > 0 ? 'warn' : 'muted'),
    isCount(archived) && archived > 0 && badge(t('breaker.archivedGated', { n: archived }), 'warn'))
}

function InfraRfcTotals({ snapshot, t = DEFAULT_I18N.t }) {
  const {
    event_seq: seq,
    infra_rfc_drafts: drafts,
    infra_rfc_approved: approved,
    infra_rfc_building_smoke: smoke,
    infra_rfc_frozen_proving: frozen,
    infra_rfc_completed: completed,
  } = snapshot ?? {}
  if (!isCount(seq) || !isCount(drafts) || !isCount(approved) || drafts > seq || approved > seq - drafts) return null
  const hasLifecycle = smoke !== undefined || frozen !== undefined || completed !== undefined
  if (!hasLifecycle) {
    return badge(t('rfc.reviewOnly', { drafts, approved }), drafts + approved === 0 ? 'muted' : undefined)
  }
  if (!isCount(smoke) || !isCount(frozen) || !isCount(completed)) return null
  const total = drafts + approved + smoke + frozen + completed
  if (!Number.isSafeInteger(total) || total > seq) return null
  return badge(t('rfc.lifecycle', { drafts, approved, smoke, frozen, completed }), total === 0 ? 'muted' : undefined)
}

function DiscoveryBadge({ value, currentSeq, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  if (!value || typeof value !== 'object') return null
  if (value.status === 'starting') return labelValue(t('t3.label'), t('t3.waiting'), na)
  if (value.status === 'unavailable') return labelValue(t('t3.label'), t('t3.unavailable'), na)
  const hash = value.observed_head
  const claimTier = value.baseline_scope === 'claim_t1' ? 'T1' : value.baseline_scope === 'claim_t3' ? 'T3' : null
  if (Object.hasOwn(value, 'baseline_scope') && !claimTier) return null
  if (value.advisory_only !== true || !isCount(value.ledger_event_seq)
    || typeof hash !== 'string' || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(hash)) return null
  let summary
  switch (value.status) {
    case 'unchanged': summary = t('t3.unchanged'); break
    case 'diverged': summary = t('t3.diverged'); break
    case 'structural_change':
      if (!isCount(value.changed_module_count) || value.changed_module_count > 255
        || typeof value.module_order_changed !== 'boolean') return null
      summary = t('t3.changed', { count: value.changed_module_count })
        + (value.module_order_changed ? t('t3.orderChanged') : '')
      break
    default: return null
  }
  const scopeNote = claimTier ? t('t3.scopeNote') : ''
  return h('span', { title: t('t3.title', { hash, scope: scopeNote }) },
    labelValue(t('t3.lastLabel'), summary + (claimTier ? t('t3.claimBaseline', { tier: claimTier }) : '') + t('t3.advisory')
      + (isCount(currentSeq) && value.ledger_event_seq < currentSeq ? t('t3.olderSample') : ''), na))
}

const T2_AUDIT_STATUS_KEYS = {
  starting: 't2.starting',
  running: 't2.running',
  unavailable: 't2.unavailable',
  waiting: 't2.waiting',
  head_changed: 't2.headChanged',
}

function hasExactOwnKeys(value, ...keys) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    && Reflect.ownKeys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key))
}

function T2AuditBadge({ value, currentSeq, t = DEFAULT_I18N.t }) {
  if (!isCount(currentSeq)) return null
  const hasObservation = hasExactOwnKeys(value, 'status', 'attempts', 'exhausted', 'observation')
  if (!hasObservation && !hasExactOwnKeys(value, 'status', 'attempts', 'exhausted')) return null
  const { status, attempts, exhausted } = value
  if (hasObservation !== (status === 'observation') || !isCount(attempts) || attempts > 64
    || typeof exhausted !== 'boolean' || (exhausted && attempts === 0)
    || (status === 'starting' && attempts !== 0)
    || (status === 'running' && (attempts === 0 || exhausted))) return null

  const attemptsText = t(attempts === 1 ? 't2.attemptOne' : 't2.attemptMany', { n: attempts })
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

    title = t('t2.titleBase', { oid })
    if (heartbeats) {
      const { regressed, max_command_regressed: max, total_commands_regressed: total } = sample
      if (typeof max !== 'boolean' || typeof total !== 'boolean' || regressed !== (max || total)) return null
      title += t('t2.titleHb')
      const reasons = [max ? t('t2.hbMax') : '', total ? t('t2.hbTotal') : ''].filter(Boolean).join(', ')
      summary = (regressed ? t('t2.hbMet', { reasons }) : t('t2.hbNotMet')) + ' · ' + attemptsText
    } else {
      const { compile_ms: ms, module_count: modules, debt_findings: debt, independent_checks: checks } = sample
      if ((!independent && schema !== 'ai4math.t2.resident-sample.v1')
        || (independent && (debt !== 0 || !isCount(checks) || checks < 1 || checks > 8))
        || !isCount(ms) || ms > 120000 || !isCount(modules) || modules < 1 || modules > 128
        || !isCount(debt) || debt > 3072) return null
      if (independent) title += t('t2.titleInd')
      summary = t('t2.compile', { ms }) + ' · ' + t(modules === 1 ? 't2.modOne' : 't2.modMany', { n: modules })
        + ' · ' + t(debt === 1 ? 't2.debtOne' : 't2.debtMany', { n: debt })
        + (independent ? ' · ' + t(checks === 1 ? 't2.indOne' : 't2.indMany', { n: checks }) : '')
        + ' · ' + attemptsText
    }
    if (sampleSeq < currentSeq) summary = t('t2.older')
    else if (sampleSeq > currentSeq) summary = t('t2.future')
  } else {
    if (typeof status !== 'string' || !Object.hasOwn(T2_AUDIT_STATUS_KEYS, status)) return null
    summary = t(T2_AUDIT_STATUS_KEYS[status]) + ' · ' + attemptsText
  }
  return h('span', { className: 'ai4m-badge', title },
    t('t2.prefix') + summary + (exhausted ? t('t2.exhausted') : ''))
}

function leanAdvisoryBadge(broker, t = DEFAULT_I18N.t) {
  const { lean_advisory_busy: busy, lean_advisory_capacity: capacity, fast_busy: fastBusy, fast_capacity: fastCapacity } = broker ?? {}
  if (!isCount(busy) || !isCount(capacity) || capacity < 1 || capacity > 4
    || !isCount(fastCapacity) || capacity > fastCapacity
    || !isCount(fastBusy) || busy > fastBusy || busy > capacity) return null
  return badge(t('hud.leanAdvisory', { busy, cap: capacity }))
}

function LiveHud({ live, open, wbStore, openWorkbench, i18n }) {
  const [navigationError, setNavigationError] = useState(false)
  const { t } = useI18n(i18n)
  const na = t('common.na')
  const state = useSyncExternalStore(live.subscribe, live.getSnapshot)
  const store = wbStore ?? EMPTY_STORE
  const wbSnap = useSyncExternalStore(store.subscribe, store.getSnapshot)
  const activeProject = store.activeProject(wbSnap?.data)
  const mismatched = store.mismatchedProject(wbSnap)
  const wbError = wbSnap?.notice?.tone === 'danger' ? resolveNoticeText(wbSnap.notice, t) : null
  const { phase, snapshot, snapshotStale, recentAlerts, breaker } = state
  const snapshotFresh = phase === 'live' && !snapshotStale
  const latestBreaker = phase === 'live' && readBreakerEvent(breaker, t)
  const alert = recentAlerts[0]
  const alertText = text(alert?.headline, '')
  const memory = snapshot?.memory
  const pressure = typeof memory?.pressure === 'string' ? memory.pressure : undefined
  const broker = snapshot?.broker
  const advisoryBadge = snapshotFresh ? leanAdvisoryBadge(broker, t) : null
  const workers = snapshot?.workers
  const staleClaims = snapshot?.stale_claims
  const oracleHeld = snapshot?.task_scheduler?.exploration?.held_for_oracle
  const brokerText = !broker ? na
    : t('hud.brokerSummary', {
        fastBusy: countOf(broker.fast_busy, na),
        fastCap: countOf(broker.fast_capacity, na),
        heavyRun: countOf(broker.heavy_running, na),
        heavyQueue: countOf(broker.heavy_queued, na),
      })
  const phaseLabel = phase === 'live' ? (snapshotStale ? t('hud.connectedStale') : t('hud.live'))
    : phase === 'reconnecting' ? (snapshot ? t('hud.reconnectingStale') : t('hud.reconnecting')) : t('hud.connecting')
  return h('section', { className: 'ai4m ai4m-hud', 'aria-label': t('hud.aria') },
    h('div', { className: 'ai4m-hud-line' },
      h('div', { className: 'ai4m-hud-brand' },
        openWorkbench
          ? h('button', {
              type: 'button', className: 'ai4m-hud-brand-btn',
              title: activeProject ? t('hud.openWorkbenchProjectTitle', { root: activeProject.root }) : t('hud.openWorkbenchTitle'),
              onClick: () => openWorkbench(),
            }, 'AI4Math')
          : h('strong', null, 'AI4Math'),
        activeProject && badge(activeProject.name, 'brand'),
        h('span', { className: 'ai4m-hud-status', role: 'status' }, badge(phaseLabel, snapshotFresh ? 'good' : 'warn'))),
      h('div', { className: 'ai4m-hud-telemetry' },
        h('div', { className: 'ai4m-hud-group' },
          h('span', { className: 'ai4m-hud-metric', 'data-pressure': pressure },
            labelValue(t('hud.ram'), pctOf(memory?.used_percent, na) + (pressure ? ' · ' + pressure : ''), na)),
          h('span', { title: t('hud.brokerTitle') },
            labelValue(t('hud.broker'), brokerText, na), advisoryBadge && ' ', advisoryBadge),
          labelValue(t('hud.workers'), countOf(typeof workers === 'number' ? workers : workers?.active, na), na)),
        h('div', { className: 'ai4m-hud-group' },
          trustTape(snapshot?.trust, t)),
        h('div', { className: 'ai4m-hud-group' },
          h('span', { className: 'ai4m-hud-metric', 'data-nonzero': isCount(staleClaims) && staleClaims > 0 ? 'true' : 'false' },
            labelValue(t('hud.stale'), countOf(staleClaims, na), na)),
          snapshotFresh && h(T2AuditBadge, { value: snapshot?.t2_audit, currentSeq: snapshot?.event_seq, t }),
          snapshotFresh && h(DiscoveryBadge, { value: snapshot?.t3_discovery, currentSeq: snapshot?.event_seq, t }),
          h(BreakerTotals, { snapshot, t }),
          snapshotFresh && h(InfraRfcTotals, { snapshot, t }),
          snapshotFresh && isCount(oracleHeld) && oracleHeld > 0 && h('span', {
            className: 'ai4m-hud-metric', 'data-nonzero': 'true',
            title: t('hud.oracleHeldTitle'),
          }, labelValue(t('hud.oracleHeld'), countOf(oracleHeld, na), na)))),
      h('div', { className: 'ai4m-hud-pinned' },
        ...PRIORITIES.map(priority => {
          const value = snapshot?.pending_inbox?.[priority]
          const active = isCount(value) && value > 0
          return h('span', { key: priority, className: 'ai4m-hud-priority', 'data-zero': active ? 'false' : 'true' },
            badge(priority + ' ' + countOf(value, na),
              active ? (priority === 'P0' ? 'danger' : priority === 'P1' ? 'warn' : 'brand') : 'muted'))
        }),
        h('span', { className: 'ai4m-hud-seq', title: text(snapshot?.observed_at, '') }, labelValue(t('hud.seq'), countOf(snapshot?.event_seq, na), na))),
      h('nav', { className: 'ai4m-hud-nav', 'aria-label': t('hud.panelsAria') }, panels.map(panel =>
        h('button', { key: panel.key, type: 'button', className: 'ai4m-button', title: t(panel.descKey), onClick: event => {
          setNavigationError(!open(panel.key, event.currentTarget))
        } }, t(panel.labelKey))))),
    navigationError && banner(t('hud.openSessionFirst'), 'warn', 'status'),
    mismatched && h('div', { className: 'ai4m-hud-notice', role: 'status' },
      h('span', null, t('hud.mismatchBanner', { name: mismatched.name })),
      button(wbSnap?.busy ? t('hud.switching') : t('hud.switchProject'),
        () => void store.projectSelect?.({ project_id: mismatched.id, auto_start: true }), wbSnap?.busy, 'primary')),
    wbError && h('div', { className: 'ai4m-hud-alert', role: 'alert' },
      h('span', null, wbError),
      button(t('common.dismiss'), () => store.clearNotice?.())),
    h(AlertCatchUp, { live, state, t }),
    h('div', { role: 'status', 'aria-atomic': true }, (latestBreaker?.probe || latestBreaker?.state === 'ARCHIVED') &&
      h('div', { className: 'ai4m-hud-alert ai4m-hud-alert--breaker' },
        t('hud.latestBreaker') + ' · ', h('code', null, text(breaker.entity_id, na)), ' · #' + countOf(breaker.event_seq, na) + ' ',
        h(BreakerReading, { reading: latestBreaker, t }))),
    alertText && h('div', { className: 'ai4m-hud-alert' },
      badge(t('hud.lastP0'), 'danger'), ' ', alertText,
      alert.briefing_id && h('span', null, ' · ', h('code', null, text(alert.briefing_id, na)))))
}

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

function queryNote(query, fallback, t = DEFAULT_I18N.t) {
  if (query.phase === 'loading') return banner(t('common.loading'), 'loading', 'status')
  if (query.phase === 'error') return banner(t('common.unavailable'), 'danger', 'alert')
  if (query.phase === 'idle') return fallback ? empty(fallback, t) : null
  return null
}

function Panel({ title, subtitle, actions, children }) {
  return h('section', { className: 'ai4m ai4m-panel' },
    h('header', null,
      h('h2', null, title), subtitle && note(subtitle),
      actions && h('div', { className: 'ai4m-actions' }, actions)),
    children)
}

function MathCard({ informal, formal, target, t = DEFAULT_I18N.t }) {
  const missing = t('common.notSupplied')
  const informalText = text(informal, missing)
  const formalText = text(formal, missing)
  return h('div', { className: 'ai4m-math' },
    h('div', { className: 'ai4m-math-informal' },
      h('strong', null, t('math.informal')),
      h('p', { className: informalText === missing ? 'ai4m-placeholder' : undefined }, informalText)),
    h('div', { className: 'ai4m-math-formal' },
      h('strong', null, t('math.formal')),
      h('pre', { className: formalText === missing ? 'ai4m-placeholder' : undefined }, formalText),
      text(target, '') && h('p', { className: 'ai4m-math-target' }, labelValue(t('math.target'), h('code', null, target), t('common.na')))))
}

function ClaimCard({ claim, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const accent = claim.stale === true || claim.oracle_debt === true ? 'warn'
    : claim.effective_trust === 'L2b' ? 'danger'
    : claim.effective_trust === 'L5' ? 'good' : undefined
  return h('article', { className: 'ai4m-card', 'data-accent': accent },
    h('h4', null, text(claim.title, text(claim.claim_id, t('claim.unnamed')))),
    h('div', { className: 'ai4m-meta' },
      h('code', null, text(claim.claim_id, na)), badge(text(claim.engine, na), 'muted'),
      badge(t('claim.effective', { trust: text(claim.effective_trust, na) }), trustTone(claim.effective_trust)),
      badge(t('claim.local', { trust: text(claim.local_trust, na) }), trustTone(claim.local_trust)),
      claim.critical === true && badge(t('claim.critical'), 'brand'),
      claim.stale === true && badge(t('claim.stale'), 'warn'),
      claim.oracle_debt === true && badge(t('claim.oracleDebt'), 'warn')),
    h(MathCard, { informal: claim.informal_statement, formal: claim.formal_signature, target: claim.formal_target, t }),
    typeof claim.wiki_id === 'string' && h('div', { className: 'ai4m-card-foot' }, labelValue(t('claim.wikiRef'), h('code', null, claim.wiki_id), na)))
}

function InboxCard({ item, projectRoot, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const accent = item.priority === 'P0' ? 'danger' : item.priority === 'P1' ? 'warn' : undefined
  return h('article', { className: 'ai4m-card', 'data-accent': accent },
    h('h4', null, text(item.summary, t('inbox.noSummary'))),
    h('div', { className: 'ai4m-meta' },
      badge(text(item.priority, na), item.priority === 'P0' ? 'danger' : item.priority === 'P1' ? 'warn' : 'muted'),
      badge(text(item.category, na)), h('code', null, text(item.inbox_id, na)),
      labelValue(t('inbox.leverage'), numericOf(item.leverage_score, na), na),
      h('span', { className: 'ai4m-muted' }, text(item.created_at, na))),
    item.payload != null && h(Evidence, { title: t('inbox.evidence'), value: item.payload }),
    item.payload_truncated === true && note(t('inbox.truncated')),
    projectRoot && h(CliHelper, { commands: buildInboxCommands(item, projectRoot, t), t }))
}

function Evidence({ title, value, open }) {
  return h('details', { open }, h('summary', null, title), h(EvidenceText, { value }))
}

function EvidenceText({ value }) {
  return h('pre', { className: 'ai4m-evidence' }, JSON.stringify(value, null, 2)?.slice(0, 8000))
}

function Section({ title, className, children }) {
  return h('section', { className: className ? 'ai4m-section ' + className : 'ai4m-section' }, h('h3', null, title), children)
}

function Cards({ page, itemKey, card, emptyText, t = DEFAULT_I18N.t }) {
  const items = rows(page?.items)
  return h('div', { className: 'ai4m-stack' },
    items.length ? items.map((item, index) => card(item, text(item[itemKey], 'item') + ':' + index)) : empty(emptyText, t),
    page?.truncated === true && note(t('common.boundedSlice')))
}

function PlanSlice({ page, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const edges = rows(page?.items)
  return h(Section, { title: t('plan.title') }, edges.length > 0 ? h('ul', { className: 'ai4m-edges' }, edges.map((edge, index) =>
    h('li', { key: index }, h('code', null, text(edge.target_claim_id, na)), ' · ',
      h('code', null, text(edge.plan_id, na)), ' ', badge(text(edge.plan_status, na)),
      typeof edge.dep_claim_id === 'string' && h('span', null, ' → ', h('code', null, edge.dep_claim_id), ' ', text(edge.dep_kind, '')))))
    : empty(page ? t('plan.empty') : t('plan.noData'), t),
  page?.truncated === true && note(t('plan.truncated')))
}

function readBreaker(item, t = DEFAULT_I18N.t) {
  const na = t('common.na')
  const state = text(item?.state, text(item?.to_state, na))
  const status = item?.probe_status
  const passed = item?.probe_passed
  const required = item?.probe_required
  const countsAbsent = passed == null && required == null
  const validCounts = isCount(passed) && isCount(required) && required > 0 && passed <= required
  const halfOpen = state === 'HALF_OPEN'
  let probe, tone
  if (halfOpen || state === 'TRIPPED') {
    switch (status) {
      case 'AWAITING_REVIEW': probe = t('breaker.awaitingReview'); tone = 'danger'; break
      case 'NEEDS_PROBE': probe = t('breaker.needsProbe'); tone = 'danger'; break
      case 'FAILED': probe = t('breaker.failed'); tone = 'danger'; break
      case 'READY': if (halfOpen) { probe = t('breaker.ready'); tone = 'warn' } break
      case 'RUNNING': if (halfOpen) { probe = t('breaker.running'); tone = 'warn' } break
    }
  }
  if (status === 'PASSED' && state === 'CLOSED' &&
    (countsAbsent || (validCounts && passed === required))) {
    probe = t('breaker.verified')
    tone = 'good'
  }
  const progress = probe && validCounts ? t('breaker.progress', { passed, required }) : null
  if (!probe && (halfOpen || (status === 'PASSED' && state !== 'ARCHIVED'))) {
    probe = t('breaker.unavailable')
    tone = 'warn'
  }
  return { state, probe, tone, halfOpen, progress }
}

function readBreakerEvent(event, t = DEFAULT_I18N.t) {
  if (!event?.payload || event.payload_truncated === true) return null
  const reading = readBreaker(event.payload, t)
  return reading.state === t('common.na') ? null : reading
}

function BreakerReading({ reading, t = DEFAULT_I18N.t }) {
  const { state, probe, tone, halfOpen, progress } = reading
  return h('span', { className: 'ai4m-meta' },
    badge(state, state === 'TRIPPED' ? 'danger' : (halfOpen || state === 'ARCHIVED') ? 'warn' : state === 'CLOSED' ? 'good' : undefined),
    probe && badge(probe, tone),
    progress && h('span', null, progress),
    halfOpen && h('span', { className: 'ai4m-muted' }, t('breaker.queueFrozen')),
    state === 'ARCHIVED' && h('span', { className: 'ai4m-muted' }, t('breaker.queueGated')))
}

function BreakerCard({ item, projectRoot, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const reading = readBreaker(item, t)
  const accent = reading.state === 'TRIPPED' ? 'danger'
    : (reading.halfOpen || reading.state === 'ARCHIVED') ? 'warn'
    : reading.state === 'CLOSED' ? 'good' : undefined
  return h('article', { className: 'ai4m-card', 'data-accent': accent },
    h('h4', null, text(item.target_definition_id, text(item.breaker_id, t('breaker.unnamed')))),
    h('div', { className: 'ai4m-meta' }, h(BreakerReading, { reading, t }),
      h('code', null, text(item.module_path, na)), text(item.engine, na), text(item.updated_at, '')),
    projectRoot && h(CliHelper, { commands: buildBreakerCommands(item, projectRoot, t), t }))
}

function BreakerSignal({ event, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const reading = readBreakerEvent(event, t)
  return h('article', { className: 'ai4m-card', 'data-accent': 'warn' },
    h('h4', null, t('hud.latestBreaker')),
    h('div', { className: 'ai4m-meta', role: 'status' }, badge(text(event.event_type, na), 'warn'),
      h('code', null, text(event.entity_id, na)), labelValue(t('hud.seq'), countOf(event.event_seq, na), na),
      reading && h(BreakerReading, { reading, t })),
    event.payload != null && h(Evidence, { title: t('breaker.transitionEvidence'), value: event.payload }))
}

function schedulerBlockSummary(event, t = DEFAULT_I18N.t) {
  if ((event.event_kind ?? event.event_type) !== 'ALL_TRACKS_BLOCKED') return ''
  const exploration = event.payload?.exploration_blocked
  const infrastructure = event.payload?.infrastructure_blocked
  if (!isCount(exploration) || !isCount(infrastructure) || exploration < 1 || infrastructure < 1
    || exploration + infrastructure > 4096) return ''
  return t('event.allTracksBlocked', { exp: exploration, infra: infrastructure })
}

function regressionAlertSummary(event, t = DEFAULT_I18N.t) {
  if ((event.event_kind ?? event.event_type) !== 'BROKER_REGRESSION_MELTDOWN') return ''
  const { failed_module: module, distinct_candidates: candidates, window_seconds: seconds } = event.payload ?? {}
  if (typeof module !== 'string' || !/^[A-Z][A-Za-z0-9_]{0,250}\.lean$/.test(module)
    || candidates !== 3 || seconds !== 900) return ''
  return t('event.meltdown', { module, candidates })
}

function criticalBreakerSummary(event, t = DEFAULT_I18N.t) {
  if ((event.event_kind ?? event.event_type) !== 'CRITICAL_BREAKER_TRIPPED') return ''
  const { critical_claim_id: first, critical_claim_count: affected } = event.payload ?? {}
  if (typeof first !== 'string' || !first.length || first.length > 256 || /[\u0000-\u001f\u007f]/.test(first)
    || !isCount(affected) || affected < 1) return ''
  return affected > 1 ? t('event.critBreakerMany', { first, affected }) : t('event.critBreakerOne', { first })
}

function EventList({ page, openEventSeq, emptyText, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const events = rows(page?.items)
  return h('div', { className: 'ai4m-stack' },
    events.length ? events.map((event, index) => h(Evidence, { key: index, value: event,
      open: openEventSeq != null && event.event_seq === openEventSeq ? true : undefined,
      title: [text(event.event_kind, text(event.event_type, t('event.default'))), text(event.entity_id, ''), '#' + countOf(event.event_seq, na),
        schedulerBlockSummary(event, t), regressionAlertSummary(event, t), criticalBreakerSummary(event, t)]
        .filter(Boolean).join(' · ') })) : empty(emptyText ?? t('event.empty'), t),
    page?.truncated === true && note(t('event.bounded')))
}

function Briefing({ remote, live, projectRoot, wbStore, i18n }) {
  const { t } = useI18n(i18n)
  const na = t('common.na')
  const state = useSyncExternalStore(live.subscribe, live.getSnapshot)
  const store = wbStore ?? EMPTY_STORE
  const wbSnap = useSyncExternalStore(store.subscribe, store.getSnapshot)
  const resolvedRoot = projectRoot ?? store.activeProject(wbSnap?.data)?.root ?? null
  const { recentAlerts, alertHistoryIncomplete } = state
  const query = useQuery(remote)
  const pack = query.value?.fact_pack
  const checkpoint = pack?.checkpoint
  const hasCheckpoint = checkpoint != null && typeof checkpoint === 'object' && !Array.isArray(checkpoint)
  const busy = query.phase === 'loading'
  const modeLabel = query.method === 'briefingPeek' ? t('briefing.modePreview')
    : query.method === 'briefingGet' ? t('briefing.modeRetained') : t('briefing.modePull')
  const modeTone = query.method === 'briefingGet' ? 'danger' : query.method === 'briefingPull' ? 'brand' : 'muted'
  return h(Panel, { title: t('briefing.title'), subtitle: t('briefing.subtitle'),
    actions: [h('span', { key: 'peek' }, button(t('briefing.peek'), () => void query.run('briefingPeek', {}), busy)),
      h('span', { key: 'pull' }, button(t('briefing.pull'), () => void query.run('briefingPull', {}), busy, 'primary'))] },
    queryNote(query, t('briefing.idle'), t),
    query.phase === 'done' && !pack && empty(t('briefing.noPack'), t),
    (recentAlerts.length > 0 || alertHistoryIncomplete) && h(Section, { title: t('briefing.recentP0') },
      h(AlertCatchUp, { live, state, t }),
      recentAlerts.length > 0 && h('ul', { className: 'ai4m-edges ai4m-edges--alerts' }, recentAlerts.map(alert =>
        h('li', { key: alert.event_seq },
          h('span', null, '#' + countOf(alert.event_seq, na) + ' · ' + alert.headline),
          typeof alert.briefing_id === 'string' && /^[0-9a-f]{64}$/.test(alert.briefing_id)
            && button(t('briefing.openRetained'), () => void query.run('briefingGet', { id: alert.briefing_id }, alert.event_seq), busy))))),
    pack && h('div', { className: 'ai4m-stack' },
      h('div', { className: 'ai4m-meta-box' },
        badge(modeLabel, modeTone),
        labelValue(t('briefing.window'), '(' + countOf(pack.event_from_exclusive, na) + ', ' + countOf(pack.event_to_inclusive, na) + ']', na),
        query.value.retained_id != null && labelValue(t('briefing.retained'), h('code', null, text(query.value.retained_id, na)), na),
        hasCheckpoint && labelValue(t('briefing.checkpointSeq'), countOf(checkpoint.event_seq, na), na),
        hasCheckpoint && labelValue(t('briefing.checkpointAdr'), h('code', null, text(checkpoint.last_adr_id, na)), na),
        labelValue(t('briefing.sha256'), h('code', null, text(pack.fact_sha256, na)), na)),
      query.method === 'briefingGet' && h(Section, { title: t('briefing.p0Sources') },
        h(EventList, { page: { items: pack.alert_sources }, openEventSeq: query.alertEventSeq,
          emptyText: t('briefing.noP0Sources'), t })),
      h(Section, { title: t('briefing.trustLedger') },
        trustTape(pack.trust, t),
        h('div', { className: 'ai4m-summary-row' },
          labelValue(t('hud.stale'), countOf(pack.stale_claims, na), na),
          pack.risk_radar && labelValue(t('claim.oracleDebt'), countOf(pack.risk_radar.oracle_debts, na), na),
          pack.risk_radar && labelValue(t('briefing.crossEngineDebt'), countOf(pack.risk_radar.cross_engine_debts, na), na))),
      h(Section, { title: t('briefing.milestonesRefutations') },
        ...[[t('briefing.milestones'), 'milestones', pack.milestones, 'good'], [t('briefing.refutations'), 'refutations', pack.refutations, 'danger']].map(([title, keyId, page, accent]) =>
          h('div', { key: keyId, className: 'ai4m-subgroup' }, h('h4', null, title), h(Cards, { page, itemKey: 'event_seq', t, card: (item, key) =>
            h('article', { key, className: 'ai4m-card', 'data-accent': accent },
              h('h4', null, text(item.claim?.title ?? item.claim_id, na)),
              h('div', { className: 'ai4m-meta' },
                item.new_local_trust && badge(item.new_local_trust, trustTone(item.new_local_trust)),
                h('span', { className: 'ai4m-muted' }, t('briefing.eventNum', { seq: countOf(item.event_seq, na) }))),
              h(MathCard, { informal: item.claim?.informal_statement, formal: item.claim?.formal_signature,
                target: item.claim?.formal_target, t }),
              item.graveyard_id && h('div', { className: 'ai4m-card-foot' }, labelValue(t('briefing.graveyardWitness'), h('code', null, text(item.graveyard_id, na)), na))) })))),
      h(Section, { title: t('briefing.frontierClaims') }, h(Cards, { page: pack.frontier, itemKey: 'claim_id', t,
        card: (claim, key) => h(ClaimCard, { claim, key, t }) })),
      h(PlanSlice, { page: pack.frontier?.plans, t }),
      h(Section, { title: t('briefing.recentEvents') }, h(EventList, { page: pack.events, t })),
      h(Section, { title: t('briefing.breakers') }, h(Cards, { page: pack.breakers, itemKey: 'breaker_id', t,
        card: (item, key) => h(BreakerCard, { item, key, projectRoot: resolvedRoot, t }) })),
      h(Section, { title: t('briefing.inbox') }, h(Cards, { page: pack.inbox, itemKey: 'inbox_id', t,
        card: (item, key) => h(InboxCard, { item, key, projectRoot: resolvedRoot, t }) }))))
}

function Frontier({ remote, live, projectRoot, wbStore, i18n }) {
  const { t } = useI18n(i18n)
  const query = useQuery(remote)
  const { phase, snapshot, snapshotStale, breaker } = useSyncExternalStore(live.subscribe, live.getSnapshot)
  const store = wbStore ?? EMPTY_STORE
  const wbSnap = useSyncExternalStore(store.subscribe, store.getSnapshot)
  const resolvedRoot = projectRoot ?? store.activeProject(wbSnap?.data)?.root ?? null
  const refresh = useCallback(() => void query.run('frontierList', { limit: 32 }), [query.run])
  useEffect(refresh, [refresh])
  return h(Panel, { title: t('frontier.title'), subtitle: t('frontier.subtitle'),
    actions: button(t('common.refresh'), refresh, query.phase === 'loading') },
    h(Section, { title: t('briefing.breakers') },
      h('div', { className: 'ai4m-summary-row' },
        h(BreakerTotals, { snapshot, t }),
        (phase !== 'live' || snapshotStale) && h('span', { className: 'ai4m-muted' }, t('frontier.staleSuffix'))),
      phase === 'live' && breaker && h(BreakerSignal, { event: breaker, t }),
      note(t('frontier.readOnlyNote'))),
    queryNote(query, t('frontier.waiting'), t),
    query.value && h(Section, { title: t('frontier.claims') }, h(Cards, { page: query.value, itemKey: 'claim_id', t,
      card: (claim, key) => h(ClaimCard, { claim, key, t }) })),
    query.value && h(PlanSlice, { page: query.value.plans, t }),
    resolvedRoot && breaker && h(CliHelper, { commands: buildBreakerCommands(breaker.payload ?? breaker, resolvedRoot, t), t }))
}

function Inbox({ remote, projectRoot, wbStore, i18n }) {
  const { t } = useI18n(i18n)
  const query = useQuery(remote)
  const store = wbStore ?? EMPTY_STORE
  const wbSnap = useSyncExternalStore(store.subscribe, store.getSnapshot)
  const resolvedRoot = projectRoot ?? store.activeProject(wbSnap?.data)?.root ?? null
  const refresh = useCallback(() => void query.run('inboxList', { limit: 32 }), [query.run])
  useEffect(refresh, [refresh])
  return h(Panel, { title: t('inbox.title'), subtitle: t('inbox.subtitle'),
    actions: button(t('common.refresh'), refresh, query.phase === 'loading') },
    queryNote(query, t('inbox.waiting'), t),
    query.value && h(Cards, { page: query.value, itemKey: 'inbox_id', emptyText: t('inbox.empty'), t,
      card: (item, key) => h(InboxCard, { item, key, projectRoot: resolvedRoot, t }) }))
}

const provenanceIdSchema = REMOTE_METHODS.wikiProvenance.params.properties.id
const validProvenanceId = id => typeof id === 'string'
  && new TextEncoder().encode(id).byteLength <= provenanceIdSchema['x-ai4math-maxUtf8Bytes']
  && new RegExp(provenanceIdSchema.pattern, 'u').test(id)

const managedCorePath = '.managed-core/'
const isDerivedCore = (id, stratum) => stratum === 'core'
  && typeof id === 'string' && /^CORE-ADAPT-[0-9a-f]{64}$/.test(id)

function wikiCoverageNote(value, surface, t = DEFAULT_I18N.t) {
  if (value.managed_core === true) return banner(surface === 'search' ? t('wiki.covSearchYes') : t('wiki.covProvYes'))
  if (value.managed_core !== false) return banner(surface === 'search' ? t('wiki.covUnknownSearch') : t('wiki.covUnknownProv'))
  return banner(surface === 'search' ? t('wiki.covStaticSearch') : t('wiki.covStaticProv'))
}

function WikiCard({ hit, inspectProvenance, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const hasPath = typeof hit.file_path === 'string'
  const virtual = hasPath && hit.file_path.startsWith(managedCorePath)
  const derived = virtual && isDerivedCore(hit.id, hit.stratum) && hit.file_path === managedCorePath + hit.id
  return h('article', { className: 'ai4m-card', 'data-accent': derived ? 'brand' : undefined },
    h('h4', null, text(hit.title, text(hit.id, t('wiki.untitled')))),
    h('div', { className: 'ai4m-meta' }, h('code', null, text(hit.id, na)), badge(text(hit.engine, na), 'muted'),
      badge(derived ? t('wiki.derivedCore') : text(hit.stratum, na), derived ? 'brand' : undefined),
      badge(derived ? t('wiki.indexedTrust', { trust: text(hit.trust_level, na) }) : text(hit.trust_level, na), trustTone(hit.trust_level)),
      badge(text(hit.status, na), 'muted')),
    h(MathCard, { informal: hit.informal_gloss, formal: hit.formal_signature, t }),
    (hasPath || (derived && typeof hit.verified_at_commit === 'string' && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(hit.verified_at_commit)))
      && h('div', { className: 'ai4m-card-foot' },
        hasPath && labelValue(virtual ? t('wiki.virtualLocation') : t('wiki.sourcePath'), h('code', null, hit.file_path), na),
        derived && typeof hit.verified_at_commit === 'string' && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(hit.verified_at_commit)
          && labelValue(t('wiki.verifiedCommit'), h('code', null, hit.verified_at_commit), na)),
    derived && banner(t('wiki.derivedNote')),
    validProvenanceId(hit.id) && h('div', { className: 'ai4m-actions' },
      button(t('wiki.inspectProv', { id: hit.id }), () => inspectProvenance(hit.id, derived))))
}

function WikiProvenance({ target, query, sectionRef, onClose, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  const edges = rows(query.value?.edges, 24)
  const hasDerived = target.derived || edges.some(edge => isDerivedCore(edge.src_id, edge.src_stratum)
    || isDerivedCore(edge.dst_id, edge.dst_stratum))
  const endpoint = (value, stratum) => h('span', null, h('code', null, text(value, na)), ' ',
    badge(isDerivedCore(value, stratum) ? t('wiki.derivedCore') : text(stratum, t('wiki.outsideIndex')),
      isDerivedCore(value, stratum) ? 'brand' : 'muted'))
  return h('section', { ref: sectionRef, className: 'ai4m-section ai4m-provenance' },
    h('div', { className: 'ai4m-provenance-head' },
      h('h3', null, t('wiki.provTitle', { id: target.id })),
      onClose && button(t('common.close'), onClose)),
    note(t('wiki.provNote')),
    hasDerived && banner(t('wiki.provDerivedNote')),
    queryNote(query, null, t),
    query.value && wikiCoverageNote(query.value, 'provenance response', t),
    query.phase === 'done' && (edges.length ? h('ol', { className: 'ai4m-edges' }, edges.map((edge, index) =>
      h('li', { key: index }, endpoint(edge.src_id, edge.src_stratum), ' → ', endpoint(edge.dst_id, edge.dst_stratum),
        ' · ', text(edge.edge_type, na), ' · ', labelValue(t('wiki.hops'), countOf(edge.depth, na), na),
        edge.metadata && Object.keys(edge.metadata).length > 0 && h(Evidence, { title: t('wiki.edgeMeta'), value: edge.metadata }))))
      : empty(t('wiki.noEdges'), t)),
    query.value?.truncated === true && note(t('wiki.boundedProv')))
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

function RouteLessons({ entries, truncated, t = DEFAULT_I18N.t }) {
  const na = t('common.na')
  if (entries.length === 0 && !truncated) return null
  return h(Section, { title: t('wiki.lessonsTitle') },
    banner(t('wiki.lessonsNote'), 'warn'),
    ...entries.map(item => h('article', { key: item.lesson_id, className: 'ai4m-card', 'data-accent': 'warn' },
      h('h4', null, item.title),
      h('div', { className: 'ai4m-meta' }, badge(t('wiki.lessonBadge'), 'warn')),
      h('p', null, item.lesson),
      h('div', { className: 'ai4m-card-foot' },
        labelValue(t('wiki.lessonLabel'), h('code', null, item.lesson_id), na),
        labelValue(t('wiki.approvedAdr'), h('code', null, item.adr_id), na),
        labelValue(t('wiki.archivedRoutes'), item.plan_ids.join(', '), na)))),
    truncated && note(t('wiki.boundedLessons')))
}

function Wiki({ remote, i18n }) {
  const { t } = useI18n(i18n)
  const na = t('common.na')
  const query = useQuery(remote)
  const provenance = useQuery(remote)
  const [provenanceTarget, setProvenanceTarget] = useState(null)
  const provenanceRef = useRef(null)
  const [input, setInput] = useState('')
  const [scope, setScope] = useState('core')
  const [searched, setSearched] = useState(null)
  useEffect(() => {
    if (provenanceTarget) provenanceRef.current?.scrollIntoView?.({ block: 'nearest', behavior: 'smooth' })
  }, [provenanceTarget])
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
  return h(Panel, { title: t('wiki.title'), subtitle: t('wiki.subtitle') },
    h('form', { className: 'ai4m-search-form', onSubmit: event => { event.preventDefault(); submit(term, scope) } },
      h('label', { className: 'ai4m-field ai4m-field--grow' }, t('wiki.query') + ' ', h('input', { type: 'search', value: input, maxLength: 512,
        placeholder: t('wiki.queryPlaceholder'),
        onChange: event => setInput(event.target.value), required: true })),
      h('label', { className: 'ai4m-field' }, t('wiki.scope') + ' ', h('select', { value: scope, onChange: event => setScope(event.target.value) },
        WIKI_SCOPES.map(value => h('option', { key: value, value }, value.toUpperCase())))),
      h('button', { className: 'ai4m-button', 'data-variant': 'primary', type: 'submit', disabled: busy || !term || invalid }, t('wiki.search'))),
    invalid && banner(t('wiki.invalidQuery'), 'warn', 'status'),
    queryNote(query, t('wiki.idle'), t),
    query.value && searched && h('p', { className: 'ai4m-results-head' }, t('wiki.resultsFor', { term: searched.term, stratum: searched.stratum.toUpperCase() })),
    query.value && searched?.stratum === 'core' && wikiCoverageNote(query.value, 'search', t),
    query.value && searched?.stratum === 'graveyard' && banner(t('wiki.graveyardWarn'), 'warn'),
    provenanceTarget && h(WikiProvenance, {
      target: provenanceTarget, query: provenance, sectionRef: provenanceRef, onClose: () => setProvenanceTarget(null), t,
    }),
    query.value && (hits.length > 0 ? h('div', { className: 'ai4m-stack' },
      hits.map((hit, index) => h(WikiCard, { key: text(hit.id, 'hit') + ':' + index, hit, inspectProvenance, t })))
      : (lessonRows.length === 0 && !routeLessonsTruncated && empty(t('wiki.noMatch'), t))),
    h(RouteLessons, { entries: lessonRows, truncated: routeLessonsTruncated, t }),
    query.value?.truncated === true && note(t('wiki.boundedSearch')),
    shadow && searched && h('details', null,
      h('summary', null, t('wiki.shadowSummary', { sed: countOf(shadow.sediment_count, na), grv: countOf(shadow.graveyard_count, na) })),
      h('div', { className: 'ai4m-shadow-body' },
        shadow.top_sediment && note(t('wiki.shadowSediment', { label: text(shadow.top_sediment.title, text(shadow.top_sediment.id, na)) })),
        shadow.top_graveyard_id && note(t('wiki.shadowGraveyard', { id: text(shadow.top_graveyard_id, na) })),
        h('div', { className: 'ai4m-actions' },
          ...['sediment', 'graveyard'].map(stratum => h('span', { key: stratum }, button(t('wiki.searchStratum', { stratum: stratum.toUpperCase() }), () => {
            setInput(searched.term); setScope(stratum); submit(searched.term, stratum)
          }, busy || !(shadow[stratum + '_count'] > 0))))))))
}

module.exports = { LiveHud, panels, views: { briefing: Briefing, frontier: Frontier, inbox: Inbox, wiki: Wiki },
  trustTape, BreakerTotals, AlertCatchUp,
  MathCard, ClaimCard, InboxCard, WikiCard, EvidenceText }
