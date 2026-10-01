# 1. Your first instance

In this chapter you will start one private chain, identify its endpoints, and
watch a second one be thrown away. The next chapter will explain what the
three services behind those endpoints do.

An **instance** is one named set of Docker containers, a private Docker
network, volumes, and host ports. The default name is `default`. Starting it
again creates a **fresh** chain and wallet; stopping it removes its data.
An **image** is the built template from which Docker starts a container;
changing source files does not alter an image that already exists.

![Five startup stages left to right: ths start, checking Docker and images, claiming the fixed loopback ports, starting Zakura then lightwalletd then the app, and ready. A note records that init derives the fixed seed and that a fresh chain opens near height 103. Below, Ctrl+C or any failure deletes only this instance.](images/instance-lifecycle.svg)

1. `ths` checks Docker and the required images.
2. It removes leftovers for the selected instance name, then creates its
   network, volumes, and containers. Other names are untouched.
3. An init container creates the seed, account records, wallet database, and
   Zakura Regtest configuration. Zakura starts, then lightwalletd, then the
   app.
4. The app synchronizes its wallet and provisions Account 1. The launcher
   prints endpoints only when the app is ready.
5. Ctrl+C, a normal shutdown, or a partial startup failure removes that
   instance's resources and endpoint record.

## Prepare Docker

Install Docker Desktop on macOS or Docker Engine/Desktop on Linux. Start it
and confirm that both the client and daemon respond:

```console
docker version
```

The server section must respond without `sudo`. If it does not, fix Docker
access before continuing. `ths doctor` performs the same basic Docker
reachability check after installation.

## Install the launcher

The installer supports Intel and ARM macOS and Linux. It verifies the
release archive's SHA-256 digest, installs `ths` in `~/.local/bin` by default,
and pulls matching runtime images:

```console
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/zcashlabs/thus-spoke-zakura/main/install.sh | sh
ths --version
ths doctor
```

`ths --version` prints the launcher version. This book documents **0.2.1**.

The installer reads three environment variables: `TSZ_INSTALL_DIR` chooses the
destination directory (default `~/.local/bin`), `TSZ_VERSION` installs an exact
release instead of the latest, and `TSZ_SKIP_IMAGE_PULL=1` skips the image
download. The image pull happens **before** the binary is moved into place, so
if Docker is not running the installer aborts with nothing installed. Start
Docker first, or install with `TSZ_SKIP_IMAGE_PULL=1` and run `ths pull` later.

If your shell says `ths: command not found`, either the install aborted or
`~/.local/bin` is not on `PATH`. See [troubleshooting](troubleshooting.md) for
the exact checks.

If you are working from a clone, use the [source build
instructions](development.md) instead. The launcher uses Docker images, so a
source edit does not automatically change a running app.

## Start the running example

Open a terminal and run it there. Keep that terminal open; `--no-open` only
skips opening the browser, and the dashboard still runs. The launcher ends
with:

```console
$ ths start --no-open
default is ready 🌸
  Dashboard    http://127.0.0.1:32805
  Zakura RPC   http://127.0.0.1:18232
  lightwalletd http://127.0.0.1:9067  (network=regtest, tls=false)
  P2P          127.0.0.1:18233
```

On a warm machine that takes well under a minute. The **first** run is much
slower, because Docker must pull about 1.7 GB of images. There is no fixed
`sleep` interval that means the wallet is ready; wait for `default is ready`.

## What scrolls past before that line

Everything above `default is ready` is the launcher narrating its work, and
it is noisier than the result. You will see `Preparing a fresh default
environment…`, `Starting default…`, then raw Docker network, volume, and
container IDs, because `ths` lets Docker write to your terminal directly.

