use axum::http::HeaderValue;
use serde_json::Value;

use vela_relay_core::cost::BillingTerms;

use crate::{
    app::rpc::types::{RpcError, RpcResponse},
    gas_price::{GasPriceError, GasPriceManager, GasPriceQuote},
};

pub async fn handle(
    id: Value,
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    gas_price_manager: GasPriceManager,
    terms: BillingTerms,
) -> (RpcResponse<Value>, Option<String>) {
    match gas_price_manager
        .user_operation_gas_prices(chain_id, user_rpc_url)
        .await
    {
        Ok(quote) => success_response(id, chain_id, &terms, quote),
        Err(error) => {
            tracing::warn!(?error, "could not estimate user operation gas prices");
            (RpcResponse::error(id, response_error(error)), None)
        }
    }
}

fn success_response(
    id: Value,
    chain_id: u64,
    terms: &BillingTerms,
    quote: GasPriceQuote,
) -> (RpcResponse<Value>, Option<String>) {
    (
        RpcResponse::result(
            id,
            serde_json::to_value(vela_relay_core::wire::gas_price_tiers(
                quote.tiers,
                chain_id,
                terms,
            ))
            .expect("gas price response must serialize"),
        ),
        Some(quote.rpc_domain),
    )
}

fn response_error(error: GasPriceError) -> RpcError {
    match error {
        GasPriceError::ResponseDeadlineExceeded => RpcError::gas_price_timeout(),
        _ => RpcError::gas_price_unavailable(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use vela_relay_core::{
        cost::BillingTerms,
        gas_math::{NetworkGasPrice, TierTips, tiers},
        wire::gas_price_tiers,
    };

    use crate::gas_price::GasPriceError;

    use super::response_error;

    #[test]
    fn converts_gas_price_tiers_to_the_pimlico_response_shape() {
        // Straight through the tier arithmetic, so the wire shape is checked
        // against the numbers a client will actually be quoted: base 100,
        // market tip 40, tier tips 40 / 50 / 80 read from rewards, the default
        // 1.1× markup on Ethereum.
        let rows = tiers(NetworkGasPrice {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 40,
            tier_tips: TierTips {
                slow: 40,
                standard: 50,
                fast: 80,
            },
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(gas_price_tiers(rows, 1, &BillingTerms::default())).unwrap(),
            json!({
                "slow": {
                    "maxFeePerGas": "0xbe",           // 1.5 × 100 + 40  = 190
                    "maxPriorityFeePerGas": "0x28",   //                   40
                    "networkFeePerGas": "0x82",       // 0.9 × 100 + 40  = 130 (frozen R)
                    "relayerFeePerGas": "0x3c",       //                   60
                    "inBandFeePerGas": "0xd1"         // 190 × 1.1       = 209
                },
                "standard": {
                    "maxFeePerGas": "0xc8",           // 1.5 × 100 + 50  = 200
                    "maxPriorityFeePerGas": "0x32",   //                   50
                    "networkFeePerGas": "0xaa",       // 1.2 × 100 + 50  = 170
                    "relayerFeePerGas": "0x1e",       //                   30
                    "inBandFeePerGas": "0xdc"         // 200 × 1.1       = 220
                },
                "fast": {
                    "maxFeePerGas": "0xff",           // 1.75 × 100 + 80 = 255
                    "maxPriorityFeePerGas": "0x50",   //                   80
                    "networkFeePerGas": "0x104",      // 1.8 × 100 + 80  = 260
                    "relayerFeePerGas": "0x0",        // saturating
                    "inBandFeePerGas": "0x119"        // 255 × 1.1      ⌈280.5⌉
                }
            })
        );
        // Where the estimate returns no `settlementGas`, no `inBandFeePerGas`
        // is published either: Arbitrum bills the outer limit.
        let arbitrum =
            serde_json::to_value(gas_price_tiers(rows, 42_161, &BillingTerms::default())).unwrap();
        assert!(arbitrum["standard"].get("inBandFeePerGas").is_none());
        assert_eq!(arbitrum["standard"]["maxFeePerGas"], json!("0xc8"));
    }

    #[test]
    fn returns_a_specific_error_when_the_response_deadline_is_exceeded() {
        let error = response_error(GasPriceError::ResponseDeadlineExceeded);

        assert_eq!(error.code, -32000);
        assert_eq!(error.message, "gas price RPC request timed out");
    }

    #[test]
    fn keeps_the_generic_error_for_non_timeout_failures() {
        let error = response_error(GasPriceError::NoPriceAvailable);

        assert_eq!(error.code, -32000);
        assert_eq!(error.message, "all gas price RPC sources failed");
    }
}
