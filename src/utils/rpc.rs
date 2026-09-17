use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::http::HeaderValue;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

pub const USER_RPC_URL_HEADER: &str = "x-vela-rpc-url";

const RPC_LIST_URL: &str = "https://ethereum-data.getvela.app/chains/eip155-";
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const METADATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const METADATA_REQUEST_ATTEMPTS: usize = 3;
const FAILED_RPC_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_FAILED_RPC_ENTRIES: usize = 1_024;
const CHAIN_METADATA_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_CHAIN_METADATA_CACHE_ENTRIES: usize = 512;

static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static METADATA_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static FAILED_RPCS: OnceLock<FailedRpcCache> = OnceLock::new();
static CHAIN_METADATA_CACHE: OnceLock<Mutex<HashMap<u64, CachedChainMetadata>>> = OnceLock::new();

#[derive(Debug, PartialEq)]
pub struct RpcCallResult {
    pub value: Value,
    pub domain: String,
    /// Safe for API responses: API keys, query strings, and untrusted path components are hidden.
    pub rpc_url: String,
}

#[derive(Debug, PartialEq)]
pub enum RpcSimulationError {
    Reverted(RpcRevert),
    Unavailable,
}

#[derive(Debug, PartialEq)]
pub struct RpcRevert {
    pub code: Option<i64>,
    pub message: String,
    pub data: Option<Value>,
}

#[derive(Debug, PartialEq)]
pub struct SettlementAssets {
    pub native_decimals: u32,
    pub stablecoins: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentAssets {
    pub native: NativeAsset,
    pub stablecoins: Vec<StablecoinAsset>,
    /// Informational chain-registry metadata. Execution code must still match this
    /// against an operator-owned allowlist before using it in a money path.
    pub wrapped_native_token: Option<String>,
    /// Informational metadata mapped from `dex.contracts.quoterV2`.
    pub dex_quoter_v2: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAsset {
    pub symbol: String,
    pub decimals: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StablecoinAsset {
    pub symbol: String,
    pub contract: String,
    pub decimals: Option<u32>,
}

pub async fn call(
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    method: &str,
    params: Value,
) -> Result<RpcCallResult, ()> {
    let client = http_client();

    if let Some(url) = user_rpc_url.and_then(parse_user_rpc_url) {
        if let Some(result) =
            first_result(client, chain_id, "request_header", &[url], method, &params).await
        {
            return Ok(result);
        }
    } else if user_rpc_url.is_some() {
        tracing::warn!("ignored invalid user RPC URL header");
    }

    if let Some(url) = alchemy_rpc_url(chain_id)
        && let Some(result) =
            first_result(client, chain_id, "alchemy", &[url], method, &params).await
    {
        return Ok(result);
    }

    let fallback_urls = match fetch_fallback_rpc_urls(chain_id).await {
        Ok(urls) => urls,
        Err(error) => {
            tracing::warn!(%error, "could not fetch fallback RPC URLs");
            return Err(());
        }
    };

    first_result(
        client,
        chain_id,
        "chain-directory",
        &fallback_urls,
        method,
        &params,
    )
    .await
    .ok_or(())
}

/// Return the vetted public RPC endpoints from Vela's controlled chain directory.
///
/// This is deliberately separate from request-header RPC selection: background execution may
/// broadcast signed transactions and therefore must not inherit a URL supplied by an API caller.
/// The directory response is cached with the same one-hour policy as payment-asset metadata.
pub async fn directory_rpc_urls(chain_id: u64) -> Result<Vec<String>, ()> {
    fetch_fallback_rpc_urls(chain_id).await.map_err(|error| {
        tracing::warn!(%error, chain_id, "could not fetch controlled directory RPC URLs");
    })
}

/// Call an EVM simulation method while preserving a definitive contract revert.
///
/// Transport errors, rate limits, and unsupported RPC features fail over to the next
/// source. A real EVM revert is returned immediately so a valid source does not get
/// quarantined for rejecting one particular UserOperation.
pub async fn call_simulation(
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    method: &str,
    params: Value,
) -> Result<RpcCallResult, RpcSimulationError> {
    let client = http_client();

    if let Some(url) = user_rpc_url.and_then(parse_user_rpc_url) {
        match first_simulation_result(client, chain_id, "request_header", &[url], method, &params)
            .await
        {
            Ok(Some(result)) => return Ok(result),
            Err(error) => return Err(RpcSimulationError::Reverted(error)),
            Ok(None) => {}
        }
    } else if user_rpc_url.is_some() {
        tracing::warn!("ignored invalid user RPC URL header");
    }

    if let Some(url) = alchemy_rpc_url(chain_id) {
        match first_simulation_result(client, chain_id, "alchemy", &[url], method, &params).await {
            Ok(Some(result)) => return Ok(result),
            Err(error) => return Err(RpcSimulationError::Reverted(error)),
            Ok(None) => {}
        }
    }

    let fallback_urls = match fetch_fallback_rpc_urls(chain_id).await {
        Ok(urls) => urls,
        Err(error) => {
            tracing::warn!(%error, "could not fetch fallback RPC URLs");
            return Err(RpcSimulationError::Unavailable);
        }
    };

    match first_simulation_result(
        client,
        chain_id,
        "chain-directory",
        &fallback_urls,
        method,
        &params,
    )
    .await
    {
        Ok(Some(result)) => Ok(result),
        Err(error) => Err(RpcSimulationError::Reverted(error)),
        Ok(None) => Err(RpcSimulationError::Unavailable),
    }
}

pub async fn settlement_assets(chain_id: u64) -> Result<SettlementAssets, ()> {
    let assets = payment_assets(chain_id).await?;

    Ok(SettlementAssets {
        native_decimals: assets.native.decimals,
        stablecoins: assets
            .stablecoins
            .into_iter()
            .map(|stable| stable.contract)
            .collect(),
    })
}

/// Return native and stablecoin metadata for an in-band payment quote.
///
/// Chain metadata is shared with RPC fallback resolution and cached for one hour because it
/// changes far less frequently than account balances or gas prices.
pub async fn payment_assets(chain_id: u64) -> Result<PaymentAssets, ()> {
    let metadata = fetch_chain_metadata(metadata_http_client(), chain_id)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "could not fetch chain metadata for in-band payments");
        })?;
    let native_currency = metadata.native_currency.ok_or_else(|| {
        tracing::warn!(
            chain_id,
            "chain metadata does not declare a native currency"
        );
    })?;

