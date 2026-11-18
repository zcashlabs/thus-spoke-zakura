import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { once } from 'node:events';
import { createServer } from 'node:http';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { createTestTHSClient } from '@zcashlabs/thsjs';
import * as testing from '@zcashlabs/thsjs';
const executable = new URL('./ths-fixture.mjs', import.meta.url).pathname;
const client = runtimeDir => createTestTHSClient({ executable, runtimeDir, timeoutMs: 2000 });

async function startupFixture(t, mode) {
  const directory = await mkdtemp(join(tmpdir(), 'ths-startup-'));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const startupMarker = join(directory, 'started');
  return {
    options: { executable, runtimeDir: JSON.stringify({ mode, startupMarker }), timeoutMs: 5000 },
    async wait() {
      const deadline = Date.now() + 5000;
      while (true) {
        try { await readFile(startupMarker); return; }
        catch (error) { if (error.code !== 'ENOENT') throw error; }
        assert.ok(Date.now() < deadline, 'fixture did not install its shutdown handler');
        await delay(10);
      }
    },
  };
}

test('coalesces start, parses split JSON, exposes frozen config, stops repeatedly and restarts fresh', async t => {
  const ths = client('split'); t.after(() => ths.stop());
  assert.throws(() => ths.config(), /ready/i);
  const [a, b] = await Promise.all([ths.start(), ths.start()]);
  assert.match(a.instance, /^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$/);
  assert.equal(a, b); assert.equal(a, ths.config()); assert.ok(Object.isFrozen(a));
  const details = await (await fetch(`${a.dashboard}/fixture`)).json();
  assert.deepEqual(details.args, ['--name', a.instance, '--json', 'start', '--native', '--runtime-dir', 'split',
    '--timeout', '2', '--no-open', '--no-auto-mine']);
  await ths.stop(); await ths.stop(); assert.throws(() => ths.config(), /ready/i);
  assert.notEqual((await ths.start()).instance, a.instance);
});

test('THS requests stay on loopback when the Node process enables a proxy', async t => {
  let proxied = 0;
  const proxy = createServer((_request, response) => { proxied++; response.writeHead(502).end(); });
  proxy.on('connect', (_request, socket) => {
    proxied++;
    socket.end('HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n');
  });
  proxy.listen(0, '127.0.0.1');
  await once(proxy, 'listening');
  t.after(() => new Promise(resolve => proxy.close(resolve)));
  const url = `http://127.0.0.1:${proxy.address().port}`;
  await promisify(execFile)(process.execPath, ['--input-type=module', '-e', `
    import assert from 'node:assert/strict';
    import { withTestTHS } from '@zcashlabs/thsjs';
    await withTestTHS(async ths => assert.equal(await ths.rpc('getblockcount'), 200),
      { executable: ${JSON.stringify(executable)}, runtimeDir: 'normal', timeoutMs: 3000 });
  `], { cwd: new URL('..', import.meta.url), timeout: 10_000,
    env: { ...process.env, NODE_USE_ENV_PROXY: '1', HTTP_PROXY: url, http_proxy: url, NO_PROXY: '', no_proxy: '' } });
  assert.equal(proxied, 0);
});

