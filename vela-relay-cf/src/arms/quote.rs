//! `vela_getInBandGasQuote` — the docker handler's flow with all computation
//! in `vela_relay_core::quote`; this arm owns the Multicall/Tempo RPC calls,
//! the Binance price fetch + KV cache, and error mapping.

use serde_json::json;
use vela_relay_core::quote::{
    self, MULTICALL3_ADDRESS, QuoteStable, bytes_to_hex, bytes32_quantity,
};
use vela_relay_core::wire::{InBandGasQuote, RpcError};
use worker::{Date, Env};

use super::{market, rpc};
use crate::config::CfConfig;

const MARKET_PRICE_TTL_MS: u64 = 60_000;
const MARKET_PRICE_FAILURE_TTL_MS: u64 = 3_000;
const KV_BINDING: &str = "CACHE";
const GNOSIS_CHAIN_ID: u64 = 100;

pub async fn handle(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    safe_address: String,
) -> Result<(Vec<InBandGasQuote>, String), RpcError> {
    let safe_address = parse_address(&safe_address, "safeAddress")?;
    let recipient = config
        .settlement_recipient
        .clone()
        .ok_or_else(RpcError::backend_unavailable)?;
    // Tempo has no native gas asset (same rule as the docker handler).
    if vela_relay_core::tempo::is_tempo_chain(chain_id) {
        return tempo_quote(config, env, chain_id, user_rpc_url, safe_address, recipient).await;
    }
    let metadata = market::payment_metadata(env, chain_id).await.map_err(|_| {
        worker::console_error!("could not load in-band gas quote chain metadata: {chain_id}");
        RpcError::in_band_gas_quote_unavailable()
    })?;
    let native = metadata.native_currency.ok_or_else(|| {
        worker::console_error!("chain {chain_id} metadata does not declare a native currency");
        RpcError::in_band_gas_quote_unavailable()
    })?;
    let native_price = native_usd_price(env, chain_id, &native.symbol).await;
    let stablecoins = if native_price.is_some() {
        metadata
            .stables
            .iter()
            .filter(|stable| market::is_hex_address(&stable.contract))
            .filter(|stable| quote::is_usd_stablecoin(&stable.symbol))
            .map(|stable| QuoteStable {
                symbol: stable.symbol.clone(),
                contract: stable.contract.clone(),
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let calls = quote::multicall_requests(safe_address, &stablecoins);
    let result = rpc::call(
        config,
        env,
        chain_id,
        user_rpc_url,
        "eth_call",
        json!([
            {
                "to": MULTICALL3_ADDRESS,
                "data": bytes_to_hex(&quote::encode_aggregate3(&calls)),
            },
            "latest",
        ]),
    )
    .await
    .map_err(|_| {
        worker::console_warn!("in-band gas quote Multicall request failed: {chain_id}");
        RpcError::in_band_gas_quote_unavailable()
    })?;
    let values = result
        .value
        .as_str()
        .ok_or_else(RpcError::in_band_gas_quote_unavailable)
        .and_then(|value| {
            quote::decode_aggregate3(value).map_err(|_| RpcError::in_band_gas_quote_unavailable())
        })?;

    quote::quotes_from_multicall(
        native.decimals,
        &native.symbol,
        &recipient,
        native_price,
        &stablecoins,
        values,
    )
    .map(|quotes| (quotes, result.domain))
    .map_err(|()| RpcError::in_band_gas_quote_unavailable())
}

async fn tempo_quote(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    safe_address: [u8; 20],
    recipient: String,
) -> Result<(Vec<InBandGasQuote>, String), RpcError> {
    let safe_address = alloy_address(safe_address);
    let result = rpc::call(
        config,
        env,
        chain_id,
        user_rpc_url,
        "eth_call",
        json!([
            {
                "to": vela_relay_core::tempo::PATH_USD.to_string(),
                "data": bytes_to_hex(&vela_relay_core::tempo::path_usd_balance_calldata(
                    safe_address,
                )),
            },
            "latest",
        ]),
    )
    .await
    .map_err(|_| RpcError::in_band_gas_quote_unavailable())?;
    let balance = result
        .value
        .as_str()
        .and_then(decode_hex)
        .as_deref()
        .and_then(bytes32_quantity)
        .ok_or_else(RpcError::in_band_gas_quote_unavailable)?;
    Ok((vec![quote::tempo_quote(recipient, balance)], result.domain))
}

/// Same policy as the docker `native_usd_price`: a chain whose native coin is a
/// dollar by construction (Gnosis's xDAI, Arc's USDC) is pegged at one dollar,
/// otherwise Binance with a success/failure cache (KV, timestamps embedded).
async fn native_usd_price(env: &Env, chain_id: u64, symbol: &str) -> Option<String> {
    if vela_relay_core::settlement::pegged_native_usd_price(chain_id).is_some() {
        return Some("1".into());
    }
    let symbol = symbol.trim().to_ascii_uppercase();
    if symbol.is_empty() || !symbol.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }

    let cache_key = format!("usdprice:{symbol}");
    if let Ok(kv) = env.kv(KV_BINDING)
        && let Ok(Some(cached)) = kv.get(&cache_key).text().await
        && let Ok(cached) = serde_json::from_str::<CachedPrice>(&cached)
    {
        let ttl = if cached.price.is_some() {
            MARKET_PRICE_TTL_MS
        } else {
            MARKET_PRICE_FAILURE_TTL_MS
        };
        if Date::now().as_millis().saturating_sub(cached.fetched_at_ms) < ttl {
            return cached.price;
        }
    }

    let price = market::binance_usdt_price(&symbol)
        .await
        .and_then(|price| quote::normalize_usd_price(&price));
    if price.is_none() {
        worker::console_warn!("could not obtain native USD price from Binance endpoints: {symbol}");
    }
    if let Ok(kv) = env.kv(KV_BINDING)
        && let Ok(payload) = serde_json::to_string(&CachedPrice {
            fetched_at_ms: Date::now().as_millis(),
            price: price.clone(),
        })
        && let Ok(put) = kv.put(&cache_key, payload)
    {
        let _ = put.expiration_ttl(120).execute().await;
    }
    price
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CachedPrice {
    fetched_at_ms: u64,
    price: Option<String>,
}

fn parse_address(value: &str, field: &str) -> Result<[u8; 20], RpcError> {
    quote::address(value)
        .ok_or_else(|| RpcError::invalid_params(format!("{field} must be a 20-byte address")))
}

fn alloy_address(value: [u8; 20]) -> alloy::primitives::Address {
    alloy::primitives::Address::from(value)
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    hex::decode(value).ok()
}
