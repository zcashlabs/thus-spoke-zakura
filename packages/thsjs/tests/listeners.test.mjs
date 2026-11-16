import assert from 'node:assert/strict';
import test from 'node:test';
import { createServer, Socket } from 'node:net';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { assertClosed, waitForClosed } from './support/assert-closed.mjs';

test('cleanup requires every TCP listener to close, even if it rejects HTTP', async t => {
  const server = createServer(socket => socket.destroy());
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => new Promise(resolve => server.close(resolve)));
  const open = `http://127.0.0.1:${server.address().port}`;
  const unused = createServer();
  unused.listen(0, '127.0.0.1'); await once(unused, 'listening');
  const closed = `http://127.0.0.1:${unused.address().port}`;
  await new Promise(resolve => unused.close(resolve));
  const endpoints = { dashboard: closed, rpc: closed, lightwalletd: closed, p2p: closed.slice(7) };
  await assertClosed(endpoints);
  for (const name of ['dashboard', 'rpc', 'lightwalletd', 'p2p']) {
    await assert.rejects(assertClosed({ ...endpoints, [name]: name === 'p2p' ? open.slice(7) : open }),
      /still listening/, `${name} must not be reported as stopped`);
  }
});

test('cleanup polling requires refusal after connection resets and never accepts a timeout', async t => {
  const endpoints = { dashboard: 'http://127.0.0.1:1', rpc: 'http://127.0.0.1:2',
    lightwalletd: 'http://127.0.0.1:3', p2p: '127.0.0.1:4' };
  let closed = false, timeout = false;
  t.mock.method(Socket.prototype, 'connect', function () {
    queueMicrotask(() => {
      if (timeout) this.emit('timeout');
      else this.emit('error', Object.assign(new Error('connection failed'),
        { code: closed ? 'ECONNREFUSED' : 'ECONNRESET' }));
    });
    return this;
  });
  const closing = delay(40).then(() => { closed = true; });
  try { await waitForClosed(endpoints); assert.equal(closed, true); }
  finally { await closing; }
  timeout = true;
  await assert.rejects(waitForClosed(endpoints), /Timed out.*cleanup is unconfirmed/);
});