    Ok(PaymentAssets {
        native: NativeAsset {
            symbol: native_currency.symbol,
            decimals: native_currency.decimals,
        },
        stablecoins: metadata
            .stables
            .into_iter()
            .filter_map(|stable| {
                parse_address(&stable.contract).map(|contract| StablecoinAsset {
                    symbol: stable.symbol,
                    contract,
                    decimals: stable.decimals.filter(|decimals| *decimals <= 38),
                })
            })
            .collect(),
        wrapped_native_token: metadata
            .wrapped_native_token
            .as_deref()
            .and_then(parse_address),
        dex_quoter_v2: metadata
            .dex
            .and_then(|dex| dex.contracts)
            .and_then(|contracts| contracts.quoter_v2)
            .as_deref()
            .and_then(parse_address),
    })
}

pub async fn erc20_decimals(
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    token: &str,
) -> Result<u32, ()> {
    let result = call(
        chain_id,
        user_rpc_url,
        "eth_call",
        json!([
            { "to": token, "data": "0x313ce567" },
            "latest",
        ]),
    )
    .await?;
    let value = result.value.as_str().ok_or(())?;
    let value = value.strip_prefix("0x").ok_or(())?;
    let decimals = u32::from_str_radix(value, 16).map_err(|_| ())?;
    (decimals <= 38).then_some(decimals).ok_or(())
}

fn http_client() -> &'static Client {
    HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("HTTP client configuration must be valid")
    })
}

fn metadata_http_client() -> &'static Client {
    METADATA_HTTP_CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(METADATA_CONNECT_TIMEOUT)
            .timeout(METADATA_REQUEST_TIMEOUT)
            .build()
            .expect("metadata HTTP client configuration must be valid")
    })
}

