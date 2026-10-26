# Appendix B: Develop from source

The teaching chapters use an installed `ths` launcher. For changes to this
repository, the crucial rule is that `ths` starts **Docker images**. Editing
Rust or React source does not change a running container or an already-built
image.

![The web and server sources build the app image, the lightwalletd Dockerfile builds the lightwalletd image, and Zakura is pinned and pulled. ths start refuses to run until all three exist. Editing source changes neither a running container nor an existing image.](images/source-to-runtime.svg)

1. `web/` and `crates/ths-server/` go into the app image. The app serves
   both the built React dashboard and HTTP API.
2. The build command also prepares the lightwalletd runtime image and ensures
   the pinned Zakura image is present.
3. `start` creates a fresh instance from those images. A source edit needs
   another image build and a new instance before the normal dashboard shows it.

## Build and start

Use **Rust 1.98**, **Node 24**, and a running Docker daemon. The declared
Rust minimum is 1.97; the pinned development toolchain remains 1.98.0. Run these
from the repository root of the [recorded main checkout](README.md):

```console
npm ci --prefix web
cargo run -p thus-spoke-zakura -- build --dev
cargo run -p thus-spoke-zakura -- start
```

The Cargo package is `thus-spoke-zakura`; `ths` is its binary name. The
first command installs the dashboard's locked dependencies. The second
builds runtime images, keeping workspace Rust code unoptimized for faster
development builds. The last launches the default instance in the
foreground. With no subcommand, the source launcher starts the same
environment.

Keep the launch terminal open. Ctrl+C triggers instance-scoped cleanup,
as explained in [Chapter 1](getting-started.md). After changing
`crates/ths-server` or `web/`, rerun:

```console
cargo run -p thus-spoke-zakura -- build --dev
```

Then start a new instance. `build` without `--dev` produces
release-optimized images. `start` requires the matching images already
present; it does not fetch or rebuild them.

## Run a test against a disposable instance

Once the matching runtime images exist, the source launcher can run a child
command against a settled, fresh environment:

```console
cargo run -p thus-spoke-zakura -- run --timeout 180 -- sh -c 'curl -fsS "$THS_API_URL/api/v1/accounts"'
```

The child receives the instance's `THS_*` endpoint variables, and the launcher
attempts instance-scoped cleanup after it exits. The timeout covers startup
and settlement, not the child runtime. Use non-interactive commands. The
[CLI reference](cli.md) covers collision rules, interruption, and retained
recovery metadata. To wait for an instance already started in another terminal,
use `cargo run -p thus-spoke-zakura -- --name alice wait --timeout 180`.

## Find the part you need to change

| Need | Start reading |
| --- | --- |
| Launcher commands and flags | `crates/ths-cli/src/args.rs` and `commands.rs` |
| Docker ports, instance startup and cleanup | `crates/ths-cli/src/runtime/`, starting with `instance.rs`, `startup.rs`, and `cleanup.rs` |
| Server routes | `crates/ths-server/src/api.rs` |
| Payment and mine handlers | `crates/ths-server/src/api/payments.rs` and `api/mining.rs` |
| Payment progression, confirmation, and treasury replenishment | `crates/ths-server/src/payments/` |
| Wallet snapshot and sync status | `crates/ths-server/src/state.rs` and `sync.rs` |
| User accounts, treasury boundary, activity and keys | `crates/ths-server/src/db.rs` and `db/` |
| Wallet scan, transaction construction, spendability | `crates/ths-server/src/wallet.rs` and `wallet/payments.rs` |
| Canonical checkpoint and reorganization handling | `crates/ths-server/src/reconcile.rs` |
| Dashboard requests, schemas, and query invalidation | `web/src/lib/api/` and `web/src/hooks/` |

Follow one request across boundaries before changing behavior: a
dashboard action calls an API route, the server may change wallet or
chain state, then events trigger query refetches. [Chapters 2–6](architecture/overview.md)
explain why these owners are separate. Keep host-published ports on
`127.0.0.1` and Account 6 out of user-facing lists.

## Iterate on the frontend

For a React-only change, build and start the runtime once. In a second
terminal, start Vite:

```console
npm run dev --prefix web
```

Vite proxies to the default dashboard port, `32805`. For a
`--port-offset` instance, set `THS_DEV_API` to its URL (here `alice`):

```console
THS_DEV_API="$(cargo run -q -p thus-spoke-zakura -- --name alice endpoints --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["dashboard"])')" npm run dev --prefix web
```

Vite proxies API calls to that running app. A frontend edit appears in
the Vite page immediately. Rebuild the app image before checking it in
the normal `ths` dashboard.

## Run the relevant checks

