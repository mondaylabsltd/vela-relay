use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;

use crate::app::AppState;

mod handlers;
pub mod types;

pub(crate) use handlers::supported_entry_points::SUPPORTED_ENTRY_POINTS;

use types::{
    EstimateUserOperationGasParams, GetInBandGasQuoteParams, GetUserOperationByHashParams,
    GetUserOperationReceiptParams, GetUserOperationStatusParams, RpcError, RpcMethod, RpcResponse,
    SendUserOperationParams,
};

pub const RPC_DOMAIN_RESPONSE_HEADER: &str = "x-vela-rpc-domain";

type RpcHttpResponse = (HeaderMap, Json<RpcResponse<Value>>);

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chain_id): Path<u64>,
    body: Bytes,
) -> RpcHttpResponse {
    // Envelope parsing, version check, and method/params validation are the
    // core's wire vocabulary (spec 002): both shells share these bytes.
    let request = match vela_relay_core::wire::parse_envelope(&body) {
        Ok(request) => request,
        Err(error_response) => return response(error_response),
    };

    let method = match vela_relay_core::wire::validate_call(&request.method, request.params.clone())
    {
        Ok(method) => method,
        Err(error) => return response(RpcResponse::error(request.id, error)),
    };

    tracing::info!(
        chain_id,
        method = method.as_str(),
        "bundler RPC request received"
    );

    let gas_price = state.gas_price();

    match method {
        RpcMethod::SupportedEntryPoints => {
            response(handlers::supported_entry_points::handle(request.id))
        }
        RpcMethod::GetUserOperationGasPrice => {
            let (response_body, rpc_domain) = handlers::user_operation_gas_price::handle(
                request.id,
                chain_id,
                headers.get(crate::utils::rpc::USER_RPC_URL_HEADER),
                gas_price,
            )
            .await;
            response_with_rpc_domain(response_body, rpc_domain)
        }
        RpcMethod::GetInBandGasQuote => {
            let params = match serde_json::from_value::<GetInBandGasQuoteParams>(request.params) {
                Ok(params) => params,
                Err(error) => {
                    return response(RpcResponse::error(
                        request.id,
                        RpcError::invalid_params(error.to_string()),
                    ));
                }
            };
            let (response_body, rpc_domain) = handlers::in_band_gas_quote::handle(
                request.id,
                chain_id,
                headers.get(crate::utils::rpc::USER_RPC_URL_HEADER),
                &state,
                params,
            )
            .await;
            response_with_rpc_domain(response_body, rpc_domain)
        }
        RpcMethod::EstimateUserOperationGas => {
            let params =
                match serde_json::from_value::<EstimateUserOperationGasParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => {
                        return response(RpcResponse::error(
                            request.id,
                            RpcError::invalid_params(error.to_string()),
                        ));
                    }
                };
            let (response_body, rpc_domain) = handlers::estimate_user_operation_gas::handle(
                request.id,
                chain_id,
                headers.get(crate::utils::rpc::USER_RPC_URL_HEADER),
                state.billing_terms(),
                params,
            )
            .await;
            response_with_rpc_domain(response_body, rpc_domain)
        }
        RpcMethod::GetUserOperationStatus => {
            let params =
                match serde_json::from_value::<GetUserOperationStatusParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => {
                        return response(RpcResponse::error(
                            request.id,
                            RpcError::invalid_params(error.to_string()),
                        ));
                    }
                };
            response(handlers::user_operation_status::get_status(request.id, &state, params).await)
        }
        RpcMethod::GetUserOperationByHash => {
            let params =
                match serde_json::from_value::<GetUserOperationByHashParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => {
                        return response(RpcResponse::error(
                            request.id,
                            RpcError::invalid_params(error.to_string()),
                        ));
                    }
                };
            response(handlers::user_operation_status::get_by_hash(request.id, &state, params).await)
        }
        RpcMethod::GetUserOperationReceipt => {
            let params =
                match serde_json::from_value::<GetUserOperationReceiptParams>(request.params) {
                    Ok(params) => params,
                    Err(error) => {
                        return response(RpcResponse::error(
                            request.id,
                            RpcError::invalid_params(error.to_string()),
                        ));
                    }
                };
            response(handlers::user_operation_status::get_receipt(request.id, &state, params).await)
        }
        RpcMethod::SendUserOperation => {
            let params = match serde_json::from_value::<SendUserOperationParams>(request.params) {
                Ok(params) => params,
                Err(error) => {
                    return response(RpcResponse::error(
                        request.id,
                        RpcError::invalid_params(error.to_string()),
                    ));
                }
            };
            response(
                handlers::send_user_operation::handle(
                    request.id,
                    chain_id,
                    headers.get(crate::utils::rpc::USER_RPC_URL_HEADER),
                    &state,
                    params,
                )
                .await,
            )
        }
    }
}

fn response(response: RpcResponse<Value>) -> RpcHttpResponse {
    (HeaderMap::new(), Json(response))
}

