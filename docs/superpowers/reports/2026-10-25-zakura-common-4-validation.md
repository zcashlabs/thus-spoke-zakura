# chore(deps): Zakura Common 4 validation

Initial validation on 2026-10-25 used baseline commit
`42dfcea09ee3cf696dcf6cd922bf370a10e9da77` plus this working-tree migration.
The baseline rc7 executable was built with its original lockfile before changing
pins and preserved separately at `/private/tmp/ths-436-baseline/ths-server`.

## Implemented behavior

Common dependencies resolve once at 4.0.0; backend/SQLite are exactly rc9.4;
the selected node is `zakuracore/zakura:1.7.0`. The unused direct PCZT signer
workaround is removed; the resolved transitive PCZT is rc9. Independent
`zcash_script` is exactly 0.6.0. New explicit feature requirements and public
transparent-ledger configuration preserve the existing account-key sync path.

Wallet migrations preserve six account identities, user account visibility,
canonical balances/history, treasury/history cursors, idempotency mappings and
original transaction bytes/expiry. Fresh ordinary NU7 construction uses target
height +120. Tip-level Ironwood receipts can require another anchor block:
qualified unspent/unlocked receipts remain pending, automatic preparation advances
at most one block and retries once, and other mining modes preserve their existing
mining rules. Treasury anchor waits precede reward replenishment. Startup only
matures the original funding transaction, including after an interrupted attempt;
it does not mature later receipts or change on restart.

## Checks and artifacts

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Passed |
| `RUST_TEST_THREADS=4 cargo test --workspace` | 695 passed, zero failures; ordinary ignored live tests selected separately below |
| `cargo test -p thus-spoke-zakura --features release-distribution` | 356 passed, zero failures |
| `cargo +1.97.0 check --locked --offline --workspace --all-targets --all-features` | Passed using a separate temporary target directory |
| Cargo metadata family guard and its Python unit tests | Passed; 6 Python tests |
| `cargo run -p thus-spoke-zakura -- build --dev` | Passed for final production source |
| mdBook 0.5.2, `tools/build_site.py`, `tests/book_links.py` | Passed; zero broken book links |
| Independent final code review and scoped fix reviews | No outstanding findings |

The development/release toolchain stays Rust 1.98.0; the new declared 1.97
minimum was also compiled. Node image metadata advertises Linux AMD64 and ARM64;
local container execution used Linux ARM64. The remote release-candidate matrix
and publication were not run.

A disposable `ths --name common4-436-validation run` exercised the rebuilt app
container and reached funded readiness: account IDs `[1,2,3,4,5]`, Account 1 at
500,000,000 Ironwood zatoshi, and matching wallet/node height/hash at height108.
The launcher completed instance-scoped cleanup. No generated test containers or
named-smoke volumes remained at the final inspection.

Runtime image IDs are recorded in `/private/tmp/ths-436-runtime-images.txt`.
The node's published manifest digest is
`sha256:cf3b2d09a0ab3cd3a5e17cbbb676b7abe719607391402ea07d6402e9af897b6b`.
App/lightwalletd tags remain the local Cargo-version tags `0.4.1`; release
version selection belongs to release preparation, not this change.

## Selected real Regtest cases

Each invocation selected exactly one ignored test and passed, including its
cleanup assertions. Commands use
`cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery -- --ignored --exact <case>`.
The upgrade case also sets `THS_UPGRADE_BASELINE_SERVER` and tests both old signed
payment recovery and a fresh payment constructed with the migrated keys.