Interleaved with those IDs, the init container prints the instance's
**secrets**: a `⚠ DISPOSABLE REGTEST SECRETS` banner, the BIP-39 mnemonic,
and for each of the five development accounts its unified address and its
**unified spending key** in hex. That is deliberate. These are the supported
way to import one of these accounts into an external wallet, and they are
safe to print because they protect nothing — [Chapter
3](accounts-and-balances.md#an-account-is-a-wallet-identity) explains why.

## A fresh chain already has 103 blocks

Before Account 1 can hold anything, the treasury must have a *spendable*
mining reward, and a coinbase reward is not spendable until it has matured
by `COINBASE_MATURITY_BLOCKS` (100) confirmations. So startup mines through
maturity, shields a reward into the treasury, and pays Account 1 its 5
Orchard ZEC. A newly ready instance therefore sits at height **103**, with
the wallet fully scanned to the same height. Nothing is wrong; the Network
page just does not start at 0.

## Read the endpoints from a second terminal

In a **second terminal**, inspect the same instance:

```console
$ ths status
default: running
  Dashboard    http://127.0.0.1:32805
  Zakura RPC   http://127.0.0.1:18232
  lightwalletd http://127.0.0.1:9067  (network=regtest, tls=false)
  P2P          127.0.0.1:18233
```

`ths status` reports whether the app container is running and prints
endpoints. It does **not** report the node height or wallet scan status.
`ths endpoints --json` reports these keys:

| Key | Owner and purpose |
| --- | --- |
| `dashboard` | HTTP URL for the React UI and `/api/v1/` server API. |
| `rpc` | Zakura's local JSON-RPC URL. |
| `lightwalletd` | Plaintext lightwalletd gRPC endpoint for local clients. |
| `p2p` | Zakura's local peer address. |
| `network` | The chain this instance runs: always `"regtest"`. |
| `tls` | Always `false`; nothing here uses TLS. |

Host ports are **fixed defaults**, all published on `127.0.0.1`: dashboard
`32805`, Zakura RPC `18232`, P2P `18233`, lightwalletd `9067`. They do not
change between runs, and the launcher never picks a substitute — if one is
occupied, startup fails and names it. `ths start --port-offset <N>` shifts
all four by `N`, which must be a multiple of 10; that is the only thing that
moves them. Scripts should still read `ths endpoints --json` rather than
hard-code a port, because a given instance may have been started with an
offset. `ths open` opens the dashboard for you.

The dashboard's Network page or `GET /api/v1/status` shows node height and
wallet sync state if you need more than process status.

## Give another experiment its own name

A second instance needs its own ports. Since every instance wants the same
four defaults, the name alone is not enough:

```console
$ ths --name alice start --no-open
Preparing a fresh alice environment…
Starting alice…
Error: port 32805 is already in use on 127.0.0.1
```

Give it an offset instead:

```console
$ ths --name alice start --no-open --port-offset 10
alice is ready 🌸
  Dashboard    http://127.0.0.1:32815
  Zakura RPC   http://127.0.0.1:18242
  lightwalletd http://127.0.0.1:9077  (network=regtest, tls=false)
  P2P          127.0.0.1:18243
```

Address that instance with the same global option:

```console
ths --name alice status
ths --name alice endpoints --json
```

Names contain 1–40 lowercase letters, digits, or internal hyphens. Each name
gets its own chain, wallet, volumes, and cleanup. Commands without `--name`
target `default`. The offset belongs to the `start` that created the
instance; later commands find its ports from the instance record.

## Stop deliberately

Stopping is not a crash to recover from — it is how the environment is meant
to end. Try it on `alice`, which you do not need again. Return to the
terminal running it and press **Ctrl+C**. The launcher deletes that
instance's containers, volumes, chain, seed, keys, and test funds. `default`
is untouched. If you need a result out of an instance, copy it out before
stopping.

If an interrupted process left resources behind, `ths --name alice reset
--force` deletes only `alice` and its volumes. This is irreversible. Never
use Regtest addresses or keys for real ZEC, and never publish these local
services beyond loopback.

**Leave `default` running.** [Chapter 4](using-the-dashboard.md) funds and
spends on it. If you do stop it, start it again with `ths start --no-open`;
you will get a fresh chain back at height 103 with the same five accounts,
but every block, transaction, and balance from this run will be gone.

**What you learned:** `ths` controls an instance, not a persistent wallet.
Data belongs to a particular run, and the ports belong to the instance's
offset. **Next:** [meet the services inside the
instance](architecture/overview.md).
