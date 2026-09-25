# `ths` CLI reference

`ths` is the Thus Spoke Zakura launcher. It manages the Docker-based Regtest
environment and, once an environment is running, talks to its dashboard API on
your behalf. This page documents every command in detail, with examples.

For a quick tour of the dashboard itself, see the [README](../README.md).

## Conventions used below

- All commands accept two **global options**, which can be placed before or
  after the subcommand:
  - `--name <NAME>` — target a named, isolated environment instead of the
    default one (default: `default`).
  - `--json` — print machine-readable JSON instead of human-readable text,
    where the command supports it.
- Accounts are referred to by **index**, `1` through `5`. Every environment
  starts with five disposable development accounts; there is no need to copy
  full Regtest addresses to use the commands on this page. (A hidden sixth
  account acts as the mining/faucet treasury and is not addressable from the
  CLI.)
- Amounts are ZEC decimal strings with up to 8 decimal places, e.g. `1`,
  `0.5`, `2.25000001`. These are disposable Regtest coins with no value.
- Every command in this reference (other than `start`, `build`, `pull`,
  `update`, `uninstall`, `list`, and `doctor`) requires a running environment.
  Start one first with `ths` (or `ths --name <NAME>`).

---

## Environment lifecycle

### `ths` / `ths start`

Starts a fresh environment in the foreground and opens the dashboard in your
browser. Interrupting with Ctrl+C stops the environment and deletes its chain,
wallet, keys, and Docker volumes.

```console
ths
ths start --no-open      # don't open a browser tab
ths --name alice         # start a second, independent environment named "alice"
```

### `ths build [--dev]`

Builds the runtime Docker images from the current source checkout (for
development from a clone, not needed for the installed release).

```console
ths build             # release-optimized images
ths build --dev       # keep workspace Rust code unoptimized for faster rebuilds
```

### `ths pull`

Pulls the exact runtime images that match this launcher's version, instead of
building them locally.

```console
ths pull
```

### `ths status [--json]`

Shows whether the named environment is running and prints its endpoints.

```console
ths status
ths status --name alice --json
```

### `ths open`

Opens the running dashboard in your default browser.

```console
ths open
```

### `ths endpoints [--json]`

Prints the dashboard, Zakura RPC, lightwalletd, and P2P endpoints, useful for
scripts and other developer tooling.

```console
ths endpoints
ths endpoints --json
```

### `ths logs [app|zakura|lightwalletd] [-f|--follow]`

Prints or streams logs for a service in the named environment. Defaults to the
`app` (dashboard/server) service.

```console
ths logs                       # last logs from the app service
ths logs zakura -f             # follow node logs
ths logs lightwalletd --follow # follow lightwalletd logs
```

### `ths stop`

Stops the named environment and deletes its containers, volumes, and network.

```console
ths stop
```

### `ths reset --force`

Same as `stop`, but named explicitly as a destructive action; requires
`--force` since it permanently deletes chain, wallet, and seed data.

```console
ths reset --force
```

### `ths list [--json]`

Lists every known environment and its dashboard URL.

```console
ths list
```

### `ths doctor [--json]`

Checks that Docker is reachable and prints the launcher's configuration
directory.

```console
ths doctor
```

### `ths update [VERSION] [--check]` / `ths uninstall`

Checks for, installs, or rolls back a released launcher version, or removes
the installed `ths` executable.

```console
ths update --check     # only report whether a newer release exists
ths update              # install the latest verified release
ths update v0.2.0       # install (or roll back to) an exact version
ths uninstall
```

### `ths mine <BLOCKS>`

Mines blocks on the running environment and synchronizes its wallet — useful
for testing confirmations, expiry, or coinbase maturity.

```console
ths mine 1
ths mine 10 --json
```

---

## Funding an address: `ths faucet <ADDRESS> [--amount <ZEC>]`

