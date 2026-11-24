import assert from 'node:assert/strict';
import { test } from 'node:test';
import { withTestTHS } from '@zcashlabs/thsjs';
import { balance, openWallet, receiveAddress, regtestNetwork, sync } from '../src/wallet.js';

// Unset means: use `ths` from PATH.
const launcher = { executable: process.env.THS_BINARY, runtimeDir: process.env.THS_RUNTIME_DIR };
// Public test mnemonic. Never use it outside Regtest.
const mnemonic = new TextEncoder().encode(
  'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about');

test('a funded payment becomes spendable after enough confirmations', { timeout: 300_000 }, async t => {
  // Starts a fresh sandbox and stops it when the callback ends, even on failure.
  await withTestTHS(async ths => {
    const config = ths.config();
    const wallet = await openWallet(config.lightwalletd, await regtestNetwork(config));
    try {
      await sync(wallet);
      const { account, signer } = await wallet.accounts.create({ mnemonic });
      await signer.dispose(); // Receiving doesn't need the signer.
      const address = await receiveAddress(wallet, account.id);

      const amount = 1_000_000n; // Zatoshi: 0.01 ZEC.
      const { txid } = await ths.fundAddress(address, amount);
      const confirmations = async () => (await ths.rpc('getrawtransaction', [txid, 1])).confirmations;

      await ths.produceBlock(3);
      assert.equal(await confirmations(), 3);
      assert.deepEqual(await balance(wallet, account.id), { total: amount, spendable: 0n });

      await ths.produceBlock(1);
      assert.equal(await confirmations(), 4);
      assert.deepEqual(await balance(wallet, account.id), { total: amount, spendable: amount });
    } finally {
      await wallet.close();
    }
  }, { ...launcher, signal: t.signal });
});
