import assert from 'node:assert/strict';
import test from 'node:test';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { createTestTHSClient } from '@zcashlabs/thsjs';
import { assertClosed, waitForClosed } from './support/assert-closed.mjs';

const executable = fileURLToPath(new URL('./ths-fixture.mjs', import.meta.url));

for (const signal of ['SIGTERM', 'SIGKILL']) {
  for (const phase of ['ready', 'starting']) {
    test(`${signal} on a ${phase} worker closes only its own sandbox`, async t => {
      const survivor = createTestTHSClient({ executable, runtimeDir: 'normal', timeoutMs: 5000 });
      t.after(() => survivor.stop());
      await survivor.start();
      const directory = await mkdtemp(join(tmpdir(), 'ths-worker-'));
      t.after(() => rm(directory, { recursive: true, force: true }));
      const startupMarker = join(directory, 'started');
      const runtimeDir = phase === 'ready' ? 'slow-stop' : JSON.stringify({ mode: 'slow-stop', startupMarker });
      const worker = spawn(process.execPath, [fileURLToPath(new URL('./support/worker.mjs', import.meta.url)),
        JSON.stringify({ executable, runtimeDir, timeoutMs: 5000 })], { stdio: ['ignore', 'ignore', 'pipe', 'ipc'] });
      let diagnostics = '', config;
      worker.stderr.on('data', chunk => { diagnostics += chunk; });
      const exited = once(worker, 'exit');
      t.after(async () => {
        if (worker.exitCode === null && worker.signalCode === null) worker.kill('SIGTERM');
        await exited;
        if (config) {
          // Clean up leaked fixtures when this regression runs against an older client.
          try { await assertClosed(config); }
          catch (error) {
            if (!(error instanceof assert.AssertionError)) throw error;
            const response = await fetch(`${config.dashboard}/fixture/stop`);
            await response.body?.cancel(); await waitForClosed(config);
          }
        }
      });
      if (phase === 'ready') {
        [config] = await Promise.race([
          once(worker, 'message', { signal: AbortSignal.timeout(5000) }),
          exited.then(() => { throw Error(`Worker exited before ready: ${diagnostics}`); }),
        ]);
      } else {
        const deadline = Date.now() + 5000;
        while (!config) {
          try { config = JSON.parse(await readFile(startupMarker, 'utf8')); }
          catch (error) { if (error.code !== 'ENOENT') throw error; }
          assert.ok(Date.now() < deadline, `Startup fixture did not become ready: ${diagnostics}`);
          if (!config) await delay(10);
        }
      }
      worker.kill(signal);
      assert.deepEqual(await exited, [null, signal]);
      await waitForClosed(config);
      assert.equal(await survivor.rpc('getblockcount'), 200);
    });
  }
}
