# 2. Components and state

Chapter 1 started an instance and printed four endpoints. Now we will
follow those endpoints inward. The important question is not merely which
container handles a request, but **which component owns the answer**.

A **block** is a unit of chain history. The **node tip** is Zakura's latest
block height and hash. The **mempool** holds transactions the node knows
about before they are included in a block. A **wallet** scans chain data for
funds it can spend. A wallet can lag the node even when both services are
healthy.

![Your machine holds the launcher, a browser and your own code; all four published ports bind to loopback. Inside a private Docker network sit the app, lightwalletd and Zakura containers, each backed by its own volume.](../images/architecture-topology.svg)

1. `ths` supervises a Zakura container, a lightwalletd container, and an app
   container on an instance-specific Docker network.
2. Zakura owns Regtest blocks, mempool, and JSON-RPC. lightwalletd follows
   Zakura and serves compact blocks to wallet clients over gRPC.
3. The app runs `tsz-server`. It serves both `/api/v1/` and the built React
   dashboard, owns the wallet databases, and uses lightwalletd to scan and
   broadcast.
4. A browser or local program reaches only the loopback-published ports.
   Internal container listeners can use `0.0.0.0` inside the private network;
   every host-published service binds to `127.0.0.1`.

## The five owners of information

| Component | Owns | Read it through |
| --- | --- | --- |
| Zakura | Canonical chain, block height and hash, mempool, transaction lookup | JSON-RPC or the server's explorer API |
| lightwalletd | Compact-block index and gRPC stream over the node's chain | A lightwalletd-compatible wallet client |
| Wallet SDK in `tsz-server` | Scanned notes, transparent outputs, and spendable balances in `wallet.db` | The server's wallet snapshot and accounts API |
| Server SQLite store | Seed, derived account records, activity, and idempotency keys in `tsz.db` | Server API and local development commands |
| Browser | UI state and a query cache of server responses | Wallet, Explorer, and Network pages |

The app's wallet and SQLite files share the instance's `wallet` volume, but
they answer different questions. `wallet.db` tells the wallet what it has
scanned and can spend. `tsz.db` records which development request produced a
transaction ID. Activity cannot reconstruct a balance.

![The chain flows left to right from Zakura through lightwalletd's index into the server wallet's scan, which publishes an account snapshot the dashboard reads. SQLite activity is a separate path written when the server sends, not when it scans.](../images/state-ownership.svg)

1. Zakura advances the node tip. lightwalletd indexes compact blocks from
   that chain.
2. The server wallet scans through lightwalletd and compares its checkpoint
   height **and hash** with Zakura before publishing a fresh account snapshot.
   If the chain reorganizes, it rewinds to a common block and scans again.
3. `GET /api/v1/accounts` reads the server snapshot. The dashboard's query
   hooks fetch that endpoint. Server-sent `wallet`, `chain`, and `sync` events
   tell the hooks to refetch, rather than carrying balances themselves.
4. SQLite activity records a send or faucet request's transaction ID and
   confirmation status. `GET /api/v1/activity` reads it separately.

This ordering explains a common observation: the Network page may show a new
node height while Wallet still shows the last known balances. The wallet must
wait for indexing and scanning, and the server must validate the chain
checkpoint. `GET /api/v1/status` reports `wallet_sync.state` as `syncing`,
`ready`, or `error`, along with observed and scanned heights and hashes.
When synchronization fails, the last snapshot may remain visible. Treat it as
stale until the server reaches `ready`.

## Startup and data ownership

The app image first runs `init`. It creates `tsz.db`, `wallet.db`, the seed,
six derived account records, and a Regtest Zakura configuration. Account 6 is
the hidden mining and faucet treasury; only Accounts 1–5 are exposed as user
accounts. The node mines to the treasury's transparent address. Normal
transparent-output queries cover user accounts; treasury rewards are
discovered when a faucet payment needs replenishment.

The instance also has separate `chain`, `lightwalletd`, and `config`
volumes. `ths` removes volumes belonging to the selected instance on
shutdown, interruption, or startup failure. Other named instances survive.

For the exact container and endpoint lifecycle, revisit [Chapter
1](../getting-started.md). For the meaning of an account
balance, continue to [Chapter 3](../accounts-and-balances.md).

**Source map:** [launcher runtime](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-cli/src/runtime.rs),
[server startup](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-server/src/main.rs),
[API snapshot](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-server/src/api.rs),
[reconciliation](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/crates/tsz-server/src/reconcile.rs),
and [dashboard queries](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/web/src/hooks/queries.ts).

**Next:** [accounts, pools, and balances](../accounts-and-balances.md).
