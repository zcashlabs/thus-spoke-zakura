use super::*;

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error(
        "insufficient spendable funds (have {}, need {} including fees)",
        format_zec(*available),
        format_zec(*required)
    )]
    InsufficientFunds { available: u64, required: u64 },
    #[error("faucet treasury remains insufficient after replenishment")]
    TreasuryExhausted,
    #[error("memos can only be sent to the ironwood pool; transparent outputs cannot carry a memo")]
    TransparentMemo,
    #[error("wallet requires reconciliation before constructing a payment")]
    ScanRequired,
}

/// Spendable balance, fee, and maximum sendable amount for emptying one pool.
#[derive(Serialize)]
pub struct SendQuote {
    pub available_zatoshi: u64,
    pub fee_zatoshi: u64,
    pub max_zatoshi: u64,
}

fn format_zec(zatoshi: u64) -> String {
    let whole = zatoshi / ZATOSHIS_PER_ZEC;
    let fraction = zatoshi % ZATOSHIS_PER_ZEC;
    if fraction == 0 {
        format!("{whole} ZEC")
    } else {
        format!(
            "{whole}.{} ZEC",
            format!("{fraction:08}").trim_end_matches('0')
        )
    }
}

/// Database capabilities the send proposal paths share; a single bound keeps
/// `propose_send` and `quote` from drifting apart.
trait ProposalDb:
    WalletWrite
    + InputSource<Error = <Self as WalletRead>::Error, NoteRef: Copy + Eq + Ord + std::fmt::Display>
    + WalletRead<
        AccountId = <Self as InputSource>::AccountId,
        Error: std::error::Error + Send + Sync + 'static,
    >
{
}

impl<T> ProposalDb for T
where
    T: WalletWrite
        + InputSource<Error = <T as WalletRead>::Error>
        + WalletRead<AccountId = <T as InputSource>::AccountId>,
    <T as InputSource>::NoteRef: Copy + Eq + Ord + std::fmt::Display,
    <T as WalletRead>::Error: std::error::Error + Send + Sync + 'static,
{
}

