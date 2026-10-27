use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use bip39::Mnemonic;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use uuid::Uuid;
use zcash_keys::{
    address::Address,
    keys::{Era, UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedSpendingKey},
};

use crate::network::regtest_network;

mod accounts;
mod activity;
mod address_faucet;
mod schema;

pub use accounts::{Account, DevelopmentAccountSecret, DevelopmentSecrets};
pub(crate) use activity::ActivityRecovery;
pub use activity::{Activity, ActivityIntent, PreparedTransaction, Recipient};
pub use address_faucet::AddressFaucet;

pub const ZATOSHIS_PER_ZEC: u64 = 100_000_000;
pub const USER_ACCOUNT_COUNT: u8 = 5;
pub const TREASURY_ACCOUNT_ID: u8 = 6;
#[derive(Debug, thiserror::Error)]
#[error("idempotency key was already used for a different payment")]
pub struct IdempotencyConflict;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreasuryCursor {
    pub receiver: String,
    pub height: u32,
    pub block_hash: String,
}

#[derive(Clone)]
pub struct Store(Arc<Mutex<Connection>>);

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }
}
