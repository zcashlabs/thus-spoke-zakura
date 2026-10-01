# AGENTS.md

Thus Spoke Zakura runs a local Zcash Regtest environment. The `ths` launcher (`crates/tsz-cli`) starts Zakura, lightwalletd, and an app container that serves `tsz-server` (`crates/tsz-server`) and the React dashboard (`web/`). Human-facing setup is in `CONTRIBUTING.md`; releases are in `RELEASING.md`.

## The book

`book/` holds the documentation: how the environment is used, what each component owns, and what an API response does and does not prove. Read it before changing behaviour that it describes, and update it in the same change. `mdbook build && python3 tools/build_site.py` renders it; `python3 tests/book_links.py` checks every link.

Published at <https://amiabix.github.io/thus-spoke-zakura-book/>. For reading rather than browsing, every page is also served as markdown, and
<https://amiabix.github.io/thus-spoke-zakura-book/book.md> is the whole book in one file (about 21k tokens). <https://amiabix.github.io/thus-spoke-zakura-book/llms.txt> indexes both.

## Commands

Run from the repository root. Web commands need Node 24 (for example, `nvm use 24`).

Rust checks:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test -p thus-spoke-zakura --features release-distribution
```

Web checks:

```console
npm run lint --prefix web
npm run format:check --prefix web
npm test --prefix web
npm run build --prefix web
```

- Run the Rust checks after changing `crates/`, and the web checks after changing `web/`.
- Run `tests/install.sh` after changing `install.sh`.
- Use `cargo run -p thus-spoke-zakura -- <args>` to run the launcher from source. The package is `thus-spoke-zakura`; `ths` is only the binary name.
- Rebuild images with `cargo run -p thus-spoke-zakura -- build --dev` after changing `crates/tsz-server` or `web/`. `ths` runs prebuilt Docker images and will not pick up source changes otherwise.
- Use `TSZ_DEV_API=<dashboard-url> npm run dev --prefix web` for frontend work against a running instance. Get the URL from `ths endpoints`.
- Expect `cargo test --workspace` to run without Docker. The live `activity_recovery` regression is `#[ignore]`d and needs Docker; read "Activity-recovery integration test" in `README.md` before running or changing it.

## Always

- Keep everything Regtest-only. Bind every host-published service to `127.0.0.1`; container-internal listeners may use `0.0.0.0`.
- Preserve instance-scoped cleanup after normal shutdown, interruption, and partial startup failure. Never remove resources belonging to another instance.
- Use the Zakura wallet crates (`zakura-keys`, `zakura-primitives`, `zakura-client-*`). `crates/tsz-server/Cargo.toml` imports them under `zcash_*` aliases.
- Treat accounts 1 to 5 as user accounts. Keep account 6 (`TREASURY_ACCOUNT_ID` in `crates/tsz-server/src/db.rs`), the mining and faucet treasury, out of user-facing account lists and balances.
- Do not remove or weaken idempotency-key validation, deduplication, or activity recovery. Changes to these flows require regression tests.
- Update the command table in `README.md` when adding or changing `ths` commands or flags.

## Code quality

- Follow the conventions and style of the surrounding code. Reuse existing abstractions, naming, error handling, test patterns, and file organization instead of introducing a new style for the same problem.
- Keep changes focused. Do not reformat, rename, reorganize, or refactor unrelated code.
- Keep the server wallet snapshot authoritative for account balances. The dashboard reads balances through the query hooks in `web/src/hooks/`; do not mirror server query data in component state or reconstruct balances from activity or chain data. Deriving display aggregates from query data is fine.
- Reuse `ApiError`/`ApiResult` for server errors and `web/src/lib/api` including its client and Zod schemas for web requests.
- Add a trait or abstraction only when it is reused or needed as a test seam, like `FaucetRuntime` in `crates/tsz-server/src/api.rs`. Prefer Docker-free tests through existing seams; add live Docker tests only when the behavior cannot be covered otherwise.
- Do not discard unexpected errors with `.ok()`, `unwrap_or_default()`, or `let _ =`. Use them only for deliberate optional or fail-open behavior that is clear from the surrounding code or an explanatory comment.
- Write comments that explain constraints, invariants, or reasoning. Avoid comments that merely restate the code.

## Sensitive changes

Only change the following when required by the task. Explain the compatibility or release impact:

- Changing exact-pinned versions (`=x.y.z`) in `crates/tsz-server/Cargo.toml` or the `[patch.crates-io]` git revisions in `Cargo.toml`. The wallet crates are release candidates and must move together.
- Changing image names or tags in `crates/tsz-cli/src/runtime.rs`, `Dockerfile`, or `docker/lightwalletd.Dockerfile`.
- Adding dependencies to either crate or to `web/package.json`. Prefer the standard library, browser APIs, or existing dependencies when they already provide the required behavior.
- Editing `.github/workflows/` or the release process.

## Never

- Connect to mainnet or testnet, or bind a service beyond loopback.
- Add the upstream `zcash_keys` or `zcash_primitives` packages. CI rejects them.
- Edit `web/dist/` or `target/`. They are build output.

## Git

Use Conventional Commits, for example `fix(cli): ...` or `feat(web): ...`. Follow the Pull requests section in `CONTRIBUTING.md`.
