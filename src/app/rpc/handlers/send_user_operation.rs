//! Shell driver for `eth_sendUserOperation`.
//!
//! The two-phase admission protocol — validation, durable Redis record, Iggy
//! append, admitted mark — is decided by `vela_relay_core::admission`; this
//! handler executes its operations against real infrastructure and renders
//! the settled outcome to the JSON-RPC wire.

use axum::http::HeaderValue;
use serde_json::Value;
use vela_relay_core::admission::{
    AdmissionApp, AdmissionEffect, AdmissionEvent, AdmissionOperation, AdmissionOutcome,
    AdmissionResult, CONFLICT_MESSAGE, SubmitRequest,
};

use crate::{
    app::AppState,
    app::rpc::types::{RpcError, RpcResponse, SendUserOperationParams},
    utils::rpc,
};

pub async fn handle(
    id: Value,
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    state: &AppState,
    params: SendUserOperationParams,
) -> RpcResponse<Value> {
    let SendUserOperationParams(user_operation, entry_point, submission_tier) = params;

    let core: crux_core::Core<AdmissionApp> = crux_core::Core::new();
    let mut effects = core.process_event(AdmissionEvent::Submit(Box::new(SubmitRequest {
        chain_id,
        entry_point: entry_point.clone(),
        user_operation,
        settlement_recipient: state.settlement_recipient().map(str::to_owned),
        submission_tier,
    })));

    loop {
        let Some(effect) = effects.pop() else {
            break;
        };
        let AdmissionEffect::Work(mut request) = effect;
        let result = execute(chain_id, user_rpc_url, state, &request.operation).await;
        match core.resolve(&mut request, result) {
            Ok(next) => effects = next,
            Err(_) => {
                return RpcResponse::error(id, RpcError::user_operation_status_store_unavailable());
            }
        }
    }

    match core.view().outcome {
        Some(outcome) => render(id, chain_id, outcome),
        None => RpcResponse::error(id, RpcError::user_operation_status_store_unavailable()),
    }
}

/// Executes one admission operation. Failures fold into result variants — the
/// core decides what they mean (including the deliberate crash-window rule
/// that keeps an unadmitted record after a lost Iggy acknowledgement).
async fn execute(
    chain_id: u64,
    user_rpc_url: Option<&HeaderValue>,
    state: &AppState,
    operation: &AdmissionOperation,
) -> AdmissionResult {
    match operation {
        AdmissionOperation::LoadSettlementAssets => match rpc::settlement_assets(chain_id).await {
            Ok(assets) => AdmissionResult::Assets {
                native_decimals: assets.native_decimals,
                stablecoins: assets.stablecoins,
            },
            Err(()) => AdmissionResult::AssetsUnavailable,
        },
        AdmissionOperation::FetchTokenDecimals { token } => {
            match rpc::erc20_decimals(chain_id, user_rpc_url, token).await {
                Ok(decimals) => AdmissionResult::Decimals { decimals },
                Err(()) => AdmissionResult::DecimalsUnavailable,
            }
        }
        AdmissionOperation::CreateQueued { operation } => {
            let Some(status_store) = state.user_operation_status_store() else {
                return AdmissionResult::StoreFailed;
            };
            if state.user_operation_queue().is_none() {
                return AdmissionResult::QueueUnavailable;
            }
            match status_store.create_queued(operation.clone()).await {
                Ok(created) => AdmissionResult::Created { created },
                Err(error) => {
                    tracing::warn!(
                        chain_id,
                        user_operation_hash = %operation.user_operation_hash,
                        %error,
                        "could not create queued UserOperation status in Redis"
                    );
                    AdmissionResult::StoreFailed
                }
            }
        }
        AdmissionOperation::LoadExisting { hash } => {
            let Some(status_store) = state.user_operation_status_store() else {
                return AdmissionResult::StoreFailed;
            };
            match status_store.get(hash).await {
                Ok(record) => AdmissionResult::Record { record },
                Err(error) => {
                    tracing::warn!(
                        chain_id,
                        user_operation_hash = %hash,
                        %error,
                        "could not read existing UserOperation status from Redis"
                    );
                    AdmissionResult::StoreFailed
                }
            }
        }
        AdmissionOperation::Enqueue { envelope, retry } => {
            let Some(queue) = state.user_operation_queue() else {
                return AdmissionResult::QueueUnavailable;
            };
            let user_operation_hash = envelope
                .get("userOperationHash")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if *retry {
                // The first producer may have lost Iggy's acknowledgement or
                // crashed before setting the admission marker. Re-appending is
                // the only safe recovery: Iggy and the consumer provide
                // at-least-once delivery, while the Redis hash makes execution
                // idempotent.
                tracing::info!(
                    chain_id,
                    user_operation_hash = %user_operation_hash,
                    "retrying an incomplete UserOperation queue admission"
                );
            }
            match queue.enqueue(chain_id, envelope).await {
                Ok(()) => AdmissionResult::Enqueued,
                Err(error) => {
                    tracing::warn!(
                        chain_id,
                        user_operation_hash = %user_operation_hash,
                        %error,
                        "could not confirm UserOperation append to Iggy; preserving Redis admission for recovery"
                    );
                    AdmissionResult::QueueUnavailable
                }
            }
        }
        AdmissionOperation::MarkAdmitted { hash } => {
            let Some(status_store) = state.user_operation_status_store() else {
                return AdmissionResult::StoreFailed;
            };
            match status_store.mark_admitted(hash).await {
                Ok(marked) => AdmissionResult::Marked { marked },
                Err(error) => {
                    tracing::error!(
                        chain_id,
                        user_operation_hash = %hash,
                        %error,
                        "Iggy accepted UserOperation but Redis could not finalize admission"
                    );
                    AdmissionResult::StoreFailed
                }
            }
        }
    }
}

