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
    default one (default: `default`, or a fresh `run-<UUID>` for `run`). Names
    use 1–40 lowercase ASCII letters, digits, or internal hyphens.
  - `--json` — print machine-readable JSON instead of human-readable text,
    where the command supports it.
- Use `-h` / `--help` or `ths help <command>` for help; `-V` /
  `--version` prints the launcher version. `--json` does not convert logs,
  browser opening, builds, or cleanup messages into JSON and is rejected for `run`.
- Accounts are referred to by **index**, `1` through `5`. Every environment
  starts with five disposable development accounts; there is no need to copy
  full Regtest addresses to use the commands on this page. (A hidden sixth
  account acts as the mining/faucet treasury and is not addressable from the
  CLI.)
- Amounts are ZEC decimal strings with up to 8 decimal places, e.g. `1`,
  `0.5`, `2.25000001`. These are disposable Regtest coins with no value.
- Commands that call the app (`mine`, `reorg`, `rollback`, `faucet`, and `wallet` subcommands) require
  a running environment. Start one first with `ths` (or `ths --name <NAME>`).
  `stop` and `reset --force` can remove resources left by an interrupted run.

---

## Environment lifecycle

### `ths` / `ths start`

Starts a fresh environment in the foreground and opens the dashboard in your
browser. Ctrl+C starts bounded, instance-scoped deletion of its chain, wallet,
keys, containers, and volumes. Incomplete or uncertain cleanup exits with an
error and retains `lifecycle-recovery.json`; see [Chapter 1](getting-started.md).

```console
ths
ths start --no-open      # don't open a browser tab
ths --name alice start --port-offset 10  # start a second, independent environment
```

The default loopback ports are dashboard `32805`, Zakura RPC `18232`, P2P
`18233`, and lightwalletd `9067`. `--port-offset` is the amount added to
each host port (default: `0`), and must be a multiple of 10. It is not multiplied
by 10; the result must fit a valid nonzero host port. Startup fails and names an occupied port
instead of selecting a random port.

### `ths wait [--timeout <SECONDS>]`

Waits for an instance's node, lightwalletd index, and wallet to be settled at
the same canonical tip, and for the `ths start` or `ths run` that owns it to
report ready. It also waits for an instance that is still being created in
another shell, and fails after a few seconds if no command is creating it or
if its launcher has exited and left only its containers behind. It creates
no instance and makes no chain changes. The timeout defaults to **120 seconds**, and status reads are bounded
by the remaining budget. Timeout or interruption exits nonzero and leaves the
instance running. On timeout, the error reports the last observed heights and
settlement blocker. `--json` emits the settlement object on success:
`node_height`, `lightwalletd_height`, `wallet_scanned_height`,
`wallet_sync_state`, `settled`, and nullable `settlement_blocker`.

```console
ths --name alice wait --timeout 180 --json
```

### `ths run [--timeout <SECONDS>] -- <COMMAND> [ARGUMENTS...]`

Starts a fresh instance, waits for readiness and full settlement, runs a child
command in your current directory, then attempts bounded instance cleanup.
The **120-second** default timeout covers startup and settlement; it does not
limit child execution. No browser opens. Without `--name`, the name is
`run-<32 UUID hex digits>`; an explicit name must be fresh. Existing metadata
or resources cause a collision error rather than reuse. `run` uses the default
ports and has no `--port-offset`; concurrent runs can therefore collide on ports.

```console
ths run --timeout 180 -- sh -c 'curl -fsS "$THS_API_URL/api/v1/accounts"'
```

The child inherits standard streams and the calling directory, but `run`
supports non-interactive commands: it creates a separate process group without
transferring control of the terminal. Commands that read from the controlling
terminal are unsupported. No persistent environment is provided. Seeds and
spending keys are not exported. `--json` is rejected
because the child owns stdout. The child receives:

| Variable | Value |
| --- | --- |
| `THS_INSTANCE` | Selected/generated name |
| `THS_NETWORK` | `regtest` |
| `THS_API_URL` | Dashboard/API base URL |
| `THS_ZAKURA_RPC_URL` | Zakura JSON-RPC URL |
| `THS_LIGHTWALLETD_URL` | Plaintext compact-block endpoint |
| `THS_LIGHTWALLETD_TLS` | `false` |
| `THS_P2P_ADDR` | Loopback peer `host:port` |

