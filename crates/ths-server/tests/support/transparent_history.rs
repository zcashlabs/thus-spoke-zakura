use transparent::{
    address::Script,
    bundle::{Authorized as TransparentAuthorized, Bundle, OutPoint, TxIn, TxOut},
};
use zcash_client_backend::{
    data_api::{
        chain::ChainState,
        scanning::{ScanPriority, ScanRange},
    },
    proto::{compact_formats::CompactBlock, service::RawTransaction},
};
use zcash_keys::address::Address;
use zcash_primitives::{
    block::BlockHash,
    transaction::{Authorized, TransactionData, TxVersion},
};
use zcash_protocol::{consensus::BranchId, value::Zatoshis};
use zcash_script::{script, script::Evaluable};

use crate::{db::Store, network::regtest_network, wallet::RealWallet};

pub async fn setup(height: u32) -> (tempfile::TempDir, Store, RealWallet) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("server.db")).unwrap();
    store.initialize().unwrap();
    let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
    wallet.update_chain_tip(u64::from(height)).await.unwrap();
    wallet
        .scan_batch(
            ScanRange::from_parts(2.into()..(height + 1).into(), ScanPriority::Historic),
            (2..=height)
                .map(|h| CompactBlock {
                    height: u64::from(h),
                    hash: vec![h as u8; 32],
                    prev_hash: vec![(h - 1) as u8; 32],
                    chain_metadata: Some(Default::default()),
                    ..Default::default()
                })
                .collect(),
            ChainState::empty(1.into(), BlockHash([1; 32])),
        )
        .await
        .unwrap();
    (dir, store, wallet)
}

// Transparent-only transactions deliberately produce no shielded wallet note.
// The controlled source supplies mined evidence; these aren't consensus proofs.
pub fn transaction(receiver: &str, prevout: OutPoint, tag: u32, height: u32) -> RawTransaction {
    let Address::Transparent(address) = Address::decode(&regtest_network(), receiver).unwrap()
    else {
        panic!("transparent receiver required");
    };
    let tx = TransactionData::<Authorized>::from_parts(
        TxVersion::V5,
        BranchId::for_height(&regtest_network(), height.into()),
        tag,
        200.into(),
        Some(Bundle {
            vin: vec![TxIn::from_parts(
                prevout,
                Script(script::Code(vec![1, 2])),
                u32::MAX,
            )],
            vout: vec![TxOut::new(
                Zatoshis::const_from_u64(10_000_000),
                Script(script::Code(address.script().to_bytes())),
            )],
            authorization: TransparentAuthorized,
        }),
        None,
        None,
        None,
    )
    .freeze()
    .unwrap();
    let mut data = Vec::new();
    tx.write(&mut data).unwrap();
    RawTransaction {
        data,
        height: u64::from(height),
    }
}
