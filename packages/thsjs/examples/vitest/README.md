# thsjs with Vitest

Run your tests against a local Zcash Regtest network that thsjs starts and
stops for you. `sandbox.test.js` shows the two ways to do it:

- **Shared:** one sandbox for a group of tests, started in `beforeAll`.
- **Per test:** `withTestTHS` gives one test its own sandbox.

To test your app, pass it `ths.config().lightwalletd` or `ths.config().rpc`
and replace the assertions.

## Run

You need Node 22.23.4+ and the `ths` launcher with its
[native runtime](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/book/development.md#native-runtime-no-docker).
From the repository root:

```sh
npm ci --prefix packages/thsjs && npm run build --prefix packages/thsjs
npm install --prefix packages/thsjs/examples/vitest
THS_BINARY=/path/to/ths THS_RUNTIME_DIR=/path/to/ths-runtime \
  npm test --prefix packages/thsjs/examples/vitest
```

Leave out both variables if `ths` is on your `PATH`. Expected output:

```
Test Files  1 passed (1)
     Tests  4 passed (4)
```

Copying this folder into your own project? Install thsjs as the
[package README](../../README.md) describes instead of using `file:../..`.
