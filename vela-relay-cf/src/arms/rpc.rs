//! Chain JSON-RPC over fetch with the docker shell's failover ORDER
//! (user-header URL → Alchemy → directory fallbacks). Failover/cooldown are
//! shell-owned transport policy (Constitution, Shell-owned concerns).

use serde_json::{Value, json};
use vela_relay_core::{
    chain_directory::Listing,
    rpc_host::RpcHostPolicy,
    treasury::{ClientRpc, ProbeFailure},
};
use worker::Env;

use super::market;
use crate::config::CfConfig;

/// The same request header the docker shell honors for caller-supplied RPCs.
pub const USER_RPC_URL_HEADER: &str = "x-vela-rpc-url";

/// Mirror of the docker shell's `RpcCallResult` surface consumed here: the
/// JSON-RPC result plus the answering host (for `x-vela-rpc-domain`).
pub struct CallResult {
    pub value: Value,
    pub domain: String,
}

pub async fn call(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    method: &str,
    params: Value,
) -> Result<CallResult, ()> {
    let policy = config.rpc_host_policy;
    if let Some(url) = user_rpc_url
        .map(str::trim)
        .filter(|url| usable_rpc_url(url, policy))
    {
        if let Ok(Some(value)) = market::json_rpc(url, method, &params).await {
            return Ok(CallResult {
                value,
                domain: rpc_domain(url),
            });
        }
    } else if user_rpc_url.is_some() {
        worker::console_warn!("ignored invalid user RPC URL header");
    }

    if let Some(api_key) = &config.alchemy_api_key
        && let Some(url) = vela_relay_core::alchemy::rpc_url(chain_id, api_key)
        && let Ok(Some(value)) = market::json_rpc(&url, method, &params).await
    {
        return Ok(CallResult {
            domain: rpc_domain(&url),
            value,
        });
    }

    let fallback_urls = match market::fallback_rpc_urls(env, chain_id, policy).await {
        Ok(urls) => urls,
        Err(error) => {
            worker::console_warn!("could not fetch fallback RPC URLs: {error}");
            return Err(());
        }
    };
    for url in fallback_urls {
        if let Ok(Some(value)) = market::json_rpc(&url, method, &params).await {
            return Ok(CallResult {
                domain: rpc_domain(&url),
                value,
            });
        }
    }
    Err(())
}

/// The treasury probe's read (docker `rpc::treasury_read`): the same sources
/// in the same order as [`call`], but when there is no balance it says why —
/// "this chain cannot be served" (`404`) or "not now" (`503`), decided by the
/// core ([`vela_relay_core::treasury::unreadable`]). The directory is asked
/// first: a chain it does not list cannot be quoted or executed, however well
/// the wallet's own RPC answers.
pub async fn treasury_read(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    method: &str,
    params: Value,
) -> Result<CallResult, ProbeFailure> {
    let policy = config.rpc_host_policy;
    let (listing, directory_urls) = match market::fallback_rpc_urls(env, chain_id, policy).await {
        Ok(urls) => (Listing::Listed, urls),
        Err(market::MetadataError::NotListed) => return Err(ProbeFailure::NotListed),
        Err(error) => {
            worker::console_warn!(
                "could not fetch the chain directory for the treasury probe: chain_id={chain_id} error={error}"
            );
            (Listing::Unavailable, Vec::new())
        }
    };

    let header_url = user_rpc_url
        .map(str::trim)
        .filter(|url| usable_rpc_url(url, policy));
    let client_rpc = match (user_rpc_url, header_url) {
        (None, _) => ClientRpc::Absent,
        (Some(_), Some(_)) => ClientRpc::Used,
        (Some(_), None) => {
            worker::console_warn!("ignored invalid user RPC URL header");
            ClientRpc::Refused
        }
    };

    let mut sources: Vec<String> = header_url.into_iter().map(str::to_owned).collect();
    if let Some(api_key) = &config.alchemy_api_key
        && let Some(url) = vela_relay_core::alchemy::rpc_url(chain_id, api_key)
    {
        sources.push(url);
    }
    sources.extend(directory_urls);

    let tried = !sources.is_empty();
    for url in sources {
        if let Ok(Some(value)) = market::json_rpc(&url, method, &params).await {
            return Ok(CallResult {
                domain: rpc_domain(&url),
                value,
            });
        }
    }
    Err(vela_relay_core::treasury::unreadable(
        listing, client_rpc, tried,
    ))
}

fn rpc_domain(value: &str) -> String {
    worker::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<unknown>".into())
}

/// Simulation failover with the docker semantics: an upstream JSON-RPC error
/// classified as a contract revert (core `estimate::is_execution_revert`)
/// stops the failover and surfaces; anything else tries the next source.
pub async fn call_simulation(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    method: &str,
    params: Value,
) -> Result<CallResult, vela_relay_core::estimate::SimulationCallError> {
    use vela_relay_core::estimate::{SimulationCallError, is_execution_revert};

    let policy = config.rpc_host_policy;
    let mut sources: Vec<String> = Vec::new();
    if let Some(url) = user_rpc_url
        .map(str::trim)
        .filter(|url| usable_rpc_url(url, policy))
    {
        sources.push(url.to_owned());
    } else if user_rpc_url.is_some() {
        worker::console_warn!("ignored invalid user RPC URL header");
    }
    if let Some(api_key) = &config.alchemy_api_key
        && let Some(url) = vela_relay_core::alchemy::rpc_url(chain_id, api_key)
    {
        sources.push(url);
    }
    if let Ok(fallback_urls) = market::fallback_rpc_urls(env, chain_id, policy).await {
        sources.extend(fallback_urls);
    }

    for url in sources {
        match market::json_rpc_simulation(&url, method, &params).await {
            Ok(market::SimulationReply::Result(value)) => {
                return Ok(CallResult {
                    domain: rpc_domain(&url),
                    value,
                });
            }
            Ok(market::SimulationReply::UpstreamError(revert)) => {
                if is_execution_revert(&revert) {
                    return Err(SimulationCallError::Reverted(revert));
                }
                // Non-revert JSON-RPC error (rate limit, missing state
                // override support, …): try the next source.
            }
            Err(_) => {}
        }
    }
    Err(SimulationCallError::Unavailable)
}

/// `decimals()` via `eth_call`, exactly as the docker arm: ≤ 38 accepted.
pub async fn erc20_decimals(
    config: &CfConfig,
    env: &Env,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    token: &str,
) -> Result<u32, ()> {
    let result = call(
        config,
        env,
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

/// Every URL the relay did not choose itself — the wallet's header and the
/// directory's list — passes here (docker `parse_rpc_url`): public `https`
/// only unless the operator opted in ([`vela_relay_core::rpc_host`]), and never
/// a directory template still holding a `${API_KEY}` placeholder.
///
/// Before this, the header accepted any `http` or `https` URL here while the
/// docker shell refused plain `http` and private hosts — two deployments of
/// one relay answering the same request differently.
pub fn usable_rpc_url(url: &str, policy: RpcHostPolicy) -> bool {
    if url.contains("${") {
        return false;
    }
    let Ok(parsed) = worker::Url::parse(url) else {
        return false;
    };
    parsed
        .host_str()
        .is_some_and(|host| policy.allows(parsed.scheme(), host))
}
