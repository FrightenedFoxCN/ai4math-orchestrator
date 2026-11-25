// Fixed subprocess entry for one advisory model assignment. Never mounted as a tool.
import { Socket } from 'node:net';
import { TextDecoder } from 'node:util';
import { Worker } from 'node:worker_threads';
import { MODEL_TASK_REQUEST_MAX_BYTES, MODEL_TASK_RESPONSE_MAX_BYTES } from './model-task-process.mjs';

function exact(value, keys) {
  return value && Object.getPrototypeOf(value) === Object.prototype &&
    Object.keys(value).length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}
const invalid = () => new Error('Invalid model process input.');

async function request() {
  const chunks = [];
  let size = 0;
  for await (const chunk of process.stdin) {
    size += chunk.length;
    if (size > MODEL_TASK_REQUEST_MAX_BYTES) throw invalid();
    chunks.push(chunk);
  }
  const text = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(Buffer.concat(chunks, size));
  if (!text.endsWith('\n') || text.indexOf('\n') !== text.length - 1) throw invalid();
  const body = JSON.parse(text);
  if (JSON.stringify(body) + '\n' !== text || !exact(body, ['version', 'packet', 'workerData']) || body.version !== 1 ||
      !exact(body.workerData, ['adapter', 'dshRoot', 'model', 'apiKey', 'baseUrl', 'allowRemote', 'maxModelCalls']) ||
      body.workerData.adapter !== 'deepseek' || typeof body.workerData.allowRemote !== 'boolean') throw invalid();
  const { dshRoot, model, apiKey, baseUrl, allowRemote, maxModelCalls } = body.workerData;
  return {
    packet: body.packet,
    route: { dshRoot, model, apiKey, baseUrl: baseUrl ?? undefined, allowRemote, maxModelCalls: maxModelCalls ?? undefined },
  };
}

const abort = () => process.exit(1);
let lifeline;
let deadline;
try {
  // fd 3 is held open by the parent, without payload. EOF also arrives on
  // abrupt parent death, independently of whether the SDK's thread cooperates.
  lifeline = new Socket({ fd: 3, readable: true, writable: false });
  for (const event of ['data', 'end', 'error', 'close']) lifeline.once(event, abort);
  deadline = setTimeout(abort, 60_000);
  const data = await request();
  const seconds = data.packet?.max_wall_seconds;
  if (!Number.isInteger(seconds) || seconds < 1 || seconds > 60) throw invalid();
  clearTimeout(deadline);
  deadline = setTimeout(abort, seconds * 1000);
  const worker = new Worker(new URL('./model-task-process-worker.mjs', import.meta.url), {
    workerData: data, execArgv: [], stdout: true, stderr: true,
  });
  let logs = 0;
  for (const pipe of [worker.stdout, worker.stderr]) {
    pipe.on('data', (bytes) => { logs += bytes.length; if (logs > 64 * 1024) abort(); });
    pipe.on('error', abort);
  }
  const frame = await new Promise((resolve, reject) => {
    let response;
    worker.on('message', (value) => {
      if (response !== undefined || typeof value !== 'string' || Buffer.byteLength(value) > MODEL_TASK_RESPONSE_MAX_BYTES) abort();
      response = value;
    });
    worker.once('error', reject);
    worker.once('exit', (code) => code === 0 && response !== undefined ? resolve(response) : reject(invalid()));
  });
  await new Promise((resolve, reject) => process.stdout.write(frame, (err) => err ? reject(err) : resolve()));
} catch {
  // Neither SDK exceptions nor request data belong in process diagnostics.
  abort();
} finally {
  clearTimeout(deadline);
  lifeline?.removeAllListeners();
  lifeline?.destroy();
}
