import { createConnection } from 'node:net';
import { isAbsolute } from 'node:path';
import { TextDecoder } from 'node:util';

export const MAX_FRAME_BYTES = 1024 * 1024; // JSON bytes, excluding LF (and optional CR).
export const MAX_PENDING = 16;
const messages = {
  unconfigured: 'The ai4math daemon socket path is not configured.',
  disconnected: 'The ai4math daemon socket is unavailable or closed.',
  cancelled: 'The ai4math operation was cancelled.',
  timeout: 'The ai4math daemon did not respond before the deadline.',
  protocol: 'The ai4math daemon sent an invalid NDJSON RPC frame.',
  busy: 'The ai4math bridge reached its in-flight operation limit.',
  overflow: 'The ai4math event consumer could not keep up.',
  daemon: 'The ai4math daemon refused the operation.',
  denied: 'This ai4math role does not permit the operation.',
  invalid: 'Invalid ai4math operation parameters.',
};

export class BridgeError extends Error {
  constructor(code, rpcCode) {
    const detail = code === 'daemon' && rpcCode === -32006
      ? ' Initialize the managed Git repository in the Workbench before registering tasks.'
      : '';
    super(messages[code] + detail);
    this.name = 'Ai4mathBridgeError';
    this.code = code;
    if (rpcCode !== undefined) this.rpcCode = rpcCode;
  }
}

export function requireSocketPath(value) {
  // Reject abstract sockets, network endpoint strings, and implicit process cwd.
  // Darwin/BSD sun_path provides 104 bytes including the NUL terminator.
  const permittedBytes = process.platform === 'linux' ? 107 : 103;
  if (typeof value !== 'string' || !isAbsolute(value) || value.includes('\0') || Buffer.byteLength(value) > permittedBytes) {
    throw new BridgeError('unconfigured');
  }
  return value;
}

export const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);
const exactKeys = (record, names) => Object.keys(record).every((name) => names.includes(name));
const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

function validateJson(value, depth = 0) {
  // Protect downstream validators/encoders from unbounded recursive nesting.
  if (depth > 64 || (typeof value === 'number' && !Number.isFinite(value))) throw new BridgeError('protocol');
  if (value !== null && typeof value === 'object') {
    for (const key of Object.keys(value)) validateJson(value[key], depth + 1);
  }
}

/** One bounded concurrent RPC connection. Reconnects on the next new request, never replays one. */
export class NdjsonConnection {
  #path;
  #timeout;
  #max;
  #notify;
  #offline;
  #conn;
  #nextId = 0;
  #pending = new Map();
  #disposed = false;

