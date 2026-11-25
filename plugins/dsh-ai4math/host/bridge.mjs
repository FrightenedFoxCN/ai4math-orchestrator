import { setTimeout as delay } from 'node:timers/promises';
import { BridgeError, isObject, NdjsonConnection, requireSocketPath } from './connection.mjs';
import { REMOTE_METHODS } from './remote-contract.mjs';

export const AGENT_ROLES = Object.freeze(['chief', 'regional', 'explorer', 'builder', 'prover', 'automation', 'reviewer']);
// Global frontier / briefing / inbox may contain cross-task facts. No scoped view is available yet.
// In particular the blind Reviewer cannot retrieve orchestration history through this bridge.
const readWiki = Object.freeze(['status', 'wikiSearch']);
const roleMethods = Object.freeze({
  owner: Object.freeze(Object.keys(REMOTE_METHODS).filter(key => !REMOTE_METHODS[key].agentOnly && !REMOTE_METHODS[key].hostOnly)),
  chief: Object.freeze(['status', 'briefingPeek', 'frontierList', 'inboxList', 'wikiSearch', 'wikiCheckGraveyard', 'wikiProvenance']),
  regional: Object.freeze([...readWiki, 'wikiCheckGraveyard']),
  explorer: Object.freeze([...readWiki, 'wikiCheckGraveyard', 'wikiProvenance']),
  builder: Object.freeze([...readWiki, 'wikiCheckGraveyard']),
  prover: Object.freeze([...readWiki,'proofSubmit']),
  automation: Object.freeze([...readWiki,'proofSubmit']),
  reviewer: Object.freeze(['wikiSearch', 'wikiCheckGraveyard']),
});
const coreOnly = Object.freeze(['regional', 'builder', 'prover', 'automation']);
export const methodsForRole = (role) => Object.hasOwn(roleMethods, role) ? roleMethods[role] : Object.freeze([]);

function validString(rule, value) {
  if (typeof value !== 'string' || (rule.enum && !rule.enum.includes(value)) || value.includes('\0')) return false;
  const wikiMax = rule['x-ai4math-maxUtf8Bytes'];
  return !(wikiMax === undefined && rule.minLength && !value.trim()) &&
    !(rule.maxLength && Buffer.byteLength(value, 'utf8') > (wikiMax ?? rule.maxLength)) &&
    (!rule.pattern || new RegExp(rule.pattern, 'u').test(value));
}

/** Revalidate/reconstruct inputs even outside Gateway (e.g. on model tool calls). */
export function parseParams(key, params) {
  if (!Object.hasOwn(REMOTE_METHODS, key) || !isObject(params) ||
      ![Object.prototype, null].includes(Object.getPrototypeOf(params))) throw new BridgeError('invalid');
  const { properties, required } = REMOTE_METHODS[key].params;
  const entries = Reflect.ownKeys(params);
  if (required.some((name) => !Object.hasOwn(params, name)) ||
      entries.some((name) => !Object.hasOwn(properties, name))) throw new BridgeError('invalid');
  const result = {};
  for (const key of entries) {
    const prop = Object.getOwnPropertyDescriptor(params, key);
    if (!Object.hasOwn(prop, 'value')) throw new BridgeError('invalid');
    const rule = properties[key];
    const value = prop.value;
    if (rule.type === 'integer') {
      if (!Number.isInteger(value) || value < rule.minimum || value > rule.maximum) throw new BridgeError('invalid');
    } else if (rule.type === 'string') {
      if (!validString(rule, value)) throw new BridgeError('invalid');
    } else if (rule.type === 'boolean') {
      if (typeof value !== 'boolean') throw new BridgeError('invalid');
    } else if (rule.type === 'array') {
      if (!Array.isArray(value) || value.length < (rule.minItems ?? 0) || value.length > rule.maxItems) throw new BridgeError('invalid');
      const items = [];
      for (let index = 0; index < value.length; index++) {
        const item = Object.getOwnPropertyDescriptor(value, index);
        if (!item || !Object.hasOwn(item, 'value') || !validString(rule.items, item.value)) throw new BridgeError('invalid');
        items.push(item.value);
      }
      if (rule.uniqueItems && new Set(items).size !== items.length) throw new BridgeError('invalid');
      result[key] = items;
      continue;
    } else throw new BridgeError('invalid');
    result[key] = value;
  }
  return result;
}

export const MAX_STREAMS = 8;
const MAX_EVENT_QUEUE = 64;
const MAX_EVENT_QUEUE_BYTES = 2 * 1024 * 1024;
const CONNECTION_TOPIC = 'ai4math.connection';
const transition = (state) => ({ topic: CONNECTION_TOPIC, data: { state } });
const retryable = (error) => error instanceof BridgeError && ['disconnected', 'timeout'].includes(error.code);

class EventQueue {
  #items = [];
  #bytes = 0;
  #wake;
  #error;

