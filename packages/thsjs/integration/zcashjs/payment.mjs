import assert from 'node:assert/strict';
import { createLightClient, createWalletClient, defineNetwork, encodeNetworkParameters, resolveBirthday } from '@zcashlabs/zcash.js';
import { withTestTHS } from '@zcashlabs/thsjs';

const amount = 1_000_000n;
const confirmations = { trusted: 3, untrusted: 3, allowZeroConfirmationShielding: false };
// Public, disposable test mnemonic; never use it outside Regtest.
const mnemonic = () => new TextEncoder().encode(
  'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about');

await withTestTHS(async ths => {
  const { instance, genesisHash, lightwalletd } = ths.config();
  const { upgrades } = await ths.rpc('getblockchaininfo');
  assert.equal(upgrades?.['77190ad9']?.activationheight, 1, 'This example requires THS Regtest with NU7 at height 1.');
  const network = await defineNetwork({
    identity: instance,
    genesisHash,
    ...encodeNetworkParameters({ encoding: 'regtest', Overwinter: 1, Sapling: 1, Blossom: 1,
      Heartwood: 1, Canopy: 1, Nu5: 1, Nu6: 1, Nu6_1: 1, Nu6_2: 1, Nu6_3: 1, Nu7: 1 }),
  });
  const transportOptions = { timeoutMs: 15_000 };
  const light = await createLightClient(lightwalletd, { network, transportOptions });
  const openWallet = () => createWalletClient(lightwalletd, {
    network, storage: { kind: 'memory' }, confirmations, transportOptions,
  });
  let wallet = await openWallet();

  async function sync() {
    // THS has already settled, but this wallet still needs to scan independently.
    const receipt = await wallet.sync({ signal: AbortSignal.timeout(60_000) });
    assert.equal(receipt.targetReached, true);
    return receipt.target;
  }
  try {
    await sync();
    const { account, signer } = await wallet.accounts.create({ mnemonic: mnemonic() });
    await signer.dispose(); // Receiving and checking spendability do not require signing.
    let accountId = account.id;
    const birthday = await resolveBirthday({ light, firstScanHeight: account.birthdayHeight });
    const { address } = await wallet.addresses.next({ accountId, request: {
      format: 'unified', transparent: 'omit', sapling: 'omit', ironwood: 'require',
    } });
    const { txid, status } = await ths.fundAddress(address, amount);
    assert.equal(status, 'broadcast');

    async function observe(stage, expectedConfirmations, expectedSpendable) {
      const tip = await sync();
      const chain = await ths.rpc('getblockchaininfo');
      const tx = await ths.rpc('getrawtransaction', [txid, 1]);
      assert.equal(tip.height, chain.blocks);
      assert.equal(tip.hash, chain.bestblockhash);
      assert.equal(tx.confirmations, expectedConfirmations);
      const history = (await wallet.getHistory({ accountId })).items.filter(item => item.txid === txid);
      assert.equal(history.length, 1);
      assert.equal(history[0].minedHeight, tip.height - tx.confirmations + 1);
      const { amounts } = await wallet.getBalance({ accountId });
      assert.equal(amounts.total, amount);
      assert.equal(amounts.ironwood.spendable, expectedSpendable);
      console.log(`${stage}: ${txid} in ${tx.blockhash}; tip ${tip.height} ${tip.hash}; ` +
        `${tx.confirmations} confirmations, total ${amounts.total}, spendable ${amounts.ironwood.spendable} zatoshi`);
    }

    await ths.produceBlock(1);
    await observe('Mined', 1, 0n);
    await ths.produceBlock(2);
    await observe('Below threshold', 3, 0n);
    // A three-block anchor depth puts the note behind the tip at four confirmations.
    await ths.produceBlock(1);
    await observe('Spendable', 4, amount);

    await ths.rollback(1);
    // SDK 0.1.2 does not rewind an existing scan when the new tip is its ancestor.
    // Restore this disposable account and scan the shorter chain from its original birthday.
    await wallet.close();
    wallet = await openWallet();
    const restored = await wallet.accounts.import({ mnemonic: mnemonic(), accountIndex: 0, birthday });
    await restored.signer.dispose();
    accountId = restored.account.id;
    await observe('After rollback and rescan (payment still mined)', 3, 0n);
  } finally {
    await wallet.close();
  }
}, { executable: process.env.THS_BINARY, runtimeDir: process.env.THS_RUNTIME_DIR });
