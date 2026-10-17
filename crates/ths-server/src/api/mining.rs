use super::error::require_started;
use super::{
    error::{ApiError, ApiJson, ApiPath, ApiResult},
    payments::require_key,
};
use crate::mining::{MiningJob, MiningState};
use crate::state::AppState;
use anyhow::Context;
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Deserialize)]
pub(crate) struct StartMiningRequest {
    blocks: u32,
    idempotency_key: String,
}

pub(crate) async fn start_mining_job(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<StartMiningRequest>,
) -> ApiResult<(StatusCode, Json<MiningJob>)> {
    require_started(&state)?;
    require_key(&req.idempotency_key)?;
    let admission = state.start_mining(req.blocks, req.idempotency_key).await?;
    Ok((StatusCode::ACCEPTED, Json(admission.job)))
}

pub(crate) async fn latest_mining_job(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"job":state.0.mining.latest().await}))
}

pub(crate) async fn mining_job(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<MiningJob>> {
    state
        .0
        .mining
        .get(&id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "Mining job does not exist".into(),
        })
}

#[derive(Deserialize)]
pub(crate) struct MineRequest {
    blocks: u32,
}
pub(crate) async fn mine(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<MineRequest>,
) -> ApiResult<Json<Value>> {
    require_started(&state)?;
    if !(1..=10_000).contains(&req.blocks) {
        return Err(ApiError::bad_request("blocks must be between 1 and 10,000"));
    }
    let mut admission = state
        .start_mining(req.blocks, uuid::Uuid::new_v4().to_string())
        .await?;
    loop {
        let update = admission.updates.borrow_and_update().clone();
        match update.job.state {
            MiningState::Completed => {
                let hashes = update
                    .hashes
                    .context("Completed mining job omitted its hashes")?;
                return Ok(Json(json!({"blocks":hashes.len(),"hashes":*hashes})));
            }
            MiningState::Failed => {
                return Err(anyhow::anyhow!(
                    update
                        .job
                        .error
                        .context("Failed mining job omitted its error")?
                )
                .into());
            }
            MiningState::Mining | MiningState::Syncing => {}
        }
        admission
            .updates
            .changed()
            .await
            .context("Mining job update channel closed before completion")?;
    }
}

#[cfg(test)]
#[path = "../../tests/unit/api/mining/tests.rs"]
pub(crate) mod tests;