for (const mode of ['public', 'credentials', 'bad-p2p', 'identity', 'network', 'rpc-network', 'genesis', 'redirect', 'tls', 'url-path', 'zero-port', 'wallet-error', 'invalid-json', 'oversized']) {
  test(`rejects unsafe startup: ${mode}`, async t => {
    const ths = client(mode); t.after(() => ths.stop());
    await assert.rejects(ths.start());
    assert.throws(() => ths.config()); await ths.stop();
  });
}
test('validates parameters before mutation, sends exact payloads and waits for settlement', async t => {
  const ths = client('normal'); t.after(() => ths.stop());
  await ths.start();
  for (const n of [0, -1, 1.5, NaN, Infinity, '1', 10001]) await assert.rejects(ths.produceBlock(n));
  for (const method of ['reorg', 'rollback']) {
    for (const n of [0, -1, 1.5, 1001]) await assert.rejects(ths[method](n));
  }
  await assert.rejects(ths.reorg(1, { keepTransactions: 'yes' }));
  await ths.produceBlock(10); await ths.reorg(2, { keepTransactions: true }); await ths.reorg(1); await ths.rollback(3);
  const details = await (await fetch(`${ths.config().dashboard}/fixture`)).json();
  assert.deepEqual(details.calls, [['/api/v1/mine', { blocks: 10 }], ['/api/v1/reorg', { depth: 2, keep_txs: true }],
    ['/api/v1/reorg', { depth: 1, keep_txs: false }], ['/api/v1/rollback', { depth: 3 }]]);
  assert.ok(details.pending < 0);
  await fetch(`${ths.config().dashboard}/fixture`, { method: 'POST' });
  await assert.rejects(ths.produceBlock(1), /identity/i);
  assert.equal((await (await fetch(`${ths.config().dashboard}/fixture`)).json()).calls.length, 4);
});
test('stop cancels startup and awaits process cleanup', async t => {
  const fixture = await startupFixture(t, 'slow');
  const ths = createTestTHSClient(fixture.options); t.after(() => ths.stop());
  const starting = ths.start();
  const rejected = assert.rejects(starting);
  await fixture.wait(); await Promise.all([ths.stop(), ths.stop()]); await rejected; await ths.stop();
});
for (const runtimeDir of ['no-output', 'hang-http', 'unsettled']) {
  test(`bounds startup timeout: ${runtimeDir}`, async () => {
    const ths = createTestTHSClient({ executable, runtimeDir, timeoutMs: 150 });
    await assert.rejects(ths.start()); await ths.stop();
  });
}
test('spawn errors and process errors are observable', async () => {
  await assert.rejects(createTestTHSClient({ executable: '/nonexistent/ths', timeoutMs: 150 }).start(), /ENOENT/);
  await assert.rejects(client('exit').start(), /7|fixture service failed/);
  const ths = client('late-exit'); await ths.start(); await delay(400);
  assert.throws(() => ths.config(), /exit|failure/i);
  await assert.rejects(ths.produceBlock(1), /exit|failure/i);
  await assert.rejects(ths.start(), /exit|failure/i);
  await assert.rejects(ths.stop(), /9|late service failure/); await ths.stop();
});
test('mutation API errors are observable', async t => {
  const ths = client('api-error'); t.after(() => ths.stop()); await ths.start();
  await assert.rejects(ths.produceBlock(1), /500/);
});
test('shutdown timeout stays observable and blocks restart until cleanup is retried', async () => {
  const ths = createTestTHSClient({ executable, runtimeDir: 'ignore-stop', timeoutMs: 1000 });
  await ths.start();
  await assert.rejects(ths.stop(), /cleanup is unconfirmed/);
  assert.throws(() => ths.config(), /cancelled/);
  await assert.rejects(ths.start(), /stopping/);
  await ths.stop(); await ths.stop();
});
test('stop waits for slow owned cleanup', async () => {
  const ths = client('slow-stop'); await ths.start();
  const began = Date.now(); await ths.stop(); assert.ok(Date.now() - began >= 140);
});
test('a response arriving around process exit never yields a usable client', async () => {
  for (let attempt = 0; attempt < 25; attempt++) {
    const ths = client('output-exit');
    try {
      await assert.rejects(ths.start());
      assert.throws(() => ths.config());
    } finally { await ths.stop(); }
  }
});
test('mutation requests have a total bounded timeout', async t => {
  const ths = createTestTHSClient({ executable, runtimeDir: 'hang-mutation', timeoutMs: 1000 });
  t.after(() => ths.stop()); await ths.start();
  await assert.rejects(ths.produceBlock(1), /timeout|aborted/i);
});

test('rejects invalid configuration before launching', () => {
  for (const timeoutMs of [0, -1, NaN, Infinity, 1.5, 2147483648, '100']) {
    assert.throws(() => createTestTHSClient({ timeoutMs }));
  }
  assert.throws(() => createTestTHSClient({ executable: '' }));
  assert.throws(() => createTestTHSClient({ runtimeDir: '' }));
});

