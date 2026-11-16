import assert from 'node:assert/strict';
import { Socket } from 'node:net';
import { setTimeout as delay } from 'node:timers/promises';

export async function assertClosed(endpoints) {
  for (const endpoint of [endpoints.dashboard, endpoints.rpc, endpoints.lightwalletd, `tcp://${endpoints.p2p}`]) {
    const url = new URL(endpoint);
    const socket = new Socket();
    try {
      const listening = await new Promise((resolve, reject) => {
        socket.once('connect', () => resolve(true));
        socket.once('error', error => error.code === 'ECONNREFUSED' ? resolve(false) : reject(error));
        socket.setTimeout(1000, () => reject(new Error(`Timed out checking ${endpoint}; cleanup is unconfirmed`)));
        socket.connect(Number(url.port), url.hostname);
      });
      assert.equal(listening, false, `${endpoint} is still listening`);
    } finally { socket.destroy(); }
  }
}

export async function waitForClosed(endpoints, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (true) {
    try { await assertClosed(endpoints); return; }
    catch (error) {
      // A connection can reset while a service stops; wait for connection refusal.
      if (!(error instanceof assert.AssertionError) && error.code !== 'ECONNRESET') throw error;
      if (Date.now() >= deadline) throw error;
    }
    await delay(20);
  }
}
