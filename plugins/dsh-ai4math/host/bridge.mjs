import { setTimeout as delay } from 'node:timers/promises';
import { BridgeError, isObject, NdjsonConnection, requireSocketPath } from './connection.mjs';
import { REMOTE_METHODS } from './remote-contract.mjs';

export const AGENT_ROLES = Object.freeze(['chief', 'regional', 'explorer', 'builder', 'prover', 'automation', 'reviewer']);
// Global frontier / briefing / inbox may contain cross-task facts. No scoped view is available yet.
// In particular the blind Reviewer cannot retrieve orchestration history through this bridge.
const readWiki = Object.freeze(['status', 'wikiSearch']);
const readGraph = Object.freeze(['claimRead', 'planRead']);
const roleMethods = Object.freeze({
  owner: Object.freeze(Object.keys(REMOTE_METHODS).filter(key => !REMOTE_METHODS[key].agentOnly && !REMOTE_METHODS[key].hostOnly)),
  chief: Object.freeze(['status', 'briefingPeek', 'frontierList', 'inboxList', 'wikiSearch', 'wikiCheckGraveyard', 'wikiProvenance',
    ...readGraph, 'claimCreate', 'planCreate', 'claimApplyOracle', 'inboxPush', 'rfcApprove', 'rfcReject', 'rfcSupersede', 'rfcInspect', 'taskRegister', 'taskStatus', 'promotionPreview', 'promotionStatus']),
  regional: Object.freeze([...readWiki, ...readGraph, 'wikiCheckGraveyard', 'rfcInspect', 'taskStatus']),
  explorer: Object.freeze([...readWiki, ...readGraph, 'wikiCheckGraveyard', 'wikiProvenance', 'rfcSubmit', 'rfcInspect']),
  builder: Object.freeze([...readWiki, ...readGraph, 'wikiCheckGraveyard', 'rfcInspect', 'rfcSmokeStart', 'rfcFreeze', 'rfcComplete']),
  prover: Object.freeze([...readWiki,'proofSubmit','workerAcquire','workerSubmit','workerCancel','workerStatus']),
  automation: Object.freeze([...readWiki,'proofSubmit']),
  reviewer: Object.freeze(['wikiSearch', 'wikiCheckGraveyard', ...readGraph, 'rfcInspect','workerAcquire','workerSubmit','workerCancel','workerStatus']),
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


function validJson(value, depth = 0) {
  if (depth > 64 || (typeof value === 'number' && !Number.isFinite(value))) return false;
  if (value === null || typeof value === 'string') return typeof value !== 'string' || !value.includes('\0');
  if (typeof value === 'number' || typeof value === 'boolean') return true;
  if (Array.isArray(value)) {
    for (let i = 0; i < value.length; i++) {
      const item = Object.getOwnPropertyDescriptor(value, i);
      if (!item || !Object.hasOwn(item, 'value') || !validJson(item.value, depth + 1)) return false;
    }
    return Object.keys(value).length === value.length;
  }
  if (!isObject(value) || ![Object.prototype, null].includes(Object.getPrototypeOf(value))) return false;
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== 'string') return false;
    const item = Object.getOwnPropertyDescriptor(value, key);
    if (!item || !Object.hasOwn(item, 'value') || !validJson(item.value, depth + 1)) return false;
  }
  return true;
}