impl RealWallet {
    /// Builds the proposal a send would use, without signing or broadcasting.
    fn propose_send<DbT>(
        db: &mut DbT,
        params: &LocalNetwork,
        account_id: <DbT as InputSource>::AccountId,
        source_pool: &str,
        recipient: &Address,
        amount: Zatoshis,
        memo: Option<MemoBytes>,
    ) -> Result<Proposal<StandardFeeRule, <DbT as InputSource>::NoteRef>>
    where
        DbT: ProposalDb,
    {
        let proposal = if source_pool == "transparent" {
            let request = TransactionRequest::new(vec![Payment::new(
                recipient.to_zcash_address(params),
                Some(amount),
                memo,
                None,
                None,
                vec![],
            )?])?;
            let selector = GreedyInputSelector::<DbT>::new();
            let change = SingleOutputChangeStrategy::<DbT>::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Ironwood,
                DustOutputPolicy::default(),
            );
            let policy = SpendPolicy::shielded_pools([])
                .with_transparent(TransparentSpendPolicy::any_account_addr());
            propose_transfer::<_, _, _, _, Infallible>(
                db,
                params,
                account_id,
                &selector,
                &change,
                request,
                ConfirmationsPolicy::MIN,
                &policy,
                None,
                None,
            )
        } else if source_pool == "ironwood" {
            // Once NU6.3 is active the SDK builds a unified-address payment's
            // Orchard receiver as an Ironwood output, and routes change there too.
            propose_standard_transfer_to_address::<_, _, Infallible>(
                db,
                params,
                StandardFeeRule::Zip317,
                account_id,
                ConfirmationsPolicy::MIN,
                recipient,
                amount,
                memo,
                None,
                ShieldedPool::Ironwood,
                None,
                None,
            )
        } else {
            bail!("source pool must be transparent or ironwood")
        }
        .map_err(|error| match error {
            WalletError::InsufficientFunds {
                available,
                required,
            } => anyhow::Error::new(PaymentError::InsufficientFunds {
                available: u64::from(available),
                required: u64::from(required),
            }),
            WalletError::ScanRequired => anyhow::Error::new(PaymentError::ScanRequired),
            error => anyhow::anyhow!("proposing {source_pool} transaction: {error}"),
        })?;
        Ok(proposal)
    }

    fn send_input(
        account_ids: &[AccountUuid],
        from_account: u8,
        destination: &str,
    ) -> Result<(u8, AccountUuid, LocalNetwork, Address)> {
        let account_index = from_account
            .checked_sub(1)
            .context("invalid source account")?;
        let account_id = *account_ids
            .get(account_index as usize)
            .context("source account does not exist")?;
        let params = regtest_network();
        let recipient =
            Address::decode(&params, destination).context("invalid destination address")?;
        Ok((account_index, account_id, params, recipient))
    }

    pub(super) fn serialize_transaction(
        txid: TxId,
        transaction: &Transaction,
    ) -> Result<PreparedPayment> {
        let mut raw_transaction = vec![];
        transaction.write(&mut raw_transaction)?;
        Ok(PreparedPayment {
            txid: txid.to_string(),
            raw_transaction,
            expiry_height: u64::from(u32::from(transaction.expiry_height())),
        })
    }

    fn build_and_serialize<DbT, N>(
        db: &mut DbT,
        params: &LocalNetwork,
        usk: UnifiedSpendingKey,
        proposal: &Proposal<StandardFeeRule, N>,
        build_label: &str,
        missing_transaction: &'static str,
    ) -> Result<PreparedPayment>
    where
        DbT: WalletWrite + WalletCommitmentTrees,
        <DbT as WalletRead>::Error: std::error::Error + Send + Sync + 'static,
        <DbT as WalletCommitmentTrees>::Error: std::fmt::Display,
        N: std::fmt::Display,
    {
        let prover = LocalTxProver::bundled();
        let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
            db,
            params,
            &prover,
            &prover,
            &SpendingKeys::from_unified_spending_key(usk),
            OvkPolicy::Sender,
            proposal,
            None,
        )
        .map_err(|error| anyhow::anyhow!("building {build_label}: {error}"))?;
        let txid = *txids.first();
        let transaction = db.get_transaction(txid)?.context(missing_transaction)?;
        Self::serialize_transaction(txid, &transaction)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare<T, F>(
        &self,
        activity_id: Option<&str>,
        replacement_txid: Option<&str>,
        seed_hex: &str,
        from_account: u8,
        source_pool: &str,
        destination: &str,
        amount: u64,
        memo: Option<MemoBytes>,
        record: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(PreparedPayment) -> Result<T> + Send + 'static,
    {
        let activity_id = activity_id.map(str::to_owned);
        let replacement_txid = replacement_txid.map(str::to_owned);
        let seed_hex = seed_hex.to_owned();
        let source_pool = source_pool.to_owned();
        let destination = destination.to_owned();
        let account_ids = self.account_ids.clone();
        self.with_db(move |db| {
            let prepared = Self::prepare_in(
                db,
                &account_ids,
                activity_id.as_deref(),
                replacement_txid.as_deref(),
                &seed_hex,
                from_account,
                &source_pool,
                &destination,
                amount,
                memo,
            )?;
            record(prepared)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_in(
        db: &mut Db,
        account_ids: &[AccountUuid],
        activity_id: Option<&str>,
        replacement_txid: Option<&str>,
        seed_hex: &str,
        from_account: u8,
        source_pool: &str,
        destination: &str,
        amount: u64,
        memo: Option<MemoBytes>,
    ) -> Result<PreparedPayment> {
        let journal = activity_id
            .map(|id| prepared_payment(db, id))
            .transpose()?
            .flatten();
        if let Some(prepared) = journal {
            if replacement_txid != Some(prepared.txid.as_str()) {
                return Ok(prepared);
            }
        } else {
            anyhow::ensure!(
                replacement_txid.is_none(),
                "replacement authorization has no journal"
            );
        }
        let (account_index, account_id, params, recipient) =
            Self::send_input(account_ids, from_account, destination)?;
        if memo.is_some() && matches!(recipient, Address::Transparent(_) | Address::Tex(_)) {
            return Err(PaymentError::TransparentMemo.into());
        }
        let amount = Zatoshis::from_u64(amount).map_err(|_| anyhow::anyhow!("invalid amount"))?;
        let proposal = Self::propose_send(
            &mut *db,
            &params,
            account_id,
            source_pool,
            &recipient,
            amount,
            memo,
        )?;
        let seed = hex::decode(seed_hex)?;
        let usk = UnifiedSpendingKey::from_seed(
            &params,
            &seed,
            zip32::AccountId::try_from(u32::from(account_index))
                .map_err(|_| anyhow::anyhow!("invalid account index"))?,
        )
        .map_err(|e| anyhow::anyhow!("deriving spending key: {e:?}"))?;
        db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
            let prepared = Self::build_and_serialize(
                wallet,
                &params,
                usk,
                &proposal,
                "transaction",
                "built transaction was not stored",
            )?;
            if let Some(activity_id) = activity_id {
                ext.execute(
                    "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height)
                     VALUES(?1,?2,?3,?4)
                     ON CONFLICT(activity_id) DO UPDATE SET
                       txid=excluded.txid,
                       raw_transaction=excluded.raw_transaction,
                       expiry_height=excluded.expiry_height",
                    rusqlite::params![
                        activity_id,
                        prepared.txid,
                        prepared.raw_transaction,
                        prepared.expiry_height
                    ],
                )?;
            }
            Ok(prepared)
        })
    }

    /// Returns the exact fee and maximum spendable amount for emptying a pool.
    pub async fn send_quote(
        &self,
        from_account: u8,
        source_pool: &str,
        destination: &str,
    ) -> Result<SendQuote> {
        let (_, account_id, params, recipient) =
            Self::send_input(&self.account_ids, from_account, destination)?;
        let source_pool = source_pool.to_owned();
        self.with_db(move |db| Self::quote(db, &params, account_id, &source_pool, &recipient))
            .await
    }

    fn quote<DbT>(
        db: &mut DbT,
        params: &LocalNetwork,
        account_id: <DbT as InputSource>::AccountId,
        source_pool: &str,
        recipient: &Address,
    ) -> Result<SendQuote>
    where
        DbT: ProposalDb,
    {
        if source_pool == "ironwood" {
            match propose_send_max_transfer::<_, _, _, Infallible>(
                db,
                params,
                account_id,
                &[ShieldedPool::Ironwood],
                &StandardFeeRule::Zip317,
                recipient.to_zcash_address(params),
                None,
                MaxSpendMode::MaxSpendable,
                ConfirmationsPolicy::MIN,
                &LockedInputPolicy::Exclude,
                None,
            ) {
                Ok(proposal) => {
                    let fee = proposal
                        .steps()
                        .iter()
                        .try_fold(0u64, |total, step| {
                            total.checked_add(u64::from(step.balance().fee_required()))
                        })
                        .context("transaction fee exceeds the maximum money supply")?;
                    let max = u64::from(
                        proposal
                            .steps()
                            .first()
                            .transaction_request()
                            .payments()
                            .values()
                            .next()
                            .and_then(|payment| payment.amount())
                            .unwrap_or(Zatoshis::ZERO),
                    );
                    Ok(SendQuote {
                        available_zatoshi: max + fee,
                        fee_zatoshi: fee,
                        max_zatoshi: max,
                    })
                }
                Err(WalletError::InsufficientFunds {
                    available,
                    required,
                }) => Ok(SendQuote {
                    available_zatoshi: u64::from(available),
                    fee_zatoshi: u64::from(required).saturating_sub(1),
                    max_zatoshi: 0,
                }),
                Err(WalletError::ScanRequired) => {
                    Err(anyhow::Error::new(PaymentError::ScanRequired))
                }
                Err(error) => Err(anyhow::anyhow!("proposing max spend: {error}")),
            }
        } else if source_pool == "transparent" {
            let (target_height, _) = db
                .get_target_and_anchor_heights(ConfirmationsPolicy::MIN.trusted())?
                .ok_or_else(|| anyhow::Error::new(PaymentError::ScanRequired))?;
            let spendable = db
                .select_spendable_transparent_outputs(
                    account_id,
                    target_height,
                    ConfirmationsPolicy::MIN,
                    CoinbaseFilter::NonCoinbaseOnly,
                    None,
                    TargetValue::AllFunds(MaxSpendMode::MaxSpendable),
                    usize::MAX,
                    &StandardFeeRule::Zip317,
                    LockFilter::Policy(&LockedInputPolicy::Exclude),
                )?
                .into_iter()
                .try_fold(0u64, |total, utxo| {
                    total.checked_add(u64::from(utxo.value()))
                })
                .context("transparent balance exceeds the maximum money supply")?;
            // There is no send-max proposal for transparent inputs, but the
            // failed proposal still reports the fee for spending them all.
            let probe_amount = spendable.max(1);
            let probe = Self::propose_send(
                db,
                params,
                account_id,
                source_pool,
                recipient,
                Zatoshis::from_u64(probe_amount).map_err(|_| anyhow::anyhow!("invalid amount"))?,
                None,
            );
            match probe {
                Err(error) => match error.downcast_ref() {
                    Some(PaymentError::InsufficientFunds { required, .. }) => {
                        let fee = required.saturating_sub(probe_amount);
                        Ok(SendQuote {
                            available_zatoshi: spendable,
                            fee_zatoshi: fee,
                            max_zatoshi: spendable.saturating_sub(fee),
                        })
                    }
                    _ => Err(error),
                },
                Ok(_) => bail!("a send of the entire balance proposed cleanly"),
            }
        } else {
            bail!("source pool must be transparent or ironwood")
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/wallet/payments/tests.rs"]
mod tests;

impl RealWallet {
    pub(crate) async fn prepare_shield_coinbase(
        &self,
        seed_hex: &str,
        treasury_account: u8,
        from: &str,
        to: &str,
        minimum_net: u64,
        replacement_txid: Option<&str>,
    ) -> Result<PreparedPayment> {
        if treasury_account != crate::db::TREASURY_ACCOUNT_ID {
            bail!("coinbase shielding requires the treasury account");
        }
        let replacement_txid = replacement_txid.map(str::to_owned);
        let seed_hex = seed_hex.to_owned();
        let from = from.to_owned();
        let to = to.to_owned();
        let prepared = self
            .with_db(move |db| {
                if let Some(prepared) = prepared_payment(db, TREASURY_SHIELDING_JOURNAL_ID)? {
                    if replacement_txid.as_deref() != Some(prepared.txid.as_str()) {
                        return Ok(prepared);
                    }
                } else {
                    anyhow::ensure!(
                        replacement_txid.is_none(),
                        "shielding replacement authorization has no journal"
                    );
                }
                let params = regtest_network();
                let from =
                    match Address::decode(&params, &from).context("invalid treasury address")? {
                        Address::Transparent(address) => address,
                        _ => bail!("treasury address is not transparent"),
                    };
                let to = Address::decode(&params, &to)
                    .context("invalid shielding address")?
                    .to_zcash_address(&params);
                let threshold = Zatoshis::from_u64(minimum_net)
                    .map_err(|_| anyhow::anyhow!("invalid shielding threshold"))?;
                // Existing wallets may already hold thousands of rewards. Grow the SDK's
                // input cap only until its fee-aware proposal covers this payment.
                let mut limit = Some(1);
                let proposal = loop {
                    match propose_shielding_coinbase::<_, _, _, _, Infallible>(
                        &mut *db,
                        &params,
                        &GreedyInputSelector::new(),
                        &StandardFeeRule::Zip317,
                        threshold,
                        &[from],
                        to.clone(),
                        None,
                        limit,
                        None,
                    ) {
                        Ok(proposal) => break proposal,
                        Err(WalletError::InsufficientFunds { .. }) if limit.is_some() => {
                            limit = limit.filter(|value| *value < 2048).map(|value| value * 2);
                        }
                        Err(WalletError::InsufficientFunds { .. } | WalletError::ScanRequired) => {
                            return Err(anyhow::Error::new(PaymentError::TreasuryExhausted));
                        }
                        Err(error) => {
                            return Err(anyhow::anyhow!("proposing coinbase shielding: {error}"));
                        }
                    }
                };
                let seed = hex::decode(seed_hex)?;
                let account_index = treasury_account
                    .checked_sub(1)
                    .context("invalid treasury account")?;
                let usk = UnifiedSpendingKey::from_seed(
                    &params,
                    &seed,
                    zip32::AccountId::try_from(u32::from(account_index))
                        .map_err(|_| anyhow::anyhow!("invalid treasury account"))?,
                )
                .map_err(|e| anyhow::anyhow!("deriving treasury key: {e:?}"))?;
                db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                    let prepared = Self::build_and_serialize(
                        wallet,
                        &params,
                        usk,
                        &proposal,
                        "shielding transaction",
                        "shielding transaction was not stored",
                    )?;
                    ext.execute(
                        "INSERT INTO ext_tsz_prepared_payments(activity_id,txid,raw_transaction,expiry_height)
                         VALUES(?1,?2,?3,?4)
                         ON CONFLICT(activity_id) DO UPDATE SET
                           txid=excluded.txid,
                           raw_transaction=excluded.raw_transaction,
                           expiry_height=excluded.expiry_height",
                        rusqlite::params![
                            TREASURY_SHIELDING_JOURNAL_ID,
                            prepared.txid,
                            prepared.raw_transaction,
                            prepared.expiry_height
                        ],
                    )?;
                    Ok(prepared)
                })
            })
            .await?;
        Ok(prepared)
    }
}