fn failed_rpcs() -> &'static FailedRpcCache {
    FAILED_RPCS.get_or_init(|| FailedRpcCache::new(FAILED_RPC_COOLDOWN, MAX_FAILED_RPC_ENTRIES))
}

fn alchemy_rpc_url(chain_id: u64) -> Option<String> {
    let api_key = std::env::var("ALCHEMY_API_KEY").ok()?;
    let api_key = api_key.trim();

    (!api_key.is_empty()).then(|| crate::utils::alchemy::rpc_url(chain_id, api_key))?
}

fn parse_user_rpc_url(value: &HeaderValue) -> Option<String> {
    let value = value.to_str().ok()?.trim();
    parse_rpc_url(value)
}

async fn fetch_fallback_rpc_urls(chain_id: u64) -> Result<Vec<String>, String> {
    let response = fetch_chain_metadata(metadata_http_client(), chain_id).await?;

    Ok(response
        .rpc
        .into_iter()
        .filter_map(|url| parse_rpc_url(&url))
        .collect())
}

async fn fetch_chain_metadata(client: &Client, chain_id: u64) -> Result<ChainMetadata, String> {
    let now = Instant::now();
    if let Some(metadata) = cached_chain_metadata(chain_id, now) {
        return Ok(metadata);
    }

    let url = format!("{RPC_LIST_URL}{chain_id}.json");
    let mut last_error = None;
    for attempt in 1..=METADATA_REQUEST_ATTEMPTS {
        match fetch_chain_metadata_once(client, &url).await {
            Ok(metadata) => {
                store_chain_metadata(chain_id, metadata.clone(), now);
                return Ok(metadata);
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < METADATA_REQUEST_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                }
            }
        }
    }

    Err(format!(
        "metadata request failed after {METADATA_REQUEST_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "unknown error".into())
    ))
}

async fn fetch_chain_metadata_once(client: &Client, url: &str) -> Result<ChainMetadata, String> {
    let body = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .text()
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&body).map_err(|error| error.to_string())
}

fn cached_chain_metadata(chain_id: u64, now: Instant) -> Option<ChainMetadata> {
    let mut cache = chain_metadata_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| entry.expires_at > now);
    cache.get(&chain_id).map(|entry| entry.metadata.clone())
}

fn store_chain_metadata(chain_id: u64, metadata: ChainMetadata, now: Instant) {
    let mut cache = chain_metadata_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.retain(|_, entry| entry.expires_at > now);
    if !cache.contains_key(&chain_id) && cache.len() >= MAX_CHAIN_METADATA_CACHE_ENTRIES {
        tracing::warn!(
            max_entries = MAX_CHAIN_METADATA_CACHE_ENTRIES,
            "chain metadata cache is full; skipped cache entry"
        );
        return;
    }

    cache.insert(
        chain_id,
        CachedChainMetadata {
            metadata,
            expires_at: now + CHAIN_METADATA_CACHE_TTL,
        },
    );
}

fn chain_metadata_cache() -> &'static Mutex<HashMap<u64, CachedChainMetadata>> {
    CHAIN_METADATA_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_rpc_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    let host = url.host_str()?;

    (url.scheme() == "https" && !is_local_host(host)).then(|| url.into())
}

fn is_local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    host.parse::<IpAddr>().is_ok_and(|address| match address {
        IpAddr::V4(address) => {
            address.is_loopback() || address.is_private() || address.is_link_local()
        }
        IpAddr::V6(address) => address.is_loopback() || address.is_unspecified(),
    })
}

