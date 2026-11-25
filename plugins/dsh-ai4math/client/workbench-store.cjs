const { call } = require('./remote.cjs')

function normalizeDir(value) {
  if (typeof value !== 'string' || value.length === 0) return null
  return value.length > 1 && value.endsWith('/') ? value.slice(0, -1) : value
}

function matchProjectForCwd(projects, cwd) {
  const cleanCwd = normalizeDir(cwd)
  if (!cleanCwd || !Array.isArray(projects)) return null
  // Prefer the deepest matching project root if nested.
  let best = null
  for (const project of projects) {
    const root = normalizeDir(project?.root)
    if (!root) continue
    if (cleanCwd === root || cleanCwd.startsWith(root + '/')) {
      if (!best || root.length > best.root.length) best = project
    }
  }
  return best
}

function createWorkbenchStore(remote) {
  const listeners = new Set()
  let state = {
    phase: 'idle',
    data: null,
    busy: false,
    notice: null,
    currentSession: { sessionId: null, cwd: null },
  }
  let controller = null
  let disposed = false

  function emit(next) {
    state = next
    for (const fn of [...listeners]) fn()
  }

  function activeProject(data = state.data) {
    if (!data || !Array.isArray(data.projects)) return null
    return data.projects.find(p => p.id === data.active_project_id)
      ?? data.projects.find(p => p.active)
      ?? null
  }

  function mismatchedProject(snapshot = state) {
    const { data, currentSession } = snapshot
    if (!data || !currentSession?.cwd) return null
    const matched = matchProjectForCwd(data.projects, currentSession.cwd)
    if (!matched) return null
    if (matched.id === data.active_project_id || matched.active) return null
    return matched
  }

  async function invoke(method, params, { setBusy = true, successNotice } = {}) {
    if (disposed || typeof remote?.[method] !== 'function') return null
    if (setBusy) emit({ ...state, busy: true, notice: null })
    const owner = new AbortController()
    const prev = controller
    if (method === 'workbenchState') {
      prev?.abort()
      controller = owner
    }
    try {
      const value = await call(remote, method, params, owner.signal)
      if (disposed || owner.signal.aborted) return null
      const nextData = method === 'workbenchState' ? value : (value.state ?? state.data)
      let notice = state.notice
      if (method !== 'workbenchState') {
        if (value.ok === false) {
          notice = {
            tone: 'danger',
            key: typeof value.code === 'string' ? 'err.' + value.code : 'notice.actionFailed',
            fallback: typeof value.message === 'string' ? value.message : undefined,
          }
        } else if (successNotice) {
          const built = typeof successNotice === 'function' ? successNotice(value) : successNotice
          notice = { tone: 'good', ...built }
        }
      }
      emit({
        ...state,
        phase: 'ready',
        data: nextData,
        busy: false,
        notice,
      })
      return value
    } catch {
      if (disposed || owner.signal.aborted) return null
      emit({
        ...state,
        phase: state.data ? 'ready' : 'error',
        busy: false,
        notice: method === 'workbenchState'
          ? state.notice
          : { tone: 'danger', key: 'notice.requestFailed' },
      })
      return null
    } finally {
      if (controller === owner) controller = null
    }
  }

  return Object.freeze({
    subscribe(fn) {
      listeners.add(fn)
      return () => listeners.delete(fn)
    },
    getSnapshot: () => state,
    activeProject,
    mismatchedProject,
    start() {
      disposed = false
      if (typeof remote?.workbenchState === 'function') void invoke('workbenchState', {}, { setBusy: false })
      return () => {
        disposed = true
        controller?.abort()
        controller = null
      }
    },
    refresh: () => invoke('workbenchState', {}, { setBusy: false }),
    projectInit: (params) => invoke('projectInit', params, {
      successNotice: res => ({
        key: res.initialized ? 'notice.initNew' : 'notice.initAdopt',
        vars: { name: res.project?.name ?? 'math' },
      }),
    }),
    projectSelect: (params) => invoke('projectSelect', params, {
      successNotice: res => ({ key: 'notice.switched', vars: { name: res.project?.name ?? 'math' } }),
    }),
    projectRemove: (params) => invoke('projectRemove', params, {
      successNotice: { key: 'notice.removed' },
    }),
    daemonStart: (params = {}) => invoke('daemonStart', params, {
      successNotice: res => ({ key: res.mode === 'external' ? 'notice.daemonExternal' : 'notice.daemonManaged' }),
    }),
    daemonStop: (params = {}) => invoke('daemonStop', params, {
      successNotice: { key: 'notice.daemonStopped' },
    }),
    setCurrentSession(sessionId, cwd) {
      const nextId = typeof sessionId === 'string' && sessionId.length > 0 ? sessionId : null
      const nextCwd = normalizeDir(cwd)
      if (state.currentSession.sessionId === nextId && state.currentSession.cwd === nextCwd) return
      emit({ ...state, currentSession: { sessionId: nextId, cwd: nextCwd } })
    },
    setNotice(tone, keyOrText, vars) {
      if (!tone || !keyOrText) {
        emit({ ...state, notice: null })
        return
      }
      emit({ ...state, notice: { tone, key: keyOrText, vars } })
    },
    clearNotice() {
      if (state.notice !== null) emit({ ...state, notice: null })
    },
  })
}

function resolveNoticeText(notice, t) {
  if (!notice) return ''
  if (typeof notice.key === 'string') {
    const translated = t(notice.key, notice.vars)
    if (translated !== notice.key) return translated
  }
  return notice.fallback ?? notice.text ?? t('notice.actionFailed')
}

module.exports = { createWorkbenchStore, matchProjectForCwd, normalizeDir, resolveNoticeText }
