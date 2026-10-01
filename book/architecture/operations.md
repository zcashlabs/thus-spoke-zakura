# 6. Uncertain operations

The previous chapter showed that a block can be generated before the wallet
publishes a new snapshot. A payment has a similar boundary: it can be
broadcast before the server records the request. This chapter names each
state and tells you what an HTTP response can and cannot prove.

The terms are deliberately narrow:

- **Broadcast** means lightwalletd accepted the transaction for submission.
  It does not mean the transaction is in a block.
- **Activity** is the server's SQLite record of a development payment. It
  includes a transaction ID (`txid`), `status`, and optional `block_hash`.
- **Confirmed activity** means Zakura reported the transaction in a block
  when the server last checked it, and SQLite stored that block hash. The
  status is a record of that check, not a live proof of canonical inclusion.
- An **idempotency key** is a caller-chosen string used to look up an
  already-recorded payment. It is not an end-to-end exactly-once guarantee.

## Payment submission and confirmation

![A send or faucet request is validated, then its idempotency key is looked up. A recorded key returns the stored activity. A new key is built and broadcast, then recorded, then confirmed. The broadcast happens before the record, so a crash in between leaves a live transaction with no row.](../images/payment-flow.svg)

1. `POST /api/v1/send` validates different user account IDs (1–5), source
   and destination pools, amount, memo rules, and an `idempotency_key` of
   **8–128 visible ASCII characters**. `POST /api/v1/faucet` validates
   its user account, pool, amount greater than zero and at most 5 ZEC,
   and key. Validation happens before the recorded-key lookup.
2. If SQLite already has activity for the key, the server tries to finish
   confirmation of **that recorded transaction**. It does not construct a
   second payment. The server does **not** compare the replay's valid
   payload with the original payload, so a key must belong to one intent.
3. With a new key, the server synchronizes its wallet. The SDK chooses
   spendable funds and a fee, constructs the transaction, and broadcasts it
   through lightwalletd. For faucet payments, the source is hidden treasury
   Account 6; it may replenish itself first.
4. **After** broadcast, the server inserts the activity and key in SQLite.
   The initial activity status is `broadcast` and carries a transaction ID.
5. The server asks Zakura whether the transaction is confirmed. If needed,
   it mines one block, waits for wallet reconciliation, and asks again. It
   returns `confirmed` activity with a block hash if successful. If
   auto-mining fails, it can return `broadcast` activity with the txid
   and no block hash. Background sync can later confirm recorded activity.

The important ordering is **broadcast → SQLite record**. A timeout or
process failure in that gap can leave a transaction on the chain with no
recorded key. A second request with the same key may then build another
transaction. Two concurrent requests with one new key can also both pass
the initial lookup. This version does not promise exactly-once submission.

## Call the API with a deliberate key

This example uses integer zatoshi. Run it against the live dashboard URL of
the **same instance** you used in earlier chapters:

```console
DASHBOARD_URL="$(ths endpoints --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["dashboard"])')"
curl -fsS "$DASHBOARD_URL/api/v1/send" \
  -H 'content-type: application/json' \
  --data '{"from_account":1,"to_account":2,"source_pool":"orchard","destination_pool":"orchard","amount_zatoshi":100000000,"idempotency_key":"book-account-1-to-2-v1"}'
```

The response is one activity object, and the same object is what
`GET /api/v1/activity` returns in a list. It has eleven fields:

| Field | Meaning |
| --- | --- |
| `id` | The activity row's own UUID. Not the idempotency key and not the txid. |
| `kind` | `send` or `faucet`. |
| `from_account` | Source account 1–5, or `null` for a faucet payment out of the hidden treasury. |
| `to_account` | Destination account 1–5. |
| `source_pool` | `orchard` or `transparent`. |
| `destination_pool` | `orchard` or `transparent`. |
| `amount_zatoshi` | Integer zatoshi paid to the destination. It does not include the fee. |
| `txid` | The broadcast transaction's ID. |
| `block_hash` | The confirming block's hash, or `null` while status is `broadcast`. |
| `status` | `broadcast` or `confirmed`, as of the server's last check. |
| `created_at` | When the row was inserted, as `YYYY-MM-DD HH:MM:SS` UTC. |

Read `txid`, `status`, and `block_hash` first; do not infer confirmation
merely because HTTP returned 200. There is no memo field here, and none is
stored: a memo goes into the transaction and is never read back through this
API.
Keep this key only for this intended payment. If the server has recorded it,
repeating the **same direct API request** with the same key retrieves or
finishes that activity. The dashboard and CLI generate a **fresh** key for
each submission, so clicking Send or running `ths wallet send` again is a
new attempt.

## Decide what to do after a lost response

![If the call was a send or faucet you chose the idempotency key, so look at the activity: a row with your key can be resent safely, while no row does not prove nothing was broadcast. Mine and address-faucet take no key, so compare the chain instead.](../images/retry-decisions.svg)

1. Save any transaction ID or response you did receive. Query Recent
   activity or `GET /api/v1/activity`, then inspect Zakura's transaction
   and chain state if you have a txid.
2. If the matching activity is recorded, use its status. A direct API caller
   may repeat the same validated request with its **same key** to finish
   confirmation. A `broadcast` activity may also become `confirmed` after
   another block and background synchronization.
3. If no matching activity appears, you cannot conclude that nothing was
   broadcast. The gap before SQLite recording has no safe automatic
   deduplication. Investigate the chain or mempool and the intended
   account balances before deciding whether to create a new payment.
4. `ths faucet <ADDRESS>` calls `POST /api/v1/faucet/address`. It has
   **no idempotency key or activity record**. Treat a lost response as
   ambiguous and inspect the destination/chain before repeating it.

The server validates key syntax and deduplicates **recorded** activity;
those checks are useful, but their boundary matters. Never reuse a key for
a different amount or recipient. Do not reconstruct a balance by summing
activity records; use `GET /api/v1/accounts` after wallet sync.

A chain reorganization can replace the block named by a previously confirmed
activity. The server reconciles wallet balances against the new canonical
chain, but it does not recheck activity already marked `confirmed`. If
current inclusion matters, query Zakura for the transaction and compare its
block hash with the current chain. Treat the stored activity status as the
result of its earlier confirmation check.

## Mining has a different retry boundary

`POST /api/v1/mine` validates a count of 1–10,000 and calls Zakura
`generate`. It then reads the mined tip, reconciles the wallet, notifies
dashboards, and returns `blocks` plus `hashes`. If a later step errors,
blocks may already exist, even though the request has no success response.
Mining has **no retry key** in this version.

After a timeout, compare the current **node height and tip hash** with
what you observed before the request. Use Network, `GET /api/v1/status`,
or Zakura RPC. Another process can also mine or reorganize a local chain,
so that observation guides a human decision but is not an exactly-once
receipt. [Chapter 5](../mining-and-sync.md) traces the full wallet catch-up.

**Source map:** [API handlers](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-server/src/api.rs),
[SQLite activity and keys](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-server/src/db.rs),
[wallet payments](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-server/src/wallet.rs),
and [chain reconciliation](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-server/src/reconcile.rs).

**Next:** [choose an interface for your own application](../connect-an-app.md).
