use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{app::AppState, utils::rpc};

use vela_relay_core::treasury::{self, ProbeFailure};

#[derive(Serialize)]
struct TreasuryAddress {
    address: String,
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

/// `GET /v1/treasury/{chain_id}`. The core decides which balance to read (the
/// native coin, or pathUSD on Tempo) and what it means; this handler performs
/// the one read and renders the core's answer — the same bytes the Cloudflare
/// shell renders.
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
    let read = match treasury::balance_read(chain_id, address) {
        Ok(read) => read,
        Err(failure) => return probe_failure(failure),
    };

    let result = rpc::call(
        chain_id,
        headers.get(rpc::USER_RPC_URL_HEADER),
        read.method,
        read.params,
    )
    .await
    .map(|result| result.value);

    respond(chain_id, address, result)
}

/// The RPC result → HTTP hop. A balance we could not read is NOT a balance of
/// zero: both an unreachable RPC and an unreadable answer are a 503.
fn respond(chain_id: u64, address: &str, result: Result<Value, ()>) -> Response {
    let status = result
        .map_err(|()| ProbeFailure::RpcUnavailable)
        .and_then(|value| treasury::treasury_status(chain_id, address, &value));

    match status {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(failure) => probe_failure(failure),
    }
}

fn probe_failure(failure: ProbeFailure) -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, failure.message())
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, response::Response};
    use serde_json::{Value, json};

    use super::respond;
    use crate::utils::config::DEFAULT_TREASURY_FLOOR_WEI;
    use vela_relay_core::treasury::{NATIVE_TREASURY_FLOOR, balance_read, quantity_is_below};

    const TREASURY: &str = "0x3e59292e18417f814112f731e7163534c6d2fe3c";

    async fn body(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
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

    /// The production answer of 2026-10-03, corrected: Tempo's treasury held no
    /// pathUSD, so it needs a bootstrap — whatever `eth_getBalance` says.
    #[tokio::test]
    async fn tempo_answers_with_the_treasurys_path_usd() {
        assert_eq!(balance_read(4_217, TREASURY).unwrap().method, "eth_call");

        let (status, body) = body(respond(
            4_217,
            TREASURY,
            Ok(json!(format!("0x{}", "0".repeat(64)))),
        ))
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "chainId": 4217,
                "address": TREASURY,
                "asset": "pathUSD",
                "balance": "0x0",
                "floor": "0x86470",
                "bootstrapNeeded": true,
            })
        );
    }

    #[tokio::test]
    async fn other_chains_keep_the_native_answer() {
        assert_eq!(
            balance_read(196, TREASURY).unwrap().method,
            "eth_getBalance"
        );

        let (status, body) = body(respond(196, TREASURY, Ok(json!("0x0")))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "chainId": 196,
                "address": TREASURY,
                "asset": "native",
                "balance": "0x0",
                "floor": "0x5af3107a4000",
                "bootstrapNeeded": true,
            })
        );
    }

    #[tokio::test]
    async fn an_unreadable_balance_is_a_503_never_a_zero() {
        for (chain_id, result, message) in [
            (4_217, Err(()), "treasury RPC is unavailable"),
            (196, Err(()), "treasury RPC is unavailable"),
            (
                4_217,
                Ok(json!("0x")),
                "treasury RPC returned an invalid balance",
            ),
            (
                // Tempo's native placeholder is not a pathUSD balance.
                4_217,
                Ok(json!(
                    "0x9612084f0316e0ebd5182f398e5195a51b5ca47667d4c9b26c9b26c9b26c9b2"
                )),
                "treasury RPC returned an invalid balance",
            ),
            (
                196,
                Ok(json!("15")),
                "treasury RPC returned an invalid balance",
            ),
        ] {
            let (status, body) = body(respond(chain_id, TREASURY, result)).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(body, json!({ "error": message }));
        }
    }
}
