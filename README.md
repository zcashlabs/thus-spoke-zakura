# Thus Spoke Zakura

A local-first Zcash Regtest environment powered by Zakura.

```console
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/zcashlabs/thus-spoke-zakura/main/install.sh | sh
thus-spoke-zakura
```

The launcher creates an isolated Zakura node, lightwalletd, five development
accounts, a hidden mining treasury, faucet and mining controls, and a browser-based wallet/explorer. Host
ports are chosen automatically and bind only to `127.0.0.1`.

The installer supports Linux and macOS on x86-64 and ARM64. It verifies the
release checksum, validates the downloaded launcher, pulls its exact matching
runtime images, and only then atomically replaces an existing installation.
Install a particular release with `TSZ_VERSION=v0.1.0`; use
`TSZ_SKIP_IMAGE_PULL=1` only when the images have already been provisioned.

Release `0.2.0` and later can update themselves through the same verified,
atomic installation path:

```console
thus-spoke-zakura update --check
thus-spoke-zakura update
```

The first command exits `0` when current, `10` when an update is available,
and `1` on an actual error. It also supports `--json`. Pin an exact version—or
intentionally roll back—with `thus-spoke-zakura update 0.1.0`. The existing
binary is never replaced unless the release checksum, version validation, and
matching image pull all succeed.

## Requirements

Running a published build requires Docker. `docker version` must show both the
client and server sections without `sudo`.

On Linux installations whose socket is owned by `root:docker`, add your user to
the group:

```console
sudo usermod -aG docker "$USER"
```

Then completely log out of the desktop session and log back in. Opening a new
terminal alone does not refresh supplementary groups. Verify the result with:

```console
id
docker version
```

`id` must include `docker`, and `docker version` must reach the server. As an
immediate alternative for the current shell, `newgrp docker` starts a subshell
with the new group. Membership in `docker` is effectively root-level access to
the machine, so only grant it to trusted users.

## Development

Building from source additionally requires Rust 1.98 and Node 24.

```console
cargo test --workspace
cd web && npm ci && npm run build
```

Install the launcher, then build the runtime images separately:

```console
cargo install --path crates/tsz-cli
thus-spoke-zakura doctor
thus-spoke-zakura build
thus-spoke-zakura
```

The dashboard opens automatically. Use `--no-open` in headless or scripted
environments. The current local runtime uses `zakuracore/zakura:1.2.0` and the
project app and lightwalletd images whose tags exactly match the launcher's
Cargo version. Docker builds retain Cargo
registry, Git, and target caches through BuildKit cache mounts.

## CLI

Every command accepts `--name <instance>`; the default name is `default`.

```text
pull                Pull exact production images for this launcher version
update [VERSION]    Check for or install a released launcher version
build [--dev]       Build those images from the current source checkout
start [--no-open]   Run an existing image in the foreground
status              Show health and endpoints
open                Open the dashboard
endpoints [--json]  Print integration endpoints
logs [service] -f   Follow app, zakura, or lightwalletd logs
stop                Stop and delete the selected environment
reset --force       Delete exactly one instance and its volumes
list                List instances
doctor              Verify Docker connectivity
```

Build optimized runtime images with:

```console
cargo run -p thus-spoke-zakura -- build
```

For a faster edit/build/run loop, compile the workspace code without release
optimizations or LTO while keeping third-party dependencies optimized for
usable cryptographic proving performance:

```console
cargo run -p thus-spoke-zakura -- build --dev
```

Then start the already-built images without a subcommand:

```console
cargo run -p thus-spoke-zakura
```

A published installation normally runs `thus-spoke-zakura pull` during
installation. `start` never pulls or builds implicitly, so a run is
reproducible and will fail with a precise command if an exact image is absent.
The mutable `latest` image aliases are provided for human discovery only and
are never consumed by the launcher.

Only official release binaries can replace themselves. A source build may run
`cargo run -p thus-spoke-zakura -- update --check`, but an attempted mutation
explains how to update through Git/Cargo instead. Because `0.1.0` predates the
command, existing `0.1.0` users must rerun the public installer once to move to
`0.2.0`; subsequent releases can use `thus-spoke-zakura update`.

With no subcommand, `thus-spoke-zakura` defaults to `start`. Every start creates
a fresh development environment, deleting any previous state for the selected
instance first. It keeps control of the terminal after the environment becomes
ready. Press Ctrl+C (or send the platform's termination signal) to remove its
containers, volumes, network, chain, wallet, seed, index, configuration, and
local metadata.

`reset --force` permanently removes the selected instance's chain, wallet,
seed, and index volumes. No command binds services beyond loopback.

## Architecture

```text
Browser ──HTTP/SSE── tsz-server ──JSON-RPC── Zakura (Regtest)
                         │                       │
                         └──────gRPC──── lightwalletd
```

The Rust service uses Zakura's wallet, key, primitive, proof, and SQLite crates.
It scans compact blocks through lightwalletd, reads balances from the scanned
wallet database, constructs and proves real transactions, and broadcasts them
through a hidden sixth account used as the mining and faucet treasury. On first
startup, Account 1 receives exactly 5 ZEC in Orchard from that treasury. Faucet
requests mine mature coinbase funds, shield them through Account 6, and then make the requested transfer. The activity database
is a UI index only and is never a source of wallet balances.

Each faucet request is limited to 5 ZEC. The regtest wallet begins scanning at
block 2 because lightwalletd represents height 0 as an unspecified block ID.
Before shielding, the service retrieves the mature coinbase transaction from
Zakura and stores its full metadata in the wallet; this compensates for the
compact transparent-output response not carrying a transaction index. These
details are internal to the faucet and require no manual setup.

The local chain activates NU6 at height 1 and intentionally remains before
NU6.1. Activating later upgrades at block 1 would require their consensus
lockbox disbursements in that activation block.

The launcher talks to Docker directly and labels every resource with its
instance name. The lightwalletd container currently runs as UID 0 so it can
initialize its root-owned Docker named volume; its gRPC port remains bound only
to loopback on the host.

This software is for Regtest only. Generated keys must never receive real funds.

## Releases

Maintainers create a release candidate by updating the workspace version and
pushing the matching `vX.Y.Z` tag. CI tests the workspace, creates native
launcher archives for Linux and macOS on x86-64 and ARM64, publishes multi-arch
app and lightwalletd images, emits checksums, provenance, and image SBOMs, then
opens a draft GitHub release. Publishing that draft promotes the already-built
image manifests to `latest` and runs an anonymous installation smoke test.

The complete maintainer procedure and one-time repository settings are in
[`RELEASING.md`](RELEASING.md).

## Troubleshooting

- `permission denied` while connecting to `/var/run/docker.sock`: the daemon is
  running, but the current process lacks socket access. Confirm that `id` lists
  `docker`; if it does not, log out and back in after running `usermod`.
- `Cannot connect to the Docker daemon`: start Docker Desktop or the system
  Docker service, then rerun `thus-spoke-zakura doctor`.
- Inspect a service with `thus-spoke-zakura logs app`,
  `thus-spoke-zakura logs zakura`, or
  `thus-spoke-zakura logs lightwalletd`. Add `-f` to follow logs.
- `thus-spoke-zakura reset --force` deletes the selected instance's containers,
  chain, wallet, seed, and volumes. This cannot be recovered.

## License

MIT OR Apache-2.0