fn render(id: Value, chain_id: u64, outcome: AdmissionOutcome) -> RpcResponse<Value> {
    match outcome {
        AdmissionOutcome::Accepted {
            user_operation_hash,
            sender_hex,
            entry_point,
        } => {
            tracing::info!(
                chain_id,
                entry_point = %entry_point,
                sender = %sender_hex,
                user_operation_hash = %user_operation_hash,
                settlement = "in_band",
                "UserOperation accepted into Redis and the durable Iggy queue"
            );
            RpcResponse::result(id, Value::String(user_operation_hash))
        }
        AdmissionOutcome::AlreadyQueued {
            user_operation_hash,
        } => {
            tracing::info!(
                chain_id,
                user_operation_hash = %user_operation_hash,
                "UserOperation already exists in the durable queue"
            );
            RpcResponse::result(id, Value::String(user_operation_hash))
        }
        AdmissionOutcome::Conflict {
            user_operation_hash,
            existing_chain_id,
            existing_entry_point,
        } => {
            tracing::error!(
                chain_id,
                user_operation_hash = %user_operation_hash,
                existing_chain_id,
                existing_entry_point = %existing_entry_point,
                "existing Redis admission does not match the submitted UserOperation"
            );
            RpcResponse::error(id, RpcError::invalid_params(CONFLICT_MESSAGE))
        }
        AdmissionOutcome::Invalid { message } => {
            RpcResponse::error(id, RpcError::invalid_params(message))
        }
        AdmissionOutcome::Rejected { message } => {
            RpcResponse::error(id, RpcError::user_operation_rejected(message))
        }
        AdmissionOutcome::EstimationUnavailable => {
            RpcResponse::error(id, RpcError::estimation_unavailable())
        }
        AdmissionOutcome::BackendUnavailable => {
            RpcResponse::error(id, RpcError::backend_unavailable())
        }
        AdmissionOutcome::StoreUnavailable => {
            RpcResponse::error(id, RpcError::user_operation_status_store_unavailable())
        }
        AdmissionOutcome::QueueUnavailable => {
            RpcResponse::error(id, RpcError::user_operation_queue_unavailable())
        }
    }
}