A normal child exit preserves its exit code. A signaled child exits 1. On Unix,
launcher interruption forwards the signal to the child process group, with
bounded grace before forced termination. Cleanup failure after a successful
child makes `run` fail; after a nonzero child exit its code is preserved and
cleanup errors are reported separately. Recovery metadata remains when deletion
cannot be verified. Settlement is an observation before the child starts,
not a lock against later chain changes. `ths --name <name> stop` or
`reset --force` from another shell stops the child as Ctrl+C would, cleans
up, and exits with status 1.

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

Shows whether the named environment's app container is running and prints its
endpoints; it is not `/health` or a wallet-settlement check. JSON contains
`name`, `running`, and `endpoints`.

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

### `ths logs [app|zakura|lightwalletd] [-f|--follow] [--tail N | --head N]`

Prints or streams logs for a service in the named environment. Defaults to the
`app` (dashboard/server) service. Without `--tail` or `--head`, it prints
all available Docker logs. Counts must be positive. `--head` conflicts with
`--tail` and `--follow`; `--tail` can be combined with `--follow`. Output remains
log text even with the global `--json` flag.

```console
ths logs                       # available logs from the app service
ths logs zakura -f             # follow node logs
ths logs lightwalletd --follow # follow lightwalletd logs
ths logs --tail 200 -f         # last 200 app lines, then follow
ths logs zakura --head 200     # first 200 node lines
```

### `ths stop`

Stops the named environment and deletes its containers, volumes, and network.
If a `ths start` or `ths run` for it is still running, even while starting,
`stop` asks that launcher to shut down as Ctrl+C would, waits for its cleanup,
and succeeds only once the deletion is verified.

```console
ths stop
```

`stop` uses verified, instance-scoped cleanup. A foreground `start` observes
external deletion and exits; you do not need to interrupt it separately.
Cleanup errors retain `lifecycle-recovery.json` for this instance. A successful
message requires verified deletion; neither stop nor reset removes another
instance's resources.

### `ths reset --force`

Same as `stop`, but named explicitly as a destructive action; requires
`--force` since it permanently deletes chain, wallet, and seed data.

```console
ths reset --force
```

### `ths list [--json]`

Lists known environments in sections by state, with each one's dashboard URL,
state (running, paused, degraded or stopped), and containers' status and
creation and start times. `--json` gives the same data with unix-second
timestamps. The JSON array contains `name`, metadata `version`, `endpoints`,
nullable `created_at`, `state`, and `containers`. Each container has `service`,
`state`, and nullable `created_at` / `started_at`; the one-shot `init` service
may already be exited. An environment whose container names are held by containers it
doesn't own is listed as a conflict, and if Docker can't be reached,
environments are still listed with their state as unknown.

```console
ths list
```

### `ths doctor [--json]`

Checks that Docker is reachable and prints the launcher's configuration
directory. If Docker is unreachable, the command exits nonzero in both output
modes. With `--json`, stdout still contains one diagnostic JSON object with
`ok: false` and `docker: null`; the error is written to stderr.

```console
ths doctor
```

### `ths update [VERSION] [--check]` / `ths uninstall`

Checks for, installs, or rolls back a released launcher version, or removes
the installed `ths` executable.

```console
ths update --check     # only report whether a newer release exists
ths update              # install the latest verified release
ths update v0.3.0       # install (or roll back to) an exact version
ths uninstall
```

`ths --version` prints the installed version. The recorded source baseline uses package version `0.3.0`;
the installer downloads a published release, not a build of recorded main.
An explicit VERSION conflicts with `--check`.

`uninstall` removes only the installed executable. Images and launcher
configuration remain. Capture the real directory **before** uninstalling.
The image tags below match this book's source baseline; inspect your selected
launcher's tags before removing shared images used by other instances.

```console
THS_CONFIG_DIR="$(ths doctor --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["config_dir"])')"
ths list
ths stop                 # repeat with --name for each instance you intend to delete
ths uninstall
docker image rm ghcr.io/zcashlabs/thus-spoke-zakura-app:0.3.0 \
                ghcr.io/zcashlabs/thus-spoke-zakura-lightwalletd:0.3.0 \
                zakuracore/zakura:1.6.0
```

Inspect `$THS_CONFIG_DIR` separately before deleting configuration. Retained
recovery records should be resolved before removing that directory. Executable,
shared image, and configuration removal are separate decisions.

### `ths mine <BLOCKS>`