test('accepts native graceful SIGTERM exit status only for its requested shutdown', async () => {
  const ths = client('term-code'); await ths.start(); await ths.stop(); await ths.stop();
});

test('unrequested exit 143 remains a service failure', async () => {
  const ths = client('late-term-exit'); await ths.start(); await delay(400);
  assert.throws(() => ths.config(), /143/);
  await assert.rejects(ths.stop(), /143/); await ths.stop();
});

test('startup cancellation with native code 1 waits for cleanup and preserves the failure diagnostic', async t => {
  const fixture = await startupFixture(t, 'cancel-error');
  const ths = createTestTHSClient(fixture.options); const starting = ths.start();
  const rejected = assert.rejects(starting, /failure logs retained.*|interrupted/);
  await fixture.wait(); const began = Date.now();
  await assert.rejects(ths.stop(), /failure logs retained.*|interrupted/);
  assert.ok(Date.now() - began >= 140); await rejected; await ths.stop();
});

test('test scope returns the callback result and closes its sandbox', async () => {
  let endpoint;
  const result = await testing.withTestTHS(async ths => {
    endpoint = ths.config().dashboard;
    await ths.produceBlock(2);
    return 42;
  }, { executable, runtimeDir: 'normal', timeoutMs: 2000 });
  assert.equal(result, 42);
  await assert.rejects(fetch(endpoint));
});

test('test scope preserves an assertion failure and still closes the sandbox', async () => {
  const failure = new Error('application assertion failed');
  let endpoint;
  await assert.rejects(testing.withTestTHS(ths => {
    endpoint = ths.config().dashboard;
    throw failure;
  }, { executable, runtimeDir: 'normal', timeoutMs: 2000 }), error => error === failure);
  await assert.rejects(fetch(endpoint));
});

test('test scope reports both the test failure and unexpected launcher failure', async () => {
  const failure = new Error('application assertion failed');
  await assert.rejects(testing.withTestTHS(async () => {
    await delay(400);
    throw failure;
  }, { executable, runtimeDir: 'late-exit', timeoutMs: 2000 }), error => {
    assert.ok(error instanceof AggregateError);
    assert.equal(error.errors[0], failure);
    assert.match(error.errors[1].message, /9|late service failure/);
    return true;
  });
});

test('test scope never invokes the application when startup fails', async () => {
  let invoked = false;
  await assert.rejects(testing.withTestTHS(() => { invoked = true; },
    { executable, runtimeDir: 'genesis', timeoutMs: 2000 }), /genesis/i);
  assert.equal(invoked, false);
});

test('concurrent test scopes have separate instances and clean up independently', async () => {
  let other;
  await testing.withTestTHS(async outer => {
    const own = outer.config();
    await testing.withTestTHS(inner => {
      other = inner.config();
      assert.notEqual(other.instance, own.instance);
      assert.notEqual(other.dashboard, own.dashboard);
    }, { executable, runtimeDir: 'normal', timeoutMs: 2000 });
    await assert.rejects(fetch(other.dashboard));
    assert.equal(await outer.rpc('getblockcount'), 200);
  }, { executable, runtimeDir: 'normal', timeoutMs: 2000 });
});

test('a successful callback cannot hide a launcher cleanup failure', async () => {
  await assert.rejects(testing.withTestTHS(async () => {
    await delay(400); return 42;
  }, { executable, runtimeDir: 'late-exit', timeoutMs: 2000 }), /9|late service failure/);
});

test('test cancellation closes the sandbox even while application code is waiting', async () => {
  const controller = new AbortController();
  const failure = new Error('application test cancelled');
  let ready, resume, endpoint;
  const entered = new Promise(resolve => { ready = resolve; });
  const blocked = new Promise(resolve => { resume = resolve; });
  const finished = testing.withTestTHS(async ths => {
    endpoint = ths.config().dashboard; ready(); await blocked;
  }, { executable, runtimeDir: 'normal', timeoutMs: 2000, signal: controller.signal })
    .then(() => undefined, error => error);
  await entered; controller.abort(failure);
  try {
    const error = await Promise.race([finished, delay(1000).then(() => new Error('cancellation did not finish'))]);
    assert.equal(error, failure);
    await assert.rejects(fetch(endpoint));
  } finally { resume(); await finished; }
});

