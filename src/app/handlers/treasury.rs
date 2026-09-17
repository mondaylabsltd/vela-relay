use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{app::AppState, utils::rpc};

use vela_relay_core::treasury::{NATIVE_TREASURY_FLOOR, quantity_is_below};

#[derive(Serialize)]
struct TreasuryAddress {
    address: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TreasuryStatus {
    chain_id: u64,
    address: String,
    asset: TreasuryAsset,
    balance: String,
    floor: &'static str,
    bootstrap_needed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum TreasuryAsset {
    Native,
}

pub async fn address(State(state): State<AppState>) -> Response {
    let Some(address) = state.settlement_recipient() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "settlement recipient is not configured",
        );
    };

    (
        StatusCode::OK,
        Json(TreasuryAddress {
            address: address.into(),
        }),
    )
        .into_response()
}

pub async fn status(
    State(state): State<AppState>,
    Path(chain_id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    let Some(address) = state.settlement_recipient() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "settlement recipient is not configured",
        );
    };

    let balance = match rpc::call(
        chain_id,
        headers.get(rpc::USER_RPC_URL_HEADER),
        "eth_getBalance",
        json!([address, "latest"]),
    )
    .await
    {
        Ok(result) => result,
        Err(()) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "treasury RPC is unavailable",
            );
        }
    };
    let balance = match parse_quantity(&balance.value) {
        Ok(balance) => balance,
        Err(()) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "treasury RPC returned an invalid balance",
            );
        }
    };

    (
        StatusCode::OK,
        Json(TreasuryStatus {
            chain_id,
            address: address.into(),
            asset: TreasuryAsset::Native,
            bootstrap_needed: quantity_is_below(&balance, NATIVE_TREASURY_FLOOR),
            balance,
            floor: NATIVE_TREASURY_FLOOR,
        }),
    )
        .into_response()
}

/// The JSON hop only; the grammar itself is the core's, shared with the
/// Cloudflare shell so both report the same floor (`treasury::parse_quantity`).
fn parse_quantity(value: &Value) -> Result<String, ()> {
    vela_relay_core::treasury::parse_quantity(value.as_str().ok_or(())?)
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{NATIVE_TREASURY_FLOOR, parse_quantity, quantity_is_below};
    use crate::utils::config::DEFAULT_TREASURY_FLOOR_WEI;

    #[test]
    fn validates_and_normalizes_rpc_quantities() {
        assert_eq!(parse_quantity(&json!("0x000F")), Ok("0x000f".into()));
        assert!(parse_quantity(&json!("0x")).is_err());
        assert!(parse_quantity(&json!("15")).is_err());
    }

    #[test]
    fn compares_arbitrary_size_hex_balances_against_the_floor() {
        assert_eq!(
            NATIVE_TREASURY_FLOOR,
            format!("0x{DEFAULT_TREASURY_FLOOR_WEI:x}")
        );
        assert!(quantity_is_below("0x0", NATIVE_TREASURY_FLOOR));
        assert!(quantity_is_below("0x5af3107a3fff", NATIVE_TREASURY_FLOOR));
        assert!(!quantity_is_below("0x5af3107a4000", NATIVE_TREASURY_FLOOR));
        assert!(!quantity_is_below(
            "0x10000000000000000",
            NATIVE_TREASURY_FLOOR
        ));
    }
}
