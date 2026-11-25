const { call } = require('./remote.cjs')

const ALERT_PAGE_SIZE = 64
const ALERT_PAGE_CAP = 3
const RECENT_ALERT_LIMIT = 16
// Five nominal daemon ticks (2 s each), allowing for scheduling and delivery jitter.
const SNAPSHOT_STALE_MS = 10000
const NO_ALERTS = Object.freeze([])
const INITIAL = Object.freeze({ phase: 'connecting', snapshot: null, snapshotStale: false, recentAlerts: NO_ALERTS, breaker: null,
  alertHistoryIncomplete: false, alertHistoryLoading: false })

function isAlert(value) {
  return Number.isSafeInteger(value?.event_seq) && value.event_seq >= 0
    && typeof value.headline === 'string' && value.headline.length > 0
}

function mergeAlerts(current, incoming) {
  const bySequence = new Map(current.map(alert => [alert.event_seq, alert]))
  for (const alert of incoming) {
    if (isAlert(alert) && !bySequence.has(alert.event_seq)) bySequence.set(alert.event_seq, alert)
  }
  if (bySequence.size === current.length) return current
  return [...bySequence.values()].sort((a, b) => b.event_seq - a.event_seq).slice(0, RECENT_ALERT_LIMIT)
}

function isSnapshot(value) {
  return Number.isSafeInteger(value?.event_seq) && value.event_seq >= 0
    && typeof value.observed_at === 'string' && value.trust && typeof value.trust === 'object'
    && value.pending_inbox && typeof value.pending_inbox === 'object'
}