async fn first_result(
    client: &Client,
    chain_id: u64,
    source: &str,
    urls: &[String],
    method: &str,
    params: &Value,
) -> Option<RpcCallResult> {
    for url in urls {
        if let Some(retry_after) = failed_rpcs().retry_after(chain_id, url, method) {
            tracing::debug!(
                source,
                chain_id,
                method,
                rpc_url = %redacted_rpc_url(url),
                retry_after_ms = retry_after.as_millis(),
                "skipped RPC during failure cooldown"
            );
            continue;
        }

        match fetch_result(client, url, method, params).await {
            Ok(result) => {
                tracing::info!(
                    source,
                    method,
                    rpc_url = %redacted_rpc_url(url),
                    "upstream JSON-RPC source selected"
                );
                return Some(RpcCallResult {
                    value: result,
                    domain: rpc_domain(url),
                    rpc_url: response_rpc_url(url),
                });
            }
            Err(error) => {
                failed_rpcs().freeze(chain_id, url, method);
                tracing::warn!(
                    source,
                    chain_id,
                    method,
                    rpc_url = %redacted_rpc_url(url),
                    %error,
                    "upstream JSON-RPC source failed"
                );
            }
        }
    }

    None
}

async fn first_simulation_result(
    client: &Client,
    chain_id: u64,
    source: &str,
    urls: &[String],
    method: &str,
    params: &Value,
) -> Result<Option<RpcCallResult>, RpcRevert> {
    for url in urls {
        if let Some(retry_after) = failed_rpcs().retry_after(chain_id, url, method) {
            tracing::debug!(
                source,
                chain_id,
                method,
                rpc_url = %redacted_rpc_url(url),
                retry_after_ms = retry_after.as_millis(),
                "skipped RPC during failure cooldown"
            );
            continue;
        }

        match fetch_simulation_result(client, url, method, params).await {
            Ok(result) => {
                tracing::info!(
                    source,
                    method,
                    rpc_url = %redacted_rpc_url(url),
                    "upstream JSON-RPC simulation source selected"
                );
                return Ok(Some(RpcCallResult {
                    value: result,
                    domain: rpc_domain(url),
                    rpc_url: response_rpc_url(url),
                }));
            }
            Err(SimulationUpstreamError::Reverted(error)) => {
                tracing::info!(
                    source,
                    chain_id,
                    method,
                    rpc_url = %redacted_rpc_url(url),
                    "upstream simulation reverted"
                );
                return Err(error);
            }
            Err(SimulationUpstreamError::Unavailable(error)) => {
                failed_rpcs().freeze(chain_id, url, method);
                tracing::warn!(
                    source,
                    chain_id,
                    method,
                    rpc_url = %redacted_rpc_url(url),
                    %error,
                    "upstream JSON-RPC simulation source failed"
                );
            }
        }
    }

    Ok(None)
}

async fn fetch_result(
    client: &Client,
    url: &str,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let response = client
        .post(url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        }))
        .send()
        .await
        .map_err(request_error)?
        .error_for_status()
        .map_err(response_error)?
        .json::<UpstreamRpcResponse>()
        .await
        .map_err(|_| "invalid JSON-RPC response".to_owned())?;

    if response.error.is_some() {
        return Err("upstream returned a JSON-RPC error".into());
    }

    response
        .result
        .ok_or_else(|| "upstream JSON-RPC response has no result".to_owned())
}

async fn fetch_simulation_result(
    client: &Client,
    url: &str,
    method: &str,
    params: &Value,
) -> Result<Value, SimulationUpstreamError> {
    let response = client
        .post(url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        }))
        .send()
        .await
        .map_err(|error| SimulationUpstreamError::Unavailable(request_error(error)))?
        .error_for_status()
        .map_err(|error| SimulationUpstreamError::Unavailable(response_error(error)))?
        .json::<UpstreamRpcResponse>()
        .await
        .map_err(|_| SimulationUpstreamError::Unavailable("invalid JSON-RPC response".into()))?;

    if let Some(error) = response.error {
        let error = RpcRevert {
            code: error.code,
            message: error
                .message
                .unwrap_or_else(|| "upstream JSON-RPC error".into()),
            data: error.data,
        };

        return if is_execution_revert(&error) {
            Err(SimulationUpstreamError::Reverted(error))
        } else {
            Err(SimulationUpstreamError::Unavailable(
                "upstream returned a JSON-RPC error".into(),
            ))
        };
    }

    response.result.ok_or_else(|| {
        SimulationUpstreamError::Unavailable("upstream JSON-RPC response has no result".into())
    })
}

