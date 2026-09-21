use std::{future::Future, time::Duration};

use axum::http::HeaderValue;
use serde_json::{Value, json};
use tokio::sync::oneshot;

// All price arithmetic lives in the decision core; this manager owns polling,
// caching, coalescing, and RPC failover.
use vela_relay_core::gas_math::{
    FeeHistory, legacy_price_from_result, parse_quantity, price_from_fee_history, quote_market_tip,
    tiers,
};
pub use vela_relay_core::gas_math::{
    GasPrice, GasPriceError, GasPricePolicy, GasPriceTiers, NetworkGasPrice,
};

use crate::utils::rpc;

use super::{
    cache::{CacheRequest, GasPriceCache},
    chains::{ArbitrumManager, CitreaManager, MantleManager, OptimismManager},
};

const FEE_HISTORY_BLOCK_COUNT: &str = "0x5";
// Still requested, so the call stays byte-identical to the one every upstream
// has been answering, but the reward column is no longer read: the tip is the
// node's `eth_maxPriorityFeePerGas` (`gas_math::market_tip`).
const FEE_HISTORY_PERCENTILES: [u8; 3] = [25, 50, 75];
const DEFAULT_HISTORY_SIZE: usize = 32;
const RESPONSE_BUDGET: Duration = Duration::from_millis(2_800);
const PRICE_CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct GasPriceManager {
    policy: GasPricePolicy,
    cache: GasPriceCache,
    #[expect(
        dead_code,
        reason = "Arbitrum fee tracking is consumed by future pre-verification gas calculators."
    )]
    pub arbitrum: ArbitrumManager,
    #[expect(
        dead_code,
        reason = "Citrea fee tracking is consumed by future pre-verification gas calculators."
    )]
    pub citrea: CitreaManager,
    #[expect(
        dead_code,
        reason = "Mantle fee tracking is consumed by future pre-verification gas calculators."
    )]
    pub mantle: MantleManager,
    #[expect(
        dead_code,
        reason = "Optimism fee tracking is consumed by future pre-verification gas calculators."
    )]
    pub optimism: OptimismManager,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GasPriceQuote {
    pub tiers: GasPriceTiers,
    pub rpc_domain: String,
}

impl Default for GasPriceManager {
    fn default() -> Self {
        Self::new(GasPricePolicy::default(), DEFAULT_HISTORY_SIZE)
    }
}

impl GasPriceManager {
    pub fn new(policy: GasPricePolicy, history_size: usize) -> Self {
        Self {
            policy,
            cache: GasPriceCache::new(PRICE_CACHE_TTL),
            arbitrum: ArbitrumManager::new(history_size),
            citrea: CitreaManager::new(history_size),
            mantle: MantleManager::new(history_size),
            optimism: OptimismManager::new(history_size),
        }
    }

    pub async fn user_operation_gas_prices(
        &self,
        chain_id: u64,
        user_rpc_url: Option<&HeaderValue>,
    ) -> Result<GasPriceQuote, GasPriceError> {
        match self.cache.request(chain_id, user_rpc_url) {
            CacheRequest::Hit(quote) => {
                tracing::debug!(chain_id, rpc_domain = %quote.rpc_domain, "gas price cache hit");
                Ok(quote)
            }
            CacheRequest::Follower(waiter) => wait_for_cached_quote(waiter).await,
            CacheRequest::Leader(leader) => {
                let result = self
                    .fetch_user_operation_gas_prices(chain_id, user_rpc_url)
                    .await;
                leader.complete(result.clone());
                result
            }
        }
    }

    async fn fetch_user_operation_gas_prices(
        &self,
        chain_id: u64,
        user_rpc_url: Option<&HeaderValue>,
    ) -> Result<GasPriceQuote, GasPriceError> {
        with_response_budget(async {
            let (network_price, rpc_domain) =
                self.network_gas_price(chain_id, user_rpc_url).await?;
            Ok(GasPriceQuote {
                tiers: self.tiers(network_price)?,
                rpc_domain,
            })
        })
        .await
    }

