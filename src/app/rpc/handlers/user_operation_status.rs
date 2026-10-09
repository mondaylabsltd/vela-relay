use serde_json::Value;

use crate::{
    app::rpc::types::{
        GetUserOperationByHashParams, GetUserOperationReceiptParams, GetUserOperationStatusParams,
        RpcError, RpcResponse, UserOperationByHash, UserOperationStatus, UserOperationStatusKind,
    },
    app::{AppState, StoredUserOperation},
};

pub async fn get_status(
    id: Value,
    state: &AppState,
    params: GetUserOperationStatusParams,
) -> RpcResponse<Value> {
    let GetUserOperationStatusParams([user_operation_hash]) = params;
    match load_from_redis(state, &user_operation_hash).await {
        Ok(Some(operation)) => result(id, crate::app::user_operation_store::rpc_status(&operation)),
        Ok(None) => result(
            id,
            UserOperationStatus {
                status: UserOperationStatusKind::NotFound,
                transaction_hash: None,
                last_executor_stage: None,
                last_executor_error: None,
                last_executor_attempt_at_ms: None,
                rejection_reason: None,
            },
        ),
        Err(error) => RpcResponse::error(id, error),
    }
}

pub async fn get_by_hash(
    id: Value,
    state: &AppState,
    params: GetUserOperationByHashParams,
) -> RpcResponse<Value> {
    let GetUserOperationByHashParams([user_operation_hash]) = params;
    match load_from_redis(state, &user_operation_hash).await {
        Ok(Some(operation)) => result(
            id,
            UserOperationByHash {
                user_operation: operation.user_operation,
                entry_point: operation.entry_point,
                block_number: operation.block_number,
                block_hash: operation.block_hash,
                transaction_hash: operation.transaction_hash,
            },
        ),
        Ok(None) => RpcResponse::result(id, Value::Null),
        Err(error) => RpcResponse::error(id, error),
    }
}

pub async fn get_receipt(
    id: Value,
    state: &AppState,
    params: GetUserOperationReceiptParams,
) -> RpcResponse<Value> {
    let GetUserOperationReceiptParams([user_operation_hash]) = params;
    match load_from_redis(state, &user_operation_hash).await {
        Ok(Some(operation)) => RpcResponse::result(
            id,
            receipt_response(&user_operation_hash, &operation).unwrap_or(Value::Null),
        ),
        Ok(None) => RpcResponse::result(id, Value::Null),
        Err(error) => RpcResponse::error(id, error),
    }
}

/// Status RPC methods are intentionally a Redis-only read model. The trusted executor reconciler
/// is the sole writer of chain-derived receipt state, avoiding caller/public RPC trust and query
/// amplification when clients poll these endpoints.
async fn load_from_redis(
    state: &AppState,
    user_operation_hash: &str,
) -> Result<Option<StoredUserOperation>, RpcError> {
    let store = state
        .user_operation_status_store()
        .ok_or_else(RpcError::user_operation_status_store_unavailable)?;
    store.get(user_operation_hash).await.map_err(|error| {
        tracing::warn!(%error, "could not read UserOperation status from Redis");
        RpcError::user_operation_status_store_unavailable()
    })
}

fn result<T: serde::Serialize>(id: Value, value: T) -> RpcResponse<Value> {
    match serde_json::to_value(value) {
        Ok(value) => RpcResponse::result(id, value),
        Err(_) => RpcResponse::error(id, RpcError::backend_unavailable()),
    }
}