Sends disposable Regtest ZEC from the treasury directly to any Regtest unified
or transparent address, not necessarily one of the five development accounts.
Limited to 5 ZEC per request; defaults to 1 ZEC. It does not move funds between
accounts and takes no memo. For the development accounts, use
[`ths wallet`](#ths-wallet-the-development-wallet).

```console
ths faucet uregtest1exampleaddress...
ths faucet tmExampleTransparentAddress... --amount 2.5
```

## `ths wallet`: the development wallet

Everything under `ths wallet` acts on the wallet held by the running `ths`
server: the five development accounts (index `1`-`5`) and the hidden treasury
that funds them. Its subcommands do the same things as the dashboard's Faucet
and Send dialogs, from a script or terminal:

| Subcommand | What it does |
| --- | --- |
| `ths wallet faucet` | Fund one or more accounts from the treasury |
| `ths wallet send` | Send funds from one account to another, with an optional memo |
| `ths wallet shield` | Spend one account's transparent funds into another account's Orchard balance |
| `ths wallet unshield` | Spend one account's Orchard funds into another account's transparent balance |

Every subcommand mines the confirming block automatically and prints the
resulting transaction ID (and, for `send`, `shield`, and `unshield`, the
confirming block hash). Pass `--json` for machine-readable output.

### `ths wallet faucet --accounts <LIST> [--amount <ZEC>] [--pool <POOL>]`

Funds one or more accounts from the treasury faucet in a single command.

- `--accounts <LIST>` (required): comma-separated account indices, e.g.
  `1,2,3,5`. Each listed account receives its own independent faucet
  transaction for the full `--amount`.
- `--amount <ZEC>`: amount to send to **each** account (default `1`, maximum
  `5`, matching the dashboard faucet's per-request limit).
- `--pool <orchard|transparent>`: which balance to fund (default `orchard`).

```console
# Fund accounts 1, 2, 3, and 5 with 3 ZEC each (shielded)
ths wallet faucet --accounts 1,2,3,5 --amount 3

# Fund a single account's transparent balance
ths wallet faucet --accounts 4 --amount 1 --pool transparent

# Machine-readable output, e.g. for a setup script
ths --json wallet faucet --accounts 1,2,3,4,5 --amount 5
```

If some accounts fail (for example, a treasury shortfall) the command still
funds the rest, prints a `Failed to fund ...` line per failure, and exits with
a non-zero status summarizing how many of the requests failed.

### `ths wallet send --from <N> --to <N> --amount <ZEC> [--source-pool <POOL>] [--destination-pool <POOL>] [--memo <TEXT>]`

Sends funds from one development account to another. It does the same thing as
the dashboard's **Send ZEC** dialog and uses the same endpoint
(`POST /api/v1/send`).

- `--from <N>` / `--to <N>` (required): two different account indices,
  `1`-`5`. The treasury account can't be used.
- `--amount <ZEC>` (required): limited only by the sending account's balance.
- `--source-pool <orchard|transparent>`: pool to spend from (default
  `orchard`).
- `--destination-pool <orchard|transparent>`: pool the destination account
  receives into (default `orchard`).
- `--memo <TEXT>`: optional memo for the recipient. See [Memos](#memos).

```console
# Move 1 ZEC from account 2's Orchard balance to account 3's Orchard balance
ths wallet send --from 2 --to 3 --amount 1

# Spend account 1's transparent balance into account 4's Orchard balance
ths wallet send --from 1 --to 4 --amount 0.5 --source-pool transparent

# Attach a memo for the recipient
ths wallet send --from 2 --to 3 --amount 1 --memo "rent for October"

# Rejected: transparent outputs cannot carry a memo
ths wallet send --from 2 --to 3 --amount 1 --destination-pool transparent --memo "hi"
```

### `ths wallet shield --from <N> --to <N> --amount <ZEC> [--memo <TEXT>]`

Shorthand for `ths wallet send` with `--source-pool transparent
--destination-pool orchard`: spends one account's transparent balance into
another account's Orchard balance.

- `--from <N>` (required): account to spend transparent funds from.
- `--to <N>` (required): a different account to receive them as Orchard
  funds. Sends between the same account are rejected.
- `--amount <ZEC>` (required).
- `--memo <TEXT>`: optional memo on the Orchard output (up to 512 bytes).

```console
# Shield 0.5 ZEC of account 4's transparent balance into account 2's Orchard balance
ths wallet shield --from 4 --to 2 --amount 0.5

# The same, with a memo
ths wallet shield --from 4 --to 2 --amount 0.5 --memo "welcome to the shielded pool"
```

> Note: because transparent notes must be fully spent, the wallet may route
> any leftover change from a transparent source into the shielded pool as
> well. Shielding "0.5 ZEC" from an account holding 1 transparent ZEC can
> leave that account with its change in Orchard rather than transparent.

### `ths wallet unshield --from <N> --to <N> --amount <ZEC>`

Shorthand for `ths wallet send` with `--source-pool orchard
--destination-pool transparent`: spends one account's Orchard balance into
another account's transparent balance. It takes no memo, because transparent
outputs cannot carry one.

- `--from <N>` (required): account to spend Orchard funds from.
- `--to <N>` (required): a different account to receive them as transparent
  funds.
- `--amount <ZEC>` (required).

```console
# Unshield 0.2 ZEC from account 1's Orchard balance into account 3's transparent balance
ths wallet unshield --from 1 --to 3 --amount 0.2
```

### Memos

Zcash lets the sender attach a memo of up to 512 bytes to an Orchard output
(ZIP 302). The memo is encrypted to the recipient. Only holders of that
account's keys can read it.

- A memo is only allowed when the destination pool is `orchard`. With a
  `transparent` destination, `ths`, the dashboard, and the API all reject it.
  Transparent outputs have no memo field, and none is invented.
- `ths wallet send`, `ths wallet shield`, and the dashboard's **Send ZEC**
  dialog accept a memo. `ths faucet`, `ths wallet faucet`, and
  `ths wallet unshield` do not.
- The limit is 512 **bytes**, not characters. Non-ASCII text, such as emoji or
  CJK characters, takes several bytes per character.
- Leaving out `--memo` sends no memo. `--memo ""` sends an explicit empty
  text memo.
- A memo can't end with a NUL (U+0000) character. Memos are padded with zero
  bytes, so a trailing NUL would be lost when the memo is read.
- The memo is attached to the recipient's output only. Change returned to the
  sender has no memo.
- The block explorer shows public transaction data only. It never shows memo
  plaintext.
- All five accounts and the treasury come from one seed as separate ZIP-32
  accounts. A viewing-only wallet imported with one account's viewing key can
  decrypt only the memos sent to that account.

### Putting it together: seeding a fresh environment

A typical setup script for a fresh environment might look like:

```console
ths --name demo start --no-open &
sleep 5   # or poll `ths status --name demo` until it reports "running"

ths --name demo wallet faucet --accounts 1,2,3,4,5 --amount 5
ths --name demo wallet faucet --accounts 2 --amount 1 --pool transparent
ths --name demo wallet shield --from 2 --to 4 --amount 0.5
ths --name demo wallet send --from 1 --to 3 --amount 1 --memo "welcome"
```

This funds all five accounts, gives account 2 some transparent balance,
shields part of it into account 4, and sends shielded funds with a memo from
account 1 to account 3, all without opening the dashboard.

---

## Command summary

| Command | What it does |
| --- | --- |
| `ths` | Start a fresh environment and open the dashboard |
| `ths start --no-open` | Start without opening a browser |
| `ths build [--dev]` | Build runtime images from source |
| `ths pull` | Pull the exact images for this launcher version |
| `ths status [--json]` | Show health and endpoint information |
| `ths open` | Open the running dashboard |
| `ths endpoints [--json]` | Print endpoints for scripts and developer tools |
| `ths mine <N>` | Mine blocks and synchronize the wallet |
| `ths faucet <ADDRESS> [--amount]` | Send disposable ZEC to any Regtest address |
| `ths wallet faucet --accounts <LIST> [--amount] [--pool]` | Fund one or more of the five accounts by index |
| `ths wallet send --from --to --amount [--source-pool] [--destination-pool] [--memo]` | Send between accounts by index, optionally with an Orchard memo |
| `ths wallet shield --from --to --amount [--memo]` | Spend transparent funds into another account's Orchard balance |
| `ths wallet unshield --from --to --amount` | Spend Orchard funds into another account's transparent balance |
| `ths logs [service] [-f]` | Stream or print service logs |
| `ths list` | List known environments |
| `ths stop` | Stop and delete the environment |
| `ths reset --force` | Force-delete one environment and all its data |
| `ths doctor` | Check Docker and local configuration |
| `ths update [--check]` | Check for or install a newer release |
| `ths uninstall` | Remove the installed launcher executable |

Every command accepts `--name` for isolated environments; see the
[README](../README.md#useful-commands) for more on running multiple named
environments side by side.
