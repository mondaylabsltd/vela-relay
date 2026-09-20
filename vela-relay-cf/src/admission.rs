//! Cloudflare driver for `eth_sendUserOperation` — a line-for-line port of the
//! docker shell's handler: the same `AdmissionApp` decides; only the arms
//! differ (RecordDO for Redis, Queues for Iggy, fetch for reqwest).

use serde_json::Value;
use vela_relay_core::admission::{
    AdmissionApp, AdmissionEffect, AdmissionEvent, AdmissionOperation, AdmissionOutcome,
    AdmissionResult, CONFLICT_MESSAGE, SubmitRequest,
};
use vela_relay_core::wire::{RpcError, RpcResponse, SendUserOperationParams};
use worker::Env;

use crate::{
    arms::{market, rpc},
    config::CfConfig,
    proto::{RecordCommand, RecordReply},
};

const RECORDS_BINDING: &str = "RECORDS";
const QUEUE_BINDING: &str = "OPS_QUEUE";

pub async fn handle(
    id: Value,
    chain_id: u64,
    user_rpc_url: Option<&str>,
    env: &Env,
    config: &CfConfig,
    params: SendUserOperationParams,
) -> RpcResponse<Value> {
    let SendUserOperationParams(user_operation, entry_point, submission_tier) = params;

    let core: crux_core::Core<AdmissionApp> = crux_core::Core::new();
    let mut effects = core.process_event(AdmissionEvent::Submit(Box::new(SubmitRequest {
        chain_id,
        entry_point: entry_point.clone(),
        user_operation,
        settlement_recipient: config.settlement_recipient.clone(),
        submission_tier,
    })));

    loop {
        let Some(effect) = effects.pop() else {
            break;
        };
        let AdmissionEffect::Work(mut request) = effect;
        let result = execute(chain_id, user_rpc_url, env, config, &request.operation).await;
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
/// that keeps an unadmitted record after a lost queue acknowledgement).
async fn execute(
    chain_id: u64,
    user_rpc_url: Option<&str>,
    env: &Env,
    config: &CfConfig,
    operation: &AdmissionOperation,
) -> AdmissionResult {
    match operation {
        AdmissionOperation::LoadSettlementAssets => {
            match market::settlement_assets(env, chain_id).await {
                Ok(assets) => AdmissionResult::Assets {
                    native_decimals: assets.native_decimals,
                    stablecoins: assets.stablecoins,
                },
                Err(()) => AdmissionResult::AssetsUnavailable,
            }
        }
        AdmissionOperation::FetchTokenDecimals { token } => {
            match rpc::erc20_decimals(config, env, chain_id, user_rpc_url, token).await {
                Ok(decimals) => AdmissionResult::Decimals { decimals },
                Err(()) => AdmissionResult::DecimalsUnavailable,
            }
        }
        AdmissionOperation::CreateQueued { operation } => {
            if env.queue(QUEUE_BINDING).is_err() {
                return AdmissionResult::QueueUnavailable;
            }
            match record_command(
                env,
                chain_id,
                &operation.user_operation_hash,
                &RecordCommand::CreateQueued {
                    operation: operation.clone(),
                },
            )
            .await
            {
                Ok(RecordReply::Created { created }) => AdmissionResult::Created { created },
                _ => {
                    worker::console_warn!(
                        "could not create queued UserOperation status for {}",
                        operation.user_operation_hash
                    );
                    AdmissionResult::StoreFailed
                }
            }
        }
        AdmissionOperation::LoadExisting { hash } => {
            match record_command(env, chain_id, hash, &RecordCommand::Get).await {
                Ok(RecordReply::Record { record }) => AdmissionResult::Record {
                    record: record.map(|record| *record),
                },
                _ => {
                    worker::console_warn!(
                        "could not read existing UserOperation status for {hash}"
                    );
                    AdmissionResult::StoreFailed
                }
            }
        }
        AdmissionOperation::Enqueue { envelope, retry } => {
            let Ok(queue) = env.queue(QUEUE_BINDING) else {
                return AdmissionResult::QueueUnavailable;
            };
            let user_operation_hash = envelope
                .get("userOperationHash")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if *retry {
                // Same recovery rule as the docker shell: at-least-once
                // delivery plus the durable record makes re-appending safe.
                worker::console_log!(
                    "retrying an incomplete UserOperation queue admission for {user_operation_hash}"
                );
            }
            // Send the envelope as its exact JSON text: the v8 structured
            // clone of a serde_json map arrives as a JS Map (unreadable as an
            // object), so the consumer parses the string form instead.
            match queue.send(&envelope.to_string()).await {
                Ok(()) => AdmissionResult::Enqueued,
                Err(error) => {
                    worker::console_warn!(
                        "could not confirm UserOperation append to the queue; preserving the durable admission for recovery: {error}"
                    );
                    AdmissionResult::QueueUnavailable
                }
            }
        }
        AdmissionOperation::MarkAdmitted { hash } => {
            match record_command(env, chain_id, hash, &RecordCommand::MarkAdmitted).await {
                Ok(RecordReply::Marked { marked }) => AdmissionResult::Marked { marked },
                _ => {
                    worker::console_error!(
                        "queue accepted UserOperation {hash} but the record store could not finalize admission"
                    );
                    AdmissionResult::StoreFailed
                }
            }
        }
    }
}

/// One RecordDO invocation over the serde fetch protocol.
pub async fn record_command(
    env: &Env,
    chain_id: u64,
    hash: &str,
    command: &RecordCommand,
) -> Result<RecordReply, ()> {
    let namespace = env.durable_object(RECORDS_BINDING).map_err(|_| ())?;
    let id = namespace
        .id_from_name(&format!("{chain_id}:{hash}"))
        .map_err(|_| ())?;
    let stub = id.get_stub().map_err(|_| ())?;

    let body = serde_json::to_string(command).map_err(|_| ())?;
    let headers = worker::Headers::new();
    headers
        .set("content-type", "application/json")
        .map_err(|_| ())?;
    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));
    let request = worker::Request::new_with_init("https://record-do/", &init).map_err(|_| ())?;
    let mut response = stub.fetch_with_request(request).await.map_err(|_| ())?;
    if response.status_code() != 200 {
        return Err(());
    }
    response.json().await.map_err(|_| ())
}

fn render(id: Value, chain_id: u64, outcome: AdmissionOutcome) -> RpcResponse<Value> {
    match outcome {
        AdmissionOutcome::Accepted {
            user_operation_hash,
            sender_hex,
            entry_point,
        } => {
            worker::console_log!(
                "UserOperation accepted into the record store and the durable queue: chain_id={chain_id} entry_point={entry_point} sender={sender_hex} user_operation_hash={user_operation_hash} settlement=in_band"
            );
            RpcResponse::result(id, Value::String(user_operation_hash))
        }
        AdmissionOutcome::AlreadyQueued {
            user_operation_hash,
        } => {
            worker::console_log!(
                "UserOperation already exists in the durable queue: {user_operation_hash}"
            );
            RpcResponse::result(id, Value::String(user_operation_hash))
        }
        AdmissionOutcome::Conflict {
            user_operation_hash,
            existing_chain_id,
            existing_entry_point,
        } => {
            worker::console_error!(
                "existing admission does not match the submitted UserOperation: {user_operation_hash} existing_chain_id={existing_chain_id} existing_entry_point={existing_entry_point}"
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