fn response_with_rpc_domain(
    response_body: RpcResponse<Value>,
    rpc_domain: Option<String>,
) -> RpcHttpResponse {
    let mut headers = HeaderMap::new();

    if let Some(rpc_domain) = rpc_domain {
        match HeaderValue::try_from(rpc_domain.as_str()) {
            Ok(value) => {
                headers.insert(HeaderName::from_static(RPC_DOMAIN_RESPONSE_HEADER), value);
            }
            Err(error) => tracing::warn!(?error, "could not add RPC domain response header"),
        }
    }

    (headers, Json(response_body))
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::post,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::{RPC_DOMAIN_RESPONSE_HEADER, RpcResponse, handle, response_with_rpc_domain};
    use crate::app::AppState;
    use vela_relay_core::wire::validate_call;

    fn router() -> Router {
        Router::new()
            .route("/{chain_id}", post(handle))
            .with_state(AppState::with_settlement_recipient(&[], None))
    }

    #[tokio::test]
    async fn returns_supported_entry_points() {
        let response = router()
            .oneshot(
                Request::post("/1")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 7,
                            "method": "eth_supportedEntryPoints",
                            "params": [],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(
            response["result"],
            json!(["0x0000000071727De22E5E9d8BAf0edAc6f37da032"])
        );
    }

    #[tokio::test]
    async fn rejects_invalid_method_parameters_with_a_json_rpc_error() {
        let response = router()
            .oneshot(
                Request::post("/1")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": "request-1",
                            "method": "eth_getUserOperationReceipt",
                            "params": [],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["id"], "request-1");
        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn rejects_native_prefund_user_operations_before_any_upstream_call() {
        let response = router()
            .oneshot(
                Request::post("/1")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 8,
                            "method": "eth_sendUserOperation",
                            "params": [
                                {
                                    "sender": "0x1111111111111111111111111111111111111111",
                                    "nonce": "0x0",
                                    "callData": "0x",
                                    "callGasLimit": "0x5208",
                                    "verificationGasLimit": "0x10000",
                                    "preVerificationGas": "0x1000",
                                    "maxFeePerGas": "0x1",
                                    "maxPriorityFeePerGas": "0x0",
                                    "signature": "0x1234"
                                },
                                "0x0000000071727De22E5E9d8BAf0edAc6f37da032"
                            ]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["id"], 8);
        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(
            response["error"]["data"],
            "in-band UserOperations must set maxFeePerGas and maxPriorityFeePerGas to 0x0"
        );
    }

    #[test]
    fn adds_the_selected_rpc_domain_to_the_response_header() {
        let (headers, _) = response_with_rpc_domain(
            RpcResponse::result(Value::Null, Value::Null),
            Some("rpc.example.com".into()),
        );

        assert_eq!(headers[RPC_DOMAIN_RESPONSE_HEADER], "rpc.example.com");
    }

    #[test]
    fn parses_a_v0_7_user_operation_for_submission() {
        let method = validate_call(
            "eth_sendUserOperation",
            json!([
                {
                    "sender": "0x1111111111111111111111111111111111111111",
                    "nonce": "0x0",
                    "callData": "0x",
                    "callGasLimit": "0x5208",
                    "verificationGasLimit": "0x10000",
                    "preVerificationGas": "0x1000",
                    "maxFeePerGas": "0x3b9aca00",
                    "maxPriorityFeePerGas": "0x3b9aca00",
                    "signature": "0x"
                },
                "0x2222222222222222222222222222222222222222"
            ]),
        )
        .unwrap();

        assert_eq!(method.as_str(), "eth_sendUserOperation");
    }

    #[test]
    fn preserves_the_tempo_path_usd_fee_token_extension() {
        let method = validate_call(
            "eth_sendUserOperation",
            json!([
                {
                    "sender": "0x1111111111111111111111111111111111111111",
                    "nonce": "0x0",
                    "callData": "0x",
                    "callGasLimit": "0x5208",
                    "verificationGasLimit": "0x10000",
                    "preVerificationGas": "0x1000",
                    "maxFeePerGas": "0x0",
                    "maxPriorityFeePerGas": "0x0",
                    "signature": "0x1234",
                    "feeToken": "0x20c0000000000000000000000000000000000000"
                },
                "0x2222222222222222222222222222222222222222"
            ]),
        )
        .unwrap();

        assert_eq!(method.as_str(), "eth_sendUserOperation");
    }

    #[test]
    fn accepts_the_in_band_gas_quote_request_shape() {
        assert!(
            validate_call(
                "vela_getInBandGasQuote",
                json!([{
                    "safeAddress": "0x14fB1fB21751E29F7Ec48dC450017552E3D1eA5c",
                }])
            )
            .is_ok()
        );
        assert!(validate_call("vela_getInBandGasQuote", json!([])).is_err());
    }

    #[test]
    fn accepts_the_standard_single_hash_user_operation_lookup_parameters() {
        for method in [
            "pimlico_getUserOperationStatus",
            "eth_getUserOperationByHash",
            "eth_getUserOperationReceipt",
        ] {
            assert!(validate_call(method, json!(["0xabc"])).is_ok(), "{method}");
            assert!(validate_call(method, json!([])).is_err(), "{method}");
        }
    }
}
