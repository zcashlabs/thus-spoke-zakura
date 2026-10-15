# 6. Uncertain operations

The previous chapter showed that a block can be generated before the wallet
publishes a new snapshot. A payment has a similar boundary: it can be
broadcast before its caller receives a response. This chapter names each
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

![A validated payment is durably claimed as preparing. The wallet and server save prepared transaction bytes before broadcast, then the server checks confirmation. Identical keys resume the saved payment; changed details or memo conflict.](../images/payment-flow.svg)

1. `POST /api/v1/send` validates user account IDs (1–5), source
   and destination pools, amount, memo rules, and an `idempotency_key` of
   **8–128 visible ASCII characters**. Same-account sends are allowed when
   the pools differ. A recipient can also be a Regtest address matching the
   destination pool.
2. `POST /api/v1/faucet` validates its user account, pool,
   amount greater than zero and at most 5 ZEC, and key. Validation happens
   before the recorded-key lookup.
3. If SQLite already has a claim for the key, the server resumes **that
   recorded payment**. It compares the replay's details, including the memo,
   with the original intent; changed valid details return **HTTP 409**.
   A new intent and key are recorded as `preparing` before wallet work.
4. With a new key, the server synchronizes its wallet. The SDK chooses
   spendable funds and a fee, constructs the transaction, and saves it in the
   wallet. The server saves its txid and transaction bytes as `prepared` before
   broadcast. For faucet payments, the source is hidden treasury
   Account 6; it may replenish itself first.
5. The server submits the saved transaction through lightwalletd and marks
   it `broadcast`. If the node already knows the txid, the server continues
   with that transaction. A lost response can be recovered using the saved
   payment rather than constructing another one.
6. The server asks Zakura whether the transaction is confirmed. If needed,
   it mines one block, waits for wallet reconciliation, and asks again. It
   returns `confirmed` activity with a block hash if successful. If
   auto-mining fails, it can return `broadcast` activity with the txid
   and no block hash. Background confirmation checks can later confirm
   recorded account activity independently of wallet synchronization.

The important ordering is **record intent → save transaction → broadcast**.
Concurrent requests with the same key share the recorded intent and serialized
submission. Saved payments can be resumed across a server restart while the
instance's databases remain intact. This is not an end-to-end exactly-once
guarantee: a fresh key requests a new payment, and deleting the instance deletes
its recovery records.

## Call the API with a deliberate key

This example uses integer zatoshi. Run it against the live dashboard URL of
the **same instance** you used in earlier chapters:

```console
DASHBOARD_URL="$(ths endpoints --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["dashboard"])')"
curl -fsS "$DASHBOARD_URL/api/v1/send" \
  -H 'content-type: application/json' \
  --data '{"from_account":1,"to_account":2,"source_pool":"ironwood","destination_pool":"ironwood","amount_zatoshi":100000000,"idempotency_key":"book-account-1-to-2-v1"}'
```

The response is one activity object, and the same object is what
`GET /api/v1/activity` returns in a list. It has thirteen fields:

| Field | Meaning |
| --- | --- |
| `id` | The activity row's own UUID. Not the idempotency key and not the txid. |
| `kind` | `send` or `faucet`. |
| `from_account` | Source account 1–5, or `null` for a faucet payment out of the hidden treasury. |
| `to_account` | Destination account 1–5, or `null` for an external send. |
| `to_address` | External send destination, or `null` for an account recipient. |
| `source_pool` | `ironwood` or `transparent`. |
| `destination_pool` | `ironwood` or `transparent`. |
| `amount_zatoshi` | Integer zatoshi paid to the destination. It does not include the fee. |
| `txid` | The prepared transaction's ID; empty while status is `preparing`. |
| `block_hash` | The confirming block's hash, or `null` before confirmation. |
| `status` | `preparing`, `prepared`, `broadcast`, `confirmed`, or a stored terminal `failed` result. |
| `created_at` | When the row was inserted, as `YYYY-MM-DD HH:MM:SS` UTC, including in the initial accepted response. |
| `last_error` | Stored diagnostic, or `null`. |

