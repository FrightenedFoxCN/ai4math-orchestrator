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
// package-level documentation. Use the slot owner's inject to respect its lifetime.
const inject = ['remote', 'slots', 'sidebarRight', 'sidebarRightTabs']

async function apply(ctx) {
  // The official $mount owns this contribution within the current Cordis fiber.
  await ctx.remote.$mount(contribution(contract))
  const remote = ctx.remote[REMOTE_NAMESPACE]
  const live = createLive(remote, { topics: TOPICS })
  const open = (key, element) => {
    const target = ctx.sidebarRight.commandTarget(element)
    if (target === undefined) return false
    ctx.sidebarRight.openTabFromTarget(kind(key), target)
    return true
  }
  ctx.effect(() => {
    const tag = document.createElement('style')
    tag.dataset.plugin = REMOTE_PACKAGE
    tag.textContent = css
    document.head.append(tag)
    return () => tag.remove()
  }, 'ai4math.styles')
  ctx.effect(() => live.start(), 'ai4math.live')
  ctx.effect(() => ctx.slots.inject('shell.bottom', () => ctx.slots.register({
    name: 'shell.bottom',
  }, function Hud() { return h(LiveHud, { live, open }) })), 'ai4math.hud')
  for (const [index, panel] of panels.entries()) {
    const title = () => panel.label
    ctx.effect(() => ctx.sidebarRightTabs.register({
      id: id(panel.key), kind: kind(panel.key), title,
      guide: [{ id: 'open', order: 60 + index, title, description: () => panel.description }],
    }), 'ai4math.type.' + panel.key)
    ctx.effect(() => ctx.slots.inject('sidebar.right.pane.tab', () => ctx.slots.register({
      name: 'sidebar.right.pane.tab', key: id(panel.key),
    }, function Tab() { return h(views[panel.key], { remote, live }) })), 'ai4math.body.' + panel.key)
  }
}

module.exports = { inject, apply }
