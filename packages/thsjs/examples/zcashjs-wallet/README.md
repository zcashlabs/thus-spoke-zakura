# thsjs with a ZcashJS wallet

A test that sends Regtest funds to a ZcashJS wallet and checks when they can be
spent. Change `src/wallet.js` into your app's wallet code; the test shows how to
drive it with thsjs.

| Step | What the test checks |
| --- | --- |
| `fundAddress` | THS has broadcast the payment. |
| 3 blocks | The wallet sees the funds, but can't spend them yet. |
| 4 blocks | The funds are spendable. |

Three isn't enough: the wallet only spends notes mined before its anchor, 3
blocks below the tip (`confirmations` in `src/wallet.js`). When to credit a customer is up to
your app.

## Run

You need Node 22.23.4+, the `ths` launcher with its
[native runtime](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/book/development.md#native-runtime-no-docker),
and a ZcashJS 0.1.2 archive. 0.1.2 isn't on npm yet; build it as described in
the [ZcashJS integration](../../integration/zcashjs/README.md). From the
repository root:

```sh
npm ci --prefix packages/thsjs && npm run build --prefix packages/thsjs
npm install --prefix packages/thsjs/examples/zcashjs-wallet
npm install --prefix packages/thsjs/examples/zcashjs-wallet \
  --ignore-scripts --no-save /path/to/zcashlabs-zcash.js-0.1.2.tgz
THS_BINARY=/path/to/ths THS_RUNTIME_DIR=/path/to/ths-runtime \
  npm test --prefix packages/thsjs/examples/zcashjs-wallet
```

Leave out both variables if `ths` is on your `PATH`. Expected output:

```
✔ a funded payment becomes spendable after enough confirmations
ℹ pass 1
ℹ fail 0
```

Copying this folder into your own project? Install thsjs as the
[package README](../../README.md) describes instead of using `file:../..`.
