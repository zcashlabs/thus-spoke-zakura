import { createWalletClient, defineNetwork, encodeNetworkParameters } from '@zcashlabs/zcash.js';

// Your app's wallet code. Tests point it at a THS sandbox.

// Spend only notes mined before the anchor, 3 blocks below the tip.
const confirmations = { trusted: 3, untrusted: 3, allowZeroConfirmationShielding: false };

// THS Regtest turns on every network upgrade at height 1.
export function regtestNetwork({ instance, genesisHash }) {
  return defineNetwork({
    identity: instance,
    genesisHash,
    ...encodeNetworkParameters({ encoding: 'regtest', Overwinter: 1, Sapling: 1, Blossom: 1,
      Heartwood: 1, Canopy: 1, Nu5: 1, Nu6: 1, Nu6_1: 1, Nu6_2: 1, Nu6_3: 1, Nu7: 1 }),
  });
}

export function openWallet(lightwalletd, network) {
  return createWalletClient(lightwalletd, {
    network, storage: { kind: 'memory' }, confirmations, transportOptions: { timeoutMs: 15_000 },
  });
}

export async function receiveAddress(wallet, accountId) {
  const { address } = await wallet.addresses.next({ accountId, request: {
    format: 'unified', transparent: 'omit', sapling: 'omit', ironwood: 'require',
  } });
  return address;
}

// THS doesn't sync your wallet; it has to scan new blocks itself.
export async function sync(wallet) {
  const receipt = await wallet.sync({ signal: AbortSignal.timeout(60_000) });
  if (!receipt.targetReached) throw new Error('wallet sync stopped before the chain tip');
}

export async function balance(wallet, accountId) {
  await sync(wallet);
  const { amounts } = await wallet.getBalance({ accountId });
  return { total: amounts.total, spendable: amounts.ironwood.spendable };
}
