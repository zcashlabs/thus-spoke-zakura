# A private Zcash network, on your machine.

Run a complete Zcash Regtest network locally. Fund accounts, send transactions, mine blocks, and inspect every step.

```console
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/zcashlabs/thus-spoke-zakura/main/install.sh | sh
```

Linux and macOS · Docker required · then run `ths`

## One command to confirmation.

1. **Start a network.** One command brings up Zakura, lightwalletd, and five development accounts.
2. **Send a transaction.** Move ZEC between accounts. The confirming block is mined for you.
3. **See it confirmed.** The chain advances and both balances update in the same local run.
4. **Inspect the result.** Open the block and transaction in the explorer to see what happened.

## Set the conditions.

Fund accounts, advance the chain, and return to a clean start on your terms.

- **Fund accounts.** Give a development account up to 5 ZEC per request.
- **Advance the chain.** Mine the blocks your test needs, whenever it needs them.
- **Start clean.** Discard the current local run, then start a fresh network with the same five account addresses.

5 dev accounts · 1 command to start · 0 testnet faucets

## Your endpoints, ready for your tests.

Every service binds to 127.0.0.1 only. By default the dashboard listens on 32805, Zakura RPC on 18232, P2P on 18233, and lightwalletd on 9067. Use `ths endpoints --json` to give your app or test suite the current addresses.

```console
$ ths endpoints --json
{
  "dashboard":    "http://127.0.0.1:32805",
  "rpc":          "http://127.0.0.1:18232",
  "lightwalletd": "http://127.0.0.1:9067",
  "p2p":          "127.0.0.1:18233",
  "network":      "regtest",
  "tls":          false
}
```

| Endpoint | Purpose |
| --- | --- |
| `dashboard` | Web UI, and the REST API your tests drive |
| `rpc` | Zakura JSON-RPC |
| `lightwalletd` | gRPC for light clients |
| `p2p` | Peer port, to attach a second node |

For another isolated network, run `ths --name second start --port-offset 10` and read its endpoints with `ths --name second endpoints --json`.

## Keep building.

Start with the commands, then use the account and troubleshooting guides as your tests grow.

- [Your first instance](https://ths.zypherpunk.xyz/book/getting-started.html): Install everything and bring up your first network.
- [Components and state](https://ths.zypherpunk.xyz/book/architecture/overview.html): What each service owns, and where state actually lives.
- [Accounts and balances](https://ths.zypherpunk.xyz/book/accounts-and-balances.html): The five development accounts, pools, and the public seed.
- [Fund and send](https://ths.zypherpunk.xyz/book/using-the-dashboard.html): Move ZEC from the dashboard and from the CLI.
- [Mining and wallet sync](https://ths.zypherpunk.xyz/book/mining-and-sync.html): Advance the chain and keep the wallet in step.
- [Connect your application](https://ths.zypherpunk.xyz/book/connect-an-app.html): Point your app or test suite at the running network.
- [The ths CLI](https://ths.zypherpunk.xyz/book/cli.html): Every command and option, with examples.
- [Troubleshooting](https://ths.zypherpunk.xyz/book/troubleshooting.html): Docker permissions and services that will not start.
- [Develop from source](https://ths.zypherpunk.xyz/book/development.html): Build it yourself, run the tests, and contribute.

## Your first network is one command away.

Install it, run `ths`, and you have a Zcash chain you control end to end. [View on GitHub](https://github.com/zcashlabs/thus-spoke-zakura).

---

Built by Zypherpunks: [all contributors](https://github.com/zcashlabs/thus-spoke-zakura/graphs/contributors). Built for local experiments.