function validValue(rule, value, depth = 0) {
  if (depth > 64) return false;
  if (rule.type === 'string') return validString(rule, value);
  if (rule.type === 'integer') return Number.isInteger(value) &&
    (rule.minimum === undefined || value >= rule.minimum) &&
    (rule.maximum === undefined || value <= rule.maximum);
  if (rule.type === 'number') return typeof value === 'number' && Number.isFinite(value) &&
    (rule.minimum === undefined || value >= rule.minimum) &&
    (rule.maximum === undefined || value <= rule.maximum);
  if (rule.type === 'boolean') return typeof value === 'boolean';
  if (rule.type === 'array') {
    if (!Array.isArray(value) || value.length < (rule.minItems ?? 0) || value.length > rule.maxItems) return false;
    const items = [];
    for (let i = 0; i < value.length; i++) {
      const item = Object.getOwnPropertyDescriptor(value, i);
      if (!item || !Object.hasOwn(item, 'value') || !validValue(rule.items, item.value, depth + 1)) return false;
      items.push(item.value);
    }
    if (Object.keys(value).length !== value.length) return false;
    return !rule.uniqueItems || new Set(items.map(item => JSON.stringify(item))).size === items.length;
  }
  if (rule.type === 'object') {
    if (!isObject(value) || ![Object.prototype, null].includes(Object.getPrototypeOf(value))) return false;
    const properties = rule.properties ?? {};
    const required = rule.required ?? [];
    const keys = Reflect.ownKeys(value);
    if (keys.some(key => typeof key !== 'string') || required.some(name => !Object.hasOwn(value, name))) return false;
    if (rule.additionalProperties === false && keys.some(name => !Object.hasOwn(properties, name))) return false;
    for (const key of keys) {
      const item = Object.getOwnPropertyDescriptor(value, key);
      if (!item || !Object.hasOwn(item, 'value')) return false;
      if (Object.hasOwn(properties, key)) {
        if (!validValue(properties[key], item.value, depth + 1)) return false;
      } else if (!validJson(item.value, depth + 1)) return false;
    }
    return true;
  }
  return false;
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
    if (!validValue(rule, value)) throw new BridgeError('invalid');
    try { result[key] = structuredClone(value); }
    catch { throw new BridgeError('invalid'); }
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
  const writeConnections = new Map();
  let streams = 0;
  let proofCalls = 0;

  function open(options) {
    if (!path) throw new BridgeError('unconfigured');
    const conn = new NdjsonConnection(path, { timeoutMs, ...options });
    connections.add(conn);
    return conn;
  }

  async function boundWrite(key, role, bindingId, method, params, signal) {
    let entry = writeConnections.get(key);
    if (!entry) {
      entry = { conn:undefined, bound:false, binding:undefined };
      writeConnections.set(key, entry);
      entry.conn = open({
        onDisconnect:() => { if (writeConnections.get(key) === entry) writeConnections.delete(key); },
      });
    }
    if (!entry.bound) {
      const bindParams = bindingId === undefined ? { role } : { role, binding_id:bindingId };
      entry.binding ??= entry.conn.request('session.bind', bindParams, lifetime.signal)
        .then(() => { entry.bound = true; })
        .catch(error => {
          entry.conn.close(error);
          if (writeConnections.get(key) === entry) writeConnections.delete(key);
          throw error;
        })
        .finally(() => { entry.binding = undefined; });
      await entry.binding;
    }
    return entry.conn.request(method, params, signal);
  }

  function wakeSleepers() {
    for (const sleeper of [...sleepers]) sleeper.abort();
    sleepers.clear();
  }

  function disconnect() {
    unary = undefined;
    writeConnections.clear();
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
    bind(role, { proofSlots = [], bindingId } = {}) {
      if (bindingId !== undefined &&
          (typeof bindingId !== 'string' || !bindingId || bindingId.length > 128 ||
           !/^[A-Za-z0-9_.:-]+$/.test(bindingId))) throw new BridgeError('invalid');
      const writeKey = bindingId ?? role;
      const allowed = methodsForRole(role);
      const assigned = new Set(proofSlots);
      const check = (key, params) => {
        if (!allowed.includes(key)) throw new BridgeError('denied');
        const parsed = parseParams(key, params);
        if (key === 'proofSubmit' && !assigned.has(parsed.target_id)) throw new BridgeError('denied');
        if (key === 'planCreate') {
          const claims = (parsed.dependencies ?? []).map(dep => dep.claim_id);
          if (new Set(claims).size !== claims.length) throw new BridgeError('invalid');
        }
        if (key === 'taskRegister' && (parsed.dependencies ?? []).includes(parsed.task_id)) {
          throw new BridgeError('invalid');
        }
        if ((key === 'rfcReject' || key === 'rfcSupersede') &&
            (parsed.reason.trim() !== parsed.reason || /[\u0000-\u001f\u007f]/.test(parsed.reason))) {
          throw new BridgeError('invalid');
        }
        if (key === 'workerSubmit') {
          const suggestion = parsed.suggestion;
          const keys = Object.keys(suggestion).sort().join(',');
          const valid = role === 'prover'
            ? suggestion.kind === 'proof_body' && keys === 'kind,proof_body'
            : role === 'reviewer' && suggestion.kind === 'review' && keys === 'kind,rationale,verdict';
          if (!valid) throw new BridgeError('invalid');
        }
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
          if (REMOTE_METHODS[key].agentWrite || REMOTE_METHODS[key].principalBound) {
            result = await boundWrite(writeKey, role, bindingId, REMOTE_METHODS[key].method, args, operationSignal);
          } else if (key === 'proofSubmit') {
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
