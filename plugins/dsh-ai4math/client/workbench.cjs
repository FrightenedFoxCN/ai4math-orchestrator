const { createElement: h, useCallback, useEffect, useState, useSyncExternalStore } = require('react')
const { views, trustTape, BreakerTotals, AlertCatchUp } = require('./views.cjs')
const { matchProjectForCwd, normalizeDir, resolveNoticeText } = require('./workbench-store.cjs')
const { useI18n } = require('./i18n.cjs')

const ENGINES = ['lean4', 'agda', 'rocq']
const ENGINE_LABELS = { lean4: 'Lean 4', agda: 'Agda', rocq: 'Rocq' }
const TABS = [
  { id: 'overview', labelKey: 'wb.tab.overview' },
  { id: 'briefing', labelKey: 'wb.tab.briefing' },
  { id: 'frontier', labelKey: 'wb.tab.frontier' },
  { id: 'inbox', labelKey: 'wb.tab.inbox' },
  { id: 'wiki', labelKey: 'wb.tab.wiki' },
  { id: 'setup', labelKey: 'wb.tab.setup' },
]

const noopHook = () => null
const identity = value => value
const utf8Bytes = value => new TextEncoder().encode(String(value)).byteLength
const isCount = value => Number.isSafeInteger(value) && value >= 0
const countOf = (value, na) => isCount(value) ? String(value) : na
const pctOf = (value, na) => typeof value === 'number' && Number.isFinite(value) && value >= 0 && value <= 100
  ? Math.round(value) + '%' : na
const badge = (label, tone, title) => h('span', { className: 'ai4m-badge', 'data-tone': tone, ...(title ? { title } : {}) }, label)

function daemonBadgeMeta(status, t) {
  switch (status) {
    case 'managed': return { label: t('wb.daemonManaged'), tone: 'good', title: t('wb.daemonManagedTitle') }
    case 'external': return { label: t('wb.daemonExternal'), tone: 'good', title: t('wb.daemonExternalTitle') }
    case 'starting': return { label: t('wb.daemonStarting'), tone: 'warn' }
    case 'stale': return { label: t('wb.daemonStale'), tone: 'warn' }
    case 'conflict': return { label: t('wb.daemonConflict'), tone: 'danger' }
    case 'invalid': return { label: t('wb.daemonInvalid'), tone: 'danger' }
    default: return { label: t('wb.daemonStopped'), tone: 'muted' }
  }
}

function Ai4mathNavIcon({ size = 16 }) {
  return h('svg', {
    width: size,
    height: size,
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 1.8,
    strokeLinecap: 'round',
    strokeLinejoin: 'round',
    'aria-hidden': true,
  },
  h('circle', { cx: 6, cy: 6, r: 2.5 }),
  h('circle', { cx: 18, cy: 6, r: 2.5 }),
  h('circle', { cx: 12, cy: 18, r: 2.5 }),
  h('path', { d: 'M8.2 7.2L10.8 15.6M15.8 7.2L13.2 15.6M8.5 6H15.5' }))
}

function SessionContextChip({ sessionId, useSessions = noopHook, wbStore, i18n }) {
  const { t } = useI18n(i18n)
  const sessionList = useSessions(identity)
  const wbSnap = useSyncExternalStore(wbStore.subscribe, wbStore.getSnapshot)
  const cwd = sessionId ? (sessionList?.byId?.[sessionId]?.cwd ?? null) : null
  useEffect(() => {
    wbStore.setCurrentSession(sessionId || null, cwd)
    return () => wbStore.setCurrentSession(null, null)
  }, [sessionId, cwd, wbStore])
  const mismatched = wbStore.mismatchedProject(wbSnap)
  if (!mismatched) return null
  return h('span', { className: 'ai4m ai4m-session-chip' },
    h('button', {
      type: 'button',
      className: 'ai4m-button',
      'data-variant': 'primary',
      disabled: wbSnap.busy,
      title: t('wb.sessionChipTitle', { root: mismatched.root }),
      onClick: () => void wbStore.projectSelect({ project_id: mismatched.id, auto_start: true }),
    }, wbSnap.busy ? t('hud.switching') : t('wb.sessionChip', { name: mismatched.name })))
}