After changing `crates/`:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test -p thus-spoke-zakura --features release-distribution
```

After changing `web/` (with Node 24):

```console
npm run lint --prefix web
npm run format:check --prefix web
npm test --prefix web
npm run build --prefix web
```

The normal `cargo test --workspace` run needs no Docker. Ignored live
regressions use the [Docker lifecycle](#docker-lifecycle-regressions) and
[activity-recovery](#activity-recovery-integration-test) setups below.

For contribution and pull-request policy, see
[`CONTRIBUTING.md`](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/CONTRIBUTING.md).
For release images and publication, see
[`RELEASING.md`](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/RELEASING.md).
For operational symptoms, turn to [Appendix C:
Troubleshooting](troubleshooting.md).

## Docker lifecycle regressions

These ignored launcher tests need a disposable Docker daemon. The lost-create
and lost-start cases also need prepared runtime images. List the cases, then
run the one you need with `--ignored --exact`:

```console
cargo test -p thus-spoke-zakura runtime::docker_lifecycle_tests:: -- --ignored --list
cargo test -p thus-spoke-zakura runtime::docker_lifecycle_tests::lost_start_reply_is_cleaned_up_and_can_be_retried -- --ignored --exact
```

The launcher allows 120 seconds per readiness phase, 30 seconds for ordinary
Docker startup commands, 120 seconds for attached initialization, and a separate
120 seconds for cleanup. A killed Docker client may leave a mutation running in
the daemon. Cleanup checks resource identity and never adopts a same-name
replacement or removes another instance's resources. Only one `start` or `run`
owns a name at a time; `stop` and `reset --force` ask that launcher to shut down
and wait for verified cleanup. Shutdown requests are tied to its session, so an
old request cannot stop a later launch.

## Activity-recovery integration test

Run from the repository root with Rust 1.98.0, Docker, network access, and
enough resources for the pinned images and real Ironwood proofs. The first two
Cargo commands compile the test and run its Docker-free helpers. List the
ignored cases, then run one at a time:

```console
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery --no-run
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery
docker pull zakuracore/zakura:1.7.0
docker build -f docker/lightwalletd.Dockerfile -t ths-recovery-lightwalletd:local .
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --list
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --exact broadcast_recovers_after_auto_mine_failure
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --exact reorg_and_rollback_preserve_canonical_wallet_and_payment_state
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --exact dependent_payments_resume_in_admission_order_after_restart --nocapture
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --exact external_transparent_spend_reconciles_after_restart
```

Linux CI runs reorg/rollback, broadcast recovery, concurrent sends, address-faucet restart
recovery, and queued-response cases on every push and PR. The other cases in
`tests/activity_recovery.rs` cover startup funding, pool transfers, and
external faucets. The fixture owns and cleans up only its own Docker resources;
it fails on a timeout or incomplete cleanup.

The dependent-payment case seeds a retained Faucet→Send→Send chain while the
server is stopped, with reversed IDs and tied timestamps. Restart must confirm
all three original operations without another submission. Controlled automatic
mining failures first keep the funder in the mempool; both dependents must
attempt preparation and remain pending for insufficient funds. The fixture
restarts, mines the original funder, and allows progression again. It checks
actual chain inclusion and wallet balances, then restarts again and replays
each key to verify unchanged identities, journals, balances, and chain tip.

The reorg/rollback regression observes the server's `SendTransaction` calls
through the existing gRPC proxy to check that rollback, idempotent replay, and
restart do not rebroadcast orphaned payments. Mempool absence is not that proof:
Zakura's own RPC retry queue can re-admit an earlier submission even while the
server is stopped. Canonical inclusion, balances, signed bytes, payment identity,
and the reconciled wallet checkpoint are checked separately.

## Ironwood anchor compatibility

The Common 4 SDK can require another block before a newly confirmed Ironwood
note or change output is eligible for a spend anchor. A confirmed balance
therefore does not guarantee that construction can spend it at the same tip.
Automatic Send progression mines at most one prerequisite block and retries
construction once. Manual and interval modes leave the original operation
pending until the chain advances. No public payment status is added.

The focused treasury seam regressions in
`crates/ths-server/tests/unit/payments/treasury/tests.rs` distinguish existing
funds waiting for an anchor from a treasury deficit. They simulate reaching the
required height, an anchor that stays unavailable after one retry, and a miner
that makes no progress. Anchor waits must not discover rewards or shield more
funds. These tests complement the wallet adapter's checks that the relevant
notes are unspent, unlocked, and actually mined at the selected tip; unmined
and orphaned credits cannot trigger prerequisite mining.

The server explicitly selects the SDK's public transparent ledger mode alongside
standard transaction enhancement, preserving its existing local RPC/lightwalletd
sync path without a private receiver service.

Audit the resolved dependency graph when updating the wallet family:

```console
cargo metadata --locked --format-version 1
cargo tree --locked --duplicates
```

For the Common 4 migration, verify that each Common family crate resolves once
at 4.x and that its upstream counterpart is absent, including replacement of
`zcash_note_encryption` with `zakura-note-encryption`. Independent `zcash_script`,
`zcash_encoding`, and `zcash_spec` packages keep their own versioning.
