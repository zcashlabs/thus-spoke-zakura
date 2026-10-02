# `ths` CLI reference

`ths` is the Thus Spoke Zakura launcher. It manages a local Regtest environment
using a Docker node, a local Zakura executable, or a node you started on
localhost. The app and lightwalletd run in Docker in all modes. Once an
environment is running, the launcher talks to its dashboard API on your behalf.
This page documents every command in detail, with examples.

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
- Wallet, mining, faucet, and dashboard commands require a running environment.
  Start one first with `ths` (or `ths --name <NAME>`). Lifecycle and diagnostic
  commands can also operate on stopped or prepared environments.

---

## Environment lifecycle

### `ths` / `ths start`

Starts a fresh environment in the foreground and opens the dashboard in your
browser. By default, interrupting with Ctrl+C stops the environment and deletes
its chain, wallet, keys, and Docker volumes. Local-binary mode has the same
managed lifecycle; attaching to a self-managed localhost node instead preserves
that node and the prepared wallet for reattachment.

```console
ths
ths start --no-open      # don't open a browser tab
ths --name alice start --port-offset 10  # start a second, independent environment
```

The default loopback ports are dashboard `32805`, Zakura RPC `18232`, P2P
`18233`, and lightwalletd `9067`. `--port-offset` adds a multiple of 10 to
each host port (default: `0`). Startup fails and names an occupied port
instead of selecting a random port.