Mines blocks on the running environment and synchronizes its wallet — useful
for testing confirmations, expiry, or coinbase maturity.

```console
ths mine 1
ths mine 10 --json
```

Counts must be **1–10,000**. Mining POSTs return 503 until startup finishes.
The command submits to the mining-job API (`POST /api/v1/mining/jobs` with
`{blocks, idempotency_key}`; acceptance returns HTTP 202) and polls
`GET /api/v1/mining/jobs/{id}` until the job completes or fails, printing
progress as it goes in human-readable mode (including the mined tip hash on
completion and the job ID on failure) and the final job as JSON with
`--json`. Each poll is capped at 2 seconds, so a single stalled or dropped
connection cannot delay Ctrl+C or the overall 300-second deadline; a poll
that fails to get a response is retried within that deadline rather than
ending the command, since the job may still be running on the server. The
idempotency key for a given block count is saved to the instance directory
before the request is sent and only cleared once the job reaches a terminal
state, so a submission whose response is lost or fails to decode, or a
command interrupted or timed out mid-poll, does not lose track of the job:
the next `ths mine` with the same block count reuses the saved key and picks
the same job back up instead of starting a second one. Once a job completes
or fails, its key is forgotten, so repeating that command afterward starts a
fresh job. A different manual request during mining or wallet synchronization
is rejected with HTTP 409.

`--json` output keeps the legacy endpoint's `blocks` (the confirmed count)
and `hashes` fields, alongside the job's `id`, `requested_blocks`, `state`,
`error`, and `progress_uncertain`. A failed job after partial progress still
reports what it mined rather than only the error.