  push(event, bytes) {
    if (this.#items.length >= MAX_EVENT_QUEUE || this.#bytes + bytes > MAX_EVENT_QUEUE_BYTES) {
      this.end(new BridgeError('overflow'));
      return false;
    }
    if (this.#error) return false;
    this.#items.push({ event, bytes });
    this.#bytes += bytes;
    this.#wake?.();
    return true;
  }

  end(error) {
    this.#error ??= error;
    this.#items.length = 0;
    this.#bytes = 0;
    this.#wake?.();
  }

  async next() {
    if (this.#items.length === 0 && !this.#error) {
      await new Promise((wake) => { this.#wake = wake; });
      this.#wake = undefined;
    }
    if (this.#error) throw this.#error;
    const item = this.#items.shift();
    this.#bytes -= item.bytes;
    return item.event;
  }
}

const withLifetime = (lifetime, signal) => signal ? AbortSignal.any([lifetime, signal]) : lifetime;

/** Factory kept private to the Host composition. Role is bound once from trusted context. */
export function createBridge({ socketPath, timeoutMs, reconnectMinMs = 250, reconnectMaxMs = 5000 }) {
  let path = socketPath === undefined ? undefined : requireSocketPath(socketPath);
  const lifetime = new AbortController();
  const connections = new Set();
  const sleepers = new Set();
  let unary;
  let streams = 0;
  let proofCalls = 0;

  function open(options) {
    if (!path) throw new BridgeError('unconfigured');
    const conn = new NdjsonConnection(path, { timeoutMs, ...options });
    connections.add(conn);
    return conn;
  }

  function wakeSleepers() {
    for (const sleeper of [...sleepers]) sleeper.abort();
    sleepers.clear();
  }

  function disconnect() {
    unary = undefined;
    const error = new BridgeError('disconnected');
    for (const conn of [...connections]) conn.close(error);
    connections.clear();
  }

  function retarget(nextSocketPath) {
    const validated = nextSocketPath === undefined ? undefined : requireSocketPath(nextSocketPath);
    if (validated === path) {
      wakeSleepers();
      return false;
    }
    path = validated;
    disconnect();
    wakeSleepers();
    return true;
  }

  async function* follow(topics, callerSignal) {
    const signal = withLifetime(lifetime.signal, callerSignal);
    if (signal.aborted) return;
    if (streams >= MAX_STREAMS) throw new BridgeError('busy');
    streams++;
    try {
      let backoff = reconnectMinMs;
      let reconnecting = false;
      for (;;) {
        if (signal.aborted) return;
        const queue = new EventQueue();
        let conn;
        let connected = false;
        const abort = () => conn.close();
        try {
          conn = open({
            onDisconnect: (error) => queue.end(error),
            onNotification: (event, bytes) => {
              if (!topics.includes(event.topic)) {
                conn.close(new BridgeError('protocol'));
              } else if (!queue.push(event, bytes)) conn.close(new BridgeError('overflow'));
            },
          });
          signal.addEventListener('abort', abort, { once: true });
          const ack = await conn.request('subscribe', { topics }, signal);
          if (!isObject(ack) || !Array.isArray(ack.topics) || ack.topics.length !== topics.length ||
              topics.some((topic) => !ack.topics.includes(topic))) throw new BridgeError('protocol');
          if (signal.aborted) return;
          connected = true;
          reconnecting = false;
          yield transition('connected');
          while (!signal.aborted) {
            const event = await queue.next();
            if (signal.aborted) return;
            backoff = reconnectMinMs;
            yield event;
          }
          return;
        } catch (error) {
          if (signal.aborted) return;
          if (!retryable(error)) throw error;
        } finally {
          signal.removeEventListener('abort', abort);
          if (conn) {
            // Subscription ownership is the dedicated daemon connection; close on every exit.
            conn.close();
            connections.delete(conn);
          }
        }
        if (connected || !reconnecting) {
          reconnecting = true;
          yield transition('reconnecting');
        }
        const wake = new AbortController();
        sleepers.add(wake);
        try {
          await delay(Math.ceil(backoff * (0.5 + Math.random() / 2)), undefined, { signal: AbortSignal.any([signal, wake.signal]) });
          backoff = Math.min(backoff * 2, reconnectMaxMs);
        } catch (error) {
          if (signal.aborted) return;
          if (wake.signal.aborted) {
            backoff = reconnectMinMs;
          } else throw error;
        } finally {
          sleepers.delete(wake);
        }
      }
    } finally {
      streams--;
    }
  }

  return Object.freeze({
    socketPath: () => path,
    retarget,
    disconnect,
    wake: wakeSleepers,
    bind(role, { proofSlots = [] } = {}) {
      const allowed = methodsForRole(role);
      const assigned = new Set(proofSlots);
      const check = (key, params) => {
        if (!allowed.includes(key)) throw new BridgeError('denied');
        const parsed = parseParams(key, params);
        if (key === 'proofSubmit' && !assigned.has(parsed.target_id)) throw new BridgeError('denied');
        if (key === 'wikiSearch' && coreOnly.includes(role) && parsed.scope !== undefined && parsed.scope !== 'core') {
          throw new BridgeError('denied');
        }
        return parsed;
      };
      return Object.freeze({
        async call(key, params, signal) {
          const args = check(key, params);
          if (REMOTE_METHODS[key].stream) throw new BridgeError('denied');
          const operationSignal = withLifetime(lifetime.signal, signal);
          if (operationSignal.aborted) throw new BridgeError('cancelled');
          let result;
          if (key === 'proofSubmit') {
            if (proofCalls >= 4) throw new BridgeError('busy');
            proofCalls++;
            let isolated;
            try {
              isolated = open({ timeoutMs: Math.max(timeoutMs ?? 0, 35_000) });
              result = await isolated.request(REMOTE_METHODS[key].method, args, operationSignal);
            } finally {
              if (isolated) { isolated.close(); connections.delete(isolated); }
              proofCalls--;
            }
          } else {
            unary ??= open();
            result = await unary.request(REMOTE_METHODS[key].method, args, operationSignal);
          }
          if (!isObject(result)) throw new BridgeError('protocol');
          return result;
        },
        async *subscribe(params, signal) {
          yield* follow(check('subscribe', params).topics, signal);
        },
      });
    },
    close() {
      lifetime.abort();
      for (const conn of connections) conn.close();
      connections.clear();
    },
  });
}