Use `--zakura-bin /path/to/zakurad` to launch your own build, or
`--zakura-rpc http://127.0.0.1:<port>` to attach to a previously prepared local
node. These flags are mutually exclusive. In attach mode, `--port-offset`
shifts only dashboard and lightwalletd ports, not the node's existing RPC/P2P
ports. Remote nodes are not supported. See
[Test a local Zakura build](../README.md#test-a-local-zakura-build) for setup
and compatibility requirements.

### `ths prepare --zakura-rpc <URL>`

Prepares a persistent wallet and matching configuration for a node you will
start yourself. Accepts only `http://127.0.0.1:<port>` or
`http://localhost:<port>`. Start Zakura with the printed configuration, then
attach using the same instance name and RPC URL:

```console
ths --name local prepare --zakura-rpc http://127.0.0.1:18232
/path/to/zakurad --config /printed/path/zakurad.toml start
ths --name local start --zakura-rpc http://127.0.0.1:18232
```

`--json` emits only instance metadata on stdout; setup details go to stderr.

### `ths build [--dev] [--without-zakura]`

Builds the runtime Docker images from the current source checkout (for
development from a clone, not needed for the installed release).

```console
ths build             # release-optimized images
ths build --dev       # keep workspace Rust code unoptimized for faster rebuilds
ths build --dev --without-zakura # build only companions for a local node
```

### `ths pull [--without-zakura]`

Pulls the exact runtime images that match this launcher's version, instead of
building them locally.

```console
ths pull
ths pull --without-zakura # only the app and lightwalletd images
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

In local-binary mode, `logs zakura` reads the native node log. For a
self-managed node, use the terminal or debugger that launched it.

### `ths stop`

Stops the named environment and deletes its containers, volumes, and network.
For an attached localhost node, it removes only companion containers/network;
the node and prepared wallet/indexing data remain available for reattachment.

```console
ths stop
```

### `ths reset --force`

Same as `stop`, but named explicitly as a destructive action; requires
`--force` since it permanently deletes chain, wallet, and seed data.
For an attached localhost node, it deletes only the ths wallet/indexing data,
never the self-managed node's process, configuration, or chain. Prepare again
and restart your node with the new configuration before attaching.

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
| `ths wallet shield` | Spend one account's transparent funds into another account's Ironwood balance |
| `ths wallet unshield` | Spend one account's Ironwood funds into another account's transparent balance |

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
- `--pool <ironwood|transparent>`: which balance to fund (default `ironwood`).

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
- `--source-pool <ironwood|transparent>`: pool to spend from (default
  `ironwood`).
- `--destination-pool <ironwood|transparent>`: pool the destination account
  receives into (default `ironwood`).
- `--memo <TEXT>`: optional memo for the recipient. See [Memos](#memos).

```console
# Move 1 ZEC from account 2's Ironwood balance to account 3's Ironwood balance
ths wallet send --from 2 --to 3 --amount 1

# Spend account 1's transparent balance into account 4's Ironwood balance
ths wallet send --from 1 --to 4 --amount 0.5 --source-pool transparent

# Attach a memo for the recipient
ths wallet send --from 2 --to 3 --amount 1 --memo "rent for October"

# Rejected: transparent outputs cannot carry a memo
ths wallet send --from 2 --to 3 --amount 1 --destination-pool transparent --memo "hi"
```

### `ths wallet shield --from <N> --to <N> --amount <ZEC> [--memo <TEXT>]`

Shorthand for `ths wallet send` with `--source-pool transparent
--destination-pool ironwood`: spends one account's transparent balance into
another account's Ironwood balance.

- `--from <N>` (required): account to spend transparent funds from.
- `--to <N>` (required): a different account to receive them as Ironwood
  funds. Sends between the same account are rejected.
- `--amount <ZEC>` (required).
- `--memo <TEXT>`: optional memo on the Ironwood output (up to 512 bytes).

```console
# Shield 0.5 ZEC of account 4's transparent balance into account 2's Ironwood balance
ths wallet shield --from 4 --to 2 --amount 0.5

# The same, with a memo
ths wallet shield --from 4 --to 2 --amount 0.5 --memo "welcome to the shielded pool"
```

> Note: because transparent notes must be fully spent, the wallet may route
> any leftover change from a transparent source into the shielded pool as
> well. Shielding "0.5 ZEC" from an account holding 1 transparent ZEC can
> leave that account with its change in Ironwood rather than transparent.

### `ths wallet unshield --from <N> --to <N> --amount <ZEC>`

Shorthand for `ths wallet send` with `--source-pool ironwood
--destination-pool transparent`: spends one account's Ironwood balance into
another account's transparent balance. It takes no memo, because transparent
outputs cannot carry one.

- `--from <N>` (required): account to spend Ironwood funds from.
- `--to <N>` (required): a different account to receive them as transparent
  funds.
- `--amount <ZEC>` (required).

```console
# Unshield 0.2 ZEC from account 1's Ironwood balance into account 3's transparent balance
ths wallet unshield --from 1 --to 3 --amount 0.2
```

### Memos

Zcash lets the sender attach a memo of up to 512 bytes to an Ironwood output
(ZIP 302). The memo is encrypted to the recipient. Only holders of that
account's keys can read it.

- A memo is only allowed when the destination pool is `ironwood`. With a
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
| `ths start --port-offset <N>` | Shift loopback host ports by a multiple of 10 for another instance |
| `ths build [--dev]` | Build runtime images from source |
| `ths pull` | Pull the exact images for this launcher version |
| `ths status [--json]` | Show health and endpoint information |
| `ths open` | Open the running dashboard |
| `ths endpoints [--json]` | Print endpoints for scripts and developer tools |
| `ths mine <N>` | Mine blocks and synchronize the wallet |
| `ths faucet <ADDRESS> [--amount]` | Send disposable ZEC to any Regtest address |
| `ths wallet faucet --accounts <LIST> [--amount] [--pool]` | Fund one or more of the five accounts by index |
| `ths wallet send --from --to --amount [--source-pool] [--destination-pool] [--memo]` | Send between accounts by index, optionally with an Ironwood memo |
| `ths wallet shield --from --to --amount [--memo]` | Spend transparent funds into another account's Ironwood balance |
| `ths wallet unshield --from --to --amount` | Spend Ironwood funds into another account's transparent balance |
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