fn is_execution_revert(error: &RpcRevert) -> bool {
    error.code == Some(3)
        || error
            .message
            .to_ascii_lowercase()
            .contains("execution reverted")
        || error
            .message
            .to_ascii_lowercase()
            .contains("execution error")
}

fn request_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "upstream request timed out".into()
    } else if error.is_connect() {
        "could not connect to upstream RPC".into()
    } else {
        "upstream request failed".into()
    }
}

fn response_error(error: reqwest::Error) -> String {
    match error.status() {
        Some(status) => format!("upstream returned HTTP status {status}"),
        None => "upstream returned an invalid HTTP response".into(),
    }
}

fn redacted_rpc_url(value: &str) -> String {
    let Ok(url) = reqwest::Url::parse(value) else {
        return "<invalid>".into();
    };
    let Some(host) = url.host_str() else {
        return "<invalid>".into();
    };

    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!("{}://{host}{port}/…", url.scheme())
}

fn rpc_domain(value: &str) -> String {
    reqwest::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unknown>".into())
}

fn response_rpc_url(value: &str) -> String {
    let Ok(url) = reqwest::Url::parse(value) else {
        return "<unknown>".into();
    };
    let Some(host) = url.host_str() else {
        return "<unknown>".into();
    };
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();

    if host.ends_with(".alchemy.com") && url.path().starts_with("/v2/") {
        return format!("{}://{host}{port}/v2/***", url.scheme());
    }

    format!("{}://{host}{port}/***", url.scheme())
}

fn parse_address(value: &str) -> Option<String> {
    let value = value.trim();
    let is_address = value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit());
    is_address.then(|| value.to_ascii_lowercase())
}

#[derive(Eq, Hash, PartialEq)]
struct FailedRpcKey {
    chain_id: u64,
    url: String,
    method: String,
}

struct FailedRpcCache {
    cooldown: Duration,
    max_entries: usize,
    entries: Mutex<HashMap<FailedRpcKey, Instant>>,
}

