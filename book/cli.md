# Appendix A: `ths` CLI reference

`ths` is the Thus Spoke Zakura launcher. It manages the Docker-based Regtest
environment and, once an environment is running, talks to its dashboard API on
your behalf. This page documents every command in detail, with examples.

Read [Chapter 1](getting-started.md) for an instance's lifecycle, [Chapter
3](accounts-and-balances.md) for accounts and pools, and [Chapter
6](architecture/operations.md) before retrying a payment or mine request.
The examples below demonstrate individual commands; they are not one script
to run in order. For a worked dashboard flow, see [Chapter
4](using-the-dashboard.md).

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
- Commands that call the app (`mine`, `faucet`, and `wallet` subcommands) require
  a running environment. Start one first with `ths` (or `ths --name <NAME>`).
  `stop` and `reset --force` can remove resources left by an interrupted run.

---

## Environment lifecycle

### `ths` / `ths start`

Starts a fresh environment in the foreground and opens the dashboard in your
browser. Interrupting with Ctrl+C stops the environment and deletes its chain,
wallet, keys, and Docker volumes.

```console
ths
ths start --no-open      # don't open a browser tab
ths --name alice start --port-offset 10  # start a second, independent environment
```

The default loopback ports are dashboard `32805`, Zakura RPC `18232`, P2P
`18233`, and lightwalletd `9067`. `--port-offset` adds a multiple of 10 to
each host port (default: `0`). Startup fails and names an occupied port
instead of selecting a random port.

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

JSON output includes `"network": "regtest"` and `"tls": false`; lightwalletd
uses plaintext HTTP on loopback.

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

> Note: `stop` removes the Docker resources but does not signal a `ths start`
> running in another terminal. That launcher stays in the foreground
> supervising nothing until you press Ctrl+C there. Prefer Ctrl+C in the
> launcher's own terminal; use `stop` for an environment whose launcher is
> already gone.

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

`ths --version` prints the installed version. This book documents `0.2.1`.

`uninstall` removes **only** the executable, and says so. Two things survive
it: roughly 1.7 GB of Docker images, and the launcher's configuration
directory, whose path `ths doctor` prints. To remove everything:

```console
ths stop                 # for each environment still defined; see `ths list`
ths uninstall
docker image rm ghcr.io/zcashlabs/thus-spoke-zakura-app:0.2.1 \
                ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd:0.2.1 \
                zakuracore/zakura:1.4.0
rm -rf "$(ths doctor --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["config_dir"])')"
```

Read the config directory path before uninstalling — `ths doctor` will not
run afterwards.

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

Every subcommand attempts to mine a confirming block and prints the resulting
transaction ID. If auto-mining fails, activity may remain in `broadcast`
state; `send`, `shield`, and `unshield` print a confirming block hash when one
is available. Pass `--json` for machine-readable output.

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

Memos here are **write-only**. This environment can send one and can never
show it to you again: there is no memo column in the activity table, no memo
field in any API response, and no place in the dashboard that displays one.
The memo is genuinely in the Orchard output — reading it just has to happen
elsewhere. `GET /api/v1/accounts` returns a `unified_full_viewing_key` for
each of Accounts 1–5; import that into a wallet that can decrypt memos, and
it will show the memos sent to that account.

### Putting it together: seeding a fresh environment

A typical setup starts the environment in one terminal:

```console
ths --name demo start --no-open
```

Wait for `demo is ready`, then run these commands in a second terminal:

```console
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
| `ths start --port-offset <N>` | Shift loopback host ports by a multiple of 10 for another instance |
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

Every command accepts `--name` for isolated environments. For more on running
them side by side, see [Chapter
1](getting-started.md#give-another-experiment-its-own-name).

The [troubleshooting appendix](troubleshooting.md) starts from observable
symptoms; [Chapter 7](connect-an-app.md) shows how to use the same live
endpoints from your own code.
