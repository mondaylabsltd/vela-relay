use std::{future::Future, time::Duration};

use axum::http::HeaderValue;
use serde_json::{Value, json};
use tokio::sync::oneshot;

// All price arithmetic lives in the decision core; this manager owns polling,
// caching, coalescing, and RPC failover.
use vela_relay_core::gas_math::{
    FeeHistory, fallback_priority_fee, legacy_price_from_result, median_priority_fee,
    parse_quantity, price_from_fee_history, tiers,
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
        if let Ok(response) = rpc::call(
            chain_id,
            user_rpc_url,
            "eth_feeHistory",
            json!([FEE_HISTORY_BLOCK_COUNT, "latest", FEE_HISTORY_PERCENTILES]),
        )
        .await
        {
            match self
                .eip1559_price(response.value, chain_id, user_rpc_url)
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

    async fn eip1559_price(
        &self,
        result: Value,
        chain_id: u64,
        user_rpc_url: Option<&HeaderValue>,
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
            _ => self.priority_fee(chain_id, user_rpc_url, base_fee).await?,
        };

        price_from_fee_history(&fee_history, priority_fee)
    }

    async fn priority_fee(
        &self,
        chain_id: u64,
        user_rpc_url: Option<&HeaderValue>,
        base_fee: u128,
    ) -> Result<u128, GasPriceError> {
        if let Ok(response) = rpc::call(
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

        Ok(fallback_priority_fee(
            base_fee,
            self.policy.priority_fee_divisor,
        ))
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
