# Appendix B: Develop from source

The teaching chapters use an installed `ths` launcher. For changes to this
repository, the crucial rule is that `ths` starts **Docker images**. Editing
Rust or React source does not change a running container or an already-built
image.

![The web and server sources build the app image, the lightwalletd Dockerfile builds the lightwalletd image, and Zakura is pinned and pulled. ths start refuses to run until all three exist. Editing source changes neither a running container nor an existing image.](images/source-to-runtime.svg)

1. `web/` and `crates/tsz-server/` go into the app image. The app serves
   both the built React dashboard and HTTP API.
2. The build command also prepares the lightwalletd runtime image and ensures
   the pinned Zakura image is present.
3. `start` creates a fresh instance from those images. A source edit needs
   another image build and a new instance before the normal dashboard shows it.

## Build and start

Use **Rust 1.98**, **Node 24**, and a running Docker daemon. Run these from
the repository root:

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

Keep the launch terminal open. Ctrl+C removes that instance and its data,
as explained in [Chapter 1](getting-started.md). After changing
`crates/tsz-server` or `web/`, rerun:

```console
cargo run -p thus-spoke-zakura -- build --dev
```

Then start a new instance. `build` without `--dev` produces
release-optimized images. `start` requires the matching images already
present; it does not fetch or rebuild them.

## Find the part you need to change

| Need | Start reading |
| --- | --- |
| Launcher commands, Docker ports, instance cleanup | `crates/tsz-cli/src/main.rs` and `runtime.rs` |
| Server routes, payment and mine handlers, wallet snapshot | `crates/tsz-server/src/api.rs` |
| User accounts, treasury boundary, activity and keys | `crates/tsz-server/src/db.rs` |
| Wallet scan, transaction construction, spendability | `crates/tsz-server/src/wallet.rs` |
| Canonical checkpoint and reorganization handling | `crates/tsz-server/src/reconcile.rs` |
| Dashboard requests, schemas, and query invalidation | `web/src/lib/api/` and `web/src/hooks/` |

Follow one request across boundaries before changing behavior: a
dashboard action calls an API route, the server may change wallet or
chain state, then events trigger query refetches. [Chapters 2–6](architecture/overview.md)
explain why these owners are separate. Keep host-published ports on
`127.0.0.1` and Account 6 out of user-facing lists.

## Iterate on the frontend

For a React-only change, build and start the runtime once. In a second
terminal, get its live dashboard URL with the source launcher and pass it
to Vite:

```console
TSZ_DEV_API="$(cargo run -q -p thus-spoke-zakura -- endpoints --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["dashboard"])')" npm run dev --prefix web
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

The normal `cargo test --workspace` run needs no Docker. The live
`activity_recovery` regression is ignored there and has a separate
Docker setup; read the [README's integration-test
instructions](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/README.md#activity-recovery-integration-test)
before running it.

For contribution and pull-request policy, see
[`CONTRIBUTING.md`](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/CONTRIBUTING.md).
For release images and publication, see
[`RELEASING.md`](https://github.com/zcashlabs/thus-spoke-zakura/blob/main/RELEASING.md).
For operational symptoms, turn to [Appendix C:
Troubleshooting](troubleshooting.md).