- `concurrent_identical_sends_have_one_chain_effect`
- `address_faucet_retry_after_server_restart_pays_each_destination_once`
- `internal_address_faucets_record_confirmed_activity`
- `internal_address_faucet_recovers_after_auto_mine_failure`
- `external_address_faucet_behavior_is_unchanged`
- `startup_funding_retry_after_auto_mine_failure_has_one_payout`
- `prepare_rolls_back_constructed_transaction_when_journal_write_fails`
- `queued_send_retry_survives_lost_response_and_restart`
- `queued_external_faucet_retry_survives_lost_response_and_restart`
- `expired_unseen_send_replaces_once_under_the_original_key`
- `dependent_payments_resume_in_admission_order_after_restart`
- `reorg_and_rollback_preserve_canonical_wallet_and_payment_state`
- `external_transparent_spend_reconciles_after_restart`
- `manual_mode_keeps_a_payment_pending_until_ths_mine`
- `interval_mode_confirms_a_payment_without_ths_mine`
- `mempool_mode_confirms_a_payment_without_ths_mine`
- `broadcast_recovers_after_auto_mine_failure`
- `same_account_cross_pool_round_trip_is_replay_safe`
- `retained_rc7_payment_survives_common4_and_node_upgrade`

## Verification corrections and scope

Tests were deferred until the initial implementation was written, as requested.
Validation identified and corrected the required public transparent-ledger mode
and the SDK's one-block-behind-tip anchor selection. Regression tests cover
pending vs genuinely insufficient funds, wrong account/pool, unmined credits,
reserved notes, bounded retry/no-progress behavior, and avoiding unnecessary
reward replenishment. Existing idempotency/recovery contracts remain intact.

The retained-node test uses Zakura's existing synchronous-backup test setting to
capture a consistent old-chain snapshot. Its ordinary periodic backup can lag
the visible tip at shutdown. The replacement must restore the exact checkpoint
before the new server starts; the test never repairs missing blocks by re-mining.

Two faucet fault fixtures now mature prior payout change before enabling the
confirmation-mining failure. Their original broadcast/restart/inclusion checks
remain. A pre-existing lost-reply mining regression used paused Tokio time with
real HTTP; it now waits for actual polling phases with a real clock and joins
its tasks while retaining the no-second-generate assertion.

No new Sapling scenarios were added, following the user's explicit scope.
Existing workspace tests remain enabled. The 10,000-block performance workload
was not selected, and no large-history performance claim is made. Downgrade
compatibility is not promised; retain a consistent pre-upgrade snapshot when a
rollback of app/node software may be needed.

## refactor: Ponytail simplification

Removed 20 net lines across three source/CI files: Send now performs its single
optional retry directly, proposal failures use ordinary `match`/`?` handling,
and CI pipes Cargo metadata straight into the existing family guard. All
migration fixtures, validation rules, idempotency and recovery tests remain.

Rechecked formatting, Clippy, all 695 workspace tests, all 356 distribution
tests, Rust 1.97 with all targets/features, the six dependency-guard tests and
the real metadata graph. The cross-pool round trip, dependent-payment restart,
and retained rc7→Common4 upgrade live cases each passed again. The development
images were rebuilt; their identities are in
`/private/tmp/ths-436-ponytail-runtime-images.txt`.

## chore: PR branch verification

The PR branches directly from `main` at
`8bb525c6e17cd7c5e07d3288037ed954c3c1780c`. It excludes the unrelated
raw-transaction decoding feature in PR #431. The rc7 fixture and baseline
executable retain their original source provenance above.

Formatting, Clippy, the six dependency-guard tests, the locked metadata graph,
and all 356 release-distribution tests passed on this base. The retained rc7
payment/node upgrade regression passed again in 19.09 seconds.

The first main-based workspace run had 691 passes and one failure in the
unchanged CLI test
`a_start_in_docker_preflight_already_owns_the_environment`: its elapsed-time
assertion exceeded the six-second fake-preflight bound. Its duplicate-owner
status/message assertions passed. All four ownership tests passed on a serial
rerun without source changes. The initial failure log is retained at
`/private/tmp/ths-436-pr-checks/workspace.log`.

A complete workspace rerun without concurrent container validation passed:
692 tests, zero failures, 32 ignored. The three-test difference from the
initial 695-test baseline is the excluded raw-transaction decoding feature.
The final workspace log is
`/private/tmp/ths-436-pr-checks/workspace-rerun.log`. No source changes were
made to obtain the passing rerun.