test('an already cancelled scope never starts a launcher or calls the application', async () => {
  const failure = new Error('test already cancelled');
  let called = false;
  await assert.rejects(testing.withTestTHS(() => { called = true; },
    { executable: '/nonexistent/ths', signal: AbortSignal.abort(failure) }), error => error === failure);
  assert.equal(called, false);
});

test('cancellation during startup waits for launcher cleanup without calling the application', async t => {
  const fixture = await startupFixture(t, 'slow');
  const controller = new AbortController();
  const failure = new Error('startup cancelled by the test');
  let called = false;
  const running = testing.withTestTHS(() => { called = true; },
    { ...fixture.options, signal: controller.signal });
  const rejected = assert.rejects(running, error => error === failure);
  await fixture.wait(); controller.abort(failure); await rejected;
  assert.equal(called, false);
});

test('RPC and wait handles expose chain changes without mining extra blocks', async t => {
  const ths = client('normal'); t.after(() => ths.stop()); await ths.start();
  assert.equal(await ths.rpc('getblockcount'), 200);
  assert.equal(await ths.rpc('generate', [3]), 203);
  await ths.waitUntilSettled();
  const details = await (await fetch(`${ths.config().dashboard}/fixture`)).json();
  assert.equal(details.height, 203);
  assert.ok(details.pending < 0);
  assert.deepEqual(details.calls, [['rpc', 'getblockcount', []], ['rpc', 'generate', [3]]]);
  for (const method of ['', ' ', 1, null]) await assert.rejects(ths.rpc(method));
  await assert.rejects(ths.rpc('generate', {}));
  await assert.rejects(ths.rpc('fail'), /fixture RPC rejected/);
  await assert.rejects(ths.rpc('bad-id'), /RPC/i);
  await assert.rejects(ths.rpc('missing-result'), /RPC/i);
  await fetch(`${ths.config().dashboard}/fixture`, { method: 'POST' });
  await assert.rejects(ths.rpc('generate', [1]), /identity/i);
  assert.equal((await (await fetch(`${ths.config().dashboard}/fixture`)).json()).height, 203);
});

test('RPC timeout is bounded and stop cancels an in-flight request', async () => {
  const ths = createTestTHSClient({ executable, runtimeDir: 'normal', timeoutMs: 500 });
  try {
    await ths.start();
    await assert.rejects(ths.rpc('hang'), /timeout|aborted/i);
    const rejected = assert.rejects(ths.rpc('hang'), /cancel|aborted|fetch/i);
    await delay(30); await ths.stop(); await rejected;
  } finally { await ths.stop(); }
});

test('RPC captures parameters before asynchronous identity checks', async t => {
  const ths = client('normal'); t.after(() => ths.stop()); await ths.start();
  const params = [2];
  const mining = ths.rpc('generate', params);
  params[0] = 100;
  assert.equal(await mining, 202);
});

for (const [method, expected] of [['hang-body', /timeout|aborted/i],
  ['truncated-body', /terminated|aborted|socket|reset/i], ['invalid-body', /JSON/i]]) {
  test(`RPC rejects an incomplete or invalid response without retrying: ${method}`, async t => {
    const ths = createTestTHSClient({ executable, runtimeDir: 'normal', timeoutMs: 1000 });
    t.after(() => ths.stop()); await ths.start();
    await assert.rejects(ths.rpc(method), expected);
    assert.equal(await ths.rpc('getblockcount'), 200);
    const details = await (await fetch(`${ths.config().dashboard}/fixture`)).json();
    assert.deepEqual(details.calls, [['rpc', method, []], ['rpc', 'getblockcount', []]]);
  });
}

