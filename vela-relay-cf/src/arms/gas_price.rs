//! `pimlico_getUserOperationGasPrice` — the docker `GasPriceManager` flow
//! (eth_feeHistory beside eth_maxPriorityFeePerGas → EIP-1559 with the
//! executor's own tip rule → legacy eth_gasPrice fallback) with every price
//! rule in `vela_relay_core::gas_math`. This arm owns transport, the KV price
//! cache, and the response budget only.

use futures_util::future::{Either, join, select};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vela_relay_core::gas_math::{
    FeeHistory, GasPriceError, GasPricePolicy, GasPriceTiers, NetworkGasPrice,
    legacy_price_from_result, parse_quantity, price_from_fee_history, quote_market_tip, tiers,
};
use worker::{Date, Delay, Env};

use super::rpc;
use crate::config::CfConfig;

const FEE_HISTORY_BLOCK_COUNT: &str = "0x5";
// Still requested, so the call stays byte-identical to the docker shell's, but
// the reward column is no longer read (`gas_math::market_tip`).
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
    // The tip is read beside the fee history, as the docker shell does.
    let (fee_history, max_priority_fee_per_gas) = join(
        rpc::call(
            config,
            env,
            chain_id,
            user_rpc_url,
            "eth_feeHistory",
            json!([FEE_HISTORY_BLOCK_COUNT, "latest", FEE_HISTORY_PERCENTILES]),
        ),
        quantity(
            config,
            env,
            chain_id,
            user_rpc_url,
            "eth_maxPriorityFeePerGas",
        ),
    )
    .await;
    if let Ok(response) = fee_history {
        match eip1559_price(
            config,
            env,
            response.value,
            max_priority_fee_per_gas,
            chain_id,
            user_rpc_url,
            policy,
        )
        .await
        {
            Ok(price) => return Ok((price, response.domain)),
            Err(error) => {
                worker::console_warn!("could not calculate EIP-1559 gas price: {error:?}");
            }
        }
    }

    legacy_gas_price(config, env, chain_id, user_rpc_url).await
}

/// Docker `GasPriceManager::eip1559_price`: the next block's base fee, and
/// the tip resolved exactly as the lane executor resolves the one it signs
/// with (`gas_math::quote_market_tip` → `gas_math::market_tip`).
async fn eip1559_price(
    config: &CfConfig,
    env: &Env,
    result: Value,
    max_priority_fee_per_gas: Option<u128>,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    policy: &GasPricePolicy,
) -> Result<NetworkGasPrice, GasPriceError> {
    let fee_history = serde_json::from_value::<FeeHistory>(result)
        .map_err(|_| GasPriceError::InvalidUpstreamResponse)?;
    // `eth_gasPrice` is consulted only when the node named no tip.
    let legacy_gas_price = match max_priority_fee_per_gas {
        Some(_) => None,
        None => quantity(config, env, chain_id, user_rpc_url, "eth_gasPrice").await,
    };
    let tip = quote_market_tip(
        &fee_history,
        max_priority_fee_per_gas,
        legacy_gas_price,
        policy.priority_fee_divisor,
    )?;

    price_from_fee_history(&fee_history, tip)
}

/// One no-argument quantity read. `None` when the call failed or did not
/// return a quantity — the "no answer" `gas_math::market_tip` distinguishes
/// from a zero.
async fn quantity(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    method: &str,
) -> Option<u128> {
    let response = rpc::call(
        config,
        env,
        chain_id,
        user_rpc_url,
        method,
        Value::Array(Vec::new()),
    )
    .await
    .ok()?;
    parse_quantity(response.value.as_str()?).ok()
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
