# chore(deps): Zakura Common 4 migration design

Status: implemented and validated. See the [validation record](../reports/2026-10-25-zakura-common-4-validation.md).

Source: [issue #436](https://github.com/zcashlabs/thus-spoke-zakura/issues/436).
Inspected baseline: `42dfcea09ee3cf696dcf6cd922bf370a10e9da77`.

## Goal and scope

Move the launcher and server together to Zakura Common 4.0.0, wallet
backend/SQLite 0.1.0-rc9.4, and Zakura 1.7.0. Preserve the current public API,
account/address identities, retained payment recovery, and server-authoritative
balances. Establish compatibility with both newly created and retained wallets.

The user explicitly excluded new Sapling tests during design. Verify transparent
and Ironwood transfers, shielding, mining, faucets, and synchronization. Keep
existing Sapling-related library features and existing workspace tests; do not
add Sapling routes, UI controls, or dedicated migration scenarios. This narrows
the issue's original acceptance criteria and must be stated in the PR.

This change does not introduce a new indexer, private receiver service, database
backup subsystem, payment state machine, or dependency abstraction. The wallet
releases contain one-time-key/PIR changes, but ths continues its existing
ordinary account-key synchronization and `EnhancementMode::Standard` behavior.
Implementation validation also established that rc9.4 requires explicitly
selecting `TransparentLedgerMode::Public` at wallet open; preserve ths's existing
public transparent-ledger behavior rather than enabling a PIR service.

## Evidence that shapes the design

- `Cargo.toml` declares Rust 1.91, but `rust-toolchain.toml`, the Docker builder,
  CI, and contributor instructions already select Rust 1.98. Raise the declared
  minimum to 1.97; retain the existing 1.98 build toolchain.
- Both `network::regtest_network()` and the generated node configuration already
  activate NU7 at height 1. Preserve that schedule. Some live-test address
  helpers still construct networks with `nu7: None`; align those with the chain
  wherever they describe its actual network.
- `wallet/payments.rs` constructs through `create_proposed_transactions`, not a
  direct PCZT API. A repository source search finds no direct PCZT usage, while
  `crates/ths-server/Cargo.toml` enables its `signer` feature.
- `RealWallet::open` already uses the SDK's `WalletMigrator`, with external
  migrations for prepared payments, treasury cursors, and transparent history.
  Existing extension migration UUIDs and persisted `ext_tsz_*` names are stable
  compatibility contracts.
- The live fixture and CI image pull pin 1.6.0 independently of the launcher.
  Updating only `runtime/images.rs` would leave live tests on the old node.
- `expired_unseen_send_replaces_once_under_the_original_key` currently bounds
  the expiry distance at 100 blocks; the new ordinary NU7 default is 120 blocks
  **from the candidate inclusion height**, not from an arbitrary observed tip.
- ths reads SDK tables/views directly for balances and history. Compilation
  does not check these SQL queries against the migrated schema.

Upstream sources inspected: [Common v4.0.0](https://github.com/zakura-core/common/tree/v4.0.0)
at `e9858a104067c9348a0ea5745e44a2f091f5e896`, published
[backend 0.1.0-rc9.4](https://crates.io/crates/zakura-client-backend/0.1.0-rc9.4),
[SQLite 0.1.0-rc9.4](https://crates.io/crates/zakura-client-sqlite/0.1.0-rc9.4),
[PCZT 0.1.0-rc9](https://crates.io/crates/zakura-pczt/0.1.0-rc9), and
[Zakura v1.7.0](https://github.com/zakura-core/zakura/releases/tag/v1.7.0).
The backend publication records wallet-libraries commit
`476b2edc8463e53cb0a51561ebe8b2f2cdf50f01`.

## Approach and alternatives

**Recommended: one coordinated migration PR, implemented in reviewable stages.**
Capture old-version compatibility evidence first, move the dependency family,
adapt the existing wallet boundary, then validate the final runtime set. Every
merged build uses a coherent library/node combination.

Two alternatives are less suitable:

1. Separate independently released node and wallet upgrades. This increases the
   supported combinations and can make green live tests describe a different
   stack from the shipped launcher. Intermediate combinations are unnecessary
   for this task.
2. Require a fresh wallet or reset on upgrade. This avoids migration work but
   does not satisfy existing-wallet startup or preserve pending operation
   identities. Existing destructive instance cleanup remains an explicit user
   operation, never a migration fallback.

## Dependency and feature contract

| Dependency/configuration | Current | Proposed |
| --- | --- | --- |
| Workspace `rust-version` | `1.91` | `1.97` |
| Build toolchain | `1.98.0` | retain |
| Direct Common family pins | `=2.2.0` | `=4.0.0` |
| Backend, production and dev declarations | `=0.1.0-rc7` | `=0.1.0-rc9.4` |
| SQLite, production and dev declarations | `=0.1.0-rc7` | `=0.1.0-rc9.4` |
| Direct `pczt` declaration | `=0.1.0-rc4`, `signer` | prefer removal after feature-closure verification |
| PCZT if still required | old family | single `0.1.0-rc9`; exact-pin any retained direct declaration |
| `zcash_script` | `=0.4.5` | `=0.6.0` |
| Node image | `zakuracore/zakura:1.6.0` | `zakuracore/zakura:1.7.0` |

Keep the `zcash_*` Rust aliases that name Zakura packages. Preserve existing
features and explicitly declare the new capabilities used with defaults off:

- Keys: `backend-halo2`, `wallet`, `zip32-addresses`, alongside the current
  Orchard/transparent/unstable features.
- Primitives: `backend-halo2`, `wallet`, `pczt`; transaction circuits and
  transparent input support must remain enabled in the production feature graph.
- Sapling: `wallet` and `zip32-addresses` for the existing key/address uses;
  preserve the prover's circuit feature closure.

The new backend already forwards several of these capabilities, but direct
declarations should describe ths's requirements instead of relying on dev-only
feature unification. Test a production-only server build as well as all targets.

For PCZT, inspect `cargo tree -e features` and build after removing the unused
direct declaration. Retain it only if a concrete required feature still needs
it; document that reason and use the new pin. Enabling `primitives/pczt` does
not itself mean ths needs the separate `zakura-pczt` package. Its complete
absence is acceptable when no resolved consumer needs it.

Regenerate the lockfile through Cargo. Verify the actual package identities:
every resolved crate belonging to Common's versioned 4.0.0 family has one 4.x
version, with no leftover 2.x/3.x copy or corresponding upstream crypto package.
Require `zakura-note-encryption` and reject `zcash_note_encryption`. Do not ban
all packages starting with `zcash_`: `zcash_script`, `zcash_encoding`, and other
independent packages are not duplicates of the Common family. Likewise, helper
crates and wallet/PIR releases do not all have Common's major version.

## Wallet construction and recovery

Keep the request path: validate → persist intent → synchronize → propose →
construct and atomically journal signed bytes → submit → reconcile inclusion →
publish a wallet snapshot. Keep fees, receiver choice, and input selection in
the SDK, with the existing ths source-pool restrictions.

Allow Common's height-aware defaults to set expiry. Do not introduce an explicit
40-block override to preserve old test expectations. Check a newly constructed
ordinary NU7 transaction against its proposal target plus 120 blocks, and keep
old retained transactions' stored expiry unchanged.

The new SDK checks saved proposals against current target height, branch,
expiry, and padded shielded budgets. Follow those checks through ths's error
handling. Stale unsigned work can be proposed again under the same intent;
retained signed work must pass the existing original-transaction reconciliation
before any replacement. A new SDK error alone never authorizes replacement.
Budget rejection must leave no committed transaction, prepared journal, or
leaked input reservation; preserve the existing operation/error reporting
contract rather than inventing new public statuses in this upgrade.

Use the new script API at real imports in production and tests. Adapt Orchard
signature/proof/key types only where ths or its fixtures actually consume them.
There are no direct ths uses of `DynamicUsage`, `redpallas`, or `ProofError` in
the inspected source, so upstream release-note entries are audit points rather
than reasons to add wrappers or memory estimation code.

## Retained database compatibility

Use the SDK migration path already invoked by `RealWallet::open`. Preserve
extension IDs/names and seed/account derivation. Audit the balance and history
SQL against the new SDK schema and views; adjust only queries whose contract
changed. Do not hide query failures behind empty balances or empty history.

Create compatibility fixtures **with the baseline rc7 code and lockfile**, before
changing dependencies. Use disposable deterministic Regtest seed material and
record the source commit, versions, schema/migration IDs, and fixture generation
procedure. Include both `wallet.db` and `ths.db`, six account identities,
transparent/Ironwood state, history, treasury cursor/history state, and retained
prepared payment records. Capture consistent SQLite snapshots with the old
server stopped and its WAL checkpointed. Never copy real user wallet material.

Docker-free tests open a temporary copy with the new `RealWallet::open`, query
balances/history/extensions, and reopen it a second time. Check unchanged seed,
account UUIDs/addresses, idempotency mappings, activity identities, original
txids/bytes/expiry, and treasury state. A fixture generated with the new SDK does
not prove upgrade compatibility.

Add a live upgrade scenario using the existing owned Regtest fixture: run the
old server against 1.6.0, retain its private data and pending payment evidence,
stop it, switch to the new server and 1.7.0 with the same selected chain/state,
then reconcile and replay. Extend the fixture only enough to select the two
binaries/images and restart its owned resources. Record both binaries' source
revisions. Ordinary `ths stop` deletes instance data and cannot stand in for a
retained-wallet restart.

Compatibility is an acceptance gate, not a claim already established by the
upstream migration code. If opening fails, surface the migration error and
preserve the databases for diagnosis and retry. Upstream migrations commit per
migration; do not promise that a failed upgrade leaves the entire database
byte-for-byte untouched. Test reopening after an interrupted migration. Do not
promise downgrade compatibility: returning to old software requires restoring
a consistent pre-upgrade snapshot of the affected app/node state.

## Behavior and verification matrix

The existing book and issue establish the preserved behavior below. The Sapling
exclusion is user-confirmed; this document's migration approach is proposed.

| Flow | Sequence and expected result | Verification |
| --- | --- | --- |
| F1: fresh instance | Create wallet, derive six accounts, fund users, reach ready at the canonical height/hash; expose accounts 1–5 only. | Fresh Docker-free initialization plus rebuilt app/1.7.0 startup. |
| F2: retained wallet | Open old databases, migrate, scan, reopen; same accounts, addresses, history, journals, and treasury ownership. | Baseline fixtures plus live old→new upgrade. |
| F3: pending payment | Retain preparing/prepared/broadcast work, lose a response, upgrade/restart, replay the same key; resolve the original intent without a second payout. Changed details still conflict. | Database identity checks and existing queued/broadcast/concurrent recovery scenarios. |
| F4: expiry | New ordinary transactions use target +120. Old signed bytes retain their original expiry. Inclusion at expiry remains valid; absence after the last includable block permits replacement only through existing checks. | Proposal/construction tests and `expired_unseen_send_replaces_once_under_the_original_key`, including inclusion/mempool guards. |
| F5: transfers | Transparent→transparent, transparent→Ironwood, Ironwood→transparent, Ironwood→Ironwood; include same-account cross-pool and external recipients. | Existing proposal tests and live round trip; verify requested receiver/amount, fees/change, node inclusion and scanned balances. |
| F6: mining/faucet | Mine to account 6, discover mature rewards, shield/replenish, pay account and address faucets, replay. | Startup funding, internal/external faucets, shielding, treasury isolation, and persisted cursor checks. |
| F7: construction refusal | Reject oversized padded shielded work or stale unsigned proposals without broadcasting or committing construction artifacts. Retry preserves the original intent and any preexisting signed work. | SDK test-database/proposal seam and ths journal rollback/error regressions; no expensive oversized proof required. |
| F8: reorg/sync | Rewind, rescan replacement chain, reconcile payments and balances, restart; readiness requires the selected height and hash. | Existing reorg/rollback and external transparent spend scenarios on 1.7.0. |
| F9: failure/cleanup | Interrupt startup, migration, or owned-resource teardown; preserve diagnosable state where required and clean only registered resources. Retry completes recovery; foreign instances survive. | Existing launcher/fixture failure tests and cleanup assertions in each selected live run. |

F7 does not add automatic splitting of oversized payments or a new permanent
failure classification. Existing diagnostics and retry semantics remain the
public contract. No performance claim is made for the 10,000-block treasury
workload; that benchmark is outside this migration's required live set.

## Implementation sequence and file ownership

These stages are design-level work packages. Expand them into an executable
implementation plan after design review.

1. **Capture the old-version contract.** Add fixture provenance and baseline
   snapshots under `crates/ths-server/tests/fixtures/wallet-rc7/`; add migration
   tests under `tests/unit/wallet/` using the existing child-module pattern.
   Record account, journal, treasury, balance, and history expectations before
   the dependency update. This establishes F2/F3 independently of the new SDK.
2. **Move the family and adapt the wallet boundary.** Update `Cargo.toml`,
   `crates/ths-server/Cargo.toml`, and `Cargo.lock`; audit
   `src/wallet.rs`, `src/wallet/{payments,history,journal,recovery,transparent_history}.rs`,
   `src/payments/{execution,progression,submission,treasury}.rs`, and affected
   tests. Remove the direct PCZT workaround if the verified feature graph allows
   it. Add focused F4/F7 regressions, run fixture migrations, and compile both
   production and test feature graphs.
3. **Align the actual runtime and live fixtures.** Change
   `crates/ths-cli/src/runtime/images.rs`,
   `crates/ths-server/tests/support/regtest.rs`, its unit tests, and CI's pinned
   node pull. Correct actual-network constructors and the <100-block assumption
   in `tests/scenarios/activity_recovery/`. Audit `src/chain.rs`'s documented
   1,000-block non-finalized window against 1.7.0 before updating its version
   reference. Add the retained old→new live scenario and run F1–F9.
4. **Document and validate the release set.** Update `CHANGELOG.md`,
   `RELEASING.md`, `book/development.md`, `book/cli.md`, and the book's
   state/operations guidance with the observed compatibility result. Update
   `diagrams/build.py` and regenerate `book/images/source-to-runtime.svg`.
   Strengthen the existing dependency-family check in `.github/workflows/ci.yml`
   to enforce the version/identity contract above; keep workflow changes limited
   to this check and required node-tag alignment. Preserve historical changelog
   entries and old design documents as historical records.

## Validation gates

Run the repository's required Rust commands on the final change:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test -p thus-spoke-zakura --features release-distribution
cargo build --locked -p ths-server
cargo metadata --locked --format-version 1
cargo tree --locked -e features -p ths-server
```

Check metadata against the Common-family identity/version rules, including
dev dependencies. Verify the declared 1.97 minimum in an isolated toolchain
check if claiming that floor is tested; a successful 1.98 build proves only the
selected build toolchain.

Build and exercise the actual runtime artifacts:

```console
cargo run -p thus-spoke-zakura -- build --dev
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery --no-run
cargo test --locked --profile dev-runtime -p ths-server --test activity_recovery
docker pull zakuracore/zakura:1.7.0
docker build -f docker/lightwalletd.Dockerfile -t ths-recovery-lightwalletd:local .
```

Run the new old→new scenario and explicitly select the existing live cases
covering the matrix: `broadcast_recovers_after_auto_mine_failure`,
`concurrent_identical_sends_have_one_chain_effect`,
`address_faucet_retry_after_server_restart_pays_each_destination_once`,
`same_account_cross_pool_round_trip_is_replay_safe`,
`internal_address_faucets_record_confirmed_activity`,
`internal_address_faucet_recovers_after_auto_mine_failure`,
`external_address_faucet_behavior_is_unchanged`,
`startup_funding_retry_after_auto_mine_failure_has_one_payout`,
`prepare_rolls_back_constructed_transaction_when_journal_write_fails`,
`queued_send_retry_survives_lost_response_and_restart`,
`queued_external_faucet_retry_survives_lost_response_and_restart`,
`expired_unseen_send_replaces_once_under_the_original_key`,
`dependent_payments_resume_in_admission_order_after_restart`,
`reorg_and_rollback_preserve_canonical_wallet_and_payment_state`, and
`external_transparent_spend_reconciles_after_restart`.
Use `-- --ignored --exact <case>` and confirm one selected test each time.
Keep cleanup failures as failures. Also start one disposable named instance
through the rebuilt launcher/app image: the integration target uses a host-built
server and cannot by itself prove the app-container path.

For docs, run `python3 diagrams/build.py`, `mdbook build`,
`python3 tools/build_site.py`, and `python3 tests/book_links.py`; verify generated
figures are reproducible. Run all web checks if implementation touches `web/`.
No installer change is planned; run `tests/install.sh` if that scope changes.

Record source revisions, binary/profile identities, node/app/lightwalletd image
identities, selected test counts, chain inclusion/checkpoints, wallet outcomes,
and cleanup results. Check that the selected node image supports the release's
Linux AMD64/ARM64 targets. Run the normal release-candidate matrix before making
a release compatibility claim. This design authorizes no publishing or tagging.

## Release impact and completion criteria

The build minimum changes from Rust 1.91 to 1.97; the normal development
toolchain stays 1.98.0. The node image changes to 1.7.0. Public command/API
shapes, Regtest-only operation, loopback bindings, and account visibility stay
compatible. SDK database schemas may migrate forward; downgrade compatibility
must not be asserted without evidence. Describe restoring a consistent
pre-upgrade snapshot when a downgrade is required. Do not reset retained data
to make an upgrade test pass.

Ship a newly versioned launcher/app/lightwalletd release set using the existing
release process; do not replace a published tag. Version selection belongs to
release preparation, not this design-only change. Keep the existing lightwalletd
source version unless the 1.7.0 integration evidence demonstrates a necessary
compatibility fix.

The migration is complete when the final graph is coherent, required Rust and
image builds pass, fresh and retained wallets satisfy F1–F9, the selected real
Regtest flows pass against 1.7.0, documentation states measured compatibility
and release impact, and the PR explicitly records the user-approved Sapling
test exclusion. The linked validation record records the completed implementation checks and
remaining release-only gates.