// One logical subscriber per mounted plugin, not per session, tab, or React render.
function createLive(remote, { topics, retryMs = 1500,
  schedule = (fn, ms) => setTimeout(fn, ms), cancel = timer => clearTimeout(timer) }) {
  const listeners = new Set()
  let view = INITIAL
  let lifetime
  let requestAlertCatchUp
  function update(patch) {
    view = { ...view, ...patch }
    for (const listener of listeners) listener()
  }
  function start() {
    if (lifetime) throw new Error('AI4Math Live HUD already started')
    const owner = new AbortController()
    lifetime = owner
    const signal = owner.signal
    let activeHandle
    let timeout
    let cancelRetry
    let connection
    // A live push or status can be newer than alerts still missing inside the
    // gap. Only fully validated history pages can advance the history cursor.
    let alertCursor = 0

    // A newly ready connection fetches one batch. Subsequent batches need an
    // explicit user action, even when a page failed or the cap was reached.
    async function catchUp(conn) {
      if (!conn?.ready || conn.signal.aborted || conn.catchingUp) return
      conn.catchingUp = true
      update({ alertHistoryLoading: true })
      let incomplete = true
      try {
        for (let index = 0; index < ALERT_PAGE_CAP; index++) {
          const after = alertCursor
          const page = await call(remote, 'alertsList', { after, limit: ALERT_PAGE_SIZE }, conn.signal)
          if (conn.signal.aborted) return
          if (!Array.isArray(page.items) || page.items.length > ALERT_PAGE_SIZE
            || typeof page.truncated !== 'boolean' || (page.truncated && page.items.length === 0)) break
          let cursor = after
          for (const item of page.items) {
            if (!isAlert(item) || item.event_seq <= cursor) throw new Error('Invalid AI4Math alert page')
            cursor = item.event_seq
          }
          alertCursor = cursor
          const recentAlerts = mergeAlerts(view.recentAlerts, page.items)
          if (recentAlerts !== view.recentAlerts) update({ recentAlerts })
          if (!page.truncated) { incomplete = false; break }
        }
      } catch { /* A continuation can resume after the last validated page. */ }
      finally {
        conn.catchingUp = false
        if (!conn.signal.aborted) update({ alertHistoryIncomplete: incomplete, alertHistoryLoading: false })
      }
    }
    requestAlertCatchUp = () => {
      if (view.phase === 'live' && view.alertHistoryIncomplete) void catchUp(connection)
    }

    function delay(ms) {
      return new Promise(resolve => {
        cancelRetry = resolve
        timeout = schedule(resolve, ms)
      }).finally(() => { timeout = undefined; cancelRetry = undefined })
    }

    function clearSnapshotTimer(conn) {
      if (conn?.snapshotTimer === undefined) return
      const timer = conn.snapshotTimer
      conn.snapshotTimer = undefined
      cancel(timer)
    }
    function closeConnection() {
      clearSnapshotTimer(connection)
      connection?.controller.abort()
      connection = undefined
    }

    async function watch() {
      let retry = retryMs
      while (!signal.aborted) {
        const round = new AbortController()
        const roundSignal = AbortSignal.any([signal, round.signal])
        function accept(conn, snapshot) {
          if (!conn || conn.signal.aborted || !isSnapshot(snapshot)) return
          const prior = view.snapshot
          if (conn.ready && (snapshot.event_seq < prior.event_seq
            || (snapshot.event_seq === prior.event_seq && snapshot.observed_at < prior.observed_at))) return
          const first = !conn.ready
          // Older servers need no new fields; an equal sample can update the
          // display as before, but cannot prove newer telemetry arrived.
          const advances = first || snapshot.event_seq > prior.event_seq || snapshot.observed_at > prior.observed_at
          // Only a fresh connection can establish a lower sequence, such as
          // a daemon on a newly initialized or restored project. Do not carry
          // the old project's cursor/alert into that connection.
          const highWater = Math.max(prior?.event_seq ?? -1, view.recentAlerts[0]?.event_seq ?? -1)
          const reset = first && snapshot.event_seq < highWater
          if (reset) alertCursor = 0
          conn.ready = true
          const recentAlerts = mergeAlerts(reset ? NO_ALERTS : view.recentAlerts, [...conn.pendingAlerts, snapshot.last_alert])
          conn.pendingAlerts = NO_ALERTS
          // Use local elapsed time since acceptance, not the daemon's clock.
          // Expiry only marks this sample stale; the stream and cursor stay live.
          if (advances) {
            clearSnapshotTimer(conn)
            const timer = schedule(() => {
              if (conn.signal.aborted || conn.snapshotTimer !== timer) return
              conn.snapshotTimer = undefined
              update({ snapshotStale: true })
            }, SNAPSHOT_STALE_MS)
            conn.snapshotTimer = timer
          }
          update({ phase: 'live', snapshot, snapshotStale: !advances && view.snapshotStale,
            recentAlerts, ...(reset ? { alertHistoryIncomplete: false } : {}) })
          retry = retryMs
          // Wait for the first status or tick instead of querying with a
          // possibly wrong cursor before we can recognize a reset.
          if (first) void catchUp(conn)
        }
        function refresh(conn) {
          if (!conn || conn.signal.aborted) return
          conn.pending = true
          if (conn.reading) return
          conn.reading = true
          void (async () => {
            while (conn.pending && !conn.signal.aborted) {
              conn.pending = false
              try {
                accept(conn, await call(remote, 'status', {}, conn.signal))
              } catch { /* The stream may still deliver a newer snapshot. */ }
            }
            conn.reading = false
          })()
        }
        try {
          activeHandle = remote.subscribe({ topics: [...topics] }, roundSignal)
          for await (const message of activeHandle) {
            if (signal.aborted) break
            const data = message?.data
            switch (message?.topic) {
              case 'ai4math.connection':
                if (data?.state === 'reconnecting') {
                  closeConnection()
                  update({ phase: 'reconnecting', alertHistoryLoading: false })
                } else if (data?.state === 'connected') {
                  closeConnection()
                  const controller = new AbortController()
                  connection = { controller, signal: AbortSignal.any([roundSignal, controller.signal]),
                    ready: false, pending: false, reading: false, pendingAlerts: NO_ALERTS }
                  if (view.breaker || (view.snapshot && !view.snapshotStale)) {
                    update({ breaker: null, snapshotStale: view.snapshot !== null })
                  }
                  refresh(connection)
                }
                break
              case 'vital_signs.tick': accept(connection, data); break
              case 'alert.p0':
                if (!connection) break
                if (!connection.ready) connection.pendingAlerts = mergeAlerts(connection.pendingAlerts, [data])
                else {
                  const recentAlerts = mergeAlerts(view.recentAlerts, [data])
                  if (recentAlerts !== view.recentAlerts) update({ recentAlerts })
                }
                refresh(connection)
                break
              case 'breaker.state_changed':
                // Keep the latest event for this connection; replayed evidence
                // must not replace a newer blocked or verified outcome.
                if (connection && Number.isSafeInteger(data?.event_seq) && data.event_seq >= 0
                  && data.event_seq > (view.breaker?.event_seq ?? -1)) update({ breaker: data })
                refresh(connection)
                break
            }
          }
        } catch { /* No transport internals or Host addresses go into the HUD. */ }
        finally {
          closeConnection()
          round.abort()
          activeHandle?.dispose?.()
          activeHandle = undefined
        }
        if (signal.aborted) break
        update({ phase: 'reconnecting', alertHistoryLoading: false })
        await delay(retry)
        retry = Math.min(retry * 2, 30000)
      }
    }

    void watch()
    return () => {
      if (signal.aborted) return
      owner.abort()
      closeConnection()
      activeHandle?.dispose?.()
      if (timeout !== undefined) cancel(timeout)
      cancelRetry?.()
      lifetime = undefined
      requestAlertCatchUp = undefined
      view = INITIAL
    }
  }
  return { start, getSnapshot: () => view, continueAlertCatchUp: () => requestAlertCatchUp?.(), subscribe(listener) {
    listeners.add(listener)
    return () => listeners.delete(listener)
  } }
}

module.exports = { createLive }
