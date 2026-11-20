# Receive a Regtest payment with ZcashJS

`payment.mjs` uses `@zcashlabs/thsjs` to start and stop a disposable native THS
instance. It funds an in-memory ZcashJS Ironwood wallet, manually mines blocks,
and syncs the SDK wallet separately at each step. Under the example's
three-block anchor depth, the same 1,000,000-zatoshi payment becomes spendable
at four confirmations. Rolling back one block returns it to three confirmations:
it is still mined, but no longer spendable.

ZcashJS 0.1.2 can retain the previous spendable balance when a rollback only
shortens the same branch. For this disposable demo, the script restores the same
account in a fresh memory wallet and scans from its saved birthday after rollback.

Use a matching THS launcher and [native runtime bundle](../../../../RELEASING.md#native-runtime-contract)
with the default Regtest schedule: all upgrades from Overwinter through NU7
activate at height 1. The example does not support custom upgrade heights.

Use Node 22.23.4 and ZcashJS 0.1.2 at
[`cb1b706caf2c30e49fb4b9db64730e5d1dc6f518`](https://github.com/zcashlabs/zcashjs/commit/cb1b706caf2c30e49fb4b9db64730e5d1dc6f518).
With the SDK's [build prerequisites](https://github.com/zcashlabs/zcashjs/blob/cb1b706caf2c30e49fb4b9db64730e5d1dc6f518/docs/api/building-locally.md), run
`npm ci && npm pack --pack-destination /tmp` in a separate SDK checkout at
that revision. This builds the wallet runtime and proving assets into the SDK
archive. Install the locally built archives below, not public npm releases.

From the THS repository root:

```sh
npm ci --prefix packages/thsjs
npm pack ./packages/thsjs --pack-destination /tmp
npm install --prefix packages/thsjs/integration/zcashjs \
  --ignore-scripts --no-save --package-lock=false \
  /tmp/zcashlabs-thsjs-0.1.0.tgz /tmp/zcashlabs-zcash.js-0.1.2.tgz
THS_BINARY=/absolute/path/to/ths THS_RUNTIME_DIR=/absolute/path/to/ths-runtime \
  node packages/thsjs/integration/zcashjs/payment.mjs
```

## Broader live regression

With the same environment and packages, run
`npm test --prefix packages/thsjs/integration/zcashjs`.
The test checks native startup, exactly-once funding, a wallet-built payment,
mining, reorg, rollback, and cleanup after success, failure and cancellation.
It also kills test workers with SIGTERM and SIGKILL, checks all four sandbox
ports using TCP, and verifies that an independent instance still works.
It writes results under `packages/thsjs/.cache/zcashjs/`. It intentionally runs
outside the normal package suite because it needs real node binaries and a
built wallet SDK. The package itself has no ZcashJS dependency.