function resolveLinkedSessions(projectRoot, workspaceSnap, sessionSnap, blankLabel) {
  const root = normalizeDir(projectRoot)
  if (!root || !sessionSnap?.byId) return []
  const archived = new Set(workspaceSnap?.archivedSessionIds ?? [])
  const ws = (workspaceSnap?.items ?? []).find(item => normalizeDir(item?.path) === root)
  const seen = new Set()
  const items = []
  const pushId = (id) => {
    if (!id || seen.has(id) || archived.has(id)) return
    const summary = sessionSnap.byId[id]
    if (!summary || summary.origin === 'subagent') return
    seen.add(id)
    const projValues = sessionSnap.projectionsBySession?.[id]?.values
    const title = projValues?.sessionTitle ?? summary.projections?.sessionTitle ?? (summary.blank ? blankLabel : id.slice(0, 14))
    const preset = projValues?.agentPreset ?? summary.projections?.agentPreset ?? null
    items.push({
      sessionId: id,
      title: typeof title === 'string' && title.trim() ? title.trim() : id.slice(0, 14),
      preset: typeof preset === 'string' && preset.trim() ? preset.trim() : null,
      running: summary.running === true,
      blank: summary.blank === true,
      updatedAt: typeof summary.updatedAt === 'number' ? summary.updatedAt : 0,
    })
  }
  for (const id of ws?.sessionIds ?? []) pushId(id)
  for (const id of sessionSnap.ids ?? Object.keys(sessionSnap.byId)) {
    const cwd = normalizeDir(sessionSnap.byId[id]?.cwd)
    if (cwd && (cwd === root || cwd.startsWith(root + '/'))) pushId(id)
  }
  items.sort((a, b) => b.updatedAt - a.updatedAt)
  return items.slice(0, 12)
}

function formatUpdated(timestamp, t) {
  if (!timestamp || !Number.isFinite(timestamp)) return ''
  const deltaSec = Math.max(0, Math.floor((Date.now() - timestamp) / 1000))
  if (deltaSec < 60) return t('wb.justNow')
  if (deltaSec < 3600) return t('wb.minsAgo', { n: Math.floor(deltaSec / 60) })
  if (deltaSec < 86400) return t('wb.hoursAgo', { n: Math.floor(deltaSec / 3600) })
  return t('wb.daysAgo', { n: Math.floor(deltaSec / 86400) })
}