impl FailedRpcCache {
    fn new(cooldown: Duration, max_entries: usize) -> Self {
        Self {
            cooldown,
            max_entries,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn retry_after(&self, chain_id: u64, url: &str, method: &str) -> Option<Duration> {
        let now = Instant::now();
        let mut entries = self.lock_entries();
        entries.retain(|_, deadline| *deadline > now);
        entries
            .get(&FailedRpcKey::new(chain_id, url, method))
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn freeze(&self, chain_id: u64, url: &str, method: &str) {
        let now = Instant::now();
        let mut entries = self.lock_entries();
        entries.retain(|_, deadline| *deadline > now);

        if entries.len() >= self.max_entries {
            tracing::warn!(
                max_entries = self.max_entries,
                "RPC failure cache is full; skipped cooldown entry"
            );
            return;
        }

        entries.insert(
            FailedRpcKey::new(chain_id, url, method),
            now + self.cooldown,
        );
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<FailedRpcKey, Instant>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl FailedRpcKey {
    fn new(chain_id: u64, url: &str, method: &str) -> Self {
        Self {
            chain_id,
            url: url.into(),
            method: method.into(),
        }
    }
}

#[derive(Clone, Deserialize)]
struct ChainMetadata {
    #[serde(default)]
    rpc: Vec<String>,
    #[serde(default)]
    stables: Vec<StablecoinMetadata>,
    #[serde(rename = "nativeCurrency")]
    native_currency: Option<NativeCurrencyMetadata>,
    #[serde(rename = "wrappedNativeToken")]
    wrapped_native_token: Option<String>,
    dex: Option<DexMetadata>,
}

#[derive(Deserialize)]
struct UpstreamRpcResponse {
    result: Option<Value>,
    error: Option<UpstreamRpcError>,
}

#[derive(Deserialize)]
struct UpstreamRpcError {
    code: Option<i64>,
    message: Option<String>,
    data: Option<Value>,
}

#[derive(Clone, Deserialize)]
struct StablecoinMetadata {
    symbol: String,
    contract: String,
    #[serde(default)]
    decimals: Option<u32>,
}

#[derive(Clone, Deserialize)]
struct DexMetadata {
    contracts: Option<DexContractsMetadata>,
}

#[derive(Clone, Deserialize)]
struct DexContractsMetadata {
    #[serde(rename = "quoterV2")]
    quoter_v2: Option<String>,
}

#[derive(Clone, Deserialize)]
struct NativeCurrencyMetadata {
    symbol: String,
    decimals: u32,
}

struct CachedChainMetadata {
    metadata: ChainMetadata,
    expires_at: Instant,
}

enum SimulationUpstreamError {
    Reverted(RpcRevert),
    Unavailable(String),
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    use axum::{Json, Router, http::StatusCode, routing::post};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::{
        ChainMetadata, FailedRpcCache, SimulationUpstreamError, fetch_result,
        fetch_simulation_result, first_result, first_simulation_result, parse_rpc_url,
        redacted_rpc_url, response_rpc_url, rpc_domain,
    };

    static TEST_CHAIN_IDS: AtomicU64 = AtomicU64::new(9_000_000_000);

    #[test]
    fn parses_chain_metadata_with_payment_assets() {
        let metadata: ChainMetadata = serde_json::from_value(json!({
            "rpc": ["https://rpc.example.com"],
            "nativeCurrency": { "symbol": "ETH", "decimals": 18 },
            "wrappedNativeToken": "0x1111111111111111111111111111111111111111",
            "stables": [{
                "symbol": "USDC",
                "contract": "0x2222222222222222222222222222222222222222",
                "type": "native"
            }],
            "dex": {
                "contracts": {
                    "quoterV2": "0x3333333333333333333333333333333333333333"
                }
            },
        }))
        .unwrap();

        assert_eq!(metadata.native_currency.unwrap().decimals, 18);
        assert_eq!(metadata.stables[0].decimals, None);
        assert_eq!(
            metadata.wrapped_native_token.as_deref(),
            Some("0x1111111111111111111111111111111111111111")
        );
        assert_eq!(
            metadata
                .dex
                .and_then(|dex| dex.contracts)
                .and_then(|contracts| contracts.quoter_v2)
                .as_deref(),
            Some("0x3333333333333333333333333333333333333333")
        );
    }

    #[test]
    fn keeps_only_safe_https_fallback_urls() {
        assert_eq!(
            parse_rpc_url("https://eth.example.com"),
            Some("https://eth.example.com/".into())
        );
        assert!(parse_rpc_url("http://eth.example.com").is_none());
        assert!(parse_rpc_url("https://127.0.0.1").is_none());
    }

    #[test]
    fn redacts_rpc_url_paths_and_query_parameters_from_logs() {
        assert_eq!(
            redacted_rpc_url("https://eth-mainnet.g.alchemy.com/v2/secret?another=secret"),
            "https://eth-mainnet.g.alchemy.com/…"
        );
    }

    #[test]
    fn extracts_only_the_rpc_domain() {
        assert_eq!(
            rpc_domain("https://eth-mainnet.g.alchemy.com/v2/secret?another=secret"),
            "eth-mainnet.g.alchemy.com"
        );
    }

    #[test]
    fn exposes_only_the_safe_rpc_endpoint_prefix() {
        assert_eq!(
            response_rpc_url("https://arb-mainnet.g.alchemy.com/v2/secret?another=secret"),
            "https://arb-mainnet.g.alchemy.com/v2/***"
        );
        assert_eq!(
            response_rpc_url("https://rpc.example.com/private/path?key=secret"),
            "https://rpc.example.com/***"
        );
    }

    #[test]
    fn cooldown_is_scoped_to_the_chain_url_and_method() {
        let cache = FailedRpcCache::new(Duration::from_secs(30), 2);
        cache.freeze(1, "https://rpc.example.com", "eth_feeHistory");

        assert!(
            cache
                .retry_after(1, "https://rpc.example.com", "eth_feeHistory")
                .is_some()
        );
        assert!(
            cache
                .retry_after(2, "https://rpc.example.com", "eth_feeHistory")
                .is_none()
        );
        assert!(
            cache
                .retry_after(1, "https://rpc.example.com", "eth_gasPrice")
                .is_none()
        );
    }

    #[tokio::test]
    async fn fails_over_immediately_after_a_rate_limited_rpc_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // This needs a mutex, rather than a relaxed atomic: the HTTP response tells the client
        // that the handler finished, but it is not a Rust memory-ordering edge between test
        // runtime threads. Locking here makes the post-response observation deterministic.
        let limited_calls = Arc::new(Mutex::new(0_usize));
        let limited_calls_for_server = Arc::clone(&limited_calls);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/limited",
                        post(move || {
                            let limited_calls = Arc::clone(&limited_calls_for_server);
                            async move {
                                *limited_calls.lock().unwrap() += 1;
                                (StatusCode::TOO_MANY_REQUESTS, "rate limited")
                            }
                        }),
                    )
                    .route(
                        "/",
                        post(|| async {
                            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": "0x64" }))
                        }),
                    ),
            )
            .await
            .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let urls = vec![
            format!("http://{address}/limited"),
            format!("http://{address}"),
        ];
        let chain_id = TEST_CHAIN_IDS.fetch_add(1, Ordering::Relaxed);
        let client = reqwest::Client::new();
        let mut direct_result = fetch_result(&client, &urls[1], "eth_gasPrice", &json!([])).await;
        for _ in 0..2 {
            if direct_result.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            direct_result = fetch_result(&client, &urls[1], "eth_gasPrice", &json!([])).await;
        }
        assert_eq!(direct_result, Ok(json!("0x64")));
        let started = Instant::now();

        let result = first_result(&client, chain_id, "test", &urls, "eth_gasPrice", &json!([]))
            .await
            .unwrap();
        assert_eq!(result.value, json!("0x64"));
        assert_eq!(result.domain, "127.0.0.1");
        assert!(started.elapsed() < Duration::from_secs(1));
        let second_result =
            first_result(&client, chain_id, "test", &urls, "eth_gasPrice", &json!([]))
                .await
                .unwrap();
        assert_eq!(second_result.value, json!("0x64"));
        assert_eq!(*limited_calls.lock().unwrap(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn simulation_fails_over_when_a_node_does_not_support_state_overrides() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/unsupported",
                        post(|| async {
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "error": {
                                    "code": -32602,
                                    "message": "state overrides are unsupported"
                                }
                            }))
                        }),
                    )
                    .route(
                        "/",
                        post(|| async {
                            Json(json!({ "jsonrpc": "2.0", "id": 1, "result": "0x1234" }))
                        }),
                    ),
            )
            .await
            .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let client = reqwest::Client::new();
        let chain_id = TEST_CHAIN_IDS.fetch_add(1, Ordering::Relaxed);
        let result = first_simulation_result(
            &client,
            chain_id,
            "test",
            &[
                format!("http://{address}/unsupported"),
                format!("http://{address}"),
            ],
            "eth_call",
            &json!([]),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(result.value, json!("0x1234"));
        server.abort();
    }

    #[tokio::test]
    async fn simulation_preserves_contract_reverts_without_freezing_the_rpc() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    post(|| async {
                        Json(json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "error": {
                                "code": 3,
                                "message": "execution reverted",
                                "data": "0x08c379a0"
                            }
                        }))
                    }),
                ),
            )
            .await
            .unwrap();
        });

        tokio::time::sleep(Duration::from_millis(10)).await;

        let client = reqwest::Client::new();
        let url = format!("http://{address}");
        let mut result = fetch_simulation_result(&client, &url, "eth_call", &json!([])).await;
        for _ in 0..2 {
            if !matches!(&result, Err(SimulationUpstreamError::Unavailable(_))) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            result = fetch_simulation_result(&client, &url, "eth_call", &json!([])).await;
        }

        assert!(matches!(result, Err(SimulationUpstreamError::Reverted(_))));
        server.abort();
    }
}
