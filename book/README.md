# Thus Spoke Zakura

Thus Spoke Zakura (`ths`) gives you a disposable Zcash **Regtest** network on
your own machine. You can mine blocks, fund development accounts, send test ZEC,
inspect transactions, and connect another application. The launcher runs
Zakura, lightwalletd, and a wallet-backed dashboard in Docker. Every
host-published endpoint stays on `127.0.0.1`.

This book teaches **why each part exists and what happens when you use it**.
The chapters build on one another. The [`ths` CLI reference](cli.md),
[development guide](development.md), and [troubleshooting guide](troubleshooting.md)
are there when you need a particular command or diagnosis.

## Who this book is for

You should be comfortable running terminal commands and reading a small HTTP
example. You do not need prior Zcash knowledge. We define Regtest, blocks,
accounts, pools, notes, and wallet synchronization before relying on them.

Everything here is local development infrastructure. Regtest coins and keys
have no value on mainnet or testnet — and the keys are not secret either. The
seed is a fixed constant in the source, so every instance on every machine
derives the same accounts, and anyone who knows this project can derive them
too. That reproducibility is the point; it is also why **nothing of value may
ever touch an address shown by this environment.**

This book documents launcher version **0.2.1**; check yours with
`ths --version`.

## One experiment, several chapters

We will start the default instance, look at its five user accounts, fund
Account 2, send from Account 1 to Account 2, mine another block, and read the
result through the API. Along the way, we will answer three questions:

1. **Who owns this fact?** Zakura owns the chain, lightwalletd indexes it,
   the wallet owns spendable balances, and SQLite records development activity.
2. **When is it true?** A block can exist before lightwalletd indexes it, and
   the index can advance before the wallet publishes a fresh balance snapshot.
3. **What can be retried?** A recorded payment key can resume confirmation;
   a lost response before the key is recorded is ambiguous, and mining has no
   retry key.

Those distinctions explain most surprising behavior in the dashboard.

## How to read

Read [Chapter 1: Your first instance](getting-started.md) if you want to work
along. [Chapter 2](architecture/overview.md) defines the components;
[Chapter 3](accounts-and-balances.md) defines wallet state; [Chapter
4](using-the-dashboard.md) moves funds; [Chapter 5](mining-and-sync.md) follows
a block into the wallet; [Chapter 6](architecture/operations.md) deals with
uncertain responses; and [Chapter 7](connect-an-app.md) connects your code.

A chapter starts with the idea, traces the flow, then gives a concrete action
and what you should observe. Diagrams have text explanations beside them so
the sequence still works without images.

## Four terms to keep separate

| Term | In this book |
| --- | --- |
| **Instance** | One named set of Regtest containers, network, volumes, ports, and disposable data controlled by `ths`. |
| **Node tip** | Zakura's latest block height **and hash**. A height alone does not identify a block after a reorganization. |
| **Wallet snapshot** | The server's last reconciled view of user-account balances. The dashboard reads this through the accounts API. |
| **Activity** | A SQLite record of a faucet or send request, its transaction ID, and its confirmation status. It is not a balance ledger. |

**Next:** [start a fresh instance](getting-started.md).
