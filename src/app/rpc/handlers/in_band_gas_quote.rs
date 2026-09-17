//! Shell driver for `vela_getInBandGasQuote`. Quote computation (Multicall3
//! encoding/decoding, assembly, USD ordering) lives in
//! `vela_relay_core::quote` (spec 002); this handler owns the Multicall RPC
//! call, the Binance price fetch + cache, and error rendering.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::http::HeaderValue;
use serde_json::{Value, json};
use vela_relay_core::quote::{
    self, MULTICALL3_ADDRESS, QuoteStable, bytes_to_hex, bytes32_quantity,
};

use crate::{
    app::{
        AppState,
        rpc::{
            handlers::in_band_settlement,
            types::{GetInBandGasQuoteParams, InBandGasQuote, RpcError, RpcResponse},
        },
    },
    utils::{
        market::binance_usdt_price,
        rpc::{self, PaymentAssets},
        tempo,
    },
};

const MARKET_PRICE_TTL: Duration = Duration::from_secs(60);
const MARKET_PRICE_FAILURE_TTL: Duration = Duration::from_secs(3);
const MAX_MARKET_PRICE_CACHE_ENTRIES: usize = 128;

static MARKET_PRICE_CACHE: OnceLock<Mutex<HashMap<String, CachedMarketPrice>>> = OnceLock::new();
static MARKET_DATA_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub async fn handle(
    id: Value,
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    state: &AppState,
    params: GetInBandGasQuoteParams,
) -> (RpcResponse<Value>, Option<String>) {
    match quote(chain_id, user_rpc_url, state, params.safe_address()).await {
        Ok((quotes, rpc_domain)) => (
            RpcResponse::result(
                id,
                serde_json::to_value(quotes).expect("in-band gas quotes must serialize"),
            ),
            Some(rpc_domain),
        ),
        Err(error) => (RpcResponse::error(id, error), None),
    }
}

async fn quote(
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    state: &AppState,
    safe_address: String,
) -> Result<(Vec<InBandGasQuote>, String), RpcError> {
    let safe_address = parse_address(&safe_address, "safeAddress")?;
    let recipient = state
        .settlement_recipient()
        .ok_or_else(RpcError::backend_unavailable)?
        .to_owned();
    // Tempo has no native gas asset. Its 0x76 envelopes charge pathUSD directly, so a chain
    // metadata fetch, Multicall native-balance read, and Binance quote are all both unnecessary
    // and wrong here.
    if tempo::is_tempo_chain(chain_id) {
        return tempo_quote(chain_id, user_rpc_url, safe_address, recipient).await;
    }
    let assets = rpc::payment_assets(chain_id).await.map_err(|_| {
        tracing::error!(chain_id, "could not load in-band gas quote chain metadata");
        RpcError::in_band_gas_quote_unavailable()
    })?;
    let native_price = native_usd_price(chain_id, &assets.native.symbol).await;
    let stablecoins = quote_stablecoins(&assets, native_price.is_some());

    let calls = quote::multicall_requests(safe_address, &stablecoins);
    let result = rpc::call(
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
        tracing::warn!(chain_id, "in-band gas quote Multicall request failed");
        RpcError::in_band_gas_quote_unavailable()
    })?;
    let values = result
        .value
        .as_str()
        .ok_or_else(|| {
            tracing::warn!(
                chain_id,
                "in-band gas quote Multicall returned a non-hex result"
            );
            RpcError::in_band_gas_quote_unavailable()
        })
        .and_then(|value| {
            quote::decode_aggregate3(value).map_err(|_| {
                tracing::warn!(
                    chain_id,
                    "could not decode in-band gas quote Multicall result"
                );
                RpcError::in_band_gas_quote_unavailable()
            })
        })?;

    quote::quotes_from_multicall(
        assets.native.decimals,
        &assets.native.symbol,
        &recipient,
        native_price,
        &stablecoins,
        values,
    )
    .map(|quotes| (quotes, result.domain))
    .map_err(|()| {
        tracing::warn!(chain_id, "in-band gas quote Multicall result is incomplete");
        RpcError::in_band_gas_quote_unavailable()
    })
}

fn quote_stablecoins(assets: &PaymentAssets, native_priced: bool) -> Vec<QuoteStable> {
    if !native_priced {
        return Vec::new();
    }
    assets
        .stablecoins
        .iter()
        .filter(|stablecoin| quote::is_usd_stablecoin(&stablecoin.symbol))
        .map(|stablecoin| QuoteStable {
            symbol: stablecoin.symbol.clone(),
            contract: stablecoin.contract.clone(),
        })
        .collect()
}

