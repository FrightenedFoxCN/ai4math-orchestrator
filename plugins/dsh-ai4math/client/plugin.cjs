const { createElement: h } = require('react')
const { contribution } = require('./remote.cjs')
const { createLive } = require('./live.cjs')
const { EN, ZH, createI18n } = require('./i18n.cjs')
const { LiveHud, panels, views } = require('./views.cjs')
const { createWorkbenchStore } = require('./workbench-store.cjs')
const { Workbench, Ai4mathNavIcon, SessionContextChip } = require('./workbench.cjs')
const contract = require('./contract')
const css = require('./style')

const { REMOTE_PACKAGE, REMOTE_NAMESPACE, TOPICS } = contract
const kind = key => 'ai4math-' + key
const id = key => REMOTE_PACKAGE + '/' + key

// Cordis inject is the service gate; dsh.client.inject in package.json is only
// package-level documentation. The remote *namespace* is gated too: reading
// `ctx.remote.ai4math` needs the dotted `remote.ai4math` inject, not just
// `remote`. That inject cannot be declared on the plugin itself, because the
// contribution mounted below is what provides it: declaring it here would make
// activation wait on a service only this plugin creates. Mount in a child fiber
// and consume the namespace in a scoped inject, which activates once it exists.
const inject = ['remote', 'slots', 'sidebarRight', 'sidebarRightTabs']
const namespace = 'remote.' + REMOTE_NAMESPACE

async function apply(ctx) {
  // The official $mount owns this contribution within its child fiber.
  const transport = ctx.plugin(async owner => {
    await owner.remote.$mount(contribution(contract))
  })
  const ui = ctx.inject([namespace, 'slots', 'sidebarRight', 'sidebarRightTabs'], scoped => {
    const remote = scoped.remote[REMOTE_NAMESPACE]
    const live = createLive(remote, { topics: TOPICS })
    const wbStore = createWorkbenchStore(remote)
    const getService = name => typeof scoped.get === 'function' ? scoped.get(name) : undefined
    const localeService = getService('locale')
    if (localeService?.register) {
      scoped.effect(() => {
        const dropEn = localeService.register(REMOTE_PACKAGE, 'en', EN)
        const dropZh = localeService.register(REMOTE_PACKAGE, 'zh', ZH)
        return () => { dropZh?.(); dropEn?.() }
      }, 'ai4math.locale')
    }
    const i18n = createI18n(localeService)
    const open = (key, element) => {
      const target = scoped.sidebarRight.commandTarget(element)
      if (target === undefined) return false
      scoped.sidebarRight.openTabFromTarget(kind(key), target)
      return true
    }
    const openWorkbench = () => {
      const layout = getService('layout')
      if (!layout?.selectPanel) return false
      try { layout.selectPanel('ai4math'); return true } catch { return false }
    }
    const actions = {
      openWorkbench,
      pickDirectory: async () => {
        const uiWs = getService('uiWorkspace')
        if (!uiWs?.pickDirectory) return { error: 'unavailable' }
        try {
          const path = await uiWs.pickDirectory()
          return typeof path === 'string' && path.length > 0 ? { path } : { cancelled: true }
        } catch {
          return { error: 'failed' }
        }
      },
      openSession: (sessionId) => {
        const uiWs = getService('uiWorkspace')
        uiWs?.openSession?.(sessionId)
      },
      openChiefSession: async ({ root, preset, prompt }) => {
        const wsApi = getService('workspaces')
        const uiWs = getService('uiWorkspace')
        if (!wsApi?.create || !uiWs?.startSession) return false
        try {
          const ws = await wsApi.create({ path: root })
          uiWs.startSession(ws.workspaceId, { agentPreset: preset, prompt, whenDraftExists: 'keep' })
          return true
        } catch {
          return false
        }
      },
    }
    scoped.effect(() => {
      const tag = document.createElement('style')
      tag.dataset.plugin = REMOTE_PACKAGE
      tag.textContent = css
      document.head.append(tag)
      return () => tag.remove()
    }, 'ai4math.styles')
    scoped.effect(() => live.start(), 'ai4math.live')
    scoped.effect(() => wbStore.start(), 'ai4math.workbench-store')
    scoped.effect(() => scoped.slots.inject('shell.bottom', () => scoped.slots.register({
      name: 'shell.bottom',
    }, function Hud() {
      return h(LiveHud, { live, open, wbStore, i18n, openWorkbench: getService('layout') ? openWorkbench : undefined })
    })), 'ai4math.hud')
    scoped.effect(() => scoped.slots.inject('sidebar.panellist', () => scoped.slots.register({
      name: 'sidebar.panellist', id: 'ai4math', order: 18, label: () => i18n.t('nav.label'),
    }, function Ai4mathNav(props) { return h(Ai4mathNavIcon, { size: props?.size }) })), 'ai4math.nav')
    scoped.effect(() => scoped.slots.inject('main', () => scoped.slots.register({
      name: 'main', key: 'ai4math',
    }, function Ai4mathMain(props) {
      return h(Workbench, {
        remote, live, wbStore, actions, i18n,
        useWorkspaces: props?.useWorkspaces,
        useSessions: props?.useSessions,
      })
    })), 'ai4math.workbench')
    scoped.effect(() => scoped.slots.inject('conversation.session.header.utilities', () => scoped.slots.register({
      name: 'conversation.session.header.utilities', id: 'ai4math-session-context', order: 40,
    }, function Ai4mathSessionHeader(props) {
      return h(SessionContextChip, {
        sessionId: props?.sessionId,
        useSessions: props?.useSessions,
        wbStore,
        i18n,
      })
    })), 'ai4math.session-context')
    for (const [index, panel] of panels.entries()) {
      const title = () => i18n.t(panel.labelKey)
      scoped.effect(() => scoped.sidebarRightTabs.register({
        id: id(panel.key), kind: kind(panel.key), title,
        guide: [{ id: 'open', order: 60 + index, title, description: () => i18n.t(panel.descKey) }],
      }), 'ai4math.type.' + panel.key)
      scoped.effect(() => scoped.slots.inject('sidebar.right.pane.tab', () => scoped.slots.register({
        name: 'sidebar.right.pane.tab', key: id(panel.key),
      }, function Tab() { return h(views[panel.key], { remote, live, wbStore, i18n }) })), 'ai4math.body.' + panel.key)
    }
  })
  const dispose = ctx.effect(function* ai4mathMount() {
    yield transport.dispose
    yield ui.dispose
  }, 'ai4math.client.mount')
  try {
    await Promise.all([transport, ui])
  } catch (error) {
    await dispose()
    throw error
  }
  return dispose
}

module.exports = { inject, apply }
