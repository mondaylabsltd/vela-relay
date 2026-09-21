//! `pimlico_getUserOperationGasPrice` — the docker `GasPriceManager` flow
//! (eth_feeHistory → EIP-1559 with a priority-fee probe → legacy eth_gasPrice
//! fallback) with every price rule in `vela_relay_core::gas_math`. This arm
//! owns transport, the KV price cache, and the response budget only.

use futures_util::future::{Either, select};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vela_relay_core::gas_math::{
    FeeHistory, GasPriceError, GasPricePolicy, GasPriceTiers, NetworkGasPrice,
    fallback_priority_fee, legacy_price_from_result, median_priority_fee, parse_quantity,
    price_from_fee_history, tiers,
};
use worker::{Date, Delay, Env};

use super::rpc;
use crate::config::CfConfig;

const FEE_HISTORY_BLOCK_COUNT: &str = "0x5";
const FEE_HISTORY_PERCENTILES: [u8; 3] = [25, 50, 75];
const RESPONSE_BUDGET_MS: u64 = 2_800;
const PRICE_CACHE_TTL_MS: u64 = 5_000;
const KV_BINDING: &str = "CACHE";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GasPriceQuote {
    pub tiers: GasPriceTiers,
    pub rpc_domain: String,
}

#[derive(Deserialize, Serialize)]
struct CachedQuote {
    fetched_at_ms: u64,
    quote: GasPriceQuote,
}

pub async fn user_operation_gas_prices(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
) -> Result<GasPriceQuote, GasPriceError> {
    // KV is a cache only (FR-006): a stale-window miss just refetches. The
    // 5 s logical TTL matches the docker cache; KV's minimum expiry is 60 s,
    // so freshness is enforced by the embedded timestamp.
    //
    // A `GasPriceTiers` written by an older build lacks `networkFeePerGas`
    // and `relayerFeePerGas`, so the deserialize below fails and this falls
    // through to a refetch rather than serving a row with the missing field
    // defaulted to zero. That is the right failure for a deploy: at most one
    // extra upstream call per chain, and never a quote whose `R` is absent.
    let cache_key = format!("gasprice:{chain_id}");
    if user_rpc_url.is_none()
        && let Ok(kv) = env.kv(KV_BINDING)
        && let Ok(Some(cached)) = kv.get(&cache_key).text().await
        && let Ok(cached) = serde_json::from_str::<CachedQuote>(&cached)
        && Date::now().as_millis().saturating_sub(cached.fetched_at_ms) < PRICE_CACHE_TTL_MS
    {
        return Ok(cached.quote);
    }

    let quote = with_response_budget(fetch_user_operation_gas_prices(
        config,
        env,
        chain_id,
        user_rpc_url,
    ))
    .await?;

    if user_rpc_url.is_none()
        && let Ok(kv) = env.kv(KV_BINDING)
        && let Ok(payload) = serde_json::to_string(&CachedQuote {
            fetched_at_ms: Date::now().as_millis(),
            quote: quote.clone(),
        })
        && let Ok(put) = kv.put(&cache_key, payload)
    {
        let _ = put.expiration_ttl(60).execute().await;
    }

    Ok(quote)
}

async fn fetch_user_operation_gas_prices(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
) -> Result<GasPriceQuote, GasPriceError> {
    let policy = GasPricePolicy::default();
    let (network_price, rpc_domain) =
        network_gas_price(config, env, chain_id, user_rpc_url, &policy).await?;
    Ok(GasPriceQuote {
        tiers: tiers(network_price)?,
        rpc_domain,
    })
}

async fn network_gas_price(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    policy: &GasPricePolicy,
) -> Result<(NetworkGasPrice, String), GasPriceError> {
    if let Ok(response) = rpc::call(
        config,
        env,
        chain_id,
        user_rpc_url,
        "eth_feeHistory",
        json!([FEE_HISTORY_BLOCK_COUNT, "latest", FEE_HISTORY_PERCENTILES]),
    )
    .await
    {
        match eip1559_price(config, env, response.value, chain_id, user_rpc_url, policy).await {
            Ok(price) => return Ok((price, response.domain)),
            Err(error) => {
                worker::console_warn!("could not calculate EIP-1559 gas price: {error:?}");
            }
        }
    }

    legacy_gas_price(config, env, chain_id, user_rpc_url).await
}

async fn eip1559_price(
    config: &CfConfig,
    env: &Env,
    result: Value,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    policy: &GasPricePolicy,
) -> Result<NetworkGasPrice, GasPriceError> {
    let fee_history = serde_json::from_value::<FeeHistory>(result)
        .map_err(|_| GasPriceError::InvalidUpstreamResponse)?;
    let base_fee = fee_history
        .base_fee_per_gas
        .last()
        .ok_or(GasPriceError::InvalidUpstreamResponse)
        .and_then(|value| parse_quantity(value))?;

    let priority_fee = match median_priority_fee(&fee_history.reward) {
        Some(priority_fee) if priority_fee > 0 => priority_fee,
        _ => priority_fee_probe(config, env, chain_id, user_rpc_url, base_fee, policy).await?,
    };

    price_from_fee_history(&fee_history, priority_fee)
}

async fn priority_fee_probe(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    base_fee: u128,
    policy: &GasPricePolicy,
) -> Result<u128, GasPriceError> {
    if let Ok(response) = rpc::call(
        config,
        env,
        chain_id,
        user_rpc_url,
        "eth_maxPriorityFeePerGas",
        Value::Array(Vec::new()),
    )
    .await
        && let Some(value) = response.value.as_str()
        && let Ok(priority_fee) = parse_quantity(value)
        && priority_fee > 0
    {
        return Ok(priority_fee);
    }

    Ok(fallback_priority_fee(base_fee, policy.priority_fee_divisor))
}

async fn legacy_gas_price(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
) -> Result<(NetworkGasPrice, String), GasPriceError> {
    let response = rpc::call(
        config,
        env,
        chain_id,
        user_rpc_url,
        "eth_gasPrice",
        Value::Array(Vec::new()),
    )
    .await
    .map_err(|()| GasPriceError::NoPriceAvailable)?;
    Ok((legacy_price_from_result(response.value)?, response.domain))
}

async fn with_response_budget<T>(
    operation: impl Future<Output = Result<T, GasPriceError>>,
) -> Result<T, GasPriceError> {
    let deadline = Delay::from(std::time::Duration::from_millis(RESPONSE_BUDGET_MS));
    match select(std::pin::pin!(operation), deadline).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(GasPriceError::ResponseDeadlineExceeded),
    }
}
