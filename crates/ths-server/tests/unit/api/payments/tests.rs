use super::*;
use crate::api::{error::ApiResult, router};
use crate::state::tests::counting_rpc;
use crate::state::tests::{
    started_offline_state as offline_state,
    started_state_with_local_wallet as state_with_local_wallet,
};
use crate::{
    db::{Activity, Store, TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT},
    state::AppState,
    wallet::RealWallet,
};
use axum::body::Body;
use axum::response::IntoResponse;
use axum::{
    Json,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tower::ServiceExt;
use zcash_protocol::value::MAX_MONEY;
#[test]
fn address_faucet_resolves_only_published_user_addresses() {
    let (state, _dir) = state_with_local_wallet();
    for id in 1..=USER_ACCOUNT_COUNT {
        let account = state.0.store.account(id).unwrap();
        assert_eq!(
            internal_faucet_destination(&state.0.store, &account.transparent_address).unwrap(),
            Some((id, "transparent"))
        );
        assert_eq!(
            internal_faucet_destination(&state.0.store, &account.unified_address).unwrap(),
            Some((id, "ironwood"))
        );
        assert_eq!(
            internal_faucet_destination(
                &state.0.store,
                &format!("{} ", account.transparent_address)
            )
            .unwrap(),
            None
        );
    }
    let treasury = state.0.store.account(TREASURY_ACCOUNT_ID).unwrap();
    for address in [
        treasury.transparent_address,
        treasury.unified_address,
        "unmatched".into(),
    ] {
        assert_eq!(
            internal_faucet_destination(&state.0.store, &address).unwrap(),
            None
        );
    }
}

#[test]
fn address_faucet_resolver_propagates_store_errors() {
    let store = Store::open(":memory:").unwrap();
    assert!(internal_faucet_destination(&store, "unmatched").is_err());
}

fn address_faucet_activity(store: &Store) -> Activity {
    let row = store
        .claim_faucet(2, "transparent", 100_000_000, "adapter-test")
        .unwrap();
    let row = store
        .record_prepared(&row.id, "faucet-txid", b"prepared bytes", 140)
        .unwrap();
    store.mark_broadcast(&row.id, &row.txid).unwrap()
}

#[test]
fn address_faucet_response_preserves_pending_and_confirmed_status() {
    let (state, _dir) = state_with_local_wallet();
    let address = state.0.store.account(2).unwrap().transparent_address;
    let row = address_faucet_activity(&state.0.store);
    let pending = faucet_address_response(address.clone(), row.clone()).unwrap();
    assert_eq!(pending.status, "broadcast");
    assert_eq!(pending.block_hash, None);
    let row = state
        .0
        .store
        .confirm(&row.id, &row.txid, "inclusion-hash")
        .unwrap();
    let value =
        serde_json::to_value(faucet_address_response(address.clone(), row.clone()).unwrap())
            .unwrap();
    assert_eq!(
        value,
        json!({"address": address, "amount_zatoshi": 100_000_000, "txid": "faucet-txid", "block_hash": "inclusion-hash", "status": "confirmed", "last_error": null})
    );
    for block_hash in [None, Some(String::new())] {
        let mut invalid = row.clone();
        invalid.block_hash = block_hash;
        assert!(faucet_address_response(address.clone(), invalid).is_err());
    }
}

struct RecordingAddressFaucetRuntime {
    internal_calls: std::sync::Mutex<Vec<(u8, String, u64, String)>>,
    external_calls: std::sync::Mutex<Vec<(String, u64)>>,
    activity: Activity,
    fail_internal: bool,
    fail_external: bool,
}

impl RecordingAddressFaucetRuntime {
    fn new(store: &Store) -> Self {
        let row = address_faucet_activity(store);
        Self {
            internal_calls: Default::default(),
            external_calls: Default::default(),
            activity: store.confirm(&row.id, &row.txid, "inclusion-hash").unwrap(),
            fail_internal: false,
            fail_external: false,
        }
    }
}

#[async_trait::async_trait]
impl AddressFaucetRuntime for RecordingAddressFaucetRuntime {
    async fn fund_internal(
        &self,
        id: u8,
        pool: &str,
        amount: u64,
        key: &str,
    ) -> anyhow::Result<Activity> {
        self.internal_calls
            .lock()
            .unwrap()
            .push((id, pool.into(), amount, key.into()));
        anyhow::ensure!(!self.fail_internal, "internal funding failed");
        Ok(self.activity.clone())
    }

    async fn fund_external(
        &self,
        req: FaucetAddressRequest,
    ) -> anyhow::Result<FaucetAddressResponse> {
        self.external_calls
            .lock()
            .unwrap()
            .push((req.address.clone(), req.amount_zatoshi));
        anyhow::ensure!(!self.fail_external, "external funding failed");
        Ok(FaucetAddressResponse {
            address: req.address,
            amount_zatoshi: req.amount_zatoshi,
            txid: "external-txid".into(),
            block_hash: Some("external-block".into()),
            status: "confirmed".into(),
            last_error: None,
        })
    }
}

#[tokio::test]
async fn address_faucet_validation_precedes_dispatch() {
    let (state, _dir) = state_with_local_wallet();
    let runtime = RecordingAddressFaucetRuntime::new(&state.0.store);
    let address = state.0.store.account(2).unwrap().transparent_address;
    for (address, amount) in [
        ("not-an-address".into(), 1),
        (address.clone(), 0),
        (address.clone(), 500_000_001),
    ] {
        let error = execute_address_faucet(
            &state.0.store,
            &runtime,
            FaucetAddressRequest {
                idempotency_key: "address-test-key".into(),
                address,
                amount_zatoshi: amount,
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }
    assert!(runtime.internal_calls.lock().unwrap().is_empty());
    assert!(runtime.external_calls.lock().unwrap().is_empty());
    let Json(_response) = execute_address_faucet(
        &state.0.store,
        &runtime,
        FaucetAddressRequest {
            idempotency_key: "address-test-key".into(),
            address,
            amount_zatoshi: 500_000_000,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("{}", error.message));
    assert_eq!(runtime.internal_calls.lock().unwrap()[0].2, 500_000_000);
}

#[test]
fn reserves_the_treasury_account_from_public_operations() {
    for id in 1..=USER_ACCOUNT_COUNT {
        assert!(require_user_account(id).is_ok());
    }
    assert!(require_user_account(TREASURY_ACCOUNT_ID).is_err());
}

fn is_bad_request<T>(result: ApiResult<T>) -> bool {
    matches!(result, Err(error) if error.status == StatusCode::BAD_REQUEST)
}

#[test]
fn rejects_orchard_as_a_pool_after_nu6_3() {
    assert!(require_pool("ironwood", "pool").is_ok());
    assert!(require_pool("transparent", "pool").is_ok());
    let Err(error) = require_pool("orchard", "destination_pool") else {
        panic!("orchard must be rejected");
    };
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("use ironwood"));
    assert!(is_bad_request(require_pool("sapling", "pool")));
}

#[test]
fn absent_and_null_memos_differ_from_an_explicit_empty_memo() {
    let request = |memo: Option<Value>| {
        let mut body = json!({
            "from_account": 1, "to_account": 2, "source_pool": "ironwood",
            "destination_pool": "ironwood", "amount_zatoshi": 1,
            "idempotency_key": "memo-test-key",
        });
        if let Some(memo) = memo {
            body["memo"] = memo;
        }
        serde_json::from_value::<SendRequest>(body).expect("valid request shape")
    };
    assert_eq!(request(None).memo, None);
    assert_eq!(request(Some(Value::Null)).memo, None);
    assert_eq!(request(Some(json!(""))).memo.as_deref(), Some(""));

    assert!(matches!(parse_memo(None, "ironwood"), Ok(None)));
    assert!(matches!(parse_memo(None, "transparent"), Ok(None)));
    let Ok(Some(empty)) = parse_memo(Some(""), "ironwood") else {
        panic!("an explicit empty memo must be kept");
    };
    assert_eq!(empty.as_array(), &[0u8; 512]);
}

#[test]
fn memos_are_bounded_by_utf8_bytes() {
    let Ok(Some(memo)) = parse_memo(Some("thanks for lunch"), "ironwood") else {
        panic!("expected an encoded memo");
    };
    assert_eq!(&memo.as_slice()[..16], b"thanks for lunch");
    assert!(memo.as_slice()[16..].iter().all(|byte| *byte == 0));

    assert!(parse_memo(Some(&"a".repeat(512)), "ironwood").is_ok());
    assert!(is_bad_request(parse_memo(
        Some(&"a".repeat(513)),
        "ironwood"
    )));

    // Three bytes per character: 170 fit (510 bytes), 171 do not (513).
    let Ok(Some(cjk)) = parse_memo(Some(&"桜".repeat(170)), "ironwood") else {
        panic!("510 bytes of multibyte text must fit");
    };
    assert_eq!(&cjk.as_slice()[..510], "桜".repeat(170).as_bytes());
    assert!(is_bad_request(parse_memo(
        Some(&"桜".repeat(171)),
        "ironwood"
    )));

    // Four bytes per character: exactly 512 fits, one more does not.
    assert!(parse_memo(Some(&"🌸".repeat(128)), "ironwood").is_ok());
    assert!(is_bad_request(parse_memo(
        Some(&"🌸".repeat(129)),
        "ironwood"
    )));
}

#[test]
fn memos_must_not_end_with_nul() {
    assert!(is_bad_request(parse_memo(Some("hi\0"), "ironwood")));
    assert!(is_bad_request(parse_memo(Some("\0"), "ironwood")));
    assert!(parse_memo(Some("a\0b"), "ironwood").is_ok());
}

const REPLAY_KEY: &str = "existing-idempotency-key";

#[tokio::test]
async fn payment_admission_rejects_transparent_dust_before_claiming() {
    for source in ["ironwood", "transparent"] {
        for destination in ["transparent", "ironwood"] {
            for amount in [10, 53, 54] {
                let (state, _dir) = offline_state();
                let account = state.0.store.account(2).unwrap();
                let treasury = state.0.store.account(TREASURY_ACCOUNT_ID).unwrap();
                let address = |account: &crate::db::Account| {
                    if destination == "transparent" {
                        account.transparent_address.clone()
                    } else {
                        account.unified_address.clone()
                    }
                };
                let cases = [
                    (
                        "/api/v1/send",
                        json!({"from_account": 1, "to_account": 2, "source_pool": source,
                            "destination_pool": destination, "amount_zatoshi": amount,
                            "idempotency_key": "dust-send-account"}),
                    ),
                    (
                        "/api/v1/send",
                        json!({"from_account": 1, "to_address": address(&treasury),
                            "source_pool": source, "destination_pool": destination,
                            "amount_zatoshi": amount, "idempotency_key": "dust-send-address"}),
                    ),
                    (
                        "/api/v1/faucet",
                        json!({"account_id": 2, "pool": destination, "amount_zatoshi": amount,
                            "idempotency_key": "dust-account-faucet"}),
                    ),
                    (
                        "/api/v1/faucet/address",
                        json!({"address": address(&account), "amount_zatoshi": amount,
                            "idempotency_key": "dust-internal-faucet"}),
                    ),
                    (
                        "/api/v1/faucet/address",
                        json!({"address": address(&treasury), "amount_zatoshi": amount,
                            "idempotency_key": "dust-external-faucet"}),
                    ),
                ];
                for (uri, body) in cases {
                    let key = body["idempotency_key"].as_str().unwrap();
                    let (status, response) = api_request(&state, "POST", uri, body.clone()).await;
                    let rejected = destination == "transparent" && amount < 54;
                    assert_eq!(
                        status,
                        if rejected {
                            StatusCode::BAD_REQUEST
                        } else {
                            StatusCode::ACCEPTED
                        },
                        "{uri}: {source} -> {destination}, {amount}: {response}"
                    );
                    let activity = state.0.store.activity_for_key(key).unwrap();
                    let external = state.0.store.address_faucet_for_key(key).unwrap();
                    if rejected {
                        assert_eq!(
                            response["error"]["message"],
                            "transparent outputs must be at least 54 zatoshi"
                        );
                        assert!(activity.is_none() && external.is_none());
                    } else {
                        assert_eq!(response["status"], "preparing");
                        assert_eq!(response["txid"], "");
                        assert!(activity.is_some() ^ external.is_some());
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn address_faucet_rejects_dust_for_unified_transparent_fallbacks() {
    // Regtest UAs containing unknown typecode 4 ([1; 32]) and P2PKH/P2SH ([2; 20]).
    let addresses = [
        "uregtest1zclnxv0zjpp8sljklmdlnfg8v0tlskqzvd53y7ywm4clm3zrdj5nasgynnwtyenxgvn9eunzz06qp27gxczny2jjswtjdq2vy6hfh5s4efzlxd0mmedqm5xj0z",
        "uregtest18g4234glzemc98nhmnz6lxvacy70fx6960rycsh0mm2mu6v8shfxzqqxlaq95d4l0c0fsrtj5v9r8c0cz3w034lw3z4pwlyvz6pkx5fgfx2vrwwufmhqydmu6z",
    ];
    for address in addresses {
        let (state, _dir) = offline_state();
        let Address::Unified(decoded) = require_faucet_address(address).unwrap() else {
            panic!("fixture must be a unified address");
        };
        assert!(decoded.has_transparent() && !decoded.has_orchard() && !decoded.has_sapling());
        for amount in [53, 54] {
            let body = json!({"address": address, "amount_zatoshi": amount,
                "idempotency_key": "dust-unified-fallback"});
            let (status, response) =
                api_request(&state, "POST", "/api/v1/faucet/address", body).await;
            let saved = state
                .0
                .store
                .address_faucet_for_key("dust-unified-fallback")
                .unwrap();
            if amount == 53 {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
                assert_eq!(
                    response["error"]["message"],
                    "transparent outputs must be at least 54 zatoshi"
                );
                assert!(saved.is_none());
            } else {
                assert_eq!(status, StatusCode::ACCEPTED, "{response}");
                assert_eq!(saved.unwrap().amount_zatoshi, 54);
            }
            assert!(state.0.store.activities(100).unwrap().is_empty());
        }
    }
}

/// A state with a real store and offline wallet whose node RPC refuses
/// connections, so any synchronization or network access surfaces as a
/// 500 rather than the 400 or replay these tests expect.
pub(crate) fn valid_send() -> Value {
    json!({
        "from_account": 1,
        "to_account": 2,
        "source_pool": "ironwood",
        "destination_pool": "ironwood",
        "amount_zatoshi": 100_000,
        "idempotency_key": REPLAY_KEY,
    })
}

async fn post_send(state: &AppState, body: &Value) -> (StatusCode, Value) {
    let response = router(state.clone())
        .oneshot(
            Request::post("/api/v1/send")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn activity_ids(state: &AppState) -> Vec<String> {
    state
        .0
        .store
        .activities(100)
        .unwrap()
        .into_iter()
        .map(|activity| activity.id)
        .collect()
}

#[test]
fn send_validation_allows_same_account_only_between_pools() {
    for to in [1, 2] {
        for source in ["ironwood", "transparent"] {
            for destination in ["ironwood", "transparent"] {
                let mut body = valid_send();
                body["to_account"] = json!(to);
                body["source_pool"] = json!(source);
                body["destination_pool"] = json!(destination);
                let req = serde_json::from_value::<SendRequest>(body).unwrap();
                let result = validate_send(&req);
                if to == 1 && source == destination {
                    let error = result.unwrap_err();
                    assert_eq!(error.status, StatusCode::BAD_REQUEST);
                    assert_eq!(
                        error.message,
                        "choose a different account or a different destination pool"
                    );
                } else {
                    assert!(
                        matches!(result, Ok((Recipient::Account(id), None)) if id == to),
                        "{to}: {source} -> {destination}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn invalid_sends_are_rejected_before_replay_or_synchronization() {
    let (state, _dir) = offline_state();
    let original = state
        .0
        .store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            100_000,
            REPLAY_KEY,
            None,
        )
        .unwrap();
    let before = activity_ids(&state);

    let with = |fields: &[(&str, Value)]| {
        let mut body = valid_send();
        for (field, value) in fields {
            body[*field] = value.clone();
        }
        body
    };
    let transparent = ("destination_pool", json!("transparent"));
    let own_address = state.0.store.account(1).unwrap().unified_address;
    let cases = [
        ("neither recipient", with(&[("to_account", Value::Null)])),
        (
            "both recipients",
            with(&[("to_address", json!(own_address))]),
        ),
        (
            "pasted same account and pool",
            with(&[
                ("to_account", Value::Null),
                ("to_address", json!(own_address)),
            ]),
        ),
        (
            "invalid address",
            with(&[
                ("to_account", Value::Null),
                ("to_address", json!("not-an-address")),
            ]),
        ),
        ("short key", with(&[("idempotency_key", json!("short"))])),
        (
            "key with spaces",
            with(&[("idempotency_key", json!("has spaces here"))]),
        ),
        ("account 0", with(&[("from_account", json!(0))])),
        (
            "treasury account",
            with(&[("to_account", json!(TREASURY_ACCOUNT_ID))]),
        ),
        ("same account and pool", with(&[("to_account", json!(1))])),
        (
            "same account and transparent pool",
            with(&[
                ("to_account", json!(1)),
                ("source_pool", json!("transparent")),
                ("destination_pool", json!("transparent")),
            ]),
        ),
        (
            "same account and unsupported source",
            with(&[("to_account", json!(1)), ("source_pool", json!("sapling"))]),
        ),
        (
            "same account and unsupported destination",
            with(&[
                ("to_account", json!(1)),
                ("destination_pool", json!("orchard")),
            ]),
        ),
        (
            "same account unshield with memo",
            with(&[
                ("to_account", json!(1)),
                ("destination_pool", json!("transparent")),
                ("memo", json!("hi")),
            ]),
        ),
        (
            "bad source pool",
            with(&[("source_pool", json!("sapling"))]),
        ),
        (
            "bad destination pool",
            with(&[("destination_pool", json!("sprout"))]),
        ),
        ("zero amount", with(&[("amount_zatoshi", json!(0))])),
        (
            "amount over MAX_MONEY",
            with(&[("amount_zatoshi", json!(MAX_MONEY + 1))]),
        ),
        ("memo too long", with(&[("memo", json!("a".repeat(513)))])),
        ("memo ending in NUL", with(&[("memo", json!("hi\u{0}"))])),
        (
            "memo to transparent",
            with(&[transparent.clone(), ("memo", json!("hi"))]),
        ),
        (
            "empty memo to transparent",
            with(&[transparent, ("memo", json!(""))]),
        ),
    ];
    for (name, body) in cases {
        let (status, response) = post_send(&state, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {response}");
        assert_eq!(activity_ids(&state), before, "{name} changed activity");
    }
    assert_eq!(
        state
            .0
            .store
            .activity_for_key(REPLAY_KEY)
            .unwrap()
            .unwrap()
            .id,
        original.id
    );
}

#[tokio::test]
async fn same_account_cross_pool_replays_preserve_activity_and_identity() {
    for (source, destination) in [("ironwood", "transparent"), ("transparent", "ironwood")] {
        let (state, _dir) = offline_state();
        let original = state
            .0
            .store
            .claim_transfer(
                1,
                crate::db::Recipient::Account(1),
                source,
                destination,
                100_000,
                REPLAY_KEY,
                None,
            )
            .unwrap();
        state
            .0
            .store
            .record_prepared(&original.id, "txid", b"signed transaction", 0)
            .unwrap();
        state.0.store.mark_broadcast(&original.id, "txid").unwrap();
        state
            .0
            .store
            .confirm(&original.id, "txid", &"c".repeat(64))
            .unwrap();
        let before = activity_ids(&state);
        let mut body = valid_send();
        body["to_account"] = json!(1);
        body["source_pool"] = json!(source);
        body["destination_pool"] = json!(destination);
        for _ in 0..2 {
            let (status, response) = post_send(&state, &body).await;
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(response["id"], original.id);
            assert_eq!(response["txid"], "txid");
            assert_eq!(response["from_account"], 1);
            assert_eq!(response["to_account"], 1);
            assert_eq!(response["source_pool"], source);
            assert_eq!(response["destination_pool"], destination);
        }
        let account = state.0.store.account(1).unwrap();
        let address = if destination == "transparent" {
            account.transparent_address
        } else {
            account.unified_address
        };
        let mut pasted = body.clone();
        pasted.as_object_mut().unwrap().remove("to_account");
        pasted["to_address"] = json!(address);
        let (status, response) = post_send(&state, &pasted).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["id"], original.id);
        assert_eq!(response["to_account"], 1);
        assert_eq!(response["to_address"], Value::Null);
        assert_eq!(response["txid"], "txid");
        for changed in [("amount_zatoshi", json!(100_001)), ("to_account", json!(2))] {
            let mut conflict = body.clone();
            conflict[changed.0] = changed.1;
            assert_eq!(post_send(&state, &conflict).await.0, StatusCode::CONFLICT);
        }
        let mut reverse = body.clone();
        reverse["source_pool"] = json!(destination);
        reverse["destination_pool"] = json!(source);
        assert_eq!(post_send(&state, &reverse).await.0, StatusCode::CONFLICT);
        assert_eq!(activity_ids(&state), before);
    }
}

#[tokio::test]
async fn a_valid_replay_returns_the_original_without_the_wallet_or_network() {
    let (state, _dir) = offline_state();
    let original = state
        .0
        .store
        .claim_transfer(
            1,
            Recipient::Account(2),
            "ironwood",
            "ironwood",
            100_000,
            REPLAY_KEY,
            None,
        )
        .unwrap();
    state
        .0
        .store
        .record_prepared(&original.id, "txid", b"signed transaction", 0)
        .unwrap();
    state.0.store.mark_broadcast(&original.id, "txid").unwrap();
    state
        .0
        .store
        .confirm(&original.id, "txid", &"c".repeat(64))
        .unwrap();
    let before = activity_ids(&state);

    for memo in [None, Some(Value::Null)] {
        let mut body = valid_send();
        if let Some(memo) = memo {
            body["memo"] = memo;
        }
        let (status, response) = post_send(&state, &body).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["id"], original.id);
        assert_eq!(response["txid"], "txid");
    }
    for memo in ["", "different memo"] {
        let mut body = valid_send();
        body["memo"] = json!(memo);
        let (status, _) = post_send(&state, &body).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
    assert_eq!(activity_ids(&state), before);

    let mut fresh = valid_send();
    fresh["idempotency_key"] = json!("a-key-never-used-before");
    let (status, _) = post_send(&state, &fresh).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let after = activity_ids(&state);
    assert_eq!(after.len(), before.len() + 1);
    assert!(before.iter().all(|id| after.contains(id)));
    let retained = state
        .0
        .store
        .activity_for_key("a-key-never-used-before")
        .unwrap()
        .unwrap();
    assert_eq!(retained.status, "preparing");
    assert_eq!(retained.last_error, None);
    assert!(state.0.store.prepared_transaction(&retained.id).is_err());
    assert!(
        state
            .0
            .wallet
            .prepared(&retained.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn address_faucet_rejects_missing_or_invalid_operation_keys_before_network_work() {
    let (state, _dir) = state_with_local_wallet();
    let address = state.0.store.account(1).unwrap().unified_address;
    for key in [None, Some("short"), Some("has spaces")] {
        let mut body = json!({"address": address, "amount_zatoshi": 1});
        if let Some(key) = key {
            body["idempotency_key"] = json!(key);
        }
        let response = router(state.clone())
            .oneshot(
                Request::post("/api/v1/faucet/address")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn internal_address_faucet_replays_preserve_activity_after_store_reopen() {
    for pool in ["transparent", "ironwood"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let account = store.account(2).unwrap();
        let address = if pool == "transparent" {
            account.transparent_address
        } else {
            account.unified_address
        };
        let original = store.claim_faucet(2, pool, 123, "internal-replay").unwrap();
        store
            .record_prepared(&original.id, "original-txid", b"signed bytes", 140)
            .unwrap();
        store
            .confirm(&original.id, "original-txid", "original-block")
            .unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let state = AppState::new(store, wallet, "http://127.0.0.1:1".into(), "test".into());
        state.mark_started();
        let body = json!({"address": address, "amount_zatoshi": 123, "idempotency_key": "internal-replay"});
        for _ in 0..2 {
            let response = router(state.clone())
                .oneshot(
                    Request::post("/api/v1/faucet/address")
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let result: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(result["txid"], "original-txid");
            assert_eq!(result["block_hash"], "original-block");
            assert_eq!(result["status"], "confirmed");
        }
        for (field, value) in [
            ("amount_zatoshi", json!(124)),
            (
                "address",
                json!(state.0.store.account(3).unwrap().transparent_address),
            ),
        ] {
            let mut conflict = body.clone();
            conflict[field] = value;
            let response = router(state.clone())
                .oneshot(
                    Request::post("/api/v1/faucet/address")
                        .header("content-type", "application/json")
                        .body(Body::from(conflict.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
        }
        let activities = state.0.store.activities(100).unwrap();
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].id, original.id);
        assert_eq!(activities[0].txid, "original-txid");
    }
}

/// An unknown pool is a client error, not the wallet's internal `bail!`
/// surfacing as a 500 that the dashboard reports as an unexpected error.
#[tokio::test]
async fn send_quote_rejects_unknown_pools_as_bad_requests() {
    let (state, _dir) = state_with_local_wallet();
    let app = router(state);
    for (body, message) in [
        (
            json!({"from_account": 1, "source_pool": "sapling", "destination_pool": "ironwood"}),
            "source_pool must be transparent or ironwood",
        ),
        (
            json!({"from_account": 1, "source_pool": "ironwood", "destination_pool": "sapling"}),
            "destination_pool must be transparent or ironwood",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/send/quote")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains(message),
            "expected {message} in {bytes:?}"
        );
    }
}

/// While startup is pending the quote is unavailable, like send and faucet,
/// instead of failing against an unscanned wallet.
#[tokio::test]
async fn send_quote_reports_startup_as_unavailable() {
    let (state, _dir) = crate::state::tests::offline_state();
    let (status, body) = api_request(
        &state,
        "POST",
        "/api/v1/send/quote",
        json!({"from_account": 1, "source_pool": "ironwood", "destination_pool": "ironwood"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({"error": {"message": "the environment is still starting", "status": 503}}),
    );
}

/// A started server with an unscanned wallet reports the quote as
/// unavailable rather than as an unexpected error, for either pool.
#[tokio::test]
async fn send_quote_reports_an_unscanned_wallet_as_unavailable() {
    let (state, _dir) = state_with_local_wallet();
    for (source_pool, destination_pool) in [("ironwood", "ironwood"), ("transparent", "ironwood")] {
        let (status, body) = api_request(
            &state,
            "POST",
            "/api/v1/send/quote",
            json!({"from_account": 1, "source_pool": source_pool, "destination_pool": destination_pool}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{source_pool}->{destination_pool}"
        );
        assert_eq!(
            body["error"]["status"], 503,
            "{source_pool}->{destination_pool}: {body}"
        );
    }
}

use crate::state::tests::{api_get, api_request};
#[tokio::test]
async fn address_faucet_status_reads_internal_and_external_records() {
    let (state, _dir) = state_with_local_wallet();
    let store = &state.0.store;
    let external = store
        .claim_address_faucet("tm-external", 50, "status-external")
        .unwrap();
    store
        .record_address_error(&external.id, "preparing", "", "treasury is syncing")
        .unwrap();
    let transparent = store
        .claim_faucet(2, "transparent", 60, "status-transparent")
        .unwrap();
    store
        .record_prepared(&transparent.id, "transparent-txid", b"raw", 140)
        .unwrap();
    store
        .mark_broadcast(&transparent.id, "transparent-txid")
        .unwrap();
    let ironwood = store
        .claim_faucet(3, "ironwood", 70, "status-ironwood")
        .unwrap();
    store
        .record_prepared(&ironwood.id, "ironwood-txid", b"raw", 140)
        .unwrap();
    store
        .confirm(&ironwood.id, "ironwood-txid", "ironwood-block")
        .unwrap();
    let external_address = store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .transparent_address;
    let confirmed_external = store
        .claim_address_faucet(&external_address, 80, "status-confirmed-external")
        .unwrap();
    store
        .record_address_prepared(&confirmed_external.id, "external-txid", b"raw", 140)
        .unwrap();
    store
        .confirm_address(&confirmed_external.id, "external-txid", "external-block")
        .unwrap();
    store
        .claim_transfer(
            1,
            crate::db::Recipient::Account(2),
            "ironwood",
            "ironwood",
            90,
            "status-send",
            None,
        )
        .unwrap();

    let transparent_address = store.account(2).unwrap().transparent_address;
    let unified_address = store.account(3).unwrap().unified_address;
    for (key, expected) in [
        (
            "status-external",
            json!({"address": "tm-external", "amount_zatoshi": 50, "txid": "", "block_hash": null, "status": "preparing", "last_error": "treasury is syncing"}),
        ),
        (
            "status-transparent",
            json!({"address": transparent_address, "amount_zatoshi": 60, "txid": "transparent-txid", "block_hash": null, "status": "broadcast", "last_error": null}),
        ),
        (
            "status-ironwood",
            json!({"address": unified_address, "amount_zatoshi": 70, "txid": "ironwood-txid", "block_hash": "ironwood-block", "status": "confirmed", "last_error": null}),
        ),
        (
            "status-confirmed-external",
            json!({"address": external_address, "amount_zatoshi": 80, "txid": "external-txid", "block_hash": "external-block", "status": "confirmed", "last_error": null}),
        ),
    ] {
        assert_eq!(
            api_get(
                &state,
                &format!("/api/v1/faucet/address?idempotency_key={key}")
            )
            .await,
            (StatusCode::OK, expected),
            "{key}"
        );
    }

    for (key, address, amount) in [
        ("status-ironwood", unified_address.as_str(), 70),
        ("status-confirmed-external", external_address.as_str(), 80),
    ] {
        let (_, read) = api_get(
            &state,
            &format!("/api/v1/faucet/address?idempotency_key={key}"),
        )
        .await;
        let (code, replay) = api_request(
            &state,
            "POST",
            "/api/v1/faucet/address",
            json!({"address": address, "amount_zatoshi": amount, "idempotency_key": key}),
        )
        .await;
        assert_eq!((code, replay), (StatusCode::OK, read), "{key}");
    }

    for key in ["status-send", "status-missing"] {
        let (code, body) = api_get(
            &state,
            &format!("/api/v1/faucet/address?idempotency_key={key}"),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{key}");
        assert_eq!(body["error"]["status"], 404, "{key}");
    }
    assert_eq!(
        api_get(&state, "/api/v1/faucet/address?idempotency_key=")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api_get(&state, "/api/v1/faucet/address").await.0,
        StatusCode::BAD_REQUEST
    );
}

fn text_memo_param(text: &str) -> String {
    zip321::memo_to_base64(&MemoBytes::from(&text.parse::<Memo>().unwrap()))
}

fn foreign_unified_address<P: zcash_protocol::consensus::Parameters>(params: &P) -> String {
    use zcash_keys::keys::{UnifiedAddressRequest, UnifiedSpendingKey};
    let usk = UnifiedSpendingKey::from_seed(params, &[7u8; 32], zip32::AccountId::ZERO).unwrap();
    let (address, _) = usk
        .to_unified_full_viewing_key()
        .default_address(UnifiedAddressRequest::AllAvailableKeys)
        .unwrap();
    address.encode(params)
}

#[test]
fn payment_uri_resolves_a_development_account_with_amount_and_memo() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let account = store.account(2).unwrap();
    let uri = format!(
        "zcash:{}?amount=1.5&memo={}",
        account.unified_address,
        text_memo_param("rent for October")
    );

    let parsed = payment_uri(&store, &uri).unwrap();
    assert_eq!(parsed.address, account.unified_address);
    assert_eq!(parsed.destination_pool, "ironwood");
    assert_eq!(parsed.to_account, Some(2));
    assert_eq!(parsed.amount_zatoshi, Some(150_000_000));
    assert_eq!(parsed.memo.as_deref(), Some("rent for October"));
}

#[test]
fn payment_uri_rejects_an_explicit_zero_and_keeps_a_missing_amount() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let address = store.account(2).unwrap().unified_address;

    for amount in ["0", "0.0"] {
        let error = payment_uri(&store, &format!("zcash:{address}?amount={amount}")).unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST, "{amount}");
        assert_eq!(
            error.message, "amount must be greater than zero",
            "{amount}"
        );
    }

    let omitted = payment_uri(&store, &format!("zcash:{address}")).unwrap();
    assert_eq!(omitted.amount_zatoshi, None);

    let positive = payment_uri(&store, &format!("zcash:{address}?amount=1")).unwrap();
    assert_eq!(positive.amount_zatoshi, Some(100_000_000));
}

#[test]
fn payment_uri_accepts_transparent_and_external_regtest_addresses() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let transparent = store.account(3).unwrap().transparent_address;
    let parsed = payment_uri(&store, &format!("zcash:?address={transparent}")).unwrap();
    assert_eq!(parsed.destination_pool, "transparent");
    assert_eq!(parsed.to_account, Some(3));
    assert_eq!(parsed.amount_zatoshi, None);

    let external = foreign_unified_address(&regtest_network());
    let parsed = payment_uri(&store, &format!("zcash:{external}?amount=0.1")).unwrap();
    assert_eq!(parsed.to_account, None);
    assert_eq!(parsed.address, external);
}

#[test]
fn payment_uri_rejects_unsupported_requests() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let ua = store.account(1).unwrap().unified_address;
    let taddr = store.account(2).unwrap().transparent_address;
    let mainnet = foreign_unified_address(&zcash_protocol::consensus::MainNetwork);
    let testnet = foreign_unified_address(&zcash_protocol::consensus::TestNetwork);
    let binary_memo = zip321::memo_to_base64(&MemoBytes::from_bytes(&[0xff; 512]).unwrap());
    let rejected = [
        format!("zcash:{testnet}?amount=1"),
        format!("zcash:{ua}?memo={binary_memo}"),
        format!("zcash:{taddr}?amount=1&memo={}", text_memo_param("hi")),
        format!("zcash:?address={ua}&amount=1&address.1={taddr}&amount.1=2"),
        format!("zcash:{mainnet}?amount=1"),
        format!("zcash:{ua}?amount=1&req-unknown=x"),
        format!("zcash:{ua}?amount=0.000000001"),
        "not a uri".to_owned(),
    ];
    for uri in rejected {
        let error = payment_uri(&store, &uri).expect_err(&uri);
        assert_eq!(error.status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[test]
fn send_addresses_map_to_their_pool() {
    let store = Store::open(":memory:").unwrap();
    store.initialize().unwrap();
    let account = store.account(1).unwrap();
    assert_eq!(
        require_send_address(&account.unified_address).ok(),
        Some("ironwood")
    );
    assert_eq!(
        require_send_address(&account.transparent_address).ok(),
        Some("transparent")
    );
    let mainnet = foreign_unified_address(&zcash_protocol::consensus::MainNetwork);
    assert!(require_send_address(&mainnet).is_err());
    assert!(require_send_address("not-an-address").is_err());
}

#[test]
fn send_validation_requires_a_matching_pool_for_an_address() {
    let external = foreign_unified_address(&regtest_network());
    let request = |destination_pool: &str| {
        let mut body = valid_send();
        body.as_object_mut().unwrap().remove("to_account");
        body["to_address"] = json!(external);
        body["destination_pool"] = json!(destination_pool);
        serde_json::from_value::<SendRequest>(body).unwrap()
    };
    let req = request("ironwood");
    assert!(matches!(
        validate_send(&req),
        Ok((Recipient::Address(address), None)) if address == external
    ));
    let req = request("transparent");
    assert_eq!(
        validate_send(&req).unwrap_err().status,
        StatusCode::BAD_REQUEST
    );

    let mut both = valid_send();
    both["to_address"] = json!(external);
    let req = serde_json::from_value::<SendRequest>(both).unwrap();
    assert_eq!(
        validate_send(&req).unwrap_err().status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn payment_request_endpoint_round_trips_an_exact_amount_and_memo() {
    let (state, _dir) = state_with_local_wallet();
    let account = state.0.store.account(3).unwrap();
    let memo = "rent for October 🌸";
    let response = router(state)
        .oneshot(
            Request::post("/api/v1/zip321/request")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "account_id": account.id,
                        "amount_zatoshi": 123_456_789,
                        "memo": memo,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let uri = body["uri"].as_str().unwrap();
    assert!(uri.starts_with("zcash:uregtest1"));
    let parsed = zip321::TransactionRequest::from_uri(uri).unwrap();
    let payment = parsed.payments().values().next().unwrap();
    assert_eq!(parsed.payments().len(), 1);
    assert_eq!(
        payment.recipient_address().encode(),
        account.unified_address
    );
    assert_eq!(payment.amount().map(u64::from), Some(123_456_789));
    let encoded_memo = payment.memo().unwrap().as_slice();
    assert_eq!(&encoded_memo[..memo.len()], memo.as_bytes());
    assert!(encoded_memo[memo.len()..].iter().all(|byte| *byte == 0));
}

#[tokio::test]
async fn payment_request_endpoint_omits_an_optional_amount_and_memo() {
    let (state, _dir) = state_with_local_wallet();
    let account = state.0.store.account(5).unwrap();
    let response = router(state)
        .oneshot(
            Request::post("/api/v1/zip321/request")
                .header("content-type", "application/json")
                .body(Body::from(json!({"account_id": account.id}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["uri"].as_str(),
        Some(format!("zcash:{}", account.unified_address).as_str())
    );
}

#[tokio::test]
async fn payment_request_endpoint_rejects_invalid_accounts_amounts_and_memos() {
    let (state, _dir) = state_with_local_wallet();
    let cases = [
        json!({"account_id": 0}),
        json!({"account_id": TREASURY_ACCOUNT_ID}),
        json!({"account_id": 1, "amount_zatoshi": 0}),
        json!({"account_id": 1, "amount_zatoshi": MAX_MONEY + 1}),
        json!({"account_id": 1, "memo": "a".repeat(513)}),
        json!({"account_id": 1, "memo": "memo\0"}),
    ];

    for body in cases {
        let response = router(state.clone())
            .oneshot(
                Request::post("/api/v1/zip321/request")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn all_payment_posts_admit_without_execution_while_payments_are_busy() {
    let (local, _dir) = offline_state();
    let (rpc_url, calls) = counting_rpc(false).await;
    let state = AppState::new(
        local.0.store.clone(),
        local.0.wallet.clone(),
        rpc_url,
        "test".into(),
    );
    state.mark_started();
    let _payment = state.0.payments.lock().await;
    let internal = state.0.store.account(2).unwrap().transparent_address;
    let external = state
        .0
        .store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .unified_address;
    let cases = [
        ("/api/v1/send", valid_send(), REPLAY_KEY, false),
        (
            "/api/v1/faucet",
            json!({"account_id": 3, "pool": "ironwood", "amount_zatoshi": 100_000, "idempotency_key": "busy-account-faucet"}),
            "busy-account-faucet",
            false,
        ),
        (
            "/api/v1/faucet/address",
            json!({"address": internal, "amount_zatoshi": 100_000, "idempotency_key": "busy-internal-address"}),
            "busy-internal-address",
            false,
        ),
        (
            "/api/v1/faucet/address",
            json!({"address": external, "amount_zatoshi": 100_000, "idempotency_key": "busy-external-address"}),
            "busy-external-address",
            true,
        ),
    ];
    for (uri, request, key, external) in cases {
        let (status, location, response) = payment_post(&state, uri, &request).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{uri}");
        assert_eq!(response["status"], "preparing");
        assert_eq!(response["last_error"], Value::Null);
        assert_eq!(response["txid"], "");
        let id = if external {
            assert!(state.0.store.activity_for_key(key).unwrap().is_none());
            let saved = state.0.store.address_faucet_for_key(key).unwrap().unwrap();
            assert_eq!(saved.address, request["address"].as_str().unwrap());
            saved.id
        } else {
            assert!(state.0.store.address_faucet_for_key(key).unwrap().is_none());
            let saved = state.0.store.activity_for_key(key).unwrap().unwrap();
            assert_eq!(saved.amount_zatoshi, 100_000);
            saved.id
        };
        assert!(state.0.store.prepared_transaction(&id).is_err());
        assert!(state.0.wallet.prepared(&id).await.unwrap().is_none());
        let before = (
            state.0.store.unresolved_activities().unwrap(),
            state.0.store.unresolved_address_faucets().unwrap(),
        );
        let durable_response = if uri == "/api/v1/faucet/address" {
            response
        } else {
            let saved = state.0.store.activity_for_key(key).unwrap().unwrap();
            let mut admitted = response;
            admitted["created_at"] = json!(saved.created_at);
            let durable = serde_json::to_value(saved).unwrap();
            assert_eq!(admitted, durable);
            durable
        };
        assert_eq!(
            payment_post(&state, uri, &request).await,
            (status, location, durable_response)
        );
        assert_eq!(
            (
                state.0.store.unresolved_activities().unwrap(),
                state.0.store.unresolved_address_faucets().unwrap()
            ),
            before,
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(state.0.store.activities(100).unwrap().len(), 3);
    assert_eq!(state.0.store.unresolved_address_faucets().unwrap().len(), 1);
    assert!(state.0.mining.latest().await.is_none());
}

#[tokio::test]
async fn payment_replays_use_stored_status_and_exact_encoded_location() {
    const KEY: &str = "payment-key?x=1&y=+%/#";
    const ENCODED_KEY: &str = "payment-key%3Fx%3D1%26y%3D%2B%25%2F%23";
    for endpoint in ["send", "faucet", "internal-address", "external-address"] {
        for stored_status in ["preparing", "prepared", "broadcast", "confirmed", "failed"] {
            let (state, _dir) = offline_state();
            let store = &state.0.store;
            let external = endpoint == "external-address";
            let address = store
                .account(if external { TREASURY_ACCOUNT_ID } else { 2 })
                .unwrap()
                .unified_address;
            let (uri, status_path, request) = match endpoint {
                "send" => {
                    let mut body = valid_send();
                    body["idempotency_key"] = json!(KEY);
                    body["memo"] = json!("stored memo");
                    ("/api/v1/send", "/api/v1/activity", body)
                }
                "faucet" => (
                    "/api/v1/faucet",
                    "/api/v1/activity",
                    json!({"account_id": 2, "pool": "ironwood", "amount_zatoshi": 100_000, "idempotency_key": KEY}),
                ),
                _ => (
                    "/api/v1/faucet/address",
                    "/api/v1/faucet/address",
                    json!({"address": address, "amount_zatoshi": 100_000, "idempotency_key": KEY}),
                ),
            };
            let id = if external {
                store
                    .claim_address_faucet(&address, 100_000, KEY)
                    .unwrap()
                    .id
            } else if endpoint == "send" {
                store
                    .claim_transfer(
                        1,
                        Recipient::Account(2),
                        "ironwood",
                        "ironwood",
                        100_000,
                        KEY,
                        Some("stored memo"),
                    )
                    .unwrap()
                    .id
            } else {
                store.claim_faucet(2, "ironwood", 100_000, KEY).unwrap().id
            };
            match stored_status {
                "failed" => {
                    if external {
                        store.fail_address(&id, "stored failure").unwrap();
                    } else {
                        store.fail(&id, "stored failure").unwrap();
                    }
                }
                "prepared" | "broadcast" | "confirmed" => {
                    if external {
                        store
                            .record_address_prepared(
                                &id,
                                "original-txid",
                                b"original signed bytes",
                                140,
                            )
                            .unwrap();
                        if stored_status != "prepared" {
                            store.mark_address_broadcast(&id, "original-txid").unwrap();
                        }
                        if stored_status == "confirmed" {
                            store
                                .confirm_address(&id, "original-txid", "original-block")
                                .unwrap();
                        }
                    } else {
                        store
                            .record_prepared(&id, "original-txid", b"original signed bytes", 140)
                            .unwrap();
                        if stored_status != "prepared" {
                            store.mark_broadcast(&id, "original-txid").unwrap();
                        }
                        if stored_status == "confirmed" {
                            store
                                .confirm(&id, "original-txid", "original-block")
                                .unwrap();
                        }
                    }
                }
                _ => {}
            }
            if matches!(stored_status, "preparing" | "prepared" | "broadcast") {
                let txid = if stored_status == "preparing" {
                    ""
                } else {
                    "original-txid"
                };
                if external {
                    store
                        .record_address_error(&id, stored_status, txid, "pending execution error")
                        .unwrap();
                } else {
                    store
                        .record_error(&id, stored_status, txid, "pending execution error")
                        .unwrap();
                }
            }
            let expected = if endpoint == "send" || endpoint == "faucet" {
                serde_json::to_value(store.activity_for_key(KEY).unwrap().unwrap()).unwrap()
            } else if external {
                serde_json::to_value(external_faucet_response(
                    store.address_faucet_for_key(KEY).unwrap().unwrap(),
                ))
                .unwrap()
            } else {
                serde_json::to_value(
                    faucet_address_response(
                        address.clone(),
                        store.activity_for_key(KEY).unwrap().unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap()
            };
            let before = (
                store.activity_for_key(KEY).unwrap(),
                store.address_faucet_for_key(KEY).unwrap(),
            );
            let _payment = state.0.payments.lock().await;
            for _ in 0..2 {
                let (code, location, body) = payment_post(&state, uri, &request).await;
                let terminal = matches!(stored_status, "confirmed" | "failed");
                assert_eq!(
                    code,
                    if terminal {
                        StatusCode::OK
                    } else {
                        StatusCode::ACCEPTED
                    },
                    "{endpoint}: {stored_status}"
                );
                assert_eq!(
                    location,
                    format!("{status_path}?idempotency_key={ENCODED_KEY}")
                );
                assert_eq!(body, expected);
                let (read_code, read) = api_get(&state, &location).await;
                assert_eq!(read_code, StatusCode::OK);
                assert_eq!(
                    read,
                    if status_path == "/api/v1/activity" {
                        json!([body])
                    } else {
                        body
                    }
                );
            }
            assert_eq!(
                (
                    store.activity_for_key(KEY).unwrap(),
                    store.address_faucet_for_key(KEY).unwrap()
                ),
                before
            );
            if matches!(stored_status, "prepared" | "broadcast" | "confirmed") {
                assert_eq!(
                    store.prepared_transaction(&id).unwrap().raw_transaction,
                    b"original signed bytes"
                );
            } else {
                assert!(store.prepared_transaction(&id).is_err());
            }
            assert!(state.0.wallet.prepared(&id).await.unwrap().is_none());
        }
    }
}

#[tokio::test]
async fn payment_admission_replays_enforce_fingerprints_and_cross_store_identity() {
    let (state, _dir) = offline_state();
    let _payment = state.0.payments.lock().await;
    let mut send_body = valid_send();
    send_body["memo"] = json!("original memo");
    payment_post(&state, "/api/v1/send", &send_body).await;
    let original = state.0.store.activity_for_key(REPLAY_KEY).unwrap().unwrap();
    for (field, value) in [
        ("from_account", json!(3)),
        ("to_account", json!(3)),
        ("source_pool", json!("transparent")),
        ("destination_pool", json!("transparent")),
        ("amount_zatoshi", json!(100_001)),
        ("memo", json!("changed memo")),
        ("memo", Value::Null),
    ] {
        let mut conflict = send_body.clone();
        conflict[field] = value;
        if field == "destination_pool" {
            conflict.as_object_mut().unwrap().remove("memo");
        }
        assert_eq!(
            api_request(&state, "POST", "/api/v1/send", conflict)
                .await
                .0,
            StatusCode::CONFLICT,
            "{field}"
        );
        assert_eq!(
            state.0.store.activity_for_key(REPLAY_KEY).unwrap().unwrap(),
            original
        );
    }
    let faucet_body = json!({"account_id": 2, "pool": "ironwood", "amount_zatoshi": 100_000, "idempotency_key": "fingerprint-faucet"});
    payment_post(&state, "/api/v1/faucet", &faucet_body).await;
    let faucet = state
        .0
        .store
        .activity_for_key("fingerprint-faucet")
        .unwrap()
        .unwrap();
    for (field, value) in [
        ("account_id", json!(3)),
        ("pool", json!("transparent")),
        ("amount_zatoshi", json!(100_001)),
    ] {
        let mut conflict = faucet_body.clone();
        conflict[field] = value;
        assert_eq!(
            api_request(&state, "POST", "/api/v1/faucet", conflict)
                .await
                .0,
            StatusCode::CONFLICT
        );
    }
    let internal_address = state.0.store.account(2).unwrap().unified_address;
    let address_body = json!({"address": internal_address, "amount_zatoshi": 100_000, "idempotency_key": "fingerprint-faucet"});
    assert_eq!(
        payment_post(&state, "/api/v1/faucet/address", &address_body)
            .await
            .2["status"],
        "preparing"
    );
    assert_eq!(
        state
            .0
            .store
            .activity_for_key("fingerprint-faucet")
            .unwrap()
            .unwrap(),
        faucet
    );
    let mut wrong_kind = faucet_body.clone();
    wrong_kind["idempotency_key"] = json!(REPLAY_KEY);
    assert_eq!(
        api_request(&state, "POST", "/api/v1/faucet", wrong_kind)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let external_address = state
        .0
        .store
        .account(TREASURY_ACCOUNT_ID)
        .unwrap()
        .unified_address;
    let mut wrong_store = address_body.clone();
    wrong_store["address"] = json!(external_address);
    assert_eq!(
        api_request(&state, "POST", "/api/v1/faucet/address", wrong_store)
            .await
            .0,
        StatusCode::CONFLICT
    );

    let external = state
        .0
        .store
        .claim_address_faucet(&internal_address, 100_000, "external-before-classification")
        .unwrap();
    let replay_body = json!({"address": internal_address, "amount_zatoshi": 100_000, "idempotency_key": "external-before-classification"});
    for terminal in [false, true] {
        if terminal {
            state
                .0
                .store
                .fail_address(&external.id, "stored failure")
                .unwrap();
        }
        let (code, location, body) =
            payment_post(&state, "/api/v1/faucet/address", &replay_body).await;
        assert_eq!(
            code,
            if terminal {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            }
        );
        assert_eq!(
            body["status"],
            if terminal { "failed" } else { "preparing" }
        );
        assert_eq!(api_get(&state, &location).await.1, body);
        assert!(
            state
                .0
                .store
                .activity_for_key("external-before-classification")
                .unwrap()
                .is_none()
        );
        let saved = state
            .0
            .store
            .address_faucet_for_key("external-before-classification")
            .unwrap()
            .unwrap();
        assert_eq!(saved.id, external.id);
        for (field, value, expected) in [
            ("amount_zatoshi", json!(100_001), StatusCode::CONFLICT),
            ("address", json!(external_address), StatusCode::CONFLICT),
            ("address", json!("invalid-address"), StatusCode::BAD_REQUEST),
            ("amount_zatoshi", json!(0), StatusCode::BAD_REQUEST),
            ("idempotency_key", json!("short"), StatusCode::BAD_REQUEST),
        ] {
            let mut conflict = replay_body.clone();
            conflict[field] = value;
            assert_eq!(
                api_request(&state, "POST", "/api/v1/faucet/address", conflict)
                    .await
                    .0,
                expected
            );
            assert_eq!(
                state
                    .0
                    .store
                    .address_faucet_for_key("external-before-classification")
                    .unwrap()
                    .unwrap(),
                saved
            );
        }
        let mut wrong_store = faucet_body.clone();
        wrong_store["idempotency_key"] = json!("external-before-classification");
        assert_eq!(
            api_request(&state, "POST", "/api/v1/faucet", wrong_store)
                .await
                .0,
            StatusCode::CONFLICT
        );
    }
    assert_eq!(state.0.store.activities(100).unwrap().len(), 2);
    for id in [&original.id, &faucet.id, &external.id] {
        assert!(state.0.store.prepared_transaction(id).is_err());
        assert!(state.0.wallet.prepared(id).await.unwrap().is_none());
    }
}

async fn payment_post(state: &AppState, uri: &str, body: &Value) -> (StatusCode, String, Value) {
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        router(state.clone()).oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        ),
    )
    .await
    .expect("payment admission waited for execution")
    .unwrap();
    let status = response.status();
    let location = response.headers()[LOCATION].to_str().unwrap().to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, location, serde_json::from_slice(&bytes).unwrap())
}