Read `txid`, `status`, and `block_hash` first; do not infer confirmation
merely because HTTP returned 200. There is no memo field here. The supplied
memo is stored with the payment intent to compare replays, but received memos
are not read back through this API.
Keep this key only for this intended payment. If the server has recorded it,
repeating the **same direct API request** with the same key retrieves or
finishes that activity.

## Which caller are you retrying?

| Caller | How to resume the payment |
| --- | --- |
| Direct API | Repeat the identical request with your saved key. A new key requests a new payment. |
| Dashboard Send or Faucet | Pending payments retain their key and exact details across dialog closure, refresh, and browser restart. Use **Resume** for the saved payment. A `broadcast` response does not clear it; terminal `confirmed` or `failed` results release the retained identity. |
| CLI send, shield, unshield, or faucet | Rerun the same arguments against the same instance. The local payment journal retains keys across errors and pending results; an identical command after terminal observation starts a new payment. |

The dashboard reads retained payment outcomes separately from wallet balances.
A confirmation can appear before the wallet snapshot catches up. A partial CLI
account-faucet batch retains its keys until every member has a terminal result;
keep the same account selection when retrying it.

## Decide what to do after a lost response

![After a lost payment response, direct API callers reuse their saved key, dashboard callers resume retained payments, and CLI callers rerun the same command and instance. A new key requests a new payment. Activity omits keys and external faucet records; inspect txids and wallet snapshots separately.](../images/retry-decisions.svg)

1. Save any transaction ID or response you did receive. Query Recent
   activity or `GET /api/v1/activity`, then inspect Zakura's transaction
   and chain state if you have a txid. With a saved key,
   `GET /api/v1/activity?idempotency_key=<key>` reads that payment's stored
   activity, returning a one-item array or `[]`.
2. If the matching activity is recorded, use its status. A direct API caller
   may repeat the same validated request with its **same key** to finish
   confirmation. A `broadcast` activity may also become `confirmed` after
   another block and background confirmation checks.
3. If no matching activity appears, you cannot conclude that nothing was
   broadcast. Confirm that you are querying the same instance and retain the
   original key and details. The server can resume the saved payment even
   when no transaction ID was returned to you; do not replace the key merely
   because the response was lost.
4. `ths faucet <ADDRESS>` calls `POST /api/v1/faucet/address` with a
   journaled idempotency key. A recognized user account appears in activity;
   an external destination has a separate stored faucet record. Read either
   through `GET /api/v1/faucet/address?idempotency_key=<key>` before replaying
   the original request. The lookup is read-only; a stored external faucet
   does not appear in `/api/v1/activity`.

The server validates key syntax and compares recorded payment intents;
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

`POST /api/v1/mine` validates a count of 1–10,000 and starts a server-owned
job. It generates blocks, reads the mined tip, reconciles the wallet, notifies
dashboards, and returns `blocks` plus `hashes`. If a later step errors,
blocks may already exist, even though the request has no success response.
A lost connection does not cancel admitted work; another `ths mine` invocation
uses a fresh internal key and can request additional mining.

The dashboard uses `POST /api/v1/mining/jobs` with a caller key and receives
an accepted job before completion. Replaying the same accepted key and count
returns that job within the server's lifetime. After a lost response, read
`GET /api/v1/mining/jobs` to inspect the latest job before requesting more.

After a timeout, compare the current **node height and tip hash** with
what you observed before the request. Use Network, `GET /api/v1/status`,
or Zakura RPC. Another process can also mine or reorganize a local chain,
so that observation guides a human decision but is not an exactly-once
receipt. [Chapter 5](../mining-and-sync.md) traces the full wallet catch-up.

**Source map:** [API handlers](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/ths-server/src/api.rs),
[SQLite activity and keys](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/ths-server/src/db.rs),
[wallet payments](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/ths-server/src/wallet.rs),
and [chain reconciliation](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/ths-server/src/reconcile.rs).

**Next:** [choose an interface for your own application](../connect-an-app.md).
