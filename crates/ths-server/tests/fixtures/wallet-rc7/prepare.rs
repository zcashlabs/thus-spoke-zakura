use anyhow::Result;
use ths_server::{
    db::{Recipient, Store},
    wallet::RealWallet,
};

#[tokio::main]
async fn main() -> Result<()> {
    let destination =
        std::path::PathBuf::from(std::env::args_os().nth(1).expect("capture directory"));
    let store = Store::open(destination.join("ths.db"))?;
    store.initialize()?;
    let wallet = RealWallet::open(&destination, &store.seed()?)?;
    let activity = store.claim_transfer(
        1,
        Recipient::Account(2),
        "ironwood",
        "transparent",
        1_000_000,
        "rc7-upgrade-payment",
        None,
    )?;
    let payment = wallet
        .prepare(
            Some(&activity.id),
            None,
            &store.seed()?,
            1,
            "ironwood",
            &store.account(2)?.transparent_address,
            1_000_000,
            None,
            Ok,
        )
        .await?;
    store.record_prepared(
        &activity.id,
        &payment.txid,
        &payment.raw_transaction,
        payment.expiry_height,
    )?;
    std::fs::write(
        destination.join("prepared.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"activity_id":activity.id,"txid":payment.txid,"expiry_height":payment.expiry_height,"raw_hex":hex::encode(payment.raw_transaction)}),
        )?,
    )?;
    Ok(())
}