Dashboard jobs use `POST /api/v1/mining/jobs` with `{blocks, idempotency_key}`;
acceptance returns HTTP 202. Replaying an accepted key with the same count returns
the same job, including after completion or failure. Reusing that key with a
different count returns HTTP 409. The latest-job GET returns `{job}` or
`{job:null}`; `GET /api/v1/mining/jobs/{id}` returns the job directly or 404.
The dashboard restores latest-job progress after refresh, navigation, modal
dismissal, or closing and reopening a tab. Its accepted response clears the
session admission key; a cached completed or failed job also clears a prior
key before another attempt. See [Chapter 5](mining-and-sync.md#follow-mining-progress-in-the-dashboard)
for the UI retry boundary.

Progress counts only acknowledged hashes from that job, excluding interleaved
payment or faucet mining. A failed mining RPC may have committed extra blocks;
its acknowledged count is a lower bound and the server never automatically retries
the uncertain RPC. A wallet synchronization failure after mining retains the
full mined count but leaves the job `failed`, with the synchronization error;
it does not mark the job `completed`.

Job history and keys last only for the server process lifetime, with a limit of
1,024 jobs and no key eviction. At capacity, new admissions return HTTP 503;
existing-key replays and reads continue. Stopping the server cancels running work
and discards this history. Jobs and deduplication do not survive a restart.

### `ths reorg --depth <N> [--keep-txs]` / `ths rollback --depth <N>`

`reorg` removes the last N blocks and mines N replacements, keeping the tip
height but changing those block hashes. `rollback` removes them without
replacements. Both reconcile the wallet and support `--name` and `--json`.
Depth must be 1–1,000, within Zakura's non-finalized window, and leave at least
block 1 for wallet reconciliation. Active mining, payment execution, or another
chain operation causes HTTP 409.

```console
ths reorg --depth 3
ths reorg --depth 3 --keep-txs --json
ths rollback --depth 3
ths wait
```

Without `--keep-txs`, ths does not rebroadcast dropped transactions; replacement
mining still uses the node's mempool. Zakura can independently requeue a prior
RPC submission, so neither rollback nor reorg guarantees an empty mempool or
prevents later inclusion of a dropped transaction. With `--keep-txs`, ths
attempts to reinclude the original non-coinbase transactions and verifies their
replacement-block inclusion.
Invalid anchors or other admission failures may prevent reinclusion. Failures
are reported in both output modes; `--json` also lists `retained_txids` and
`failed_transactions`. The CLI exits nonzero if any transaction was not retained,
even though the chain changed.

Affected payment records become `unconfirmed`, keeping their txids and keys.
The server observes them but does not automatically rebroadcast, replace, or
mine them. Later canonical inclusion can restore `confirmed`. This is not a
wallet snapshot restore: unexpired local transactions can still reserve inputs.
The server owns admitted work after client disconnection. An error or lost
response can follow a partial chain change; inspect the current tip before
retrying. Neither invalidation nor replacement mining is automatically retried.
Transient lightwalletd read failures during reorg repair are retried within the
wallet synchronization deadline, without repeating the chain mutation.

---

## Funding an address: `ths faucet <ADDRESS> [--amount <ZEC>]`

Sends disposable Regtest ZEC from the treasury directly to any Regtest unified
or transparent address, not necessarily one of the five development accounts.
Limited to 5 ZEC per request; defaults to 1 ZEC. It takes no memo. For the
development accounts, use
[`ths wallet`](#ths-wallet-the-development-wallet).

```console
RECIPIENT="$(ths wallet balances --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["unified_address"])')"
ths faucet "$RECIPIENT" --amount 2.5
```

The address API requires an idempotency key, which the CLI saves in its
instance-local faucet journal. Pending results or errors retain the key; rerun
the same command with the same address, amount, and instance name to resume.
Confirmation with a block hash or a stored terminal failure clears it, so the
next identical command can fund again. Each invocation submits once then polls
read-only keyed status within a 30-second payment budget. Pending/unknown results
exit nonzero; JSON includes the retained `idempotency_key`. A recognized
user-account address produces account activity; an external
address has a separate persisted faucet record and is absent from the activity
list. Preserve the returned txid and status for observation.

## `ths wallet`: the development wallet

Everything under `ths wallet` acts on the wallet held by the running `ths`
server: the five development accounts (index `1`-`5`) and the hidden treasury
that funds them. Its subcommands do the same things as the dashboard's Faucet
and Send dialogs, from a script or terminal:

| Subcommand | What it does |
| --- | --- |
| `ths wallet balances` | Print each account's balances and addresses |
| `ths wallet activity` | Print recent sends, shields, unshields, and faucet payments |
| `ths wallet faucet` | Fund one or more accounts from the treasury |
| `ths wallet send` | Send funds between accounts or pools, with an optional memo, or pay a `zcash:` URI |
| `ths wallet shield` | Spend transparent funds into the same or another account's Ironwood balance |
| `ths wallet unshield` | Spend Ironwood funds into the same or another account's transparent balance |

The mutating subcommands attempt to mine a confirming block and
print the resulting transaction ID. If auto-mining fails, activity may remain
in `broadcast` state; `send`, `shield`, and `unshield` print a confirming block
hash when one is available. Pass `--json` for machine-readable output.

### `ths wallet balances`

Prints accounts 1–5 with their transparent and Ironwood balances in ZEC and the
addresses that receive them. With `--json`, balances are integer zatoshi and
the array contains `id`, both addresses, `transparent_zatoshi`, and
`ironwood_zatoshi`. The command first requires `wallet_sync.state: ready`;
syncing or failed scans exit nonzero instead of printing stale balances.
This is not a full settlement test: use `ths wait` if node/index/wallet
alignment is required.

```console
ths wallet balances
ths wallet balances --json
```

### `ths wallet activity [--limit <N>]`

Prints recent activity recorded by the development wallet, newest first. The
output distinguishes sends, shields, unshields, and faucet payments, and shows
their status, route, amount, transaction ID and confirmation block (when
available), creation time, and any retained payment error. External Regtest
recipients are shown by their full address. Faucet routes are labeled `faucet`;
the hidden treasury account is never exposed as a user account.

Without `--limit`, the server's default of 30 entries is used. An explicit
limit must be between 1 and 100. An environment with no activity prints a
short empty-state message.

```console
ths wallet activity
ths wallet activity --limit 20
ths wallet activity --json
ths wallet activity --limit 20 --json
```

JSON output is the unmodified activity schema returned by
`GET /api/v1/activity`, including `created_at` and nullable `block_hash`.

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

The command attempts each selected account and reports confirmed, pending,
and failed results. Pending, unknown, or rejected work exits nonzero and keeps
the batch's journaled keys, including keys for accounts already terminal. It
submits each key once, then polls local keyed status within a shared 30-second
budget. Retry the same
amount, pool, account selection, and instance name to resume that batch; an
overlapping changed selection for the same amount and pool is rejected while
it remains unfinished. Changing the amount or pool selects a different intent.
Only a fully terminal batch (`confirmed` or stored `failed` for every member)
clears all its keys. Terminal failures still exit nonzero. After that,
rerunning the same command requests a new set of payments. JSON returns
`{funded:[confirmed operations],pending:[operations with retained keys],
failed:[diagnostic strings]}`; unknown pending members include `error`.

### `ths wallet send --from <N> --to <N> (--amount <ZEC> | --max) [--source-pool <POOL>] [--destination-pool <POOL>] [--memo <TEXT>]`

Sends funds between development accounts or pools. It does the same thing as
the dashboard's **Send ZEC** dialog and uses the same endpoint
(`POST /api/v1/send`).

- `--from <N>` / `--to <N>` (required): two account indices,
  1–5. They may match when the source and destination pools differ. The treasury
  account cannot be used.
- `--amount <ZEC>` / `--max` (exactly one required): send an explicit amount,
  or ask `POST /api/v1/send/quote` for the maximum spendable amount after the
  network fee. The CLI submits the server's quoted `max_zatoshi`; it does not
  calculate the fee itself.
- `--source-pool <ironwood|transparent>`: pool to spend from (default
  `ironwood`).
- `--destination-pool <ironwood|transparent>`: pool the destination account
  receives into (default `ironwood`).
- `--memo <TEXT>`: optional memo for the recipient. See [Memos](#memos).

```console
# Move 1 ZEC from account 2's Ironwood balance to account 3's Ironwood balance
ths wallet send --from 2 --to 3 --amount 1

# Empty account 2's spendable Ironwood balance into account 3
ths wallet send --from 2 --to 3 --max

# Spend account 1's transparent balance into account 4's Ironwood balance
ths wallet send --from 1 --to 4 --amount 0.5 --source-pool transparent

# Move funds between pools within account 1
ths wallet send --from 1 --to 1 --amount 0.01 --destination-pool transparent
ths wallet send --from 1 --to 1 --amount 0.005 --source-pool transparent

# Attach a memo for the recipient
ths wallet send --from 2 --to 3 --amount 1 --memo "rent for October"

# Rejected: transparent outputs cannot carry a memo
ths wallet send --from 2 --to 3 --amount 1 --destination-pool transparent --memo "hi"
```

Same-account transfers require sufficient spendable funds for the amount and
fee; using the same account and pool is rejected. CLI `send`, `shield`, and
`unshield` journal a key for the resolved request body, including memo. Each
invocation submits once, then polls read-only keyed activity within a 30-second
budget. Pending, unknown, or rejected results retain the key; rerun unchanged
arguments and the same instance to resume. Observing `confirmed` or terminal
`failed` clears it, so an identical command afterward can create another payment.
Pending/unknown output includes the key (also in JSON) and exits nonzero;
see the [caller table](architecture/operations.md#which-caller-are-you-retrying).

### `ths wallet send --from <N> --uri <URI> [--amount <ZEC> | --max] [--source-pool <POOL>]`

Pays a single-payment ZIP-321 `zcash:` URI, like the **Payment request** field
in the dashboard's **Send ZEC** dialog. The URI sets the destination (any
Regtest unified or transparent address), its pool, and any memo, so `--to`,
`--destination-pool`, and `--memo` can't be combined with `--uri`. The server
parses the URI (`POST /api/v1/zip321/parse`), so the CLI and the dashboard
accept exactly the same URIs: one payment to a supported Regtest address,
with a text memo only for Ironwood. Multipayment, wrong-network, unsupported
receiver, and nontext-memo requests are rejected.

- `--amount <ZEC>` or `--max`: required when the URI has no amount, and rejected
  when it sets one. `--max` quotes against the destination pool parsed from the
  URI.

```console
DASHBOARD_URL="$(ths endpoints --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["dashboard"])')"
URI="$(curl -fsS "$DASHBOARD_URL/api/v1/zip321/request" -H 'content-type: application/json' --data '{"account_id":2,"amount_zatoshi":1000000,"memo":"coffee"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["uri"])')"
ths wallet send --from 1 --uri "$URI"
# Pay a request for 0.01 ZEC with the memo "coffee"
ths wallet send --from 1 --uri "zcash:uregtest1...?amount=0.01&memo=Y29mZmVl"

# A URI without an amount needs --amount
ths wallet send --from 1 --uri "zcash:tm..." --amount 0.5

# Or send the maximum available after fees
ths wallet send --from 1 --uri "zcash:tm..." --max
```

### `ths wallet shield --from <N> --to <N> (--amount <ZEC> | --max) [--memo <TEXT>]`

Shorthand for `ths wallet send` with `--source-pool transparent
--destination-pool ironwood`: spends one account's transparent balance into
the same or another account's Ironwood balance.

- `--from <N>` (required): account to spend transparent funds from.
- `--to <N>` (required): account to receive them as Ironwood funds; may match
  `--from`.
- `--amount <ZEC>` / `--max` (exactly one required).
- `--memo <TEXT>`: optional memo on the Ironwood output (up to 512 bytes).

```console
# Shield 0.5 ZEC of account 4's transparent balance into account 2's Ironwood balance
ths wallet shield --from 4 --to 2 --amount 0.5

# Shield within account 1
ths wallet shield --from 1 --to 1 --amount 0.005

# Shield the maximum spendable transparent balance after fees
ths wallet shield --from 1 --to 1 --max

# The same, with a memo
ths wallet shield --from 4 --to 2 --amount 0.5 --memo "welcome to the shielded pool"
```

These transfers require sufficient spendable funds for the amount and fee.

> Note: wallet input selection may route leftover transparent change into
> the shielded pool as well. Shielding "0.5 ZEC" from an account holding
> 1 transparent ZEC can leave that account with its change in Ironwood
> rather than transparent.

### `ths wallet unshield --from <N> --to <N> (--amount <ZEC> | --max)`

Shorthand for `ths wallet send` with `--source-pool ironwood
--destination-pool transparent`: spends one account's Ironwood balance into
the same or another account's transparent balance. It takes no memo, because
transparent outputs cannot carry one.

- `--from <N>` (required): account to spend Ironwood funds from.
- `--to <N>` (required): account to receive them as transparent funds; may match
  `--from`.
- `--amount <ZEC>` / `--max` (exactly one required).

```console
# Unshield 0.2 ZEC from account 1's Ironwood balance into account 3's transparent balance
ths wallet unshield --from 1 --to 3 --amount 0.2

# Unshield within account 1
ths wallet unshield --from 1 --to 1 --amount 0.01

# Unshield the maximum spendable Ironwood balance after fees
ths wallet unshield --from 1 --to 1 --max
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

The server stores the supplied memo separately in its idempotency record.
An omitted memo and a `null` memo are equivalent; explicitly empty text is a
different payment intent. Changing the memo under an existing direct API key
returns HTTP 409. Activity has no memo field, and
dashboard transaction views do not decrypt received memos. ZIP-321 parsing can
return a supplied URI memo without reading one from a transaction.

`GET /api/v1/accounts` returns a `unified_full_viewing_key` for each user
account. Import it into an external wallet that can decrypt memos to read
those sent to that account's Ironwood outputs.

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
| `ths wait [--timeout]` | Wait for existing node/index/wallet settlement |
| `ths run [--timeout] -- <command>` | Run a child in a fresh settled instance, then clean up |
| `ths build [--dev]` | Build runtime images from source |
| `ths pull` | Pull the exact images for this launcher version |
| `ths status [--json]` | Show app-container running state and endpoints |
| `ths open` | Open the running dashboard |
| `ths endpoints [--json]` | Print endpoints for scripts and developer tools |
| `ths mine <N>` | Mine blocks and synchronize the wallet |
| `ths reorg --depth N [--keep-txs]` | Replace recent blocks, optionally attempting transaction reinclusion |
| `ths rollback --depth N` | Remove recent blocks without replacements |
| `ths faucet <ADDRESS> [--amount]` | Send disposable ZEC to any Regtest address |
| `ths wallet balances [--json]` | Print the five accounts' balances and addresses |
| `ths wallet activity [--limit N] [--json]` | Print recent development-wallet activity |
| `ths wallet faucet --accounts <LIST> [--amount] [--pool]` | Fund one or more of the five accounts by index |
| `ths wallet send --from --to (--amount \| --max) [--source-pool] [--destination-pool] [--memo]` | Send between accounts or pools, optionally with an Ironwood memo |
| `ths wallet send --from --uri [--amount \| --max] [--source-pool]` | Pay a ZIP-321 `zcash:` URI to any Regtest address |
| `ths wallet shield --from --to (--amount \| --max) [--memo]` | Spend transparent funds into the same or another account's Ironwood balance |
| `ths wallet unshield --from --to (--amount \| --max)` | Spend Ironwood funds into the same or another account's transparent balance |
| `ths logs [service] [-f] [--tail N \| --head N]` | Stream or print service logs |
| `ths list` | List environments and their container status |
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