test('funding waits for broadcast, preserves exact amounts and keys, and leaves mining to the test', async t => {
  const ths = client('normal'); t.after(() => ths.stop()); await ths.start();
  const options = { idempotencyKey: 'funding-replay/&?key' };
  const funded = await ths.fundAddress('regtest-fixture-address', 1_000_000n, options);
  assert.deepEqual(funded, { address: 'regtest-fixture-address', amountZatoshi: 1_000_000n,
    idempotencyKey: options.idempotencyKey, txid: 'ab'.repeat(32), status: 'broadcast' });
  assert.deepEqual(await ths.fundAddress('regtest-fixture-address', 1_000_000n, options), funded);
  const details = await (await fetch(`${ths.config().dashboard}/fixture`)).json();
  assert.equal(details.funded, 1_000_000);
  assert.equal(details.height, 200);
  assert.equal(details.calls.length, 2);
  await assert.rejects(ths.fundAddress('regtest-fixture-address', 2_000_000n, options), /409/);
  await fetch(`${ths.config().dashboard}/fixture`, { method: 'POST' });
  await assert.rejects(ths.fundAddress('regtest-fixture-address', 1n), /identity/i);
});

test('funding validates arguments before sending a payment', async t => {
  const ths = client('normal'); t.after(() => ths.stop()); await ths.start();
  for (const amount of [0n, -1n, 500_000_001n, 1, 1.5, '1']) {
    await assert.rejects(ths.fundAddress('regtest-fixture-address', amount));
  }
  for (const key of ['', 'short', 'x'.repeat(129), 'spaces are not allowed', 'é'.repeat(8)]) {
    await assert.rejects(ths.fundAddress('regtest-fixture-address', 1n, { idempotencyKey: key }));
  }
  for (const address of ['', ' ', null]) await assert.rejects(ths.fundAddress(address, 1n));
  assert.deepEqual((await (await fetch(`${ths.config().dashboard}/fixture`)).json()).calls, []);
});

test('explicit funding retry after a lost response reuses the original payment', async t => {
  const ths = client('lost-funding-response'); t.after(() => ths.stop()); await ths.start();
  const options = { idempotencyKey: 'lost-response-retry' };
  await assert.rejects(ths.fundAddress('regtest-fixture-address', 1_000_000n, options), /lost-response-retry/);
  const funded = await ths.fundAddress('regtest-fixture-address', 1_000_000n, options);
  assert.equal(funded.txid, 'ab'.repeat(32));
  assert.equal((await (await fetch(`${ths.config().dashboard}/fixture`)).json()).funded, 1_000_000);
});

test('funding replay after rollback returns the original transaction as unconfirmed', async t => {
  const ths = client('funding-rollback'); t.after(() => ths.stop()); await ths.start();
  const options = { idempotencyKey: 'funding-rollback-replay' };
  const funded = await ths.fundAddress('regtest-fixture-address', 1_000_000n, options);
  await ths.produceBlock(1);
  assert.deepEqual(await ths.fundAddress(funded.address, funded.amountZatoshi, options),
    { ...funded, status: 'confirmed' });
  await ths.rollback(1);
  assert.deepEqual(await ths.fundAddress(funded.address, funded.amountZatoshi, options),
    { ...funded, status: 'unconfirmed' });
  assert.equal((await (await fetch(`${ths.config().dashboard}/fixture`)).json()).funded, 1_000_000);
});

for (const [mode, expected] of [['failed-funding', /fixture funding rejected/], ['wrong-funding', /funding response/i],
  ['bad-funding-txid', /funding response/i], ['hang-funding', /timeout|aborted/i], ['lost-funding-response', /socket hang up/i]]) {
  test(`funding failure remains observable with a retry key: ${mode}`, async t => {
    const ths = createTestTHSClient({ executable, runtimeDir: mode, timeoutMs: 500 });
    t.after(() => ths.stop()); await ths.start();
    await assert.rejects(ths.fundAddress('regtest-fixture-address', 1_000_000n), error => {
      assert.match(error.message, expected);
      assert.match(error.message, /idempotency key [a-f0-9-]{36}/);
      return true;
    });
    const details = await (await fetch(`${ths.config().dashboard}/fixture`)).json();
    assert.equal(details.calls.length, 1, 'ambiguous payments must not be submitted again automatically');
  });
}