  constructor(path, { timeoutMs = 5000, maxFrameBytes = MAX_FRAME_BYTES, onNotification, onDisconnect } = {}) {
    this.#path = requireSocketPath(path);
    if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 60_000 ||
        !Number.isInteger(maxFrameBytes) || maxFrameBytes < 64 || maxFrameBytes > MAX_FRAME_BYTES) {
      throw new BridgeError('invalid');
    }
    this.#timeout = timeoutMs;
    this.#max = maxFrameBytes;
    this.#notify = onNotification;
    this.#offline = onDisconnect;
  }

  async request(method, params, signal) {
    if (this.#disposed || signal?.aborted) throw new BridgeError('cancelled');
    if (this.#pending.size >= MAX_PENDING) throw new BridgeError('busy');
    const id = String(++this.#nextId);
    const line = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    const bytes = Buffer.byteLength(line);
    if (bytes - 1 > this.#max) throw new BridgeError('invalid');
    const conn = this.#conn ?? this.#connect();
    return new Promise((resolve, reject) => {
      const onAbort = () => settle(new BridgeError('cancelled'));
      const timer = setTimeout(() => settle(new BridgeError('timeout')), this.#timeout);
      const settle = (error, value) => {
        if (!this.#pending.delete(id)) return;
        clearTimeout(timer);
        signal?.removeEventListener('abort', onAbort);
        if (error) reject(error);
        else resolve(value);
      };
      this.#pending.set(id, { conn, settle });
      signal?.addEventListener('abort', onAbort, { once: true });
      if (signal?.aborted) return onAbort();
      conn.ready.then(() => {
        if (!this.#pending.has(id)) return;
        if (this.#conn !== conn) return settle(new BridgeError('disconnected'));
        // Bound aggregate writable buffering across cancellations and timeouts.
        if (conn.socket.writableLength + bytes > 2 * this.#max + 2) return settle(new BridgeError('busy'));
        conn.socket.write(line, (error) => {
          if (error) this.#drop(conn, new BridgeError('disconnected'));
        });
      }, settle);
    });
  }

  #connect() {
    const socket = createConnection({ path: this.#path });
    const conn = { socket, partial: undefined, used: 0, fail: undefined, ready: undefined, timer: undefined };
    conn.ready = new Promise((resolve, reject) => {
      conn.fail = reject;
      socket.once('connect', () => { clearTimeout(conn.timer); resolve(); });
    });
    conn.timer = setTimeout(() => this.#drop(conn, new BridgeError('timeout')), this.#timeout);
    this.#conn = conn;
    socket.on('data', (chunk) => {
      if (this.#conn !== conn) return;
      try { this.#consume(conn, chunk); }
      catch { this.#drop(conn, new BridgeError('protocol')); }
    });
    socket.on('error', () => this.#drop(conn, new BridgeError('disconnected')));
    socket.on('end', () => this.#drop(conn, new BridgeError(conn.used ? 'protocol' : 'disconnected')));
    socket.on('close', () => this.#drop(conn, new BridgeError('disconnected')));
    return conn;
  }

  #consume(conn, chunk) {
    for (let from = 0; from < chunk.length && this.#conn === conn;) {
      const newline = chunk.indexOf(10, from);
      const part = chunk.subarray(from, newline < 0 ? chunk.length : newline);
      const used = conn.used + part.length;
      if (used > this.#max + 1 || (used === this.#max + 1 && (part.at(-1) ?? conn.partial?.[conn.used - 1]) !== 13)) {
        throw new BridgeError('protocol');
      }
      if (newline < 0 || conn.used > 0) {
        conn.partial ??= Buffer.allocUnsafe(this.#max + 1);
        part.copy(conn.partial, conn.used);
        conn.used = used;
      }
      if (newline < 0) return;
      const frame = conn.used > 0 ? conn.partial.subarray(0, conn.used) : part;
      conn.used = 0;
      this.#message(conn, frame.at(-1) === 13 ? frame.subarray(0, -1) : frame);
      from = newline + 1;
    }
  }

  #message(conn, bytes) {
    const message = JSON.parse(decoder.decode(bytes));
    if (!isObject(message) || message.jsonrpc !== '2.0') throw new BridgeError('protocol');
    validateJson(message);
    if (!Object.hasOwn(message, 'id')) {
      if (!this.#notify || !exactKeys(message, ['jsonrpc', 'method', 'params']) ||
          typeof message.method !== 'string' || !isObject(message.params)) throw new BridgeError('protocol');
      this.#notify({ topic: message.method, data: message.params }, bytes.length);
      return;
    }
    if (typeof message.id !== 'string' || !exactKeys(message, ['jsonrpc', 'id', 'result', 'error']) ||
        Object.hasOwn(message, 'result') === Object.hasOwn(message, 'error')) throw new BridgeError('protocol');
    let error;
    if (Object.hasOwn(message, 'error')) {
      if (!isObject(message.error) || !Number.isSafeInteger(message.error.code) || typeof message.error.message !== 'string') {
        throw new BridgeError('protocol');
      }
      // The daemon may include internal paths or untrusted input in its error. Never relay it.
      error = new BridgeError('daemon', message.error.code);
    }
    const pending = this.#pending.get(message.id);
    if (pending?.conn === conn) pending.settle(error, message.result);
  }

  #drop(conn, error) {
    if (this.#conn !== conn) return;
    this.#conn = undefined;
    clearTimeout(conn.timer);
    conn.fail(error);
    conn.socket.destroy();
    for (const pending of this.#pending.values()) if (pending.conn === conn) pending.settle(error);
    this.#offline?.(error);
  }

  close(error = new BridgeError('cancelled')) {
    if (this.#disposed) return;
    this.#disposed = true;
    if (this.#conn) this.#drop(this.#conn, error);
  }
}
