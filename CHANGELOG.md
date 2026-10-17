# Changelog

For earlier releases, see [GitHub Releases](https://github.com/zcashlabs/thus-spoke-zakura/releases).

## Unreleased

## v0.4.0

![Thus Spoke Zakura v0.4.0 release artwork](assets/images/release-v0.4.0.png)

This release makes payments durable and recoverable, activates NU7 for fresh
Regtest environments, and expands the dashboard and CLI for payment, mining,
and environment workflows.

### Action required

- **Fresh NU7 environment:** New instances activate NU7 at height 1. Existing
  chains retain their historic upgrade schedule; create a fresh named instance
  to exercise NU7. [#132](https://github.com/zcashlabs/thus-spoke-zakura/pull/132)
- **Payment API migration:** A successful payment POST now means that the
  server has durably accepted the request, not that the payment completed.
  Follow its `Location` with GET and inspect the pending, confirmed, or failed
  status. Replaying a terminal request returns its stored outcome with HTTP
  200. [#235](https://github.com/zcashlabs/thus-spoke-zakura/pull/235)
- **Payment status after chain changes:** API clients must accept `unconfirmed`
  for payments whose confirming blocks are removed by `ths reorg` or
  `ths rollback`. The original transaction and idempotency key are preserved;
  the server observes these payments without automatically rebroadcasting,
  replacing, or mining them.
  [#307](https://github.com/zcashlabs/thus-spoke-zakura/pull/307)
- **Mining JSON migration:** `ths mine --json` now prints a mining job rather
  than the old `{blocks, hashes}` result. It includes the job ID, requested and
  completed blocks, state, error, and `progress_uncertain`.
  [#214](https://github.com/zcashlabs/thus-spoke-zakura/pull/214)

### Highlights

- **Durable payment recovery:** Send and faucet requests are retained before
  execution and then progressed independently by the server. The dashboard and
  CLI retain idempotency keys, restore outcomes after interruption, and expose
  pending and failed payment status without relying on a wallet scan.
  [#199](https://github.com/zcashlabs/thus-spoke-zakura/pull/199),
  [#200](https://github.com/zcashlabs/thus-spoke-zakura/pull/200),
  [#211](https://github.com/zcashlabs/thus-spoke-zakura/pull/211),
  [#213](https://github.com/zcashlabs/thus-spoke-zakura/pull/213),
  [#217](https://github.com/zcashlabs/thus-spoke-zakura/pull/217),
  [#219](https://github.com/zcashlabs/thus-spoke-zakura/pull/219),
  [#235](https://github.com/zcashlabs/thus-spoke-zakura/pull/235),
  [#243](https://github.com/zcashlabs/thus-spoke-zakura/pull/243)
- **Payment requests and external recipients:** The dashboard creates ZIP-321
  requests with a QR code, sends to pasted Regtest addresses, and faucets
  supported external transparent and unified addresses. Invalid or wrong-network
  addresses are rejected before a payment is submitted.
  [#110](https://github.com/zcashlabs/thus-spoke-zakura/pull/110),
  [#210](https://github.com/zcashlabs/thus-spoke-zakura/pull/210),
  [#384](https://github.com/zcashlabs/thus-spoke-zakura/pull/384)
- **Wallet CLI:** `ths wallet balances` lists user-account addresses and
  balances, `ths wallet activity` prints recent transfers and faucets, and
  `send`, `shield`, and `unshield` support `--max` using the server-calculated
  spendable amount. [#188](https://github.com/zcashlabs/thus-spoke-zakura/pull/188),
  [#215](https://github.com/zcashlabs/thus-spoke-zakura/pull/215),
  [#244](https://github.com/zcashlabs/thus-spoke-zakura/pull/244)
- **Mining and Regtest:** `ths mine` now follows server-owned mining jobs, so
  an interrupted client can report its job ID instead of starting duplicate
  work. Fresh environments activate NU7 at height 1, including its 25-second
  block target, action limits, and fee recycling.
  [#132](https://github.com/zcashlabs/thus-spoke-zakura/pull/132),
  [#214](https://github.com/zcashlabs/thus-spoke-zakura/pull/214)
- **Reorg and rollback testing:** `ths reorg --depth N` replaces recent blocks,
  while `ths rollback --depth N` removes them without replacements. Use
  `reorg --keep-txs` to attempt reinclusion of dropped transactions. Both
  commands reconcile wallet balances and have matching HTTP APIs. Wallet sync
  retries transient lightwalletd cache-repair errors. A failed or interrupted
  request may have changed the chain; inspect its tip before retrying.
  [#307](https://github.com/zcashlabs/thus-spoke-zakura/pull/307),
  [#388](https://github.com/zcashlabs/thus-spoke-zakura/pull/388)
- **Environment workflows:** `ths run` starts a fresh settled environment for
  a child command and cleans it up afterwards. `ths list` reports environment
  and container status, while `ths logs` accepts `--head` and `--tail`.
  `ths endpoints --env` prints shell exports matching the environment variables
  passed to a `ths run` child.
  [#76](https://github.com/zcashlabs/thus-spoke-zakura/pull/76),
  [#101](https://github.com/zcashlabs/thus-spoke-zakura/pull/101),
  [#141](https://github.com/zcashlabs/thus-spoke-zakura/pull/141),
  [#311](https://github.com/zcashlabs/thus-spoke-zakura/pull/311)
- **Loopback Host validation:** Local API clients and reverse proxies must send
  a loopback `Host` header. The app rejects non-loopback hosts to prevent DNS
  rebinding against the unauthenticated local API.
  [#218](https://github.com/zcashlabs/thus-spoke-zakura/pull/218)

### Reliability, security, and usability

- **Safe instance lifecycle:** Startup, readiness, and cleanup have bounded
  operations and preserve recovery state when cleanup cannot be verified. A
  single launcher owns each named environment; competing starts are refused,
  and `stop` and forced `reset` coordinate with that owner. `ths wait` now
  waits through startup rather than reporting a still-starting environment as
  missing. [#133](https://github.com/zcashlabs/thus-spoke-zakura/pull/133),
  [#202](https://github.com/zcashlabs/thus-spoke-zakura/pull/202),
  [#276](https://github.com/zcashlabs/thus-spoke-zakura/pull/276),
  [#277](https://github.com/zcashlabs/thus-spoke-zakura/pull/277)
- **Runtime health and storage:** Health probes check both Zakura and
  lightwalletd on every request. Zakura stores its chain state in the mounted
  chain volume rather than the container filesystem.
  [#296](https://github.com/zcashlabs/thus-spoke-zakura/pull/296),
  [#283](https://github.com/zcashlabs/thus-spoke-zakura/pull/283)
- **Launcher updates and pipes:** Update and uninstall resolve launcher
  symlinks to operate on the real binary while preserving the link. Successful
  updates in JSON mode emit the report on stdout and installer output on
  stderr. `ths logs` exits successfully when its reader closes the pipe.
  [#323](https://github.com/zcashlabs/thus-spoke-zakura/pull/323),
  [#333](https://github.com/zcashlabs/thus-spoke-zakura/pull/333),
  [#343](https://github.com/zcashlabs/thus-spoke-zakura/pull/343)
- **Secure local API and compatible releases:** The app rejects non-loopback
  Host headers and release workflow shell steps safely handle release tags.
  Linux launcher artifacts are built against an older glibc baseline and smoke
  tested on supported Linux distributions.
  [#218](https://github.com/zcashlabs/thus-spoke-zakura/pull/218),
  [#237](https://github.com/zcashlabs/thus-spoke-zakura/pull/237)
- **Clear API failures:** JSON-body, query, path, and method rejections now use
  the API error envelope, allowing clients to display actionable errors rather
  than generic HTTP-status messages. [#285](https://github.com/zcashlabs/thus-spoke-zakura/pull/285)
- **Payment validation and startup:** Send fee quotes return HTTP 503 while
  the environment is starting or the wallet remains unscanned, instead of an
  unexpected HTTP 500. ZIP-321 parsing rejects explicitly zero payment amounts
  with HTTP 400; omitting the amount still lets the sender supply one.
  [#299](https://github.com/zcashlabs/thus-spoke-zakura/pull/299),
  [#246](https://github.com/zcashlabs/thus-spoke-zakura/pull/246)
- **Accurate dashboard state:** The dashboard distinguishes node connectivity
  from wallet readiness, displays settlement and wallet-freshness information,
  preserves payment drafts and recovery identities, and keeps navigation tied
  to the current action. Queries refresh when the event stream connects or
  reconnects, preventing an older response from leaving balances stale.
  Payment creation times use the local date and time, and following a payment
  transaction closes the Payments dialog.
  [#171](https://github.com/zcashlabs/thus-spoke-zakura/pull/171),
  [#289](https://github.com/zcashlabs/thus-spoke-zakura/pull/289),
  [#382](https://github.com/zcashlabs/thus-spoke-zakura/pull/382),
  [#385](https://github.com/zcashlabs/thus-spoke-zakura/pull/385),
  [#306](https://github.com/zcashlabs/thus-spoke-zakura/pull/306),
  [#321](https://github.com/zcashlabs/thus-spoke-zakura/pull/321),
  [#337](https://github.com/zcashlabs/thus-spoke-zakura/pull/337)
- **Mining controls:** The global Mine action remains available on mobile.
  Block counts accept decimal digits only, within the existing 1–10,000 range;
  exponent and hexadecimal forms are rejected.
  [#314](https://github.com/zcashlabs/thus-spoke-zakura/pull/314),
  [#302](https://github.com/zcashlabs/thus-spoke-zakura/pull/302)
- **Explorer accuracy:** Explorer search gives useful feedback for hashes and
  transparent addresses. Transaction views keep pool values separate and show
  a fee only when complete values make it verifiable. Older-block pages label
  the first block actually shown rather than implying an exclusive cursor.
  [#129](https://github.com/zcashlabs/thus-spoke-zakura/pull/129),
  [#383](https://github.com/zcashlabs/thus-spoke-zakura/pull/383),
  [#336](https://github.com/zcashlabs/thus-spoke-zakura/pull/336)

### Project updates

- **Documentation:** The project now includes a GitBook covering setup,
  accounts, payments, mining, operations, application integration, and
  troubleshooting. Dashboard guides clarify external Regtest destinations and
  payment-request editing, and developer source maps point to the current
  server modules.
  [#122](https://github.com/zcashlabs/thus-spoke-zakura/pull/122),
  [#390](https://github.com/zcashlabs/thus-spoke-zakura/pull/390),
  [#391](https://github.com/zcashlabs/thus-spoke-zakura/pull/391)
- **Maintainability:** The server, wallet, launcher, and dashboard were split
  into focused application modules with shared lifecycle and validation code.
  [#178](https://github.com/zcashlabs/thus-spoke-zakura/pull/178),
  [#183](https://github.com/zcashlabs/thus-spoke-zakura/pull/183),
  [#186](https://github.com/zcashlabs/thus-spoke-zakura/pull/186),
  [#190](https://github.com/zcashlabs/thus-spoke-zakura/pull/190),
  [#191](https://github.com/zcashlabs/thus-spoke-zakura/pull/191),
  [#192](https://github.com/zcashlabs/thus-spoke-zakura/pull/192),
  [#238](https://github.com/zcashlabs/thus-spoke-zakura/pull/238)

### Credits and comparison

- **Contributors:** Thank you to @amiabix, @AnmolBansalDEV, @Annkkitaaa,
  @guha-rahul, @jonwalch, @kaushikc44, @MayurK-cmd, @megabyte0x,
  @nikhilkumar1612, @Not-Sarthak, @od-hunter, @paoValle, @piatoss3612,
  @PraneshASP, @raizo07, @RajeshRk18, @RealAdii, @tanctl, @vkpatva,
  and @yashgo0018.
- **Full changelog:** [Compare v0.3.0 with v0.4.0](https://github.com/zcashlabs/thus-spoke-zakura/compare/v0.3.0...v0.4.0).

## v0.3.0

![Thus Spoke Zakura v0.3.0 release artwork](docs/images/release-v0.3.0.png)

This release brings Ironwood to the local Zcash Regtest environment, adds wallet
transfers from `ths`, and improves mining progress and payment recovery. It also
changes API values and runtime resource names.

### Action required

- **Fresh Regtest instance:** The new launcher uses `ths-*` Docker resources
  instead of `tsz-*` and does not migrate an existing chain or wallet. Save
  anything you need before stopping the old instance: stopping deletes its
  chain, wallet, keys, and volumes. Once v0.3.0 is published, official v0.2.1
  users can stop their instance, run `ths update`, and start a fresh one.
  Source-build users need to rebuild or reinstall.
  [#125](https://github.com/zcashlabs/thus-spoke-zakura/pull/125)
- **API migration:** Use `ironwood` instead of `orchard` for pool values, and
  read `ironwood_zatoshi` instead of `orchard_zatoshi` from `/api/v1/accounts`.
  Requests specifying the old `orchard` pool are rejected.
  [#124](https://github.com/zcashlabs/thus-spoke-zakura/pull/124),
  [#126](https://github.com/zcashlabs/thus-spoke-zakura/pull/126)
- **Configuration migration:** Rename custom `TSZ_*` installer, updater,
  server, and development settings to `THS_*`. Browser settings and Docker
  resources using the old names are not migrated automatically.
  [#125](https://github.com/zcashlabs/thus-spoke-zakura/pull/125)
- **Endpoint changes:** The default dashboard, Zakura RPC, P2P, and lightwalletd
  host ports are now `32805`, `18232`, `18233`, and `9067`, bound to
  `127.0.0.1`. Use `ths start --port-offset 10` for another instance. Startup
  reports an occupied port instead of choosing a different one.
  [#72](https://github.com/zcashlabs/thus-spoke-zakura/pull/72)
- **Development addresses:** Fresh instances derive their accounts from a
  fixed, public mnemonic, so addresses are repeatable. Addresses from older,
  randomly seeded instances should not be assumed to match. Never send real
  funds to these Regtest accounts.
  [#103](https://github.com/zcashlabs/thus-spoke-zakura/pull/103),
  [#104](https://github.com/zcashlabs/thus-spoke-zakura/pull/104),
  [#120](https://github.com/zcashlabs/thus-spoke-zakura/pull/120)

### Highlights

- **Ironwood support:** New Regtest chains activate NU6.1, NU6.2, and NU6.3 at
  height 1. The environment uses Zakura 1.6.0 and the updated Zakura wallet
  stack. Shielded wallet operations use Ironwood, and the explorer displays
  Ironwood actions and roots.
  [#124](https://github.com/zcashlabs/thus-spoke-zakura/pull/124),
  [#126](https://github.com/zcashlabs/thus-spoke-zakura/pull/126)
- **Wallet commands:** `ths wallet faucet`, `send`, `shield`, and `unshield`
  operate on development accounts 1–5. Sends to Ironwood can include an
  encrypted memo of up to 512 bytes. A transfer may use the same source and
  destination account when the pools differ; sending back to the same account
  and pool is rejected.
  [#81](https://github.com/zcashlabs/thus-spoke-zakura/pull/81),
  [#138](https://github.com/zcashlabs/thus-spoke-zakura/pull/138)
- **Fee-aware sending:** The dashboard quotes the fee before sending and offers
  a **Max** button that leaves room for it. An unaffordable send returns HTTP
  422 with the required and available amounts.
  [#97](https://github.com/zcashlabs/thus-spoke-zakura/pull/97)
- **Mining progress across tabs:** Mining runs as a server-owned job, allowing
  the dashboard to restore progress after navigation, refresh, or closing and
  reopening the tab. `ths mine` still waits for completion. An admitted job
  continues if the CLI client times out; check its status before requesting
  another. Job history lasts for the server process and is discarded when it
  stops. [#139](https://github.com/zcashlabs/thus-spoke-zakura/pull/139)
- **Viewing key export:** `/api/v1/accounts` includes unified full viewing keys
  for user accounts 1–5. It does not expose the treasury account or spending
  keys. [#80](https://github.com/zcashlabs/thus-spoke-zakura/pull/80)

### Reliability and performance

- **Payment retries:** The server reserves idempotency keys before payment side
  effects and recovers prepared transactions across retries and restarts.
  Reusing a key with different payment parameters is rejected.
  [#71](https://github.com/zcashlabs/thus-spoke-zakura/pull/71)
- **Activity confirmation:** Payments are marked confirmed from chain evidence.
  Pending activity is reconciled after synchronization, startup, mining trouble,
  and retries. [#69](https://github.com/zcashlabs/thus-spoke-zakura/pull/69)
- **Scalable treasury synchronization:** The treasury is also the mining
  address, so every mined block adds another transparent output. Previously,
  routine wallet sync repeatedly loaded the treasury's entire unspent output
  set, making sync more expensive as the chain grew. Routine sync now refreshes
  transparent outputs for user accounts 1–5; when the faucet needs funds, it
  discovers treasury rewards block by block using a saved cursor. Reward
  imports and cursor updates commit together and rewind on a chain
  reorganization. Shielded scanning still covers all accounts, and the mining
  and faucet APIs are unchanged.
  [#114](https://github.com/zcashlabs/thus-spoke-zakura/pull/114)
- **Service timeouts:** Ordinary Zakura RPC calls and lightwalletd broadcasts
  have 30-second timeouts. Block generation has a separate, longer limit.
  [#77](https://github.com/zcashlabs/thus-spoke-zakura/pull/77),
  [#100](https://github.com/zcashlabs/thus-spoke-zakura/pull/100)
- **Explorer refresh and errors:** Open block, transaction, and address views
  refresh as the chain changes. Missing blocks and transactions return HTTP
  404; invalid Regtest transparent addresses return HTTP 400.
  [#107](https://github.com/zcashlabs/thus-spoke-zakura/pull/107),
  [#117](https://github.com/zcashlabs/thus-spoke-zakura/pull/117),
  [#88](https://github.com/zcashlabs/thus-spoke-zakura/pull/88)
- **Explorer layout:** Wide tables scroll within the explorer, and row
  animations no longer cause scrollbar flicker.
  [#70](https://github.com/zcashlabs/thus-spoke-zakura/pull/70)
- **Instance status:** Counting accounts no longer requires deriving wallet
  keys, and a stopped instance is no longer reported as ready.
  [#102](https://github.com/zcashlabs/thus-spoke-zakura/pull/102),
  [#108](https://github.com/zcashlabs/thus-spoke-zakura/pull/108)

### Project updates

- **Developer documentation:** The repository adds a detailed CLI reference,
  contributor guidance, and issue and pull request templates.
  [#81](https://github.com/zcashlabs/thus-spoke-zakura/pull/81),
  [#82](https://github.com/zcashlabs/thus-spoke-zakura/pull/82),
  [#95](https://github.com/zcashlabs/thus-spoke-zakura/pull/95)
- **License metadata:** The project and package metadata now identify MIT as
  the license. [#87](https://github.com/zcashlabs/thus-spoke-zakura/pull/87)

### Credits and comparison

- **Contributors:** Thank you to @AnmolBansalDEV, @Not-Sarthak, @PraneshASP,
  @USCMig, @amiabix, @bajpai244, @guha-rahul, @megabyte0x, @naz3eh,
  @0xpierre-dev, @piatoss3612, @tanctl, and @vkpatva.
- **Full changelog:** [Compare v0.2.1 with v0.3.0](https://github.com/zcashlabs/thus-spoke-zakura/compare/v0.2.1...v0.3.0).