async fn tempo_quote(
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    safe_address: [u8; 20],
    recipient: String,
) -> Result<(Vec<InBandGasQuote>, String), RpcError> {
    let safe_address = alloy::primitives::Address::from(safe_address);
    let result = rpc::call(
        chain_id,
        user_rpc_url,
        "eth_call",
        json!([
            {
                "to": tempo::PATH_USD.to_string(),
                "data": bytes_to_hex(&tempo::path_usd_balance_calldata(safe_address)),
            },
            "latest",
        ]),
    )
    .await
    .map_err(|_| RpcError::in_band_gas_quote_unavailable())?;
    let balance = result
        .value
        .as_str()
        .and_then(|value| in_band_settlement::decode_hex(value).ok())
        .as_deref()
        .and_then(bytes32_quantity)
        .ok_or_else(RpcError::in_band_gas_quote_unavailable)?;
    Ok((vec![quote::tempo_quote(recipient, balance)], result.domain))
}

fn parse_address(value: &str, field: &str) -> Result<[u8; 20], RpcError> {
    quote::address(value)
        .ok_or_else(|| RpcError::invalid_params(format!("{field} must be a 20-byte address")))
}

async fn native_usd_price(chain_id: u64, symbol: &str) -> Option<String> {
    // xDAI is the Gnosis native gas asset and is intentionally USD-pegged; on Arc the native gas
    // asset IS USDC. Do not depend on an exchange ticker for a value the protocol defines as one
    // dollar — and on Arc the client's "$0.01 worth of the native coin" fee floor reads its price
    // out of this quote, so a market outage must not be able to move what a person is charged.
    if vela_relay_core::settlement::pegged_native_usd_price(chain_id).is_some() {
        return Some("1".into());
    }
    let symbol = symbol.trim().to_ascii_uppercase();
    if symbol.is_empty() || !symbol.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }

    let now = Instant::now();
    if let Some(price) = cached_market_price(&symbol, now) {
        return price;
    }

    let price = binance_usdt_price(market_data_client(), &symbol)
        .await
        .and_then(|price| quote::normalize_usd_price(&price));
    if price.is_none() {
        tracing::warn!(
            symbol,
            "could not obtain native USD price from Binance endpoints"
        );
    }
    store_market_price(symbol, price.clone(), now);
    price
}

fn cached_market_price(symbol: &str, now: Instant) -> Option<Option<String>> {
    let mut cache = market_price_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| entry.expires_at > now);
    cache.get(symbol).map(|entry| entry.price.clone())
}

fn store_market_price(symbol: String, price: Option<String>, now: Instant) {
    let mut cache = market_price_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| entry.expires_at > now);
    if !cache.contains_key(&symbol) && cache.len() >= MAX_MARKET_PRICE_CACHE_ENTRIES {
        tracing::warn!(
            max_entries = MAX_MARKET_PRICE_CACHE_ENTRIES,
            "market price cache is full; skipped cache entry"
        );
        return;
    }

    cache.insert(
        symbol,
        CachedMarketPrice {
            expires_at: now
                + if price.is_some() {
                    MARKET_PRICE_TTL
                } else {
                    MARKET_PRICE_FAILURE_TTL
                },
            price,
        },
    );
}

fn market_price_cache() -> &'static Mutex<HashMap<String, CachedMarketPrice>> {
    MARKET_PRICE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn market_data_client() -> &'static reqwest::Client {
    MARKET_DATA_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(2))
            .build()
            .expect("market data HTTP client configuration must be valid")
    })
}

struct CachedMarketPrice {
    price: Option<String>,
    expires_at: Instant,
}

#[cfg(test)]
mod tests {
    // The quote computation tests moved to `vela_relay_core::quote` with the
    // functions; this module keeps only the shell-owned price policy test.
    use super::native_usd_price;

    #[tokio::test]
    async fn prices_gnosis_native_asset_at_one_usd_without_a_market_request() {
        // The symbol is deliberately invalid: chain policy, rather than a Binance ticker,
        // determines the Gnosis xDAI price.
        assert_eq!(
            native_usd_price(100, "not-a-ticker").await,
            Some("1".into())
        );
    }
}
