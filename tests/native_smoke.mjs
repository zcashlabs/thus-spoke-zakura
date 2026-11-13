// Run with: ths run --native --runtime-dir DIR -- node tests/native_smoke.mjs
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
const API = process.env.THS_API_URL;
assert.equal(process.env.THS_NETWORK, 'regtest');
assert.equal(process.env.THS_LIGHTWALLETD_TLS, 'false');
for (const url of [API, process.env.THS_ZAKURA_RPC_URL, process.env.THS_LIGHTWALLETD_URL]) {
  assert.equal(new URL(url).hostname, '127.0.0.1');
}
async function request(path, body) {
  const response = await fetch(`${API}/api/v1/${path}`, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
    redirect: 'error', signal: AbortSignal.timeout(120_000),
  });
  assert(response.ok, `${path}: ${await response.clone().text()}`);
  return response.json();
}
async function wait(predicate, description) {
  const deadline = performance.now() + 120_000;
  do {
    const value = await predicate();
    if (value) return value;
    await new Promise(resolve => setTimeout(resolve, 100));
  } while (performance.now() < deadline);
  throw new Error(`Timed out: ${description}`);
}
async function settled() {
  const status = await request('status');
  assert.equal(status.instance, process.env.THS_INSTANCE);
  assert.equal(status.network, 'Regtest');
  if (!status.settled) return undefined;
  assert.equal(status.node.bestblockhash, status.wallet_sync.fully_scanned_hash);
  assert.equal(status.node.blocks, status.wallet_sync.fully_scanned_height);
  return status;
}
const page = await fetch(API, { signal: AbortSignal.timeout(5000), redirect: 'error' });
assert((await page.text()).includes('<div id="root">'));
const initial = await wait(settled, 'initial settlement');
const accounts = await request('accounts');
assert.deepEqual(accounts.map(account => account.id), [1, 2, 3, 4, 5]);
assert.equal(accounts[0].ironwood_zatoshi, 500_000_000);
assert.equal(accounts[1].ironwood_zatoshi, 0);
const mined = await request('mine', { blocks: 10 });
assert.equal(mined.blocks, 10);
assert.equal(new Set(mined.hashes).size, 10);
assert.equal((await wait(settled, 'mine settlement')).node.blocks, initial.node.blocks + 10);
const payment = {
  from_account: 1, to_account: 2, source_pool: 'ironwood', destination_pool: 'ironwood',
  amount_zatoshi: 1_234_567, memo: 'native runtime smoke', idempotency_key: randomUUID(),
};
const first = await request('send', payment);
assert.equal((await request('send', payment)).id, first.id);
await wait(async () => (await request(`activity?idempotency_key=${payment.idempotency_key}`))[0]?.status === 'confirmed', 'payment confirmation');
await wait(settled, 'payment settlement');
assert.equal((await request('accounts'))[1].ironwood_zatoshi, 1_234_567);
const replay = await request('send', payment);
assert.equal(replay.id, first.id);
assert.equal(replay.status, 'confirmed');
await request('mine', { blocks: 2 });
const before = (await wait(settled, 'pre-reorg settlement')).node;
const reorg = await request('reorg', { depth: 1, keep_txs: false });
const after = (await wait(settled, 'reorg settlement')).node;
assert.equal(after.blocks, before.blocks);
assert.notEqual(after.bestblockhash, before.bestblockhash);
assert.equal(reorg.removed_hashes.length, 1);
assert.equal(reorg.replacement_hashes.length, 1);
assert.equal((await request('accounts'))[1].ironwood_zatoshi, 1_234_567);
await request('rollback', { depth: 1 });
assert.equal((await wait(settled, 'rollback settlement')).node.blocks, after.blocks - 1);
assert.equal((await request('accounts'))[1].ironwood_zatoshi, 1_234_567);
console.log('PASS: native dashboard, wallet funding, exact mining, shielded payment, replay, reorg, rollback');
