import { createServer } from 'node:net';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

export const line = (frame) => JSON.stringify({ jsonrpc: '2.0', ...frame }) + '\n';
export const reply = (socket, id, result) => socket.write(line({ id, result }));
export const push = (socket, method, params) => socket.write(line({ method, params }));

export async function fixture(t, handle) {
  const temp = await mkdtemp(join(tmpdir(), 'a4-'));
  const path = join(temp, 's');
  const sockets = new Set();
  let seq = 0;
  const server = createServer((socket) => {
    const generation = ++seq;
    sockets.add(socket);
    socket.once('close', () => sockets.delete(socket));
    socket.on('error', () => {});
    socket.setEncoding('utf8');
    let pending = '';
    socket.on('data', (chunk) => {
      pending += chunk;
      for (let i; (i = pending.indexOf('\n')) >= 0;) {
        const request = JSON.parse(pending.slice(0, i));
        pending = pending.slice(i + 1);
        handle(request, socket, generation);
      }
    });
  });
  t.after(async () => {
    for (const socket of sockets) socket.destroy();
    await new Promise((done) => server.close(done));
    // Only remove the test-created directory with the exact returned absolute path.
    await rm(temp, { recursive: true });
  });
  await new Promise((resolve, reject) => server.once('error', reject).listen(path, resolve));
  return path;
}
