const { createElement: h } = require('react')
const { DEFAULT_I18N } = require('./i18n.cjs')

function shellQuote(value) {
  return "'" + String(value).replace(/'/g, "'\\''") + "'"
}

function safeToken(value, fallback) {
  if (typeof value !== 'string') return fallback
  const trimmed = value.trim()
  if (!trimmed || /[\u0000-\u001f\u007f]/.test(trimmed) || trimmed.length > 256) return fallback
  return trimmed
}

function buildBreakerCommands(item, projectRoot, t = DEFAULT_I18N.t) {
  if (typeof projectRoot !== 'string' || !projectRoot.trim()) return []
  const root = shellQuote(projectRoot)
  const breakerId = shellQuote(safeToken(item?.breaker_id ?? item?.target_definition_id, '<BREAKER_ID>'))
  const state = item?.state ?? item?.to_state
  const commands = [
    {
      label: t('cli.previewArchive'),
      command: `ai4math owner-archive-preview ${root} <BARE_REPO> <FACT_SHA256> ${breakerId} 'Archive reason'`,
    },
  ]
  if (state === 'TRIPPED' || state === 'HALF_OPEN') {
    commands.push({
      label: t('cli.previewProbes'),
      command: `ai4math breaker-probes-preview ${root} <BARE_REPO> ${breakerId} <THEOREM>`,
    })
  }
  return commands
}

function buildInboxCommands(item, projectRoot, t = DEFAULT_I18N.t) {
  if (typeof projectRoot !== 'string' || !projectRoot.trim()) return []
  const root = shellQuote(projectRoot)
  const commands = [
    { label: t('cli.inspectStatus'), command: `ai4math status ${root}` },
    { label: t('cli.auditGraph'), command: `ai4math graph-audit ${root}` },
  ]
  if (item?.category === 'definition_smell' || item?.category === 'circuit_breaker') {
    commands.push({
      label: t('cli.previewArchive'),
      command: `ai4math owner-archive-preview ${root} <BARE_REPO> <FACT_SHA256> <BREAKER_ID> 'Archive reason'`,
    })
  }
  return commands
}

// Hook-free so it stays safe inside lists and matches vm-fixture.mjs constraints.
function flashButton(buttonEl, label, restoreLabel) {
  if (!buttonEl || typeof buttonEl !== 'object') return
  buttonEl.textContent = label
  if (buttonEl.__a4CopyTimer) clearTimeout(buttonEl.__a4CopyTimer)
  buttonEl.__a4CopyTimer = setTimeout(() => {
    buttonEl.textContent = restoreLabel
    buttonEl.__a4CopyTimer = null
  }, 1500)
}

function copyCommand(command, buttonEl, t) {
  const copyLabel = t('cli.copy')
  const write = typeof navigator !== 'undefined' && navigator?.clipboard?.writeText
    ? navigator.clipboard.writeText.bind(navigator.clipboard)
    : null
  if (!write) {
    flashButton(buttonEl, t('cli.copyFailed'), copyLabel)
    return
  }
  void write(command).then(
    () => flashButton(buttonEl, t('cli.copied'), copyLabel),
    () => flashButton(buttonEl, t('cli.copyFailed'), copyLabel),
  )
}

function CliHelper({ commands, t = DEFAULT_I18N.t }) {
  if (!Array.isArray(commands) || commands.length === 0) return null
  return h('details', { className: 'ai4m-cli-helper' },
    h('summary', null, t('cli.summary')),
    h('div', { className: 'ai4m-cli-list' },
      h('p', { className: 'ai4m-muted' }, t('cli.disclaimer')),
      ...commands.map((entry, index) => h('div', { key: index, className: 'ai4m-cli-row' },
        h('div', { className: 'ai4m-cli-row-head' },
          h('span', { className: 'ai4m-muted' }, entry.label),
          h('button', {
            type: 'button',
            className: 'ai4m-button',
            'aria-label': t('cli.copyAria', { label: entry.label }),
            onClick: event => copyCommand(entry.command, event.currentTarget, t),
          }, t('cli.copy'))),
        h('pre', { className: 'ai4m-cli-cmd' }, entry.command)))))
}

module.exports = { shellQuote, buildBreakerCommands, buildInboxCommands, CliHelper }
