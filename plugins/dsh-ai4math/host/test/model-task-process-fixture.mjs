// Local subprocess only: no adapter, profile, API or network.
import { writeFileSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { Socket } from 'node:net';
import { setTimeout as delay } from 'node:timers/promises';
import { MODEL_TASK_RESPONSE_MAX_BYTES } from '../model-task-process.mjs';

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const { version, packet, workerData: data } = JSON.parse(Buffer.concat(chunks));
const frame = (result) => JSON.stringify({ version, result }) + '\n';
const marker = (value = process.pid) => { if (data.marker) writeFileSync(data.marker, String(value)); };
const keepAlive = () => { setInterval(() => {}, 60_000); };

switch (data.mode) {
  case 'echo': {
    process.stderr.write(data.secret);
    const line = frame({
      packet,
      stdinMatched: data.secret === 'synthetic secret value',
      envCleared: process.env.AI4MATH_TEST_SYNTHETIC_SECRET === undefined && process.env.PATH === undefined,
      electronFlag: process.env.ELECTRON_RUN_AS_NODE === '1',
      argvClean: process.argv.length === 2 && !process.argv.some((arg) => arg.includes(data.secret)),
    });
    const bytes = Buffer.from(line);
    const split = bytes.indexOf(Buffer.from('π')) + 1;
    process.stdout.write(bytes.subarray(0, split));
    await delay(10);
    process.stdout.write(bytes.subarray(split));
    break;
  }
  case 'hang':
    marker();
    keepAlive();
    break;
  case 'lifeline': {
    const lifeline = new Socket({ fd: 3, readable: true, writable: false });
    lifeline.once('end', () => {
      writeFileSync(`${data.marker}.eof`, 'yes');
      process.exit(1);
    });
    lifeline.resume();
    marker();
    keepAlive();
    break;
  }
  case 'valid-then-hang':
    marker();
    process.stdout.write(frame(null));
    keepAlive();
    break;
  case 'group': {
    const descendant = spawn(process.execPath, ['-e', 'setInterval(() => {}, 60000)'], { stdio: ['ignore', 'inherit', 'inherit'] });
    await new Promise((resolve, reject) => { descendant.once('spawn', resolve); descendant.once('error', reject); });
    marker(`${process.pid} ${descendant.pid}`);
    keepAlive();
    break;
  }
  case 'ready-then-reply':
    marker();
    await delay(250);
    await new Promise((resolve) => process.stdout.write(frame(null), resolve));
    writeFileSync(`${data.marker}.sent`, 'yes');
    break;
  case 'crash':
    marker();
    process.stderr.write(data.secret);
    process.stdout.write(frame(null));
    process.exitCode = 7;
    break;
  case 'response-at-limit':
    process.stdout.write(frame('x'.repeat(MODEL_TASK_RESPONSE_MAX_BYTES - Buffer.byteLength(frame('')))));
    break;
  case 'oversize':
    process.stdout.write(frame('𝛼'.repeat(MODEL_TASK_RESPONSE_MAX_BYTES / 2)));
    keepAlive();
    break;
  case 'oversize-stderr':
    process.stderr.write('z'.repeat(70 * 1024));
    keepAlive();
    break;
  case 'raw':
    process.stdout.write(Buffer.from(data.frame, 'base64'));
    break;
  default:
    process.exitCode = 8;
}
