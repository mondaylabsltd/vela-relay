use axum::http::HeaderValue;
use serde_json::Value;

use crate::{
    app::rpc::types::{GasPriceTier, RpcError, RpcResponse, UserOperationGasPrice},
    gas_price::{GasPrice, GasPriceError, GasPriceManager, GasPriceQuote, GasPriceTiers},
};

pub async fn handle(
    id: Value,
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    gas_price_manager: GasPriceManager,
) -> (RpcResponse<Value>, Option<String>) {
    match gas_price_manager
        .user_operation_gas_prices(chain_id, user_rpc_url)
        .await
    {
        Ok(quote) => success_response(id, quote),
        Err(error) => {
            tracing::warn!(?error, "could not estimate user operation gas prices");
            (RpcResponse::error(id, response_error(error)), None)
        }
    }
}

fn success_response(id: Value, quote: GasPriceQuote) -> (RpcResponse<Value>, Option<String>) {
    (
        RpcResponse::result(
            id,
            serde_json::to_value(to_rpc_result(quote.tiers))
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

fn to_rpc_result(gas_prices: GasPriceTiers) -> UserOperationGasPrice {
    UserOperationGasPrice {
        slow: to_rpc_tier(gas_prices.slow),
        standard: to_rpc_tier(gas_prices.standard),
        fast: to_rpc_tier(gas_prices.fast),
    }
}

fn to_rpc_tier(gas_price: GasPrice) -> GasPriceTier {
    GasPriceTier {
        max_fee_per_gas: quantity(gas_price.max_fee_per_gas),
        max_priority_fee_per_gas: quantity(gas_price.max_priority_fee_per_gas),
        network_fee_per_gas: quantity(gas_price.network_fee_per_gas),
        relayer_fee_per_gas: quantity(gas_price.relayer_fee_per_gas),
    }
}

fn quantity(value: u128) -> String {
    format!("0x{value:x}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use vela_relay_core::gas_math::{NetworkGasPrice, tiers};

    use crate::gas_price::GasPriceError;

    use super::{response_error, to_rpc_result};

    #[test]
    fn converts_gas_price_tiers_to_the_pimlico_response_shape() {
        // Straight through the tier arithmetic, so the wire shape is checked
        // against the numbers a client will actually be quoted rather than
        // against invented ones: base 100, market tip 40.
        //
        // `maxPriorityFeePerGas` differs per tier, and that is the point: it
        // is the tip the relay will SIGN that tier with (1.0 / 1.25 / 2.0 ×
        // the market tip), not the raw market reading. A wallet shown one tip
        // and charged for another is the same defect as a tier that moved the
        // cap and left the tip alone.
        let result = to_rpc_result(
            tiers(NetworkGasPrice {
                base_fee_per_gas: 100,
                max_priority_fee_per_gas: 40,
            })
            .unwrap(),
        );

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({
                "slow": {
                    "maxFeePerGas": "0xbe",           // 1.5 × 100 + 40 = 190
                    "maxPriorityFeePerGas": "0x28",   // 1.00 × 40      =  40
                    "networkFeePerGas": "0x82",       // 0.9 × 100 + 40 = 130
                    "relayerFeePerGas": "0x3c"        // 0.6 × 100      =  60
                },
                "standard": {
                    "maxFeePerGas": "0xfa",           // 2.0 × 100 + 50 = 250
                    "maxPriorityFeePerGas": "0x32",   // 1.25 × 40      =  50
                    "networkFeePerGas": "0xaa",       // 1.2 × 100 + 50 = 170
                    "relayerFeePerGas": "0x50"        // 0.8 × 100      =  80
                },
                "fast": {
                    "maxFeePerGas": "0x17c",          // 3.0 × 100 + 80 = 380
                    "maxPriorityFeePerGas": "0x50",   // 2.00 × 40      =  80
                    "networkFeePerGas": "0x104",      // 1.8 × 100 + 80 = 260
                    "relayerFeePerGas": "0x78"        // 1.2 × 100      = 120
                }
            })
        );
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
