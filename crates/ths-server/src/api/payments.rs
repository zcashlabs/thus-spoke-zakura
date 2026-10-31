use super::error::{ApiError, ApiJson, ApiQuery, ApiResult, require_started};
use crate::state::AppState;
use crate::wallet::PaymentError;
use crate::{
    db::{Activity, AddressFaucet, Recipient, Store, USER_ACCOUNT_COUNT, ZATOSHIS_PER_ZEC},
    mining::validate_idempotency_key,
    network::regtest_network,
    wallet::SendQuote,
};
use anyhow::Context;
use axum::{Json, extract::State, http::StatusCode};
use axum::{
    http::{HeaderValue, header::LOCATION},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use zcash_keys::address::Address;
use zcash_protocol::{
    memo::{Memo, MemoBytes},
    value::{MAX_MONEY, Zatoshis},
};
use zip321::{Payment, TransactionRequest};
#[derive(Deserialize)]
/// Exactly one of `to_account` or `to_address` names the destination.
pub(crate) struct SendRequest {
    from_account: u8,
    to_account: Option<u8>,
    to_address: Option<String>,
    source_pool: String,
    destination_pool: String,
    amount_zatoshi: u64,
    idempotency_key: String,
    #[serde(default)]
    memo: Option<String>,
}
pub(crate) async fn send(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<SendRequest>,
) -> ApiResult<PaymentResponse<Activity>> {
    require_started(&state)?;
    let (recipient, _) = validate_send(&req)?;
    // A pasted address that belongs to a development account is recorded as
    // that account, before the claim, so a replay resolves to the same payment.
    let recipient = match recipient {
        Recipient::Address(address) => {
            development_account_for(&state.0.store, address)?.map_or(recipient, Recipient::Account)
        }
        Recipient::Account(id) => {
            state.0.store.account(id)?;
            recipient
        }
    };
    if recipient == Recipient::Account(req.from_account) && req.source_pool == req.destination_pool
    {
        return Err(ApiError::bad_request(
            "choose a different account or a different destination pool",
        ));
    }
    let pending = state.0.store.claim_transfer(
        req.from_account,
        recipient,
        &req.source_pool,
        &req.destination_pool,
        req.amount_zatoshi,
        &req.idempotency_key,
        req.memo.as_deref(),
    )?;
    let status = pending.status.clone();
    payment_response(&status, pending, "/api/v1/activity", &req.idempotency_key)
}

#[derive(Deserialize)]
pub(crate) struct PaymentUriRequest {
    uri: String,
}

/// A single-payment ZIP-321 request, resolved for the Send form.
#[derive(Debug, Serialize)]
pub(crate) struct PaymentUri {
    address: String,
    destination_pool: &'static str,
    /// Set when the address belongs to a development account.
    to_account: Option<u8>,
    amount_zatoshi: Option<u64>,
    memo: Option<String>,
}

pub(crate) async fn parse_payment_uri(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<PaymentUriRequest>,
) -> ApiResult<Json<PaymentUri>> {
    Ok(Json(payment_uri(&state.0.store, &req.uri)?))
}

fn payment_uri(store: &Store, uri: &str) -> ApiResult<PaymentUri> {
    let request = TransactionRequest::from_uri(uri.trim()).map_err(|error| {
        tracing::debug!(%error, "rejected ZIP-321 URI");
        ApiError::bad_request("Invalid payment URI")
    })?;
    let mut payments = request.payments().values();
    let (Some(payment), None) = (payments.next(), payments.next()) else {
        return Err(ApiError::bad_request(
            "only single-payment URIs are supported",
        ));
    };
    let address = payment.recipient_address().encode();
    let destination_pool = require_send_address(&address)?;
    let memo = match payment.memo().map(Memo::try_from).transpose() {
        Ok(None | Some(Memo::Empty)) => None,
        Ok(Some(Memo::Text(text))) => Some(String::from(&*text)),
        _ => return Err(ApiError::bad_request("only text memos are supported")),
    };
    // ZIP-321 treats an explicit zero as a requested amount. Send cannot pay
    // it, while an omitted amount stays unset so the sender can supply one.
    let amount_zatoshi = payment.amount().map(u64::from);
    if amount_zatoshi == Some(0) {
        return Err(ApiError::bad_request("amount must be greater than zero"));
    }
    Ok(PaymentUri {
        to_account: development_account_for(store, &address)?,
        address,
        destination_pool,
        amount_zatoshi,
        memo,
    })
}

fn development_account_for(store: &Store, address: &str) -> anyhow::Result<Option<u8>> {
    for id in 1..=USER_ACCOUNT_COUNT {
        let account = store.account(id)?;
        if account.unified_address == address || account.transparent_address == address {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
pub(crate) struct PaymentRequest {
    account_id: u8,
    amount_zatoshi: Option<u64>,
    #[serde(default)]
    memo: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PaymentRequestResponse {
    uri: String,
}

pub(crate) async fn payment_request(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<PaymentRequest>,
) -> ApiResult<Json<PaymentRequestResponse>> {
    require_user_account(req.account_id)?;
    let amount = req
        .amount_zatoshi
        .map(|amount| {
            if amount == 0 {
                return Err(ApiError::bad_request("amount must be greater than zero"));
            }
            Zatoshis::from_u64(amount).map_err(|_| {
                ApiError::bad_request(format!("amount_zatoshi must not exceed {MAX_MONEY}"))
            })
        })
        .transpose()?;
    let memo = parse_memo(req.memo.as_deref(), "ironwood")?;
    let account = state.0.store.account(req.account_id)?;
    let address = Address::decode(&regtest_network(), &account.unified_address)
        .context("stored account has an invalid unified address")?;
    let payment = Payment::new(
        address.to_zcash_address(&regtest_network()),
        amount,
        memo,
        None,
        None,
        vec![],
    )
    .context("constructing payment request")?;
    let uri = TransactionRequest::new(vec![payment])
        .context("constructing payment request")?
        .to_uri();
    Ok(Json(PaymentRequestResponse { uri }))
}

#[derive(Deserialize)]
pub(crate) struct SendQuoteRequest {
    from_account: u8,
    source_pool: String,
    destination_pool: String,
}
pub(crate) async fn send_quote(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<SendQuoteRequest>,
) -> ApiResult<Json<SendQuote>> {
    require_started(&state)?;
    require_user_account(req.from_account)?;
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    // The fee depends only on the destination's receiver kinds, which every
    // account shares, so the source account's own address quotes the same fee.
    let destination = state.0.store.account(req.from_account)?;
    let address = if req.destination_pool == "transparent" {
        destination.transparent_address
    } else {
        destination.unified_address
    };
    Ok(Json(
        state
            .0
            .wallet
            .send_quote(req.from_account, &req.source_pool, &address)
            .await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct FaucetRequest {
    account_id: u8,
    pool: String,
    amount_zatoshi: u64,
    idempotency_key: String,
}
pub(crate) async fn faucet(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<FaucetRequest>,
) -> ApiResult<PaymentResponse<Activity>> {
    require_started(&state)?;
    require_key(&req.idempotency_key)?;
    require_user_account(req.account_id)?;
    if req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "a faucet request is limited to 5 ZEC",
        ));
    }
    if req.amount_zatoshi == 0 {
        return Err(ApiError::bad_request("amount must be greater than zero"));
    }
    require_pool(&req.pool, "pool")?;
    require_non_dust_amount(req.amount_zatoshi, req.pool == "transparent")?;
    let pending = state.0.store.claim_faucet(
        req.account_id,
        &req.pool,
        req.amount_zatoshi,
        &req.idempotency_key,
    )?;
    let status = pending.status.clone();
    payment_response(&status, pending, "/api/v1/activity", &req.idempotency_key)
}

#[derive(Deserialize)]
pub(crate) struct FaucetAddressRequest {
    address: String,
    amount_zatoshi: u64,
    #[serde(default)]
    idempotency_key: String,
}

#[derive(Serialize)]
pub(crate) struct FaucetAddressResponse {
    address: String,
    destination_pool: &'static str,
    amount_zatoshi: u64,
    txid: String,
    block_hash: Option<String>,
    status: String,
    last_error: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct FaucetAddressParseRequest {
    address: String,
}

#[derive(Serialize)]
pub(crate) struct FaucetAddressParsed {
    /// Set when the address belongs to a development account.
    to_account: Option<u8>,
}

/// Validates `address` for the faucet (not the stricter Send rule) and reports
/// whether it belongs to a development account, so the dashboard's external
/// Faucet form can reject an invalid address and route a development
/// account's own address through the account Faucet instead.
pub(crate) async fn parse_faucet_address(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<FaucetAddressParseRequest>,
) -> ApiResult<Json<FaucetAddressParsed>> {
    let address = require_faucet_address(&req.address)?;
    require_faucet_destination_pool(&address)?;
    Ok(Json(FaucetAddressParsed {
        to_account: development_account_for(&state.0.store, &req.address)?,
    }))
}

/// The pool a payment to `address` lands in, or `None` if its receivers don't
/// resolve to a pool Send and Faucet track. The SDK falls back to a unified
/// address's transparent receiver when it has neither a supported Orchard
/// (Ironwood) nor Sapling receiver; a Sapling receiver without Orchard is the
/// gap this can't represent, since the wallet spends through Sapling there,
/// not Ironwood or transparent.
fn faucet_destination_pool(address: &Address) -> Option<&'static str> {
    match address {
        Address::Transparent(_) => Some("transparent"),
        Address::Unified(unified) if unified.has_orchard() => Some("ironwood"),
        Address::Unified(unified) if unified.has_sapling() => None,
        Address::Unified(unified) if unified.has_transparent() => Some("transparent"),
        _ => None,
    }
}

/// [`faucet_destination_pool`], rejecting an address whose receivers don't
/// resolve to a pool Send and Faucet track (a Sapling receiver without
/// Orchard) instead of mislabeling it.
fn require_faucet_destination_pool(address: &Address) -> ApiResult<&'static str> {
    faucet_destination_pool(address).ok_or_else(|| {
        ApiError::bad_request(
            "a unified destination with a Sapling receiver must also include an Ironwood receiver",
        )
    })
}

fn internal_faucet_destination(
    store: &Store,
    address: &str,
) -> anyhow::Result<Option<(u8, &'static str)>> {
    for account in store.accounts()? {
        if !(1..=USER_ACCOUNT_COUNT).contains(&account.id) {
            continue;
        }
        if account.transparent_address == address {
            return Ok(Some((account.id, "transparent")));
        }
        if account.unified_address == address {
            return Ok(Some((account.id, "ironwood")));
        }
    }
    Ok(None)
}

fn faucet_address_response(
    address: String,
    destination_pool: &'static str,
    activity: Activity,
) -> anyhow::Result<FaucetAddressResponse> {
    let block_hash = activity.block_hash.filter(|hash| !hash.is_empty());
    if activity.status == "confirmed" {
        anyhow::ensure!(
            block_hash.is_some(),
            "faucet transaction was not included in a block"
        );
    }
    Ok(FaucetAddressResponse {
        address,
        destination_pool,
        amount_zatoshi: activity.amount_zatoshi,
        txid: activity.txid,
        block_hash,
        status: activity.status,
        last_error: activity.last_error,
    })
}

fn external_faucet_response(payment: AddressFaucet) -> anyhow::Result<FaucetAddressResponse> {
    let address = Address::decode(&regtest_network(), &payment.address)
        .context("stored address faucet payment has an unparsable address")?;
    let destination_pool = faucet_destination_pool(&address)
        .context("stored address faucet payment has an unsupported destination pool")?;
    Ok(FaucetAddressResponse {
        destination_pool,
        address: payment.address,
        amount_zatoshi: payment.amount_zatoshi,
        txid: payment.txid,
        block_hash: payment.block_hash,
        status: payment.status,
        last_error: payment.last_error,
    })
}

fn stored_address_faucet(
    store: &Store,
    key: &str,
) -> anyhow::Result<Option<FaucetAddressResponse>> {
    if let Some(payment) = store.address_faucet_for_key(key)? {
        return Ok(Some(external_faucet_response(payment)?));
    }
    let Some(activity) = store
        .activity_for_key(key)?
        .filter(|activity| activity.kind == "faucet")
    else {
        return Ok(None);
    };
    let account = store.account(
        activity
            .to_account
            .context("internal faucet omitted its account")?,
    )?;
    let (address, pool) = match activity.destination_pool.as_str() {
        "transparent" => (account.transparent_address, "transparent"),
        "ironwood" => (account.unified_address, "ironwood"),
        pool => anyhow::bail!("faucet has unsupported destination pool {pool}"),
    };
    faucet_address_response(address, pool, activity).map(Some)
}
#[cfg(test)]
#[async_trait::async_trait]
#[cfg(test)]
trait AddressFaucetRuntime: Sync {
    async fn fund_internal(
        &self,
        account_id: u8,
        pool: &str,
        amount_zatoshi: u64,
        key: &str,
    ) -> anyhow::Result<Activity>;
    async fn fund_external(
        &self,
        req: FaucetAddressRequest,
    ) -> anyhow::Result<FaucetAddressResponse>;
}

#[cfg(test)]
#[cfg(test)]
async fn execute_address_faucet<R: AddressFaucetRuntime>(
    store: &Store,
    runtime: &R,
    req: FaucetAddressRequest,
) -> ApiResult<Json<FaucetAddressResponse>> {
    require_key(&req.idempotency_key)?;
    require_faucet_address(&req.address)?;
    if req.amount_zatoshi == 0 || req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "amount must be greater than zero and no more than 5 ZEC",
        ));
    }
    if store
        .address_faucet_for_key(&req.idempotency_key)?
        .is_some()
    {
        return Ok(Json(runtime.fund_external(req).await?));
    }
    if let Some((account_id, pool)) = internal_faucet_destination(store, &req.address)? {
        let activity = runtime
            .fund_internal(account_id, pool, req.amount_zatoshi, &req.idempotency_key)
            .await?;
        Ok(Json(faucet_address_response(req.address, pool, activity)?))
    } else {
        Ok(Json(runtime.fund_external(req).await?))
    }
}

pub(crate) async fn faucet_address(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<FaucetAddressRequest>,
) -> ApiResult<PaymentResponse<FaucetAddressResponse>> {
    require_started(&state)?;
    require_key(&req.idempotency_key)?;
    let address = require_faucet_address(&req.address)?;
    if req.amount_zatoshi == 0 || req.amount_zatoshi > 5 * ZATOSHIS_PER_ZEC {
        return Err(ApiError::bad_request(
            "amount must be greater than zero and no more than 5 ZEC",
        ));
    }
    let pool = require_faucet_destination_pool(&address)?;
    require_non_dust_amount(req.amount_zatoshi, pool == "transparent")?;
    // An existing external claim keeps its store even if the address later
    // becomes a development-account address.
    let destination = if state
        .0
        .store
        .address_faucet_for_key(&req.idempotency_key)?
        .is_some()
    {
        None
    } else {
        internal_faucet_destination(&state.0.store, &req.address)?
    };
    let response = if let Some((account_id, pool)) = destination {
        let pending = state.0.store.claim_faucet(
            account_id,
            pool,
            req.amount_zatoshi,
            &req.idempotency_key,
        )?;
        faucet_address_response(req.address, pool, pending)?
    } else {
        external_faucet_response(state.0.store.claim_address_faucet(
            &req.address,
            req.amount_zatoshi,
            &req.idempotency_key,
        )?)?
    };
    let status = response.status.clone();
    payment_response(
        &status,
        response,
        "/api/v1/faucet/address",
        &req.idempotency_key,
    )
}

pub(crate) fn require_key(key: &str) -> ApiResult<()> {
    validate_idempotency_key(key).map_err(ApiError::bad_request)
}

fn require_non_dust_amount(amount_zatoshi: u64, transparent: bool) -> ApiResult<()> {
    // Zakura's standard P2PKH and P2SH outputs both have a 54-zatoshi dust threshold.
    if transparent && amount_zatoshi < 54 {
        return Err(ApiError::bad_request(
            "transparent outputs must be at least 54 zatoshi",
        ));
    }
    Ok(())
}

fn require_pool(pool: &str, field: &str) -> ApiResult<()> {
    match pool {
        "transparent" | "ironwood" => Ok(()),
        // NU6.3 is active from block 1, so the Orchard pool can neither receive
        // nor hold funds on this chain.
        "orchard" => Err(ApiError::bad_request(format!(
            "{field} orchard is unavailable: NU6.3 is active and Orchard no longer accepts deposits; use ironwood"
        ))),
        _ => Err(ApiError::bad_request(format!(
            "{field} must be transparent or ironwood"
        ))),
    }
}

/// Checks the whole send request without touching the store, wallet or node,
/// and returns the encoded memo it carries.
fn validate_send(req: &SendRequest) -> ApiResult<(Recipient<'_>, Option<MemoBytes>)> {
    require_key(&req.idempotency_key)?;
    require_user_account(req.from_account)?;
    require_pool(&req.source_pool, "source_pool")?;
    require_pool(&req.destination_pool, "destination_pool")?;
    let recipient = match (req.to_account, req.to_address.as_deref()) {
        (Some(to_account), None) => {
            require_user_account(to_account)?;
            if req.from_account == to_account && req.source_pool == req.destination_pool {
                return Err(ApiError::bad_request(
                    "choose a different account or a different destination pool",
                ));
            }
            Recipient::Account(to_account)
        }
        (None, Some(address)) => {
            if require_send_address(address)? != req.destination_pool {
                return Err(ApiError::bad_request(
                    "destination_pool does not match the destination address",
                ));
            }
            Recipient::Address(address)
        }
        _ => {
            return Err(ApiError::bad_request(
                "provide exactly one of to_account or to_address",
            ));
        }
    };
    if req.amount_zatoshi == 0 || req.amount_zatoshi > MAX_MONEY {
        return Err(ApiError::bad_request(format!(
            "amount_zatoshi must be between 1 and {MAX_MONEY}"
        )));
    }
    require_non_dust_amount(req.amount_zatoshi, req.destination_pool == "transparent")?;
    let memo = parse_memo(req.memo.as_deref(), &req.destination_pool)?;
    Ok((recipient, memo))
}

/// An absent (or null) memo is `None`. Any present memo, including `""`, is
/// encoded as an explicit ZIP-302 text memo and is only valid for ironwood.
fn parse_memo(memo: Option<&str>, destination_pool: &str) -> ApiResult<Option<MemoBytes>> {
    let Some(text) = memo else {
        return Ok(None);
    };
    if destination_pool != "ironwood" {
        return Err(anyhow::Error::new(PaymentError::TransparentMemo).into());
    }
    // Text memos are zero-padded to 512 bytes, so a trailing NUL could not be
    // told apart from padding and would be silently lost on decode.
    if text.ends_with('\0') {
        return Err(ApiError::bad_request(
            "memo must not end with a NUL (U+0000) character",
        ));
    }
    MemoBytes::from_bytes(text.as_bytes())
        .map(Some)
        .map_err(|error| ApiError::bad_request(format!("invalid memo: {error}")))
}

fn require_user_account(id: u8) -> ApiResult<()> {
    if (1..=USER_ACCOUNT_COUNT).contains(&id) {
        Ok(())
    } else {
        Err(ApiError::bad_request("account must be between 1 and 5"))
    }
}

fn require_faucet_address(value: &str) -> ApiResult<Address> {
    match Address::decode(&regtest_network(), value) {
        Some(address @ (Address::Unified(_) | Address::Transparent(_))) => Ok(address),
        Some(_) => Err(ApiError::bad_request(
            "destination must be a unified or transparent Regtest address",
        )),
        None => Err(ApiError::bad_request(
            "destination is not a valid Regtest address",
        )),
    }
}

/// Returns the pool a payment to `value` lands in. Ironwood shares the Orchard
/// receiver, so a unified address needs one to receive shielded funds.
fn require_send_address(value: &str) -> ApiResult<&'static str> {
    match Address::decode(&regtest_network(), value) {
        Some(Address::Unified(address)) if address.has_orchard() => Ok("ironwood"),
        Some(Address::Transparent(_)) => Ok("transparent"),
        Some(Address::Unified(_)) => Err(ApiError::bad_request(
            "unified destination must include an Ironwood receiver",
        )),
        Some(_) => Err(ApiError::bad_request(
            "destination must be a unified or transparent Regtest address",
        )),
        None => Err(ApiError::bad_request(
            "destination is not a valid Regtest address",
        )),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/api/payments/tests.rs"]
pub(crate) mod tests;

#[derive(Deserialize)]
pub(crate) struct FaucetAddressQuery {
    idempotency_key: String,
}

pub(crate) async fn faucet_address_status(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<FaucetAddressQuery>,
) -> ApiResult<Json<FaucetAddressResponse>> {
    require_key(&query.idempotency_key)?;
    stored_address_faucet(&state.0.store, &query.idempotency_key)?
        .map(Json)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "no address faucet uses that idempotency key".into(),
        })
}

#[derive(Debug)]
pub(crate) struct PaymentResponse<T>(T, StatusCode, HeaderValue);

fn payment_response<T>(
    status: &str,
    body: T,
    status_path: &str,
    key: &str,
) -> ApiResult<PaymentResponse<T>> {
    let status = match status {
        "confirmed" | "failed" => StatusCode::OK,
        "preparing" | "prepared" | "broadcast" | "unconfirmed" => StatusCode::ACCEPTED,
        status => return Err(anyhow::anyhow!("payment has unsupported status {status}").into()),
    };
    let mut url =
        reqwest::Url::parse("http://127.0.0.1").context("constructing payment status URL")?;
    url.set_path(status_path);
    url.query_pairs_mut().append_pair("idempotency_key", key);
    let location = HeaderValue::from_str(&format!(
        "{}?{}",
        url.path(),
        url.query()
            .context("payment status URL omitted its query")?,
    ))
    .context("constructing payment status Location header")?;
    Ok(PaymentResponse(body, status, location))
}
impl<T: Serialize> IntoResponse for PaymentResponse<T> {
    fn into_response(self) -> Response {
        (self.1, [(LOCATION, self.2)], Json(self.0)).into_response()
    }
}
