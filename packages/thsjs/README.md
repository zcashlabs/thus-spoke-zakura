# Native Regtest tests with THS

`@zcashlabs/thsjs` exports `withTestTHS` and `createTestTHSClient` for Node 22.23.4 and newer (tested on Node 22 and 24). They start an isolated, manually mined Regtest instance with the existing [Thus Spoke Zakura](https://github.com/zcashlabs/thus-spoke-zakura) launcher. This is a standalone Node-only package owned by THS. It has no runtime dependencies and does not depend on ZcashJS. Applications install it as a development dependency; their wallet SDK remains separate.

Install or build a THS launcher and matching native runtime before using this API. The launcher must support `start --native --json` and the status, mine, reorg and rollback APIs. See the [native runtime instructions](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/book/development.md#native-runtime-no-docker) for installation and source builds. Native THS supports macOS and Linux on Intel and ARM. This API uses your installation and never downloads executables. Docker startup is not supported by this helper.

## Install from this checkout

The package has not been published to npm yet. From the THS repository root:

```sh
npm ci --prefix packages/thsjs
npm pack ./packages/thsjs --pack-destination /tmp
```

In your application's repository:

```sh
npm install --save-dev /tmp/zcashlabs-thsjs-0.1.0.tgz
```

The archive contains JavaScript and TypeScript declarations. Consumers do not
need a compiler, the THS source tree, or ZcashJS to use it. Installing it does
not start THS or download runtime binaries.

## Automatic setup and cleanup

Wrap a test body in `withTestTHS`. It starts a fresh sandbox, calls your test only after startup succeeds, and waits for shutdown before returning the test's result. It also shuts down when an assertion throws. If the test and shutdown both fail, an `AggregateError` preserves both errors, with the test error first. Concurrent calls use separate instances. Use the callback from Node's test runner, Vitest, or Jest; no test-runner dependency is required.

```ts
import { withTestTHS } from '@zcashlabs/thsjs';

await withTestTHS(async ths => {
  const before = await ths.rpc('getblockcount');
  await ths.produceBlock(3);
  const after = await ths.rpc('getblockcount');
  if (typeof before !== 'number' || after !== before + 3) {
    throw new Error('Expected exactly three new blocks');
  }
});
```

The second argument accepts the same launcher options as `createTestTHSClient`, plus an optional `signal`. Pass the test runner's abort signal, such as Node's `t.signal`, to shut down on test cancellation or timeout. An already aborted signal prevents startup. Cancellation waits for THS shutdown even if the callback is still waiting; application work must observe its own signal to stop too. `timeoutMs` bounds THS operations, not the entire test body.

```ts
import { test } from 'node:test';
import { withTestTHS } from '@zcashlabs/thsjs';

test('application sandbox', { timeout: 240_000 }, async t => {
  await withTestTHS(async ths => {
    await ths.produceBlock(1);
    // Exercise your application using ths.config().
  }, { signal: t.signal });
});
```

Tests that share a sandbox can instead call `start()` in their setup hook and `stop()` in their teardown hook. Register teardown before awaiting startup.

## Sandbox controls

| Handle | Behavior |
| --- | --- |
| `config()` | Returns the instance's dashboard, RPC and lightwalletd endpoints and network identity. Pass lightwalletd to your application or ZcashJS wallet. |
| `fundAddress(address, amountZatoshi, options?)` | Sends test funds to an application wallet and waits for broadcast. Amounts are bigint zatoshi, from `1n` to `500_000_000n` (5 ZEC). |
| `produceBlock(count)` | Mines blocks and waits for the node, indexer and THS wallet to sync. |
| `reorg(depth, { keepTransactions? })` | Replaces recent blocks, optionally trying to include their transactions again. |
| `rollback(depth)` | Removes recent blocks without producing replacements. |
| `rpc(method, params?)` | Calls a Zakura RPC method on this sandbox. Returns `unknown`; validate it or use the SDK's typed RPC client with `config().rpc`. |
| `waitUntilSettled()` | Waits for node, indexer and THS wallet agreement after direct RPC changes. |

`fundAddress` returns `{ address, amountZatoshi, idempotencyKey, txid, status }`, where status is `broadcast` or `confirmed`. THS validates that the address belongs to Regtest. It supports unified and transparent destinations. The helper does not mine confirmation blocks for the payment: call `produceBlock` for the confirmations your application requires, then sync the application's wallet. THS may replenish its treasury while preparing funding, so funding is not a guarantee that chain height stays fixed.

Supply a stable `idempotencyKey` when you may retry funding. Reusing it with the same address and amount retrieves the same payment; changing either is rejected by THS. Keys contain 8-128 visible ASCII characters. When omitted, the helper generates a UUID. Funding errors include the key, so a lost response can be investigated or retried with the same key instead of creating another payment. Only the status read is polled; the payment submission is never automatically repeated.

Direct RPC calls are not automatically retried or followed by settlement. They expose the methods the node supports, including mutations; a call can make the sandbox unavailable. All handles stay bound to the helper's own validated Regtest instance. A response from another instance is rejected.

## Manual lifecycle

```ts
import { createTestTHSClient } from '@zcashlabs/thsjs';

const ths = createTestTHSClient({
  executable: '/absolute/path/to/ths',
  runtimeDir: '/absolute/path/to/ths-runtime',
  timeoutMs: 180_000,
});
try {
  await ths.start();
  const config = ths.config();
  console.log(config.lightwalletd);
  await ths.produceBlock(10);
  await ths.reorg(2, { keepTransactions: true });
  await ths.rollback(1);
} finally {
  await ths.stop();
}
```

With `createTestTHSClient()` the executable defaults to `ths` on PATH and runtime discovery belongs to the launcher. `timeoutMs` defaults to 180,000 and bounds startup, each chain operation including settlement, and shutdown separately. Failed startup also waits for cleanup, so it may consume a second timeout budget.

`start()` resolves to the same frozen config returned by `config()`: `instance`, `dashboard`, `rpc`, `lightwalletd`, `p2p`, `network: 'regtest'`, `tls: false`, and `genesisHash`. Concurrent starts coalesce. Config access before readiness or after launcher failure throws. Each restart after `stop()` generates a fresh instance name. The helper accepts no existing instance or arbitrary service URL. It checks numeric loopback endpoints, the instance identity, Regtest network and exact Regtest genesis before startup completes, and checks instance identity again before chain operations. HTTP redirects are rejected.

`produceBlock(count)` accepts integers from 1 to 10,000. `reorg(depth, { keepTransactions })` and `rollback(depth)` accept integers from 1 to 1,000; the chain must also contain sufficient blocks. `keepTransactions` defaults to `false`. These methods resolve with no value after THS reports its node, index and internal wallet settled. External SDK wallets must still call `wallet.sync()` themselves. Every RPC, funding and settlement call also has one total `timeoutMs` budget and is cancelled by `stop()`. Chain operations are not automatically retried: a timed-out operation might already have changed the chain.

Always await `stop()` in `finally`. It cancels startup and pending requests, asks its supervisor to send SIGTERM to the owned launcher, and awaits the launcher's service cleanup. Repeated stops are harmless. A cleanup timeout is an error; the helper retains the run and prevents restart until cleanup is confirmed by a subsequent stop. Unexpected nonzero launcher exits are reported as errors; code 143 is accepted after the helper requests shutdown. An interrupted native startup can exit with code 1 after cleaning its processes and deleting its instance state; both `start()` and `stop()` then reject, preserving the launcher's error message. Errors use ordinary JavaScript `Error`/`AggregateError`, with launcher diagnostics for startup failures.

Each run has a private Node supervisor that watches its connection to the test
worker. If the worker exits or is killed, including with SIGKILL, the supervisor
stops its own launcher and waits for cleanup. It does not replace your
application's signal handlers or stop other THS instances. Cleanup continues
after the worker has exited; awaiting `stop()` is how a live caller confirms
that cleanup has finished.

## Package development

```sh
npm ci --prefix packages/thsjs
npm test --prefix packages/thsjs
```

Tests exercise lifecycle failures and sandbox controls with disposable local
CLI fixtures. They also pack the package, install it into a temporary consumer
without ZcashJS or build tools, exercise its API, compile a typed consumer, and
reject browser imports. No Docker or real node is needed for this suite.

The opt-in [ZcashJS integration](https://github.com/zcashlabs/thus-spoke-zakura/tree/main/packages/thsjs/integration/zcashjs) uses separately
installed package archives and a real native runtime. It covers an external
wallet, exact funding and replay, payment submission, mining, reorg, rollback,
and cleanup after passing, failing and cancelled tests. It also terminates
JavaScript workers and verifies that all four sandbox ports close while an
independent instance stays usable. ZcashJS is never
installed by the package's ordinary build or tests.

This client was extracted from ZcashJS PR #27. Import from `@zcashlabs/thsjs`
instead of the proposed `@zcashlabs/zcash.js/testing` entry point; the public
helper API is unchanged. The original MIT license is retained.
