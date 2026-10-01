# Appendix C: Troubleshooting

Start by locating the layer that failed. A launcher can be stopped while
Docker is healthy; Zakura can have a new block while the wallet is still
scanning; and an activity row can exist before its transaction confirms.
The [state ownership chapter](architecture/overview.md) explains those
layers, and [Chapter 6](architecture/operations.md) explains uncertain
payment responses.

![Four triage steps: does the launcher run at all, is this instance up and where, is the node healthy but the wallet stale, and only then a payment you are unsure of.](images/diagnose-state.svg)

1. If `ths` cannot prepare an instance, start with Docker access and
   required images.
2. If the instance is running, get **its** endpoints and node status.
   Ports are fixed per instance, but a second instance was started with a
   `--port-offset`, so confirm you are talking to the name you mean.
3. If the node is healthy but Wallet looks stale, inspect wallet
   reconciliation state and logs, not the chain height alone.
4. If a request timed out, inspect the operation's recorded activity or
   node tip before deciding whether another write is safe.

## Establish which instance you are using

In a second terminal, replace `alice` with your instance name:

```console
ths --name alice status
ths --name alice endpoints --json
ths --name alice logs app
```

Omit `--name alice` for `default`. `ths status` reports whether the app
container is running; it does **not** show the node height. The Network
page and `GET /api/v1/status` show node height and wallet sync status.
An instance keeps the same ports across runs; a different instance has
different ports only because it was started with a different
`--port-offset`.

## Match the symptom to the next observation

| What you see | What it means and what to do |
| --- | --- |
| `Docker is not reachable` | Run `docker version`. Its server section must respond without `sudo`. Start Docker Desktop or the daemon, then run `ths doctor`. |
| `ths: command not found` after installation | Two different causes. Either nothing was installed — the installer runs `ths pull` **before** moving the binary into place, so with Docker stopped it downloads images, fails, and leaves no executable; start Docker and rerun, or rerun with `TSZ_SKIP_IMAGE_PULL=1`. Or it installed fine and `~/.local/bin` is not on `PATH`; check with `ls ~/.local/bin/ths`, then fix `PATH` or reinstall with `TSZ_INSTALL_DIR` set to a directory already on it. |
| `port 32805 is already in use on 127.0.0.1` | Host ports are fixed defaults, and the launcher names the occupied one instead of choosing another. Usually another instance already holds it: run `ths list`. Start this one with `ths --name <NAME> start --port-offset 10` (any multiple of 10) to shift all four of its ports. |
| `required image ... is unavailable` | An installed launcher can run `ths pull`. From source, run `cargo run -p thus-spoke-zakura -- build --dev`. Start again after the images are present. |
| A source edit does not appear in the normal dashboard | The launcher runs an image, not live source. Rebuild with `cargo run -p thus-spoke-zakura -- build --dev` and start a fresh instance. Vite iteration has its own [source workflow](development.md#iterate-on-the-frontend). |
| `instance alice does not exist` or an endpoint is wrong | Run `ths list`, which prints every known instance with its dashboard URL. Use the same `--name` for every command and read its ports from `ths --name alice endpoints --json`. |
| Startup never reaches `alice is ready` | Read the launcher's printed error first. While the relevant container still exists, use `ths --name alice logs zakura -f`, `ths --name alice logs lightwalletd -f`, or `ths --name alice logs app -f`. Startup waits for the node, index, wallet sync, and Account 1 funding; a failed start cleans up its containers. |
| Wallet says data may be stale | Read `GET /api/v1/status`: `wallet_sync.state`, `wallet_sync.error`, observed and fully scanned heights **and hashes**. Check `ths logs lightwalletd -f` and `ths logs app -f`. Wait for `ready` before relying on balances; the server retries background reconciliation. |
| A send reports insufficient spendable funds | Ask `POST /api/v1/send/quote` for that account and source pool: it dry-runs a proposal and returns `available_zatoshi`, `fee_zatoshi`, and `max_zatoshi` without broadcasting. Compare your amount with `max_zatoshi`, not with the displayed total, which may include a different pool, pending change, or funds not yet spendable. Also confirm wallet sync is ready. |
| Faucet or send returned `broadcast` activity | Save the txid, inspect Recent activity and chain state, and mine if the transaction remains unconfirmed. Read [Chapter 6](architecture/operations.md#decide-what-to-do-after-a-lost-response) before resubmitting. |
| A mine request timed out or errored | Compare node height **and tip hash** via Network or `GET /api/v1/status` before trying again. Blocks may have been generated before wallet reconciliation failed; mining has no retry key. |

Use `ths logs app`, `ths logs zakura`, or `ths logs lightwalletd`
for recent output; add `-f` to follow. `ths doctor` checks Docker
connectivity, not wallet health.

## Reset only when you mean to lose this instance

If an interrupted run left this instance's resources, `ths --name alice
reset --force` removes its containers and volumes. That permanently
deletes its chain, wallet, seed, keys, and test funds. It does not remove
another named instance. A normal foreground run cleans itself up on
Ctrl+C.

This environment is Regtest only. Its seed is fixed and public, so these
addresses and keys are known to anyone who has read the project. Never use
them for real ZEC, and never expose these local services beyond
`127.0.0.1`.