    pub async fn network_gas_price(
        &self,
        chain_id: u64,
        user_rpc_url: Option<&HeaderValue>,
    ) -> Result<(NetworkGasPrice, String), GasPriceError> {
        // The tip is read beside the fee history, not after it: it is needed
        // on every EIP-1559 quote now, and in parallel it costs no extra
        // round trip.
        let (fee_history, max_priority_fee_per_gas) = tokio::join!(
            rpc::call(
                chain_id,
                user_rpc_url,
                "eth_feeHistory",
                json!([FEE_HISTORY_BLOCK_COUNT, "latest", FEE_HISTORY_PERCENTILES]),
            ),
            quantity(chain_id, user_rpc_url, "eth_maxPriorityFeePerGas"),
        );
        if let Ok(response) = fee_history {
            match self
                .eip1559_price(
                    response.value,
                    max_priority_fee_per_gas,
                    chain_id,
                    user_rpc_url,
                )
                .await
            {
                Ok(price) => return Ok((price, response.domain)),
                Err(error) => tracing::warn!(?error, "could not calculate EIP-1559 gas price"),
            }
        }

        self.legacy_gas_price(chain_id, user_rpc_url).await
    }

    /// The three reported tiers. No policy is involved any more: a tier is
    /// the cap it submits at, and every number reported for it derives from
    /// that cap (`gas_math::tiers`).
    pub fn tiers(&self, network_price: NetworkGasPrice) -> Result<GasPriceTiers, GasPriceError> {
        tiers(network_price)
    }

    /// The next block's base fee from the fee history, and the tip resolved
    /// exactly as the executor resolves the one it signs with
    /// (`gas_math::quote_market_tip` → `gas_math::market_tip`).
    async fn eip1559_price(
        &self,
        result: Value,
        max_priority_fee_per_gas: Option<u128>,
        chain_id: u64,
        user_rpc_url: Option<&HeaderValue>,
    ) -> Result<NetworkGasPrice, GasPriceError> {
        let fee_history = serde_json::from_value::<FeeHistory>(result)
            .map_err(|_| GasPriceError::InvalidUpstreamResponse)?;
        // `eth_gasPrice` is consulted only when the node named no tip — the
        // executor's rule, and the only case `market_tip` reads it.
        let legacy_gas_price = match max_priority_fee_per_gas {
            Some(_) => None,
            None => quantity(chain_id, user_rpc_url, "eth_gasPrice").await,
        };
        let tip = quote_market_tip(
            &fee_history,
            max_priority_fee_per_gas,
            legacy_gas_price,
            self.policy.priority_fee_divisor,
        )?;

        price_from_fee_history(&fee_history, tip)
    }

    async fn legacy_gas_price(
        &self,
        chain_id: u64,
        user_rpc_url: Option<&HeaderValue>,
    ) -> Result<(NetworkGasPrice, String), GasPriceError> {
        let response = rpc::call(
            chain_id,
            user_rpc_url,
            "eth_gasPrice",
            Value::Array(Vec::new()),
        )
        .await
        .map_err(|()| GasPriceError::NoPriceAvailable)?;
        Ok((legacy_price_from_result(response.value)?, response.domain))
    }
}

/// One no-argument quantity read (`eth_maxPriorityFeePerGas`,
/// `eth_gasPrice`). `None` when the call failed or did not return a quantity —
/// the "no answer" `gas_math::market_tip` distinguishes from a zero.
async fn quantity(chain_id: u64, user_rpc_url: Option<&HeaderValue>, method: &str) -> Option<u128> {
    let response = rpc::call(chain_id, user_rpc_url, method, Value::Array(Vec::new()))
        .await
        .ok()?;
    parse_quantity(response.value.as_str()?).ok()
}

async fn wait_for_cached_quote(
    waiter: oneshot::Receiver<Result<GasPriceQuote, GasPriceError>>,
) -> Result<GasPriceQuote, GasPriceError> {
    tokio::time::timeout(RESPONSE_BUDGET, waiter)
        .await
        .map_err(|_| GasPriceError::ResponseDeadlineExceeded)?
        .unwrap_or(Err(GasPriceError::NoPriceAvailable))
}

async fn with_response_budget<T>(
    operation: impl Future<Output = Result<T, GasPriceError>>,
) -> Result<T, GasPriceError> {
    tokio::time::timeout(RESPONSE_BUDGET, operation)
        .await
        .map_err(|_| GasPriceError::ResponseDeadlineExceeded)?
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{GasPriceError, RESPONSE_BUDGET, with_response_budget};

    // Price arithmetic tests moved to `vela_relay_core::gas_math`.
    #[tokio::test(start_paused = true)]
    async fn enforces_the_total_response_budget() {
        let result = with_response_budget(async {
            tokio::time::sleep(RESPONSE_BUDGET + Duration::from_millis(1)).await;
            Ok::<(), GasPriceError>(())
        })
        .await;

        assert_eq!(result, Err(GasPriceError::ResponseDeadlineExceeded));
    }
}
