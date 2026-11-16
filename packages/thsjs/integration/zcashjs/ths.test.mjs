// Opt-in native integration: THS_BINARY=... THS_RUNTIME_DIR=... npm test --prefix integration/zcashjs
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { spawn, execFile } from 'node:child_process';
import { once } from 'node:events';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { createWalletClient, defineNetwork, encodeNetworkParameters, createLightClient } from '@zcashlabs/zcash.js';
import { withTestTHS } from '@zcashlabs/thsjs';
import { assertClosed, waitForClosed } from '../../tests/support/assert-closed.mjs';

assert.ok(process.env.THS_BINARY, 'Set THS_BINARY to an installed native THS launcher.');
const options = { executable: process.env.THS_BINARY,
  ...(process.env.THS_RUNTIME_DIR ? { runtimeDir: process.env.THS_RUNTIME_DIR } : {}), timeoutMs: 180_000 };
const evidence = { node: process.version, checks: [] };
let config;
const json = data => JSON.stringify(data, (_, value) => typeof value === 'bigint' ? value.toString() : value, 2);
async function check(label, fn) {
  const result = await fn(); evidence.checks.push({ label, result }); console.log(`PASS ${label}`); return result;
}
try {
  await withTestTHS(async ths => {
    config = await check('native isolated startup through the library', () => ths.config());
    evidence.config = config;
    await assert.rejects(assertClosed({ ...config, dashboard: config.lightwalletd }), /still listening/);
    let wallet, signer, binding;
    async function sync() {
      const receipt = await wallet.sync({ signal: AbortSignal.timeout(60_000) });
      assert.equal(receipt.targetReached, true); return receipt;
    }
    try {
      const network = await defineNetwork({ identity: config.instance, genesisHash: config.genesisHash,
        ...encodeNetworkParameters({ encoding: 'regtest', Overwinter: 1, Sapling: 1, Blossom: 1, Heartwood: 1,
          Canopy: 1, Nu5: 1, Nu6: 1, Nu6_1: 1, Nu6_2: 1, Nu6_3: 1, Nu7: 1 }) });
      const light = await createLightClient(config.lightwalletd, { network, transportOptions: { timeoutMs: 15_000 } });
      const confirmations = { trusted: 3, untrusted: 3, allowZeroConfirmationShielding: false };
      wallet = await createWalletClient(config.lightwalletd, { network, storage: { kind: 'memory' }, confirmations,
        transportOptions: { timeoutMs: 15_000 } });
      await sync();
      const created = await wallet.accounts.create({ mnemonic: new TextEncoder().encode(
        'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about') });
      signer = created.signer; const accountId = created.account.id;
      binding = await wallet.accounts.attachSigner({ accountId, signer }); assert.equal(binding.state, 'ready');
      await sync();
      const receive = await wallet.addresses.next({ accountId, request: { format: 'unified', transparent: 'omit',
        sapling: 'omit', ironwood: 'require' } });
      await check('fundAddress pays an external SDK wallet exactly once', async () => {
        const key = crypto.randomUUID();
        const funded = await ths.fundAddress(receive.address, 1_000_000n, { idempotencyKey: key });
        assert.equal(funded.amountZatoshi, 1_000_000n);
        assert.equal(funded.idempotencyKey, key);
        assert.match(funded.txid, /^[0-9a-f]{64}$/);
        await ths.produceBlock(4); await sync();
        const replay = await ths.fundAddress(receive.address, 1_000_000n, { idempotencyKey: key });
        assert.equal(replay.txid, funded.txid);
        await ths.produceBlock(1); await sync();
        const balance = await wallet.getBalance({ accountId });
        assert.equal(balance.amounts.total, 1_000_000n);
        assert.equal(balance.amounts.ironwood.spendable, 1_000_000n);
        return { funded, replay, balance };
      });
      await check('SDK wallet builds, proves, signs and confirms a payment', async () => {
        const recipient = await wallet.addresses.next({ accountId, request: { format: 'transparent' } });
        const pending = await wallet.send({ accountId, to: recipient.address, amount: 100_000n,
          idempotencyKey: 'ths-testing-roundtrip', signal: AbortSignal.timeout(180_000) });
        const sent = await pending.snapshot(); const txid = sent.steps[0].txid;
        assert.match(txid, /^[0-9a-f]{64}$/);
        await ths.produceBlock(3); await sync();
        await pending.wait({ confirmations: 3, timeoutMs: 30_000 });
        assert.equal((await wallet.getBalance({ accountId })).amounts.transparent.regular.total, 100_000n);
        return { txid, chain: await ths.rpc('getblockchaininfo') };
      });
      await check('direct RPC mining and explicit settlement update the external wallet', async () => {
        const before = await ths.rpc('getblockcount');
        await ths.rpc('generate', [1]);
        await ths.waitUntilSettled(); await sync();
        assert.equal(await ths.rpc('getblockcount'), before + 1);
        return { height: before + 1 };
      });
      await check('reorg and rollback settle before external wallet sync', async () => {
        const before = await light.getTip();
        await ths.reorg(1, { keepTransactions: true }); const replaced = await light.getTip();
        assert.equal(replaced.height, before.height); assert.notEqual(replaced.hash, before.hash); await sync();
        await ths.rollback(1); const rolled = await light.getTip();
        assert.equal(rolled.height, before.height - 1); await sync();
        return { before, replaced, rolled };
      });
    } finally {
      try { await binding?.dispose(); }
      finally { try { await wallet?.close(); } finally { await signer?.dispose(); } }
    }
    for (const signal of ['SIGTERM', 'SIGKILL']) {
      await check(`${signal} on a JavaScript worker closes its sandbox and preserves another instance`, async () => {
        const worker = spawn(process.execPath,
          [fileURLToPath(new URL('../../tests/support/worker.mjs', import.meta.url)),
            JSON.stringify(options), import.meta.resolve('@zcashlabs/thsjs')],
          { stdio: ['ignore', 'ignore', 'pipe', 'ipc'] });
        const exited = once(worker, 'exit');
        let diagnostics = '', workerConfig, cleaned = false;
        worker.stderr.on('data', chunk => { diagnostics += chunk; });
        try {
          [workerConfig] = await Promise.race([
            once(worker, 'message', { signal: AbortSignal.timeout(options.timeoutMs) }),
            exited.then(() => { throw Error(`Worker exited before ready: ${diagnostics}`); }),
          ]);
          assert.notEqual(workerConfig.instance, config.instance);
          worker.kill(signal); assert.deepEqual(await exited, [null, signal]);
          await waitForClosed(workerConfig, options.timeoutMs); cleaned = true;
          assert.equal(typeof await ths.rpc('getblockcount'), 'number');
          return { instance: workerConfig.instance, closed: true, survivor: config.instance };
        } finally {
          if (worker.exitCode === null && worker.signalCode === null) worker.kill('SIGTERM');
          await exited;
          if (workerConfig && !cleaned) {
            await promisify(execFile)(options.executable, ['--name', workerConfig.instance, 'stop'],
              { timeout: options.timeoutMs });
          }
        }
      });
    }
  }, options);
  await check('test scope waits for all owned listeners to close', async () => {
    await assertClosed(config); return { closed: true };
  });
  await check('failed application assertion still closes its native sandbox', async () => {
    const failure = new Error('intentional application assertion');
    let failedConfig;
    await assert.rejects(withTestTHS(ths => {
      failedConfig = ths.config();
      throw failure;
    }, options), error => error === failure);
    assert.notEqual(failedConfig.instance, config.instance);
    await assertClosed(failedConfig);
    return { closed: true, instance: failedConfig.instance };
  });
  await check('test cancellation closes native services while application work is waiting', async () => {
    const controller = new AbortController();
    const failure = new Error('intentional test cancellation');
    let cancelledConfig, resume;
    const blocked = new Promise(resolve => { resume = resolve; });
    try {
      await assert.rejects(withTestTHS(async ths => {
        cancelledConfig = ths.config();
        controller.abort(failure);
        await blocked;
      }, { ...options, signal: controller.signal }), error => error === failure);
      await assertClosed(cancelledConfig);
      return { closed: true, instance: cancelledConfig.instance };
    } finally { resume(); }
  });
} catch (error) {
  evidence.error = { message: error.message, stack: error.stack }; process.exitCode = 1; console.error(error);
} finally {
  await mkdir(new URL('../../.cache/zcashjs/', import.meta.url), { recursive: true });
  await writeFile(new URL('../../.cache/zcashjs/results.json', import.meta.url), json(evidence) + '\n');
  console.log('Evidence: packages/thsjs/.cache/zcashjs/results.json');
}
