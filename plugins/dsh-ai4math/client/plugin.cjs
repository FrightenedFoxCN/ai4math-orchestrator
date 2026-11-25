const { createElement: h } = require('react')
const { contribution } = require('./remote.cjs')
const { createLive } = require('./live.cjs')
const { LiveHud, panels, views } = require('./views.cjs')
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
    const open = (key, element) => {
      const target = scoped.sidebarRight.commandTarget(element)
      if (target === undefined) return false
      scoped.sidebarRight.openTabFromTarget(kind(key), target)
      return true
    }
    scoped.effect(() => {
      const tag = document.createElement('style')
      tag.dataset.plugin = REMOTE_PACKAGE
      tag.textContent = css
      document.head.append(tag)
      return () => tag.remove()
    }, 'ai4math.styles')
    scoped.effect(() => live.start(), 'ai4math.live')
    scoped.effect(() => scoped.slots.inject('shell.bottom', () => scoped.slots.register({
      name: 'shell.bottom',
    }, function Hud() { return h(LiveHud, { live, open }) })), 'ai4math.hud')
    for (const [index, panel] of panels.entries()) {
      const title = () => panel.label
      scoped.effect(() => scoped.sidebarRightTabs.register({
        id: id(panel.key), kind: kind(panel.key), title,
        guide: [{ id: 'open', order: 60 + index, title, description: () => panel.description }],
      }), 'ai4math.type.' + panel.key)
      scoped.effect(() => scoped.slots.inject('sidebar.right.pane.tab', () => scoped.slots.register({
        name: 'sidebar.right.pane.tab', key: id(panel.key),
      }, function Tab() { return h(views[panel.key], { remote, live }) })), 'ai4math.body.' + panel.key)
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