function Workbench({ remote, live, wbStore, actions, i18n, useWorkspaces = noopHook, useSessions = noopHook }) {
  const { t } = useI18n(i18n)
  const na = t('common.na')
  const wbSnap = useSyncExternalStore(wbStore.subscribe, wbStore.getSnapshot)
  const liveSnap = useSyncExternalStore(live.subscribe, live.getSnapshot)
  const workspaceSnap = useWorkspaces(identity)
  const sessionSnap = useSessions(identity)

  const [tab, setTab] = useState('overview')
  const [showNewForm, setShowNewForm] = useState(false)
  const [rootInput, setRootInput] = useState('')
  const [nameInput, setNameInput] = useState('')
  const [engineInput, setEngineInput] = useState('lean4')
  const [autoStart, setAutoStart] = useState(true)
  const [launching, setLaunching] = useState(false)
  const [picking, setPicking] = useState(false)
  const [refreshing, setRefreshing] = useState(false)
  const [confirmRemoveId, setConfirmRemoveId] = useState(null)

  useEffect(() => {
    void wbStore.refresh()
  }, [wbStore])

  const data = wbSnap.data
  const projects = Array.isArray(data?.projects) ? data.projects : []
  const activeProject = wbStore.activeProject(data)
  const unmanaged = !activeProject ? data?.unmanaged_active : null
  const activeRoot = activeProject?.root ?? unmanaged?.root ?? null
  const activeKey = activeProject?.id ?? activeRoot ?? '__none'
  const activeName = activeProject?.name ?? (unmanaged?.root ? unmanaged.root.split('/').filter(Boolean).at(-1) : null) ?? 'AI4Math'
  const daemonStatus = activeProject?.daemon_status ?? unmanaged?.daemon_status ?? 'stopped'
  const daemonMeta = daemonBadgeMeta(daemonStatus, t)
  const chiefPreset = data?.chief_preset ?? 'ai4math-chief'
  const socketLimit = data?.socket_limit_bytes ?? 103
  const trimmedRoot = rootInput.trim()
  const previewSocketBytes = trimmedRoot ? utf8Bytes(trimmedRoot.replace(/\/+$/, '') + '/.runtime/ai4mathd.sock') : 0
  const pathTooLong = previewSocketBytes > socketLimit

  const binariesMissing = Boolean(data && (!data.binaries?.cli_ready || !data.binaries?.daemon_ready))
  const daemonUnavailableTitle = !data?.binaries?.daemon_ready ? t('wb.daemonMissingTitle') : undefined

  const snapshot = liveSnap.snapshot
  const snapshotFresh = liveSnap.phase === 'live' && !liveSnap.snapshotStale
  const hudOffline = !activeRoot || daemonStatus === 'stopped' || daemonStatus === 'stale'
  const hudBadgeLabel = snapshotFresh ? t('wb.hudLive')
    : hudOffline ? t('wb.hudOffline')
    : (liveSnap.phase === 'reconnecting' ? t('hud.reconnecting') : t('hud.connecting'))
  const hudBadgeTone = snapshotFresh ? 'good' : hudOffline ? 'muted' : 'warn'

  const pendingTotal = ['P0', 'P1', 'P2'].reduce((sum, p) => {
    const n = snapshot?.pending_inbox?.[p]
    return sum + (isCount(n) ? n : 0)
  }, 0)
  const linkedSessions = resolveLinkedSessions(activeRoot, workspaceSnap, sessionSnap, t('wb.blankSession'))
  const busy = wbSnap.busy || launching

  const handleRefresh = useCallback(async () => {
    setRefreshing(true)
    try { await wbStore.refresh() }
    finally { setRefreshing(false) }
  }, [wbStore])

  const handlePickFolder = useCallback(async () => {
    if (!actions?.pickDirectory || picking) return
    setPicking(true)
    try {
      const result = await actions.pickDirectory()
      const picked = typeof result === 'string' ? result : result?.path
      if (typeof picked === 'string' && picked.length > 0) {
        setRootInput(picked)
        setNameInput(prev => prev.trim() ? prev : (picked.split('/').filter(Boolean).at(-1) ?? ''))
        setShowNewForm(true)
      } else if (result?.error) {
        wbStore.setNotice('warn', 'wb.pickerUnavailable')
      }
    } finally {
      setPicking(false)
    }
  }, [actions, picking, wbStore])

  const handleCreateProject = useCallback(async (event) => {
    event.preventDefault()
    if (!trimmedRoot || pathTooLong || busy) return
    const params = { root: trimmedRoot, engine: engineInput, auto_start: autoStart }
    if (nameInput.trim()) params.name = nameInput.trim()
    const res = await wbStore.projectInit(params)
    if (res?.ok) {
      setRootInput('')
      setNameInput('')
      setShowNewForm(false)
    }
  }, [trimmedRoot, pathTooLong, busy, engineInput, autoStart, nameInput, wbStore])

  const handleLaunchChief = useCallback(async () => {
    if (!activeRoot || !actions?.openChiefSession) {
      setShowNewForm(true)
      wbStore.setNotice('warn', 'wb.selectBeforeChief')
      return
    }
    setLaunching(true)
    try {
      if (activeProject && daemonStatus !== 'managed' && daemonStatus !== 'external' && data?.binaries?.daemon_ready) {
        const started = await wbStore.daemonStart({ project_id: activeProject.id })
        if (!started?.ok) return
      }
      const ok = await actions.openChiefSession({
        root: activeRoot,
        preset: chiefPreset,
        prompt: t('wb.chiefPrompt', { name: activeName }),
      })
      if (!ok) {
        wbStore.setNotice('danger', 'wb.sessionOpenFailed')
      }
    } finally {
      setLaunching(false)
    }
  }, [activeRoot, activeProject, activeName, daemonStatus, data, chiefPreset, actions, wbStore, t])

  const handleTabKeyDown = useCallback((event) => {
    const idx = TABS.findIndex(item => item.id === tab)
    if (idx < 0) return
    if (event.key === 'ArrowRight') {
      event.preventDefault()
      setTab(TABS[(idx + 1) % TABS.length].id)
    } else if (event.key === 'ArrowLeft') {
      event.preventDefault()
      setTab(TABS[(idx - 1 + TABS.length) % TABS.length].id)
    }
  }, [tab])

  const showOnboarding = wbSnap.phase === 'ready' && projects.length === 0 && !unmanaged
  const effectiveFormOpen = showNewForm || showOnboarding

  return h('section', { className: 'ai4m ai4m-workbench', 'aria-label': t('wb.aria') },
    h('header', { className: 'ai4m-wb-top' },
      h('div', { className: 'ai4m-wb-title-row' },
        h('div', { className: 'ai4m-wb-brand' },
          h(Ai4mathNavIcon, { size: 18 }),
          h('div', null,
            h('h2', null, t('wb.title')),
            h('p', { className: 'ai4m-muted' }, t('wb.subtitle')))),
        h('div', { className: 'ai4m-actions' },
          !showOnboarding && h('button', {
            type: 'button',
            className: 'ai4m-button',
            disabled: busy,
            onClick: () => setShowNewForm(open => !open),
          }, effectiveFormOpen ? t('wb.hideProjectForm') : t('wb.newOpenProject')),
          h('button', {
            type: 'button',
            className: 'ai4m-button',
            'data-variant': 'primary',
            disabled: busy || !activeRoot,
            title: activeRoot ? t('wb.launchChiefTitle', { root: activeRoot, preset: chiefPreset }) : t('wb.selectProjectFirst'),
            onClick: () => void handleLaunchChief(),
          }, launching ? t('wb.launchingChief') : t('wb.newChiefSession')))),

      h('div', { className: 'ai4m-wb-bar' },
        h('label', { className: 'ai4m-wb-select-wrap' },
          h('span', { className: 'ai4m-muted' }, t('wb.projectLabel')),
          h('select', {
            'aria-label': t('wb.projectSelectAria'),
            value: activeProject?.id ?? '',
            disabled: busy || projects.length === 0,
            onChange: event => {
              const id = event.target.value
              if (id) void wbStore.projectSelect({ project_id: id, auto_start: true })
            },
          },
          projects.length === 0
            ? h('option', { value: '' }, unmanaged ? t('wb.externalOption') : t('wb.noProjectsOption'))
            : [
                !activeProject && h('option', { key: '__none', value: '' }, t('wb.externalUnlisted')),
                ...projects.map(project => h('option', { key: project.id, value: project.id },
                  project.name + ' · ' + (ENGINE_LABELS[project.engine] ?? project.engine) + ' (' + daemonBadgeMeta(project.daemon_status, t).label + ')')),
              ])),
        activeProject && badge(ENGINE_LABELS[activeProject.engine] ?? activeProject.engine, 'brand'),
        badge(daemonMeta.label, daemonMeta.tone, daemonMeta.title),
        badge(hudBadgeLabel, hudBadgeTone),
        h('div', { className: 'ai4m-wb-bar-right' },
          activeProject && daemonStatus === 'managed' && h('button', {
            type: 'button', className: 'ai4m-button', disabled: busy,
            onClick: () => void wbStore.daemonStop({ project_id: activeProject.id }),
          }, t('wb.stopDaemon')),
          activeProject && daemonStatus !== 'managed' && daemonStatus !== 'external' && h('button', {
            type: 'button', className: 'ai4m-button',
            disabled: busy || !data?.binaries?.daemon_ready,
            title: daemonUnavailableTitle,
            onClick: () => void wbStore.daemonStart({ project_id: activeProject.id }),
          }, t('wb.startDaemon')),
          h('button', {
            type: 'button', className: 'ai4m-button', disabled: busy || refreshing,
            onClick: () => void handleRefresh(),
          }, refreshing ? t('common.refreshing') : t('common.refresh'))))),

    h('nav', {
      className: 'ai4m-wb-tabs',
      role: 'tablist',
      'aria-label': t('wb.tabsAria'),
      onKeyDown: handleTabKeyDown,
    },
    TABS.map(item => h('button', {
      key: item.id,
      id: 'ai4m-wb-tab-' + item.id,
      type: 'button',
      role: 'tab',
      'aria-selected': tab === item.id,
      'aria-controls': 'ai4m-wb-tabpanel',
      className: 'ai4m-wb-tab',
      'data-active': tab === item.id ? 'true' : 'false',
      onClick: () => setTab(item.id),
    },
    t(item.labelKey),
    item.id === 'inbox' && pendingTotal > 0 && badge(String(pendingTotal), (snapshot?.pending_inbox?.P0 ?? 0) > 0 ? 'danger' : 'warn')))),

    h('div', {
      id: 'ai4m-wb-tabpanel',
      role: 'tabpanel',
      'aria-labelledby': 'ai4m-wb-tab-' + tab,
      className: 'ai4m-wb-body',
    },
      wbSnap.phase === 'idle' && h('div', { className: 'ai4m-wb-banners' },
        h('p', { className: 'ai4m-banner', 'data-tone': 'loading', role: 'status' }, t('wb.loadingState'))),
      wbSnap.phase === 'error' && !data && h('div', { className: 'ai4m-wb-banners' },
        h('div', { className: 'ai4m-wb-notice ai4m-banner', 'data-tone': 'danger', role: 'alert' },
          h('span', null, t('wb.loadError')),
          h('button', { type: 'button', className: 'ai4m-button', onClick: () => void handleRefresh() }, t('common.retry')))),

      wbSnap.notice && h('div', { className: 'ai4m-wb-banners' },
        h('div', {
          className: 'ai4m-wb-notice ai4m-banner',
          'data-tone': wbSnap.notice.tone,
          role: wbSnap.notice.tone === 'danger' ? 'alert' : 'status',
        },
        h('span', null, resolveNoticeText(wbSnap.notice, t)),
        h('button', { type: 'button', className: 'ai4m-button', onClick: () => wbStore.clearNotice() }, t('common.dismiss')))),

      binariesMissing && tab !== 'setup' && h('div', { className: 'ai4m-wb-banners' },
        h('div', { className: 'ai4m-wb-notice ai4m-banner', 'data-tone': 'warn', role: 'status' },
          h('span', null, t('wb.missingBinaries')),
          h('button', { type: 'button', className: 'ai4m-button', onClick: () => setTab('setup') }, t('wb.openSetup')))),

      effectiveFormOpen && h('form', { className: 'ai4m-wb-form', onSubmit: handleCreateProject },
        h('div', { className: 'ai4m-wb-form-head' },
          h('strong', null, showOnboarding ? t('wb.formOnboardingTitle') : t('wb.formTitle')),
          h('span', { className: 'ai4m-muted' }, t('wb.formHint'))),
        h('div', { className: 'ai4m-wb-form-row' },
          h('label', { className: 'ai4m-field ai4m-field--grow' },
            t('wb.formDirLabel'),
            h('div', { className: 'ai4m-wb-path-input' },
              h('input', {
                type: 'text',
                value: rootInput,
                placeholder: '/Users/you/math/my-problem',
                required: true,
                onChange: event => setRootInput(event.target.value),
              }),
              actions?.pickDirectory && h('button', {
                type: 'button', className: 'ai4m-button', disabled: busy || picking, onClick: () => void handlePickFolder(),
              }, picking ? t('wb.picking') : t('wb.pickFolder')))),
          h('label', { className: 'ai4m-field' },
            t('wb.formEngineLabel'),
            h('select', { value: engineInput, onChange: event => setEngineInput(event.target.value) },
              ENGINES.map(engine => h('option', { key: engine, value: engine }, ENGINE_LABELS[engine])))),
          h('label', { className: 'ai4m-field' },
            t('wb.formNameLabel'),
            h('input', {
              type: 'text', value: nameInput, maxLength: 80, placeholder: t('wb.formNamePlaceholder'),
              onChange: event => setNameInput(event.target.value),
            }))),
        h('div', { className: 'ai4m-wb-form-foot' },
          h('label', { className: 'ai4m-check' },
            h('input', { type: 'checkbox', checked: autoStart, onChange: event => setAutoStart(event.target.checked) }),
            t('wb.formAutoStart')),
          trimmedRoot && h('span', { className: pathTooLong ? 'ai4m-badge' : 'ai4m-muted', 'data-tone': pathTooLong ? 'danger' : undefined },
            t('wb.socketPathLen', { bytes: previewSocketBytes, limit: socketLimit }) + (pathTooLong ? t('wb.socketPathTooLongSuffix') : '')),
          h('div', { className: 'ai4m-actions' },
            !showOnboarding && h('button', { type: 'button', className: 'ai4m-button', onClick: () => setShowNewForm(false) }, t('common.cancel')),
            h('button', {
              type: 'submit', className: 'ai4m-button', 'data-variant': 'primary',
              disabled: busy || !trimmedRoot || pathTooLong,
            }, busy ? t('common.working') : t('wb.initAndConnect'))))),

      tab === 'overview' && h('div', { className: 'ai4m-wb-overview' },
        h('section', { className: 'ai4m-section' },
          h('h3', null, t('wb.overviewVitals')),
          trustTape(snapshot?.trust, t),
          h('div', { className: 'ai4m-summary-row' },
            h('span', null, h('span', { className: 'ai4m-muted' }, t('hud.ram') + ' '), pctOf(snapshot?.memory?.used_percent, na),
              typeof snapshot?.memory?.pressure === 'string' ? ' · ' + snapshot.memory.pressure : ''),
            h('span', null, h('span', { className: 'ai4m-muted' }, t('hud.stale') + ' '), countOf(snapshot?.stale_claims, na)),
            h('span', null, h('span', { className: 'ai4m-muted' }, t('hud.seq') + ' '), countOf(snapshot?.event_seq, na)),
            h(BreakerTotals, { snapshot, t }))),

        h('div', { className: 'ai4m-wb-grid' },
          h('section', { className: 'ai4m-section ai4m-card' },
            h('div', { className: 'ai4m-wb-card-head' },
              h('h3', null, t('wb.linkedConversations')),
              h('button', {
                type: 'button', className: 'ai4m-button', 'data-variant': 'primary',
                disabled: busy || !activeRoot, onClick: () => void handleLaunchChief(),
              }, t('wb.addChiefShort'))),
            !activeRoot
              ? h('p', { className: 'ai4m-empty' }, t('wb.noProjectSessions'))
              : linkedSessions.length === 0
                ? h('p', { className: 'ai4m-empty' }, t('wb.emptySessions', { root: activeRoot }))
                : h('ul', { className: 'ai4m-edges' }, linkedSessions.map(item =>
                    h('li', { key: item.sessionId, className: 'ai4m-wb-session-row' },
                      h('div', { className: 'ai4m-wb-session-meta' },
                        h('strong', { title: item.title }, item.title),
                        h('span', { className: 'ai4m-meta' },
                          item.preset && badge(item.preset, item.preset === chiefPreset ? 'brand' : 'muted'),
                          item.running && badge(t('wb.running'), 'good'),
                          item.updatedAt > 0 && h('span', { className: 'ai4m-muted' }, formatUpdated(item.updatedAt, t)))),
                      actions?.openSession && h('button', {
                        type: 'button', className: 'ai4m-button',
                        onClick: () => actions.openSession(item.sessionId),
                      }, t('common.open')))))),

          h('div', { className: 'ai4m-stack' },
            h('section', { className: 'ai4m-section ai4m-card' },
              h('div', { className: 'ai4m-wb-card-head' },
                h('h3', null, t('wb.alertsDecisions')),
                h('div', { className: 'ai4m-actions' },
                  h('button', { type: 'button', className: 'ai4m-button', onClick: () => setTab('briefing') }, t('wb.openBriefing')),
                  h('button', { type: 'button', className: 'ai4m-button', onClick: () => setTab('inbox') }, t('wb.openInbox')))),
              h(AlertCatchUp, { live, state: liveSnap, t }),
              liveSnap.recentAlerts.length > 0
                ? h('ul', { className: 'ai4m-edges ai4m-edges--alerts' }, liveSnap.recentAlerts.slice(0, 5).map(alert =>
                    h('li', { key: alert.event_seq },
                      h('span', null, '#' + countOf(alert.event_seq, na) + ' · ' + alert.headline),
                      h('button', { type: 'button', className: 'ai4m-button', onClick: () => setTab('briefing') }, t('common.inspect')))))
                : h('p', { className: 'ai4m-empty' }, t('wb.noP0Session'))),

            h('section', { className: 'ai4m-section ai4m-card' },
              h('div', { className: 'ai4m-wb-card-head' },
                h('h3', null, t('wb.activeProjectCard')),
                h('button', { type: 'button', className: 'ai4m-button', onClick: () => setTab('setup') }, t('wb.tab.setup'))),
              activeRoot
                ? h('div', { className: 'ai4m-stack' },
                    h('div', { className: 'ai4m-meta' },
                      h('span', { className: 'ai4m-muted' }, t('wb.rootLabel')),
                      h('code', null, activeRoot)),
                    h('div', { className: 'ai4m-meta' },
                      h('span', { className: 'ai4m-muted' }, t('wb.socketLabel')),
                      h('code', null, data?.active_socket_path ?? na)),
                    h('div', { className: 'ai4m-meta' },
                      h('span', { className: 'ai4m-muted' }, t('wb.chiefPresetLabel')),
                      h('code', null, chiefPreset),
                      badge(data?.chief_bound ? t('wb.toolsBound') : t('wb.toolsUnbound'), data?.chief_bound ? 'good' : 'warn')))
                : h('p', { className: 'ai4m-empty' }, t('wb.noActiveProject')))))),

      tab === 'briefing' && h(views.briefing, { key: activeKey, remote, live, projectRoot: activeRoot, i18n }),
      tab === 'frontier' && h(views.frontier, { key: activeKey, remote, live, projectRoot: activeRoot, i18n }),
      tab === 'inbox' && h(views.inbox, { key: activeKey, remote, projectRoot: activeRoot, i18n }),
      tab === 'wiki' && h(views.wiki, { key: activeKey, remote, i18n }),

      tab === 'setup' && h('div', { className: 'ai4m-wb-setup ai4m-stack' },
        h('section', { className: 'ai4m-section' },
          h('h3', null, t('wb.registeredProjects')),
          projects.length === 0
            ? h('p', { className: 'ai4m-empty' }, t('wb.noProjectsYet'))
            : h('div', { className: 'ai4m-stack' }, projects.map(project => {
                const status = daemonBadgeMeta(project.daemon_status, t)
                const confirming = confirmRemoveId === project.id
                return h('article', {
                  key: project.id,
                  className: 'ai4m-card',
                  'data-accent': project.active ? 'brand' : undefined,
                },
                h('div', { className: 'ai4m-wb-card-head' },
                  h('div', { className: 'ai4m-meta' },
                    h('h4', null, project.name),
                    badge(ENGINE_LABELS[project.engine] ?? project.engine, 'muted'),
                    badge(status.label, status.tone, status.title),
                    project.active && badge(t('wb.activeBadge'), 'brand')),
                  h('div', { className: 'ai4m-actions' },
                    !project.active && h('button', {
                      type: 'button', className: 'ai4m-button', 'data-variant': 'primary', disabled: busy,
                      onClick: () => void wbStore.projectSelect({ project_id: project.id, auto_start: true }),
                    }, t('common.activate')),
                    project.daemon_status === 'managed'
                      ? h('button', {
                          type: 'button', className: 'ai4m-button', disabled: busy,
                          onClick: () => void wbStore.daemonStop({ project_id: project.id }),
                        }, t('common.stop'))
                      : project.daemon_status === 'external'
                        ? h('span', { className: 'ai4m-muted' }, t('wb.supervisedExternally'))
                        : h('button', {
                            type: 'button', className: 'ai4m-button',
                            disabled: busy || !data?.binaries?.daemon_ready,
                            title: daemonUnavailableTitle,
                            onClick: () => void wbStore.daemonStart({ project_id: project.id }),
                          }, t('common.start')),
                    confirming
                      ? [
                          h('button', {
                            key: 'yes', type: 'button', className: 'ai4m-button', 'data-variant': 'danger', disabled: busy,
                            onClick: () => { setConfirmRemoveId(null); void wbStore.projectRemove({ project_id: project.id }) },
                          }, t('common.confirmRemove')),
                          h('button', {
                            key: 'no', type: 'button', className: 'ai4m-button', disabled: busy,
                            onClick: () => setConfirmRemoveId(null),
                          }, t('common.cancel')),
                        ]
                      : h('button', {
                          type: 'button', className: 'ai4m-button', disabled: busy,
                          title: t('wb.removeTitle'),
                          onClick: () => setConfirmRemoveId(project.id),
                        }, t('common.remove')))),
                h('div', { className: 'ai4m-card-foot' },
                  h('span', null, h('span', { className: 'ai4m-muted' }, t('wb.pathLabel') + ' '), h('code', null, project.root)),
                  h('span', null, h('span', { className: 'ai4m-muted' }, t('wb.socketLabel') + ' '), `${project.socket_bytes}/${project.socket_limit} B`)))
              }))),

        h('section', { className: 'ai4m-section' },
          h('h3', null, t('wb.binariesSection')),
          h('div', { className: 'ai4m-card' },
            h('div', { className: 'ai4m-meta' },
              badge(data?.binaries?.cli_ready ? t('wb.cliReady') : t('wb.cliMissing'), data?.binaries?.cli_ready ? 'good' : 'danger'),
              data?.binaries?.cli_path && h('code', null, data.binaries.cli_path)),
            h('div', { className: 'ai4m-meta' },
              badge(data?.binaries?.daemon_ready ? t('wb.daemonReady') : t('wb.daemonMissing'), data?.binaries?.daemon_ready ? 'good' : 'danger'),
              data?.binaries?.daemon_path && h('code', null, data.binaries.daemon_path)),
            h('div', { className: 'ai4m-meta' },
              h('span', { className: 'ai4m-muted' }, t('wb.rolePresetsLabel')),
              Object.keys(data?.role_presets ?? {}).length === 0
                ? badge(t('wb.noneConfigured'), 'warn')
                : Object.entries(data.role_presets).map(([role, presetId]) =>
                    badge(role + ' → ' + presetId, role === 'chief' ? 'brand' : 'muted'))),
            h('p', { className: 'ai4m-muted' }, t('wb.boundaryNote')))),

        (data?.managed_daemon?.logs?.length > 0 || data?.last_daemon_exit?.logs?.length > 0) && h('section', { className: 'ai4m-section' },
          h('h3', null, t('wb.daemonOutput')),
          h('pre', { className: 'ai4m-evidence' },
            (data?.managed_daemon?.logs?.length ? data.managed_daemon.logs : data.last_daemon_exit.logs).join('\n'))))))
}

module.exports = { Workbench, Ai4mathNavIcon, SessionContextChip, resolveLinkedSessions, matchProjectForCwd }