/// Moved to the core wire module (spec 002) so both shells render identical
/// ERC-7769 receipts; re-imported here for the call sites and tests.
use vela_relay_core::wire::receipt_response;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::receipt_response;
    use crate::app::StoredUserOperation;
    use crate::app::rpc::types::{UserOperation, UserOperationStatusKind};
    use vela_relay_core::task::UserOperationEvent;
    use vela_relay_core::task::UserOperationV0_7;

    fn stored_operation(
        status: UserOperationStatusKind,
        event_success: Option<bool>,
        has_receipt: bool,
    ) -> StoredUserOperation {
        StoredUserOperation {
            status,
            transaction_hash: has_receipt.then(|| "0xtransaction".into()),
            chain_id: 1,
            chain_id_text: "1".into(),
            entry_point: "0x2222222222222222222222222222222222222222".into(),
            user_operation: UserOperation::V0_7(Box::new(UserOperationV0_7 {
                sender: "0x1111111111111111111111111111111111111111".into(),
                nonce: "0x1".into(),
                factory: None,
                factory_data: None,
                call_data: "0x".into(),
                call_gas_limit: "0x1".into(),
                verification_gas_limit: "0x1".into(),
                pre_verification_gas: "0x1".into(),
                max_fee_per_gas: "0x0".into(),
                max_priority_fee_per_gas: "0x0".into(),
                paymaster: None,
                paymaster_verification_gas_limit: None,
                paymaster_post_op_gas_limit: None,
                paymaster_data: None,
                signature: "0x01".into(),
            eip7702_auth: None,
            fee_token: None,
            })),
            admitted: true,
            next_receipt_check_at_ms: 0,
            block_hash: has_receipt.then(|| "0xblock".into()),
            block_number: has_receipt.then(|| "0x10".into()),
            receipt: has_receipt.then(|| {
                json!({
                    "transactionHash": "0xtransaction",
                    "blockHash": "0xblock",
                    "blockNumber": "0x10",
                    "status": "0x1",
                    "logs": [{
                        "address": "0x3333333333333333333333333333333333333333",
                        "topics": ["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
                        "data": "0x01"
                    }],
                })
            }),
            event: event_success.map(|success| UserOperationEvent {
                user_operation_hash: "0xuserop".into(),
                success,
                actual_gas_cost: "0x5".into(),
                actual_gas_used: "0x6".into(),
            }),
            last_executor_stage: None,
            last_executor_error: None,
            last_executor_attempt_at_ms: None,
            rejection_reason: None,
        }
    }

    #[test]
    fn exposes_the_reason_for_a_local_rejection() {
        let mut operation = stored_operation(UserOperationStatusKind::Rejected, None, false);
        operation.last_executor_stage = Some("in_band_settlement".into());
        operation.last_executor_error = Some(
            "in-band reimbursement is below the required amount: paid=1, required=2, shortfall=1"
                .into(),
        );
        operation.last_executor_attempt_at_ms = Some(1_753_000_000_000);

        let status = crate::app::user_operation_store::rpc_status(&operation);
        assert_eq!(
            status.last_executor_stage.as_deref(),
            Some("in_band_settlement")
        );
        assert_eq!(
            status.last_executor_error.as_deref(),
            Some(
                "in-band reimbursement is below the required amount: paid=1, required=2, shortfall=1"
            )
        );
        assert_eq!(status.last_executor_attempt_at_ms, Some(1_753_000_000_000));
    }

    #[test]
    fn returns_an_included_user_operation_receipt_from_redis_state() {
        let operation = stored_operation(UserOperationStatusKind::Included, Some(true), true);

        let receipt = receipt_response("0xuserop", &operation).expect("included receipt");

        assert_eq!(receipt["userOpHash"], "0xuserop");
        assert_eq!(receipt["success"], true);
        assert_eq!(receipt["actualGasCost"], "0x5");
        assert_eq!(receipt["logs"].as_array().unwrap().len(), 1);
        assert_eq!(receipt["receipt"]["transactionHash"], "0xtransaction");
    }

    #[test]
    fn returns_an_on_chain_rejected_user_operation_receipt() {
        let operation = stored_operation(UserOperationStatusKind::Rejected, Some(false), true);

        let receipt = receipt_response("0xuserop", &operation).expect("on-chain rejection receipt");

        assert_eq!(receipt["success"], false);
        assert_eq!(receipt["actualGasUsed"], "0x6");
        assert_eq!(receipt["receipt"]["status"], "0x1");
    }

    #[test]
    fn pre_submit_or_eventless_rejections_do_not_have_a_user_operation_receipt() {
        let pre_submit = stored_operation(UserOperationStatusKind::Rejected, None, false);
        let no_event = stored_operation(UserOperationStatusKind::Rejected, None, true);
        let inconsistent_event =
            stored_operation(UserOperationStatusKind::Rejected, Some(true), true);

        assert!(receipt_response("0xuserop", &pre_submit).is_none());
        assert!(receipt_response("0xuserop", &no_event).is_none());
        assert!(receipt_response("0xuserop", &inconsistent_event).is_none());
    }
}
