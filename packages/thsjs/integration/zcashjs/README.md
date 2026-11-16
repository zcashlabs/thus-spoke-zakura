# Test a separate ZcashJS application

This private fixture imports the normal ZcashJS wallet API and the standalone
`@zcashlabs/thsjs` package. Neither library imports the other. It requires a
NU7-capable ZcashJS package archive and a native THS launcher with its matching
runtime bundle. All chain traffic stays on local Regtest.

Build and pack ZcashJS from its own checkout, following its
[build instructions](https://github.com/zcashlabs/zcashjs/blob/main/docs/api/building-locally.md).
Use the Node version required by that SDK build. This test takes the built
archive explicitly, so it can verify an SDK version before its npm release.

From the THS repository root:

```sh
npm ci --prefix packages/thsjs
npm pack ./packages/thsjs --pack-destination /tmp
npm install --prefix packages/thsjs/integration/zcashjs \
  --ignore-scripts --no-save --package-lock=false \
  /tmp/zcashlabs-thsjs-0.1.0.tgz /absolute/path/to/zcashlabs-zcash.js-0.1.2.tgz
THS_BINARY=/absolute/path/to/ths THS_RUNTIME_DIR=/absolute/path/to/ths-runtime \
  npm test --prefix packages/thsjs/integration/zcashjs
```

The SDK archive must already contain its compiled wallet runtime and proving
assets; installation does not run build scripts. No SDK source checkout or
changes to its exports are required by this consumer.

The test checks native startup, exactly-once funding, a wallet-built payment,
mining, reorg, rollback, and cleanup after success, failure and cancellation.
It writes results under `packages/thsjs/.cache/zcashjs/`. It intentionally runs
outside the normal package suite because it needs real node binaries and a
built wallet SDK. The package itself has no ZcashJS dependency.
