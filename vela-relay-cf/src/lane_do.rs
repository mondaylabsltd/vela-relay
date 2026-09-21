//! LaneDO — one Durable Object per (chain, lane): the execution unit
//! (data-model §1). Drives the core's `ExecutionApp` per delivered batch; the
//! DO's single-threaded input gate IS the lane lease, so `AcquireLaneLease`/
//! `EnsureLaneLease` are answered structurally and the docker shell's
//! heartbeat/interrupt machinery has no counterpart here (declared in
//! contracts/platform-bindings.md).
//!
//! Storage layout: `intent` → `PreparedBundleIntent`; `bundle:{txhash}` →
//! member-hash index; `seen:{txhash}` → broadcast-seen timestamp;
//! `delayed:{hash}` → parked operation (payload + attempts + due). The single
//! alarm serves the delayed inbox (earliest due; US3 adds reconcile packing).

use std::cell::RefCell;

use alloy::primitives::{Address, U256};
use serde_json::{Value, json};
use vela_relay_core::broadcast::{nonce_too_low, validate_raw_transaction};
use vela_relay_core::execution::{
    self as core_execution, ExecutionOperation as Op, ExecutionOutcome as Out,
};
use vela_relay_core::simulation::{SimulationResult, SimulationVerdict};
use vela_relay_core::task::{PreparedBundleIntent, RoutedUserOperation, truncate_diagnostic};
use vela_relay_core::vault;
use worker::{
    Date, DurableObject, Env, Request, Response, Result, State, durable_object, wasm_bindgen,
};

use crate::{
    arms::{
        market, simulate,
        trusted::{
            RpcBatchCall, TrustedRpcClient, parse_quantity, response_abi_u256, response_quantity,
            response_quantity_optional, response_value,
        },
    },
    config::CfConfig,
    proto::{
        ItemResolutionWire, LaneCommand, LaneReply, LeaseIdentity, RecordCommand, RecordReply,
        TreasuryCommand, TreasuryReply, TreasuryRequest,
    },
};

const INTENT_KEY: &str = "intent";
const BROADCAST_RETRY_INTERVAL_MS: u64 = 30_000;
/// Docker engine `BINANCE_PRICE_TTL`.
const BINANCE_PRICE_TTL_MS: u64 = 60_000;
/// Docker engine `ERC20_DECIMALS_SELECTOR` (`decimals()`).
const ERC20_DECIMALS_SELECTOR: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];
/// The lane's next reconcile deadline (armed while a prepared intent exists).
const RECONCILE_DUE_KEY: &str = "reconcileDueMs";
/// Docker `DELAYED_CLAIM_BATCH_SIZE`.
const DELAYED_REDRIVE_BATCH: usize = 100;
/// Docker `USER_OPERATION_QUEUE_RETENTION` (14 d): the delayed-payload
/// retention is `max(attempt_ttl, this)` on both shells.
const QUEUE_RETENTION_MS: u64 = 14 * 24 * 60 * 60 * 1_000;

thread_local! {
    /// Per-isolate market price cache (docker: per-process map, same TTL).
    static MARKET_PRICES: RefCell<std::collections::HashMap<String, (u64, U256)>> =
        RefCell::new(std::collections::HashMap::new());
    /// Disambiguates lease tokens minted in the same millisecond (docker:
    /// `unique_token`'s process counter).
    static TOKEN_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Docker `unique_token`: unique per acquisition attempt within this shell.
fn unique_token(prefix: &str, chain_id: u64, lane: u8) -> String {
    let counter = TOKEN_COUNTER.with(|counter| {
        let value = counter.get();
        counter.set(value.wrapping_add(1));
        value
    });
    format!(
        "{prefix}:{chain_id}:{lane}:{}:{counter}",
        Date::now().as_millis()
    )
}

/// The docker engine's shell-local dispositions for prepared-intent recovery.
#[derive(Clone, Copy, PartialEq)]
enum BundleBroadcastDisposition {
    Confirmed,
    Unknown,
}

#[derive(Clone, Copy, PartialEq)]
enum BundleResumeDisposition {
    Cleared,
    Confirmed,
    Unknown,
}

/// One batch's shared execution context: the derived policy plus the trusted
/// transport and the per-batch resolved native symbol (docker: engine handler
/// state).
struct BatchContext<'a> {
    config: &'a CfConfig,
    policy: &'a core_execution::ExecutionPolicy,
    trusted: &'a TrustedRpcClient<'a>,
    native_symbol: RefCell<Option<String>>,
    /// This batch's treasury lease token (docker: the handler's
    /// `treasury_token`); the scope is the chain's TreasuryDO itself.
    treasury_token: String,
}

impl BatchContext<'_> {
    fn treasury_lease(&self) -> LeaseIdentity {
        LeaseIdentity {
            token: self.treasury_token.clone(),
            ttl_ms: self.config.lease_ttl_ms,
        }
    }
}

/// The docker arms surface the store error as a batch-fatal `Failed`
/// (bindings contract); this is its stable shell-side text.
fn treasury_unavailable() -> Out {
    Out::Failed {
        message: "chain treasury coordinator is unavailable".into(),
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DelayedEntry {
    routed: RoutedUserOperation,
    attempts: u32,
    due_ms: u64,
    created_ms: u64,
    /// Last write (park or retry); the retention window slides from here
    /// (docker: the payload key's PEXPIRE refresh on every retry).
    #[serde(default)]
    updated_ms: u64,
}

#[durable_object]
pub struct LaneDo {
    state: State,
    env: Env,
}

impl DurableObject for LaneDo {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let command: LaneCommand = req.json().await?;
        match command {
            LaneCommand::ExecuteBatch { operations } => {
                let resolutions = self.execute_batch(operations).await;
                Response::from_json(&LaneReply::Resolutions {
                    resolutions: resolutions
                        .into_iter()
                        .map(ItemResolutionWire::from)
                        .collect(),
                })
            }
        }
    }

    /// The lane's single packed alarm (earliest-of): the prepared-intent
    /// reconcile step (docker `reconcile_prepared_bundles`, per this lane)
    /// runs first, then due delayed-inbox entries re-drive through the same
    /// batch entry (docker `reconcile_delayed_user_operations`) — the docker
    /// reconciler's loop body, fired by the platform clock instead of an
    /// interval task.
    async fn alarm(&self) -> Result<Response> {
        let now = Date::now().as_millis();
        if let Some(due) = self.reconcile_due().await
            && now >= due
        {
            self.reconcile_intent().await;
        }
        self.redrive_due_delayed(now).await;
        self.schedule_lane_alarm().await?;
        Response::empty()
    }
}

impl LaneDo {
    /// The driver loop — the docker `handle_lane_batch` without the lease
    /// machinery (the DO is the lease).
    async fn execute_batch(
        &self,
        operations: Vec<RoutedUserOperation>,
    ) -> Vec<core_execution::ItemResolution> {
        use core_execution::ItemResolution;
        let failure = |reason: &str, len: usize| -> Vec<ItemResolution> {
            (0..len)
                .map(|_| ItemResolution::Failed {
                    reason: reason.to_owned(),
                })
                .collect()
        };

        if operations.is_empty() {
            return Vec::new();
        }
        let count = operations.len();
        let chain_id = operations[0].chain_id;
        let lane = operations[0].lane;
        let config = match CfConfig::from_env(&self.env) {
            Ok(config) => config,
            Err(error) => {
                worker::console_error!("configuration error: {error}");
                return failure("executor configuration is invalid", count);
            }
        };
        let policy = match config.execution_policy(chain_id, lane) {
            Ok(policy) => policy,
            Err(error) => {
                worker::console_error!("execution policy error: {error}");
                return failure("executor configuration is invalid", count);
            }
        };

        let trusted = TrustedRpcClient::new(&config, &self.env);
        let context = BatchContext {
            config: &config,
            policy: &policy,
            trusted: &trusted,
            native_symbol: RefCell::new(None),
            treasury_token: unique_token("treasury", chain_id, lane),
        };

        let core: crux_core::Core<core_execution::ExecutionApp> = crux_core::Core::new();
        let mut effects: std::collections::VecDeque<core_execution::ExecutionEffect> = core
            .process_event(core_execution::ExecutionEvent::Start(Box::new(
                core_execution::StartBatch {
                    operations: operations.clone(),
                    policy: policy.clone(),
                    // The DO's identity is the lease; the token is bookkeeping.
                    lease_token: format!("lane:{chain_id}:{lane}"),
                },
            )))
            .into_iter()
            .collect();
        while let Some(core_execution::ExecutionEffect::Work(mut request)) = effects.pop_front() {
            let outcome = self
                .execute(&context, chain_id, lane, &operations, &request.operation)
                .await;
            match core.resolve(&mut request, outcome) {
                Ok(next) => effects.extend(next),
                Err(_) => return failure("could not resolve execution effect", count),
            }
        }

        match core.view().outcome {
            Some(resolutions) => resolutions,
            None => failure("lane batch never settled", count),
        }
    }

    async fn execute(
        &self,
        context: &BatchContext<'_>,
        chain_id: u64,
        lane: u8,
        batch: &[RoutedUserOperation],
        operation: &Op,
    ) -> Out {
        match operation {
            // --- chain support & assets ---
            Op::CheckChainSupported => Out::Supported {
                supported: self.chain_supported(context, chain_id).await,
            },
            Op::LoadChainAssets => self.load_chain_assets(context, chain_id).await,
            // --- record store (RecordDO subrequests) ---
            Op::LoadRecords { hashes } => {
                let mut records = Vec::with_capacity(hashes.len());
                for hash in hashes {
                    match self.record(chain_id, hash, &RecordCommand::Get).await {
                        Ok(RecordReply::Record { record }) => {
                            records.push(record.map(|record| *record));
                        }
                        _ => {
                            return Out::Failed {
                                message: "could not read UserOperation records".into(),
                            };
                        }
                    }
                }
                Out::Records { records }
            }
            Op::ReloadRecord { hash } => {
                match self.record(chain_id, hash, &RecordCommand::Get).await {
                    Ok(RecordReply::Record { record }) => Out::Record {
                        record: record.map(|record| *record),
                    },
                    _ => Out::Failed {
                        message: "could not read UserOperation record".into(),
                    },
                }
            }
            Op::RestoreQueued { index: _, queued } => {
                match self
                    .record(
                        chain_id,
                        &queued.user_operation_hash.clone(),
                        &RecordCommand::RestoreQueued {
                            operation: queued.clone(),
                        },
                    )
                    .await
                {
                    Ok(RecordReply::Created { .. }) => Out::Done,
                    _ => Out::Failed {
                        message: "could not restore queued UserOperation".into(),
                    },
                }
            }
            Op::MarkAdmitted { hash } => {
                match self
                    .record(chain_id, hash, &RecordCommand::MarkAdmitted)
                    .await
                {
                    Ok(RecordReply::Marked { marked }) => Out::Marked { marked },
                    _ => Out::Failed {
                        message: "could not recover UserOperation admission".into(),
                    },
                }
            }
            Op::MarkRejected { hash, cause } => {
                self.log_rejection_cause(chain_id, hash, cause);
                let patch = serde_json::json!({ "status": "rejected", "admitted": true });
                match self
                    .record(chain_id, hash, &RecordCommand::Patch { patch })
                    .await
                {
                    Ok(RecordReply::Patched { .. }) => Out::Done,
                    _ => {
                        if let core_execution::RejectionCause::StaleNonce { .. } = cause {
                            worker::console_warn!(
                                "could not persist stale nonce rejection: {hash}"
                            );
                        }
                        Out::Failed {
                            message: "could not persist UserOperation rejection".into(),
                        }
                    }
                }
            }
            Op::MarkRejectedWithReason {
                hash,
                stage,
                reason,
            } => {
                let patch = serde_json::json!({
                    "status": "rejected",
                    "admitted": true,
                    "lastExecutorStage": truncate_diagnostic(stage, 64),
                    "lastExecutorError": truncate_diagnostic(reason, 512),
                    "lastExecutorAttemptAtMs": Date::now().as_millis(),
                });
                match self
                    .record(chain_id, hash, &RecordCommand::Patch { patch })
                    .await
                {
                    Ok(RecordReply::Patched { .. }) => Out::Done,
                    _ => Out::Failed {
                        message: "could not persist UserOperation rejection".into(),
                    },
                }
            }
            Op::RecordDeferred {
                hash,
                stage,
                reason,
            } => {
                let patch = serde_json::json!({
                    "lastExecutorStage": truncate_diagnostic(stage, 64),
                    "lastExecutorError": truncate_diagnostic(reason, 512),
                    "lastExecutorAttemptAtMs": Date::now().as_millis(),
                });
                if self
                    .record(chain_id, hash, &RecordCommand::Patch { patch })
                    .await
                    .is_err()
                {
                    worker::console_warn!(
                        "could not persist executor retry diagnostic: {hash} stage={stage}"
                    );
                }
                Out::Done
            }
            Op::NotifyIssue {
                hash,
                stage,
                reason,
            } => {
                self.notify_executor_issue(context, chain_id, stage, hash, reason)
                    .await;
                Out::Done
            }
            Op::EmitDiagnostic { diagnostic } => {
                self.emit_diagnostic(chain_id, lane, diagnostic);
                Out::Done
            }
            Op::DeadLetterRouted { index, reason } => self.dead_letter(batch, *index, reason).await,
            // --- the lease IS the DO ---
            Op::AcquireLaneLease => Out::LeaseAcquired { acquired: true },
            Op::EnsureLaneLease => Out::LeaseHeld { held: true },
            // --- prepared intent ---
            Op::LoadPreparedBundle => Out::Intent {
                intent: self.intent().await,
            },
            Op::SavePreparedBundle { intent } => {
                if self.intent().await.is_some() {
                    Out::Saved { saved: false }
                } else {
                    match self.state.storage().put(INTENT_KEY, intent).await {
                        Ok(()) => {
                            // Covers the crash window between save and
                            // broadcast: the reconcile alarm resumes the
                            // durable outbox even if this batch dies here.
                            self.arm_reconcile().await;
                            Out::Saved { saved: true }
                        }
                        Err(_) => Out::Failed {
                            message: "could not persist prepared bundle intent".into(),
                        },
                    }
                }
            }
            Op::ClearStaleIntent { intent, reason } => {
                match self
                    .clear_stale_bundle_intent(context, intent, reason)
                    .await
                {
                    Ok(()) => Out::Done,
                    Err(message) => Out::Failed { message },
                }
            }
            // --- broadcast-seen cache (loss harmless) ---
            Op::CheckBroadcastSeen { transaction_hash } => Out::Seen {
                seen: self.broadcast_seen(transaction_hash).await,
            },
            Op::RememberBroadcast { transaction_hash } => {
                let _ = self
                    .state
                    .storage()
                    .put(&format!("seen:{transaction_hash}"), Date::now().as_millis())
                    .await;
                Out::Done
            }
            Op::ForgetBroadcast { transaction_hash } => {
                let _ = self
                    .state
                    .storage()
                    .delete(&format!("seen:{transaction_hash}"))
                    .await;
                Out::Done
            }
            // --- delayed inbox (the hold ladder's durable parking) ---
            Op::DeferOperation { index, cause } => {
                self.defer_operation(chain_id, batch, *index, cause).await
            }
            // --- bundle submitted (per-member RecordDO + lane index) ---
            Op::MarkBundleSubmitted { intent, gas_limit } => {
                self.mark_bundle_submitted(chain_id, lane, intent, *gas_limit)
                    .await
            }
            // --- chain IO over the trusted transport (docker engine arms) ---
            Op::ResumeBundleIntent { intent } => {
                match self.resume_bundle_intent(context, intent).await {
                    Ok(disposition) => Out::Resumed {
                        known_outcome: disposition != BundleResumeDisposition::Unknown,
                    },
                    Err(message) => Out::Failed { message },
                }
            }
            Op::SimulateIndividually {
                entry_point,
                operations,
            } => {
                let packed = operations
                    .iter()
                    .map(|(_, packed)| packed.clone())
                    .collect::<Vec<_>>();
                let hashes = operations.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
                let verdicts = simulate::simulate_individually(
                    context.trusted,
                    chain_id,
                    *entry_point,
                    context.policy.relayer,
                    context.policy.treasury,
                    &packed,
                    &hashes,
                )
                .await;
                Out::OperationVerdicts {
                    verdicts: verdicts.into_iter().map(operation_sim_verdict).collect(),
                }
            }
            Op::FetchAccountNonces {
                entry_point,
                probes,
            } => {
                let calls = probes
                    .iter()
                    .map(|(sender, nonce)| RpcBatchCall {
                        method: "eth_call",
                        params: json!([{
                            "to": entry_point.to_string(),
                            "data": format!(
                                "0x{}",
                                hex::encode(vela_relay_core::abi::get_nonce_calldata(
                                    *sender, *nonce
                                ))
                            ),
                        }, "latest"]),
                    })
                    .collect::<Vec<_>>();
                match context.trusted.batch(chain_id, &calls).await {
                    Ok(responses) => Out::AccountNonces {
                        nonces: (0..probes.len())
                            .map(|index| {
                                response_abi_u256(&responses, index, "EntryPoint getNonce")
                                    .map_err(|error| {
                                        worker::console_warn!(
                                            "could not decode EntryPoint account nonce: chain_id={chain_id} error={error}"
                                        );
                                    })
                                    .ok()
                            })
                            .collect(),
                    },
                    Err(error) => {
                        worker::console_warn!(
                            "could not resolve account nonce mismatches: chain_id={chain_id} count={} error={error}",
                            probes.len()
                        );
                        Out::Failed {
                            message: error.to_string(),
                        }
                    }
                }
            }
            Op::SimulateBundle {
                entry_point,
                operations,
            } => {
                let packed = operations
                    .iter()
                    .map(|(_, packed)| packed.clone())
                    .collect::<Vec<_>>();
                let hashes = operations.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
                let verdict = simulate::simulate_bundle(
                    context.trusted,
                    chain_id,
                    *entry_point,
                    context.policy.relayer,
                    context.policy.treasury,
                    &packed,
                    &hashes,
                )
                .await;
                Out::BundleVerdict {
                    verdict: bundle_sim_verdict(verdict),
                }
            }
            Op::FetchTransactionContext {
                entry_point,
                calldata,
            } => {
                match transaction_context(
                    context.trusted,
                    chain_id,
                    context.policy.relayer,
                    *entry_point,
                    calldata,
                )
                .await
                {
                    Ok(transaction_context) => Out::Context {
                        context: transaction_context,
                    },
                    Err(message) => Out::Failed { message },
                }
            }
            Op::FetchMarketPrice => {
                let Some(symbol) = context.native_symbol.borrow().clone() else {
                    return Out::Failed {
                        message: "chain assets were not resolved".into(),
                    };
                };
                match market_usd_price(&symbol).await {
                    Ok(price) => Out::Price { price },
                    Err(message) => Out::Failed { message },
                }
            }
            Op::SignBundle { request } => sign_bundle(context, chain_id, lane, request),
            Op::BroadcastRaw {
                raw_transaction,
                transaction_hash: _,
            } => {
                match context
                    .trusted
                    .broadcast_raw_transaction(chain_id, raw_transaction)
                    .await
                {
                    Ok(outcome) => Out::Sent {
                        reply: broadcast_reply(outcome),
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::ProbeTransactionKnown { transaction_hash } => Out::Known {
                known: transaction_is_known(context.trusted, chain_id, transaction_hash).await,
            },
            Op::ProbeStaleNonce { intent } => Out::Stale {
                stale: bundle_nonce_is_stale(context, intent).await,
            },
            Op::RecordUnprovenBroadcast {
                transaction_hash,
                ambiguous,
                reason,
            } => {
                if *ambiguous {
                    worker::console_warn!(
                        "ambiguous handleOps broadcast is not yet observable: chain_id={chain_id} lane={lane} transaction_hash={transaction_hash} reason={reason}"
                    );
                } else {
                    worker::console_warn!(
                        "rejected broadcast is unproven; retaining exact handleOps outbox: chain_id={chain_id} lane={lane} transaction_hash={transaction_hash} reason={reason}"
                    );
                }
                Out::Done
            }
            // --- treasury funding (TreasuryDO = the chain's real lock) ---
            Op::AcquireTreasuryLease => {
                match self
                    .treasury(
                        chain_id,
                        None,
                        TreasuryCommand::AcquireLease {
                            lease: context.treasury_lease(),
                        },
                    )
                    .await
                {
                    Ok(TreasuryReply::Acquired { acquired }) => Out::LeaseAcquired { acquired },
                    _ => treasury_unavailable(),
                }
            }
            Op::EnsureTreasuryLease => {
                match self
                    .treasury(
                        chain_id,
                        None,
                        TreasuryCommand::EnsureLease {
                            lease: context.treasury_lease(),
                        },
                    )
                    .await
                {
                    Ok(TreasuryReply::Held { held }) => Out::LeaseHeld { held },
                    _ => treasury_unavailable(),
                }
            }
            Op::ReleaseTreasuryLease => {
                let _ = self
                    .treasury(
                        chain_id,
                        None,
                        TreasuryCommand::ReleaseLease {
                            token: context.treasury_token.clone(),
                        },
                    )
                    .await;
                Out::Done
            }
            Op::LoadPreparedFunding => {
                match self
                    .treasury(
                        chain_id,
                        Some(context.treasury_lease()),
                        TreasuryCommand::LoadFunding,
                    )
                    .await
                {
                    Ok(TreasuryReply::Funding { intent }) => Out::FundingIntent { intent },
                    _ => treasury_unavailable(),
                }
            }
            Op::SaveFundingIntent { intent } => {
                match self
                    .treasury(
                        chain_id,
                        Some(context.treasury_lease()),
                        TreasuryCommand::SaveFunding {
                            intent: intent.clone(),
                        },
                    )
                    .await
                {
                    Ok(TreasuryReply::Saved { saved }) => Out::Saved { saved },
                    _ => treasury_unavailable(),
                }
            }
            Op::ClearFundingIntent { transaction_hash } => {
                match self
                    .treasury(
                        chain_id,
                        Some(context.treasury_lease()),
                        TreasuryCommand::ClearFunding {
                            transaction_hash: transaction_hash.clone(),
                        },
                    )
                    .await
                {
                    Ok(TreasuryReply::Cleared { .. }) => Out::Done,
                    _ => treasury_unavailable(),
                }
            }
            Op::FetchTreasuryContext => {
                let calls = [
                    RpcBatchCall {
                        method: "eth_getTransactionCount",
                        params: json!([context.policy.treasury.to_string(), "pending"]),
                    },
                    RpcBatchCall {
                        method: "eth_getBalance",
                        params: json!([context.policy.treasury.to_string(), "pending"]),
                    },
                ];
                match context.trusted.batch(chain_id, &calls).await {
                    Ok(responses) => {
                        let treasury_context =
                            response_quantity(&responses, 0, "treasury eth_getTransactionCount")
                                .and_then(|nonce| {
                                    let nonce = u64::try_from(nonce)
                                        .map_err(|_| "treasury nonce exceeds uint64".to_owned())?;
                                    let balance = response_quantity(
                                        &responses,
                                        1,
                                        "treasury eth_getBalance",
                                    )?;
                                    Ok((nonce, balance))
                                });
                        match treasury_context {
                            Ok((nonce, balance)) => Out::TreasuryContext { nonce, balance },
                            Err(message) => Out::Failed { message },
                        }
                    }
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::SignTreasuryTransfer { request } => {
                sign_treasury_transfer(context, chain_id, request)
            }
            Op::SignTreasuryPathUsd { request } => {
                sign_treasury_path_usd(context, chain_id, request)
            }
            Op::AcquireReceiptProbe { transaction_hash } => {
                match self
                    .treasury(
                        chain_id,
                        Some(context.treasury_lease()),
                        TreasuryCommand::AcquireReceiptProbe {
                            transaction_hash: transaction_hash.clone(),
                            ttl_ms: context.config.receipt_poll_ms,
                        },
                    )
                    .await
                {
                    Ok(TreasuryReply::Acquired { acquired }) => Out::LeaseAcquired { acquired },
                    _ => treasury_unavailable(),
                }
            }
            Op::FetchTransactionReceipt { transaction_hash } => {
                match context
                    .trusted
                    .call(
                        chain_id,
                        "eth_getTransactionReceipt",
                        json!([transaction_hash]),
                    )
                    .await
                {
                    Ok(receipt) => Out::Receipt {
                        receipt: (!receipt.is_null()).then_some(receipt),
                    },
                    Err(error) => Out::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Op::RecordTreasuryShortfall {
                treasury_balance,
                required_treasury,
                requested,
                minimum,
                top_up_gas_cost,
            } => {
                worker::console_warn!(
                    "treasury cannot fund the current UserOperation relayer prefund: chain_id={chain_id} treasury_native_balance={treasury_balance} required_native_balance={required_treasury} requested_top_up_native_amount={requested} minimum_top_up_native_amount={minimum} top_up_gas_cost={top_up_gas_cost} reserve_native_amount={}",
                    context.config.treasury_floor_wei
                );
                Out::Done
            }
            Op::RecordPartialTopUp {
                requested,
                submitted,
                minimum,
            } => {
                worker::console_log!(
                    "treasury funding the current UserOperation with a partial relayer float top-up: chain_id={chain_id} requested_top_up_native_amount={requested} submitted_top_up_native_amount={submitted} minimum_top_up_native_amount={minimum}"
                );
                Out::Done
            }
            Op::RecordFundingSubmitted {
                amount,
                transaction_hash,
                tempo: is_tempo,
            } => {
                if *is_tempo {
                    worker::console_log!(
                        "submitted Tempo treasury pathUSD relayer top-up: chain_id={chain_id} relayer={} amount_path_usd={amount} transaction_hash={transaction_hash}",
                        context.policy.relayer
                    );
                } else {
                    worker::console_log!(
                        "submitted treasury relayer gas top-up: chain_id={chain_id} relayer={} amount_wei={amount} transaction_hash={transaction_hash}",
                        context.policy.relayer
                    );
                }
                Out::Done
            }
            Op::RecordUnprovenFunding {
                transaction_hash,
                ambiguous,
                reason,
            } => {
                if *ambiguous {
                    worker::console_debug!(
                        "funding broadcast is ambiguous; retaining exact outbox: chain_id={chain_id} transaction_hash={transaction_hash} reason={reason}"
                    );
                } else {
                    worker::console_warn!(
                        "rejected broadcast is unproven; retaining exact funding outbox: chain_id={chain_id} transaction_hash={transaction_hash} reason={reason}"
                    );
                }
                Out::Done
            }
            Op::NoteFundingReceipt { intent, success } => {
                if *success {
                    worker::console_log!(
                        "treasury relayer gas top-up included: chain_id={} relayer={} amount_wei={} transaction_hash={}",
                        intent.chain_id,
                        intent.relayer,
                        intent.amount_wei,
                        intent.transaction_hash
                    );
                } else {
                    worker::console_error!(
                        "treasury relayer top-up transaction reverted: chain_id={} relayer={} amount_wei={} transaction_hash={}",
                        intent.chain_id,
                        intent.relayer,
                        intent.amount_wei,
                        intent.transaction_hash
                    );
                }
                Out::Done
            }
            // --- Tempo (0x76 envelope) ---
            Op::FetchTempoContext => {
                match tempo_transaction_context(context.trusted, chain_id, context.policy.relayer)
                    .await
                {
                    Ok((base_fee_atto, nonce, relayer_path_usd_balance)) => Out::TempoContext {
                        base_fee_atto,
                        nonce,
                        relayer_path_usd_balance,
                    },
                    Err(message) => Out::Failed { message },
                }
            }
            Op::SignTempoBundle { request } => sign_tempo_bundle(context, chain_id, lane, request),
            Op::FetchTempoTreasuryContext { transfer_amount } => {
                match tempo_treasury_context(
                    context.trusted,
                    chain_id,
                    context.policy.treasury,
                    context.policy.relayer,
                    *transfer_amount,
                )
                .await
                {
                    Ok((nonce, balance, raw_gas_estimate)) => Out::TempoTreasuryContext {
                        nonce,
                        balance,
                        raw_gas_estimate,
                    },
                    Err(message) => Out::Failed { message },
                }
            }
            Op::RecordTempoTreasuryShortfall {
                treasury_balance,
                required_treasury,
                top_up,
                top_up_gas_limit,
                top_up_gas_cost,
            } => {
                worker::console_warn!(
                    "Tempo treasury cannot fund the pending relayer top-up: chain_id={chain_id} treasury_path_usd_balance={treasury_balance} required_path_usd={required_treasury} top_up_path_usd={top_up} top_up_gas_limit={top_up_gas_limit} top_up_gas_path_usd={top_up_gas_cost} reserve_path_usd={}",
                    vela_relay_core::tempo::TEMPO_TREASURY_FLOOR
                );
                Out::Done
            }
        }
    }

    /// Dynamic-chain gate (research.md R10): the optional allowlist first,
    /// then trusted-RPC availability (explicit URLs → Alchemy → directory),
    /// exactly the docker `TrustedRpcClient::supports_chain` resolution.
    async fn chain_supported(&self, context: &BatchContext<'_>, chain_id: u64) -> bool {
        if !context.config.execution_chains.is_empty()
            && !context.config.execution_chains.contains(&chain_id)
        {
            return false;
        }
        context.trusted.supports_chain(chain_id).await
    }

    /// Docker `chain_assets_for`: directory payment assets, with missing
    /// stablecoin decimals resolved through one trusted `eth_call` batch and
    /// still-undecodable entries dropped.
    async fn load_chain_assets(&self, context: &BatchContext<'_>, chain_id: u64) -> Out {
        if vela_relay_core::tempo::is_tempo_chain(chain_id) {
            let resolved = tempo_chain_assets(context.config);
            *context.native_symbol.borrow_mut() = Some(resolved.native_symbol.clone());
            return Out::Assets { resolved };
        }
        let metadata = match market::payment_metadata(&self.env, chain_id).await {
            Ok(metadata) => metadata,
            Err(_) => {
                return Out::AssetsUnavailable {
                    reason: "could not load payment assets from chain directory".into(),
                };
            }
        };
        let Some(native) = metadata.native_currency else {
            return Out::AssetsUnavailable {
                reason: "could not load payment assets from chain directory".into(),
            };
        };
        let mut stablecoins = metadata
            .stables
            .into_iter()
            .filter_map(|stable| {
                stable
                    .contract
                    .parse::<Address>()
                    .ok()
                    .map(|address| (address, stable.symbol, stable.decimals))
            })
            .collect::<Vec<_>>();
        let missing_decimals = stablecoins
            .iter()
            .enumerate()
            .filter_map(|(index, (address, _, decimals))| {
                decimals.is_none().then_some((index, *address))
            })
            .collect::<Vec<_>>();
        if !missing_decimals.is_empty() {
            let calls = missing_decimals
                .iter()
                .map(|(_, address)| RpcBatchCall {
                    method: "eth_call",
                    params: json!([{
                        "to": address.to_string(),
                        "data": format!("0x{}", hex::encode(ERC20_DECIMALS_SELECTOR)),
                    }, "latest"]),
                })
                .collect::<Vec<_>>();
            let responses = match context.trusted.batch(chain_id, &calls).await {
                Ok(responses) => responses,
                Err(error) => {
                    return Out::AssetsUnavailable {
                        reason: error.to_string(),
                    };
                }
            };
            for (response_index, (stable_index, _)) in missing_decimals.into_iter().enumerate() {
                let decimals = response_abi_u256(&responses, response_index, "ERC-20 decimals")
                    .ok()
                    .and_then(|value| u32::try_from(value).ok())
                    .filter(|decimals| *decimals <= 38);
                stablecoins[stable_index].2 = decimals;
            }
        }
        let stablecoins = stablecoins
            .into_iter()
            .filter_map(|(address, symbol, decimals)| {
                let decimals = decimals?;
                Some((
                    address,
                    vela_relay_core::settlement::StablecoinConfig { symbol, decimals },
                ))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        *context.native_symbol.borrow_mut() = Some(native.symbol.clone());
        Out::Assets {
            resolved: core_execution::ResolvedChainAssets {
                assets: vela_relay_core::settlement::ChainAssetConfig {
                    native_decimals: native.decimals,
                    settlement_markup_bps: context.config.settlement_markup_bps,
                    stablecoins,
                },
                native_symbol: native.symbol,
            },
        }
    }

    async fn record(
        &self,
        chain_id: u64,
        hash: &str,
        command: &RecordCommand,
    ) -> std::result::Result<RecordReply, ()> {
        crate::admission::record_command(&self.env, chain_id, hash, command).await
    }

    /// Docker `TelegramAlertNotifier::notify_executor_issue`: silent no-op
    /// when unconfigured; otherwise fingerprint → TreasuryDO suppression slot
    /// (`SET NX PX cooldown`) → sendMessage; the slot is released on delivery
    /// failure so the alert can retry before the cooldown expires.
    async fn notify_executor_issue(
        &self,
        context: &BatchContext<'_>,
        chain_id: u64,
        stage: &str,
        user_operation_hash: &str,
        reason: &str,
    ) {
        let (Some(bot_token), Some(chat_id)) = (
            context.config.telegram_bot_token.as_deref(),
            context.config.telegram_chat_id.as_deref(),
        ) else {
            return;
        };
        let fingerprint = vela_relay_core::alert::alert_fingerprint(chain_id, stage, reason);
        let claim_token = unique_token("telegram", chain_id, 0);
        let claimed = match self
            .treasury(
                chain_id,
                None,
                TreasuryCommand::ClaimAlert {
                    fingerprint: fingerprint.clone(),
                    token: claim_token.clone(),
                    ttl_ms: context.config.telegram_cooldown_ms,
                },
            )
            .await
        {
            Ok(TreasuryReply::Acquired { acquired }) => acquired,
            _ => {
                worker::console_warn!(
                    "could not acquire Telegram alert suppression slot: chain_id={chain_id} stage={stage}"
                );
                return;
            }
        };
        if !claimed {
            return;
        }
        let text = vela_relay_core::alert::executor_alert_text(
            chain_id,
            stage,
            user_operation_hash,
            reason,
            context.config.telegram_cooldown_ms / 1_000,
        );
        if crate::arms::telegram::send_message(bot_token, chat_id, &text).await {
            worker::console_log!("sent Telegram executor alert: chain_id={chain_id} stage={stage}");
            return;
        }
        worker::console_warn!(
            "could not deliver Telegram executor alert; releasing suppression slot for retry: chain_id={chain_id} stage={stage}"
        );
        let _ = self
            .treasury(
                chain_id,
                None,
                TreasuryCommand::ReleaseAlert {
                    fingerprint,
                    token: claim_token,
                },
            )
            .await;
    }

    /// One TreasuryDO round-trip (instance = the chain). `renew` piggybacks a
    /// lease extension on every touch from the current holder — the docker
    /// heartbeat's counterpart (declared in the bindings contract).
    async fn treasury(
        &self,
        chain_id: u64,
        renew: Option<LeaseIdentity>,
        command: TreasuryCommand,
    ) -> std::result::Result<TreasuryReply, ()> {
        let namespace = self.env.durable_object("TREASURY").map_err(|_| ())?;
        let id = namespace
            .id_from_name(&chain_id.to_string())
            .map_err(|_| ())?;
        let stub = id.get_stub().map_err(|_| ())?;

        let body = serde_json::to_string(&TreasuryRequest { renew, command }).map_err(|_| ())?;
        let headers = worker::Headers::new();
        headers
            .set("content-type", "application/json")
            .map_err(|_| ())?;
        let mut init = worker::RequestInit::new();
        init.with_method(worker::Method::Post)
            .with_headers(headers)
            .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));
        let request =
            worker::Request::new_with_init("https://treasury-do/", &init).map_err(|_| ())?;
        let mut response = stub.fetch_with_request(request).await.map_err(|_| ())?;
        if response.status_code() != 200 {
            return Err(());
        }
        // Text + serde_json, never a JsValue round-trip: the funding intent's
        // u128 `amount_wei` would degrade to a float.
        let text = response.text().await.map_err(|_| ())?;
        serde_json::from_str::<TreasuryReply>(&text).map_err(|_| ())
    }

    async fn intent(&self) -> Option<PreparedBundleIntent> {
        self.state.storage().get(INTENT_KEY).await.ok().flatten()
    }

    /// Guarded delete; reports whether this caller removed the intent (the
    /// docker store's `clear_prepared_bundle_intent` atomicity contract).
    async fn clear_intent_if_matches(&self, transaction_hash: &str) -> Result<bool> {
        if let Some(intent) = self.intent().await
            && intent
                .transaction_hash
                .eq_ignore_ascii_case(transaction_hash)
        {
            self.state.storage().delete(INTENT_KEY).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Docker engine `clear_stale_bundle_intent`: only the caller that
    /// atomically removed the intent clears the seen cache and logs.
    async fn clear_stale_bundle_intent(
        &self,
        context: &BatchContext<'_>,
        intent: &PreparedBundleIntent,
        reason: &str,
    ) -> std::result::Result<(), String> {
        let cleared = self
            .clear_intent_if_matches(&intent.transaction_hash)
            .await
            .map_err(|_| "could not clear stale prepared bundle intent".to_owned())?;
        if !cleared {
            return Ok(());
        }
        self.forget_broadcast(&intent.transaction_hash).await;
        let relayer = relayer_address_for_lane(context, intent.lane)
            .map(|relayer| relayer.to_string())
            .unwrap_or_else(|| "<underivable>".into());
        worker::console_warn!(
            "discarded a prepared handleOps transaction whose nonce is already mined; queued operations will be rebuilt: chain_id={} lane={} relayer={relayer} stale_nonce={} transaction_hash={} reason={reason}",
            intent.chain_id,
            intent.lane,
            intent.nonce,
            intent.transaction_hash
        );
        Ok(())
    }

    async fn remember_broadcast(&self, transaction_hash: &str) {
        let _ = self
            .state
            .storage()
            .put(&format!("seen:{transaction_hash}"), Date::now().as_millis())
            .await;
    }

    async fn forget_broadcast(&self, transaction_hash: &str) {
        let _ = self
            .state
            .storage()
            .delete(&format!("seen:{transaction_hash}"))
            .await;
    }

    // --- prepared-intent recovery (docker engine `resume_bundle_intent`) ---

    async fn resume_bundle_intent(
        &self,
        context: &BatchContext<'_>,
        intent: &PreparedBundleIntent,
    ) -> std::result::Result<BundleResumeDisposition, String> {
        let audit = self.audit_bundle_replay(intent).await?;
        if audit.active == 0 && audit.terminal != 0 {
            self.clear_obsolete_bundle_intent(intent, audit).await?;
            return Ok(BundleResumeDisposition::Cleared);
        }
        if audit.terminal != 0 || audit.expired != 0 {
            worker::console_warn!(
                "replaying prepared bundle after auditing unavailable members: chain_id={} lane={} transaction_hash={} active_members={} terminal_members={} expired_members={}",
                intent.chain_id,
                intent.lane,
                intent.transaction_hash,
                audit.active,
                audit.terminal,
                audit.expired
            );
        }
        match self.broadcast_bundle_intent(context, intent).await? {
            BundleBroadcastDisposition::Unknown => {
                return Ok(BundleResumeDisposition::Unknown);
            }
            BundleBroadcastDisposition::Confirmed => {}
        }
        if audit.active == 0 {
            // Lifecycle records expire sooner than the signed outbox. Without a terminal record
            // or receipt there is no proof that this transaction is safe to forget, so retain it
            // and keep reconciling the relayer nonce.
            return Ok(BundleResumeDisposition::Confirmed);
        }
        let indexed = self.submit_bundle_members(intent.chain_id, intent).await?;
        if indexed != audit.active {
            // Records retain a shorter TTL than prepared outbox entries. A member can expire or
            // reach a terminal state between the preflight audit and this atomic transition.
            // Re-audit before deciding that the lane is corrupt; exact submitted membership is
            // also safe when an earlier recovery attempt already completed the transition.
            let after = self.audit_bundle_replay(intent).await?;
            if after.active == 0 {
                if after.terminal != 0 {
                    self.clear_obsolete_bundle_intent(intent, after).await?;
                    return Ok(BundleResumeDisposition::Cleared);
                }
                return Ok(BundleResumeDisposition::Confirmed);
            }
            if after.awaiting_submission != 0 {
                return Err(
                    "prepared bundle has live members that could not enter submitted state".into(),
                );
            }
        }
        Ok(BundleResumeDisposition::Confirmed)
    }

    async fn audit_bundle_replay(
        &self,
        intent: &PreparedBundleIntent,
    ) -> std::result::Result<core_execution::BundleReplayAudit, String> {
        let mut records = Vec::with_capacity(intent.user_operation_hashes.len());
        for hash in &intent.user_operation_hashes {
            match self
                .record(intent.chain_id, hash, &RecordCommand::Get)
                .await
            {
                Ok(RecordReply::Record { record }) => records.push(record.map(|record| *record)),
                _ => return Err("could not read UserOperation records".into()),
            }
        }
        core_execution::audit_bundle_replay(intent, &records)
    }

    async fn clear_obsolete_bundle_intent(
        &self,
        intent: &PreparedBundleIntent,
        audit: core_execution::BundleReplayAudit,
    ) -> std::result::Result<(), String> {
        if audit.terminal == 0 {
            return Err("refusing to clear an unproven prepared bundle".into());
        }
        self.clear_intent_if_matches(&intent.transaction_hash)
            .await
            .map_err(|_| "could not clear prepared bundle intent".to_owned())?;
        self.forget_broadcast(&intent.transaction_hash).await;
        worker::console_warn!(
            "cleared prepared bundle with no live lifecycle members: chain_id={} lane={} transaction_hash={} terminal_members={} expired_members={}",
            intent.chain_id,
            intent.lane,
            intent.transaction_hash,
            audit.terminal,
            audit.expired
        );
        Ok(())
    }

    /// Broadcasts the exact durable bytes. An ambiguous send is not mempool admission: the
    /// expected transaction hash must be observable before callers may persist `submitted`.
    async fn broadcast_bundle_intent(
        &self,
        context: &BatchContext<'_>,
        intent: &PreparedBundleIntent,
    ) -> std::result::Result<BundleBroadcastDisposition, String> {
        let raw = validate_raw_transaction(&intent.raw_transaction, &intent.transaction_hash)
            .map_err(|error| error.to_string())?;
        if self.broadcast_seen(&intent.transaction_hash).await {
            return Ok(BundleBroadcastDisposition::Confirmed);
        }
        let outcome = match context
            .trusted
            .broadcast_raw_transaction(intent.chain_id, &raw)
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                self.forget_broadcast(&intent.transaction_hash).await;
                return Err(error.to_string());
            }
        };
        use crate::arms::trusted::BroadcastOutcome;
        match outcome {
            BroadcastOutcome::Accepted(hash)
                if hash.eq_ignore_ascii_case(&intent.transaction_hash) =>
            {
                self.remember_broadcast(&intent.transaction_hash).await;
                Ok(BundleBroadcastDisposition::Confirmed)
            }
            BroadcastOutcome::Accepted(_) => {
                self.forget_broadcast(&intent.transaction_hash).await;
                Err("RPC returned a transaction hash different from the signed bytes".into())
            }
            BroadcastOutcome::Ambiguous(reason) => {
                self.forget_broadcast(&intent.transaction_hash).await;
                if transaction_is_known(context.trusted, intent.chain_id, &intent.transaction_hash)
                    .await
                {
                    self.remember_broadcast(&intent.transaction_hash).await;
                    Ok(BundleBroadcastDisposition::Confirmed)
                } else if nonce_too_low(&reason) && bundle_nonce_is_stale(context, intent).await {
                    self.clear_stale_bundle_intent(context, intent, &reason)
                        .await?;
                    Ok(BundleBroadcastDisposition::Unknown)
                } else {
                    worker::console_warn!(
                        "ambiguous handleOps broadcast is not yet observable: chain_id={} lane={} transaction_hash={} reason={reason}",
                        intent.chain_id,
                        intent.lane,
                        intent.transaction_hash
                    );
                    Ok(BundleBroadcastDisposition::Unknown)
                }
            }
            BroadcastOutcome::Rejected(reason) => {
                self.forget_broadcast(&intent.transaction_hash).await;
                if transaction_is_known(context.trusted, intent.chain_id, &intent.transaction_hash)
                    .await
                {
                    self.remember_broadcast(&intent.transaction_hash).await;
                    return Ok(BundleBroadcastDisposition::Confirmed);
                }
                if nonce_too_low(&reason) && bundle_nonce_is_stale(context, intent).await {
                    self.clear_stale_bundle_intent(context, intent, &reason)
                        .await?;
                    return Ok(BundleBroadcastDisposition::Unknown);
                }
                worker::console_warn!(
                    "rejected broadcast is unproven; retaining exact handleOps outbox: chain_id={} lane={} transaction_hash={} reason={reason}",
                    intent.chain_id,
                    intent.lane,
                    intent.transaction_hash
                );
                Ok(BundleBroadcastDisposition::Unknown)
            }
        }
    }

    async fn broadcast_seen(&self, transaction_hash: &str) -> bool {
        let seen_at: Option<u64> = self
            .state
            .storage()
            .get(&format!("seen:{transaction_hash}"))
            .await
            .ok()
            .flatten();
        seen_at.is_some_and(|seen_at| {
            Date::now().as_millis().saturating_sub(seen_at) < BROADCAST_RETRY_INTERVAL_MS
        })
    }

    async fn defer_operation(
        &self,
        chain_id: u64,
        batch: &[RoutedUserOperation],
        index: usize,
        cause: &core_execution::DeferCause,
    ) -> Out {
        let Some(routed) = batch.get(index) else {
            return Out::Failed {
                message: "could not persist deferred UserOperation".into(),
            };
        };
        let key = format!("delayed:{}", routed.user_operation_hash);
        let existing: Option<DelayedEntry> = self.state.storage().get(&key).await.ok().flatten();
        let attempts = existing.map(|entry| entry.attempts).unwrap_or(0) + 1;
        let now = Date::now().as_millis();
        let due_ms = now + vela_relay_core::hold::retry_delay_ms(attempts);
        let entry = DelayedEntry {
            routed: routed.clone(),
            attempts,
            due_ms,
            created_ms: now,
            updated_ms: now,
        };
        if self.state.storage().put(&key, &entry).await.is_err() {
            self.log_defer_failure(chain_id, &routed.user_operation_hash, cause);
            return Out::Failed {
                message: "could not persist deferred UserOperation".into(),
            };
        }
        let mut index_list: Vec<String> = self
            .state
            .storage()
            .get("delayed_index")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        if !index_list.contains(&routed.user_operation_hash) {
            index_list.push(routed.user_operation_hash.clone());
            let _ = self.state.storage().put("delayed_index", &index_list).await;
        }
        let _ = self.schedule_lane_alarm().await;
        self.log_defer_success(chain_id, &routed.user_operation_hash, attempts, cause);
        Out::Deferred { attempt: attempts }
    }

    async fn mark_bundle_submitted(
        &self,
        chain_id: u64,
        _lane: u8,
        intent: &PreparedBundleIntent,
        _gas_limit: u64,
    ) -> Out {
        match self.submit_bundle_members(chain_id, intent).await {
            Ok(indexed) => {
                // A submitted bundle now has a receipt to reconcile.
                self.arm_reconcile().await;
                Out::Indexed { indexed }
            }
            Err(message) => Out::Failed { message },
        }
    }

    /// The docker store's `mark_bundle_submitted`, spelled as per-member
    /// guarded RecordDO transitions plus the lane's bundle index.
    async fn submit_bundle_members(
        &self,
        chain_id: u64,
        intent: &PreparedBundleIntent,
    ) -> std::result::Result<usize, String> {
        let mut indexed = 0usize;
        let mut members: Vec<String> = Vec::new();
        for hash in &intent.user_operation_hashes {
            match self
                .record(
                    chain_id,
                    hash,
                    &RecordCommand::MarkBundleMemberSubmitted {
                        bundle_chain_id: intent.chain_id,
                        transaction_hash: intent.transaction_hash.clone(),
                    },
                )
                .await
            {
                Ok(RecordReply::Indexed { indexed: true }) => {
                    indexed += 1;
                    members.push(hash.clone());
                }
                Ok(_) => {}
                Err(()) => {
                    return Err("could not mark bundle members submitted".into());
                }
            }
        }
        let _ = self
            .state
            .storage()
            .put(&format!("bundle:{}", intent.transaction_hash), &members)
            .await;
        Ok(indexed)
    }

    async fn dead_letter(&self, batch: &[RoutedUserOperation], index: usize, reason: &str) -> Out {
        let Some(routed) = batch.get(index) else {
            return Out::Failed {
                message: "could not dead-letter queue message".into(),
            };
        };
        let Ok(queue) = self.env.queue("DLQ_QUEUE") else {
            return Out::Failed {
                message: "could not dead-letter queue message".into(),
            };
        };
        let payload = serde_json::json!({
            "reason": reason,
            "chainId": routed.chain_id,
            "userOperationHash": routed.user_operation_hash,
            "envelope": routed.user_operation,
        });
        match queue.send(&payload).await {
            Ok(()) => {
                worker::console_error!(
                    "dead-lettered queue message: chain_id={} user_operation_hash={} reason={reason}",
                    routed.chain_id,
                    routed.user_operation_hash
                );
                Out::Persisted { persisted: true }
            }
            Err(_) => Out::Failed {
                message: "could not dead-letter queue message".into(),
            },
        }
    }

    async fn delayed_index(&self) -> Vec<String> {
        self.state
            .storage()
            .get("delayed_index")
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    async fn delayed_entry(&self, hash: &str) -> Option<DelayedEntry> {
        self.state
            .storage()
            .get(&format!("delayed:{hash}"))
            .await
            .ok()
            .flatten()
    }

    async fn remove_delayed_entry(&self, hash: &str) {
        let _ = self
            .state
            .storage()
            .delete(&format!("delayed:{hash}"))
            .await;
        let mut index_list = self.delayed_index().await;
        index_list.retain(|entry| entry != hash);
        let _ = self.state.storage().put("delayed_index", &index_list).await;
    }

    async fn reconcile_due(&self) -> Option<u64> {
        self.state
            .storage()
            .get(RECONCILE_DUE_KEY)
            .await
            .ok()
            .flatten()
    }

    /// Arms (or advances) the reconcile deadline to at most one poll interval
    /// from now — the docker reconciler's tick, packed into the lane alarm.
    async fn arm_reconcile(&self) {
        let poll_ms = CfConfig::from_env(&self.env)
            .map(|config| config.receipt_poll_ms)
            .unwrap_or(3_000);
        let due = Date::now().as_millis() + poll_ms;
        let due = match self.reconcile_due().await {
            Some(existing) if existing <= due => existing,
            _ => due,
        };
        let _ = self.state.storage().put(RECONCILE_DUE_KEY, due).await;
        let _ = self.schedule_lane_alarm().await;
    }

    /// The single packed alarm: earliest-of(delayed dues, reconcile-due while
    /// an intent exists).
    async fn schedule_lane_alarm(&self) -> Result<()> {
        let mut earliest: Option<u64> = None;
        for hash in &self.delayed_index().await {
            if let Some(entry) = self.delayed_entry(hash).await {
                earliest = Some(earliest.map_or(entry.due_ms, |due| due.min(entry.due_ms)));
            }
        }
        if self.intent().await.is_some()
            && let Some(due) = self.reconcile_due().await
        {
            earliest = Some(earliest.map_or(due, |existing| existing.min(due)));
        }
        if let Some(due_ms) = earliest {
            let now = Date::now().as_millis();
            let delay = due_ms.saturating_sub(now).max(1);
            self.state
                .storage()
                .set_alarm(std::time::Duration::from_millis(delay))
                .await?;
        }
        Ok(())
    }

    /// One reconcile pass for this lane's prepared intent — the docker
    /// `reconcile_prepared_bundles` body scoped to one lane: resume, then a
    /// receipt check, then per-member confirmation and intent clearance. The
    /// alarm cadence is the receipt-probe throttle (this DO is the lane's
    /// only prober).
    async fn reconcile_intent(&self) {
        let Some(intent) = self.intent().await else {
            let _ = self.state.storage().delete(RECONCILE_DUE_KEY).await;
            return;
        };
        let Ok(config) = CfConfig::from_env(&self.env) else {
            return;
        };
        let Ok(policy) = config.execution_policy(intent.chain_id, intent.lane) else {
            return;
        };
        let trusted = TrustedRpcClient::new(&config, &self.env);
        let context = BatchContext {
            config: &config,
            policy: &policy,
            trusted: &trusted,
            native_symbol: RefCell::new(None),
            treasury_token: unique_token("treasury", intent.chain_id, intent.lane),
        };

        match self.resume_bundle_intent(&context, &intent).await {
            Ok(BundleResumeDisposition::Confirmed) => {
                if self.intent().await.is_some() {
                    self.check_bundle_receipt(&context, &intent).await;
                }
            }
            Ok(BundleResumeDisposition::Cleared) => {}
            Ok(BundleResumeDisposition::Unknown) => {}
            Err(message) => {
                worker::console_warn!(
                    "could not resume prepared bundle: chain_id={} lane={} transaction_hash={} error={message}",
                    intent.chain_id,
                    intent.lane,
                    intent.transaction_hash
                );
            }
        }

        // Keep ticking while the intent survives; clear the deadline once it
        // is gone.
        if self.intent().await.is_some() {
            let due = Date::now().as_millis() + config.receipt_poll_ms;
            let _ = self.state.storage().put(RECONCILE_DUE_KEY, due).await;
        } else {
            let _ = self.state.storage().delete(RECONCILE_DUE_KEY).await;
        }
    }

    /// Docker's receipt half of the reconciler: fetch the bundle receipt and
    /// apply `mark_bundle_confirmed` / `mark_bundle_failed` through per-member
    /// record patches (core `receipt` rules, frozen patch shape), then clear
    /// the intent.
    async fn check_bundle_receipt(
        &self,
        context: &BatchContext<'_>,
        intent: &PreparedBundleIntent,
    ) {
        let receipt = match context
            .trusted
            .call(
                intent.chain_id,
                "eth_getTransactionReceipt",
                json!([intent.transaction_hash]),
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                worker::console_warn!(
                    "bundle receipt batch RPC failed: chain_id={} error={error}",
                    intent.chain_id
                );
                return;
            }
        };
        if receipt.is_null() {
            return;
        }
        let members: Vec<String> = self
            .state
            .storage()
            .get(&format!("bundle:{}", intent.transaction_hash))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        match vela_relay_core::receipt::receipt_succeeded(&receipt) {
            None => {
                worker::console_warn!(
                    "bundle receipt has an invalid status: chain_id={} lane={} transaction_hash={}",
                    intent.chain_id,
                    intent.lane,
                    intent.transaction_hash
                );
                return;
            }
            Some(false) => {
                for hash in &members {
                    let patch = receipt_patch("failed", &intent.transaction_hash, &receipt, None);
                    if self
                        .record(intent.chain_id, hash, &RecordCommand::Patch { patch })
                        .await
                        .is_err()
                    {
                        worker::console_warn!(
                            "could not persist reconciled bundle receipt: chain_id={} lane={} transaction_hash={}",
                            intent.chain_id,
                            intent.lane,
                            intent.transaction_hash
                        );
                        return;
                    }
                }
            }
            Some(true) => {
                let Ok(entry_point) = intent.entry_point.parse::<Address>() else {
                    worker::console_warn!(
                        "prepared bundle EntryPoint is invalid: chain_id={} lane={} transaction_hash={}",
                        intent.chain_id,
                        intent.lane,
                        intent.transaction_hash
                    );
                    return;
                };
                let events = vela_relay_core::receipt::user_operation_events(
                    &receipt,
                    entry_point,
                    &members,
                );
                for hash in &members {
                    let event = events
                        .iter()
                        .find(|event| event.user_operation_hash == *hash);
                    let status = if event.is_some_and(|event| event.success) {
                        "included"
                    } else {
                        "rejected"
                    };
                    let patch = receipt_patch(status, &intent.transaction_hash, &receipt, event);
                    if self
                        .record(intent.chain_id, hash, &RecordCommand::Patch { patch })
                        .await
                        .is_err()
                    {
                        worker::console_warn!(
                            "could not persist reconciled bundle receipt: chain_id={} lane={} transaction_hash={}",
                            intent.chain_id,
                            intent.lane,
                            intent.transaction_hash
                        );
                        return;
                    }
                }
            }
        }
        let _ = self.clear_intent_if_matches(&intent.transaction_hash).await;
        self.forget_broadcast(&intent.transaction_hash).await;
        let _ = self
            .state
            .storage()
            .delete(&format!("bundle:{}", intent.transaction_hash))
            .await;
        worker::console_log!(
            "reconciled bundle receipt: chain_id={} lane={} transaction_hash={} members={}",
            intent.chain_id,
            intent.lane,
            intent.transaction_hash,
            members.len()
        );
    }

    /// Docker `reconcile_delayed_user_operations` for this lane: due entries
    /// re-drive through the same batch entry; a durable outcome completes the
    /// entry unless the batch re-parked it (the claim fencing — a re-park
    /// rewrites the entry, which invalidates this pass's snapshot); a
    /// transient outcome climbs the retry ladder in place.
    async fn redrive_due_delayed(&self, now: u64) {
        let retention_ms = CfConfig::from_env(&self.env)
            .map(|config| config.attempt_ttl_ms)
            .unwrap_or(0)
            .max(QUEUE_RETENTION_MS);
        let mut due = Vec::new();
        for hash in self.delayed_index().await {
            let Some(entry) = self.delayed_entry(&hash).await else {
                self.remove_delayed_entry(&hash).await;
                continue;
            };
            let touched = entry.updated_ms.max(entry.created_ms);
            if now.saturating_sub(touched) > retention_ms {
                worker::console_warn!(
                    "dropping expired delayed UserOperation: chain_id={} user_operation_hash={hash}",
                    entry.routed.chain_id
                );
                self.remove_delayed_entry(&hash).await;
                continue;
            }
            if entry.due_ms <= now && due.len() < DELAYED_REDRIVE_BATCH {
                due.push((hash, entry));
            }
        }
        if due.is_empty() {
            return;
        }
        let snapshots: Vec<(String, u32, u64)> = due
            .iter()
            .map(|(hash, entry)| (hash.clone(), entry.attempts, entry.due_ms))
            .collect();
        let operations: Vec<RoutedUserOperation> =
            due.into_iter().map(|(_, entry)| entry.routed).collect();
        let resolutions = self.execute_batch(operations).await;
        for ((hash, attempts, due_ms), resolution) in snapshots.into_iter().zip(resolutions) {
            let Some(entry) = self.delayed_entry(&hash).await else {
                continue;
            };
            if entry.attempts != attempts || entry.due_ms != due_ms {
                // Re-parked during the batch (defer bumped the entry): the
                // fresh schedule stands.
                continue;
            }
            match resolution {
                core_execution::ItemResolution::Durable => {
                    self.remove_delayed_entry(&hash).await;
                }
                core_execution::ItemResolution::Failed { .. } => {
                    let attempts = entry.attempts + 1;
                    let retry = DelayedEntry {
                        attempts,
                        due_ms: Date::now().as_millis()
                            + vela_relay_core::hold::retry_delay_ms(attempts),
                        updated_ms: Date::now().as_millis(),
                        ..entry
                    };
                    let _ = self
                        .state
                        .storage()
                        .put(&format!("delayed:{hash}"), &retry)
                        .await;
                }
            }
        }
    }

    fn log_rejection_cause(
        &self,
        chain_id: u64,
        hash: &str,
        cause: &core_execution::RejectionCause,
    ) {
        match cause {
            core_execution::RejectionCause::InvalidQueuedPayload { reason } => {
                worker::console_warn!(
                    "rejected invalid queued UserOperation: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
                );
            }
            core_execution::RejectionCause::SimulationRejected { reason } => {
                worker::console_warn!(
                    "single-operation simulation rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
                );
            }
            core_execution::RejectionCause::StaleNonce {
                user_nonce,
                onchain_nonce,
            } => {
                worker::console_warn!(
                    "stale account nonce rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} user_nonce={user_nonce} onchain_nonce={onchain_nonce}"
                );
            }
            core_execution::RejectionCause::UnsupportedTempoFeeToken { fee_token } => {
                worker::console_warn!(
                    "Tempo UserOperation requested an unsupported fee token: chain_id={chain_id} user_operation_hash={hash} fee_token={fee_token:?}"
                );
            }
        }
    }

    fn log_defer_success(
        &self,
        chain_id: u64,
        hash: &str,
        attempt: u32,
        cause: &core_execution::DeferCause,
    ) {
        match cause {
            core_execution::DeferCause::AffordableMarketHold => {
                worker::console_log!(
                    "holding UserOperation until the market fits its signed reimbursement: chain_id={chain_id} user_operation_hash={hash} attempt={attempt}"
                );
            }
            core_execution::DeferCause::FutureNonce {
                user_nonce,
                onchain_nonce,
            } => {
                worker::console_log!(
                    "future account nonce moved to durable delayed inbox: chain_id={chain_id} user_operation_hash={hash} user_nonce={user_nonce} onchain_nonce={onchain_nonce} attempt={attempt}"
                );
            }
        }
    }

    fn log_defer_failure(&self, chain_id: u64, hash: &str, cause: &core_execution::DeferCause) {
        match cause {
            core_execution::DeferCause::AffordableMarketHold => {
                worker::console_warn!(
                    "could not hold UserOperation for a cheaper market: chain_id={chain_id} user_operation_hash={hash}"
                );
            }
            core_execution::DeferCause::FutureNonce { .. } => {
                worker::console_warn!(
                    "could not persist future nonce in delayed inbox: chain_id={chain_id} user_operation_hash={hash}"
                );
            }
        }
    }

    fn emit_diagnostic(
        &self,
        chain_id: u64,
        lane: u8,
        diagnostic: &core_execution::ExecutionDiagnostic,
    ) {
        use core_execution::ExecutionDiagnostic as Diagnostic;
        match diagnostic {
            Diagnostic::SimulationDeploymentWait { hash, reason } => worker::console_log!(
                "single-operation simulation is waiting for automatic contract deployment: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
            ),
            Diagnostic::SimulationUnavailable { hash, reason } => worker::console_warn!(
                "single-operation simulation unavailable: chain_id={chain_id} user_operation_hash={hash} reason={reason}"
            ),
            Diagnostic::BundleSimulationRejected { reason } => worker::console_warn!(
                "final handleOps simulation rejected bundle: chain_id={chain_id} lane={lane} reason={reason}"
            ),
            Diagnostic::BundleSimulationNonceMismatch => worker::console_warn!(
                "final handleOps simulation reported an account nonce mismatch: chain_id={chain_id} lane={lane}"
            ),
            Diagnostic::BundleSimulationDeploymentWait { reason } => worker::console_log!(
                "final handleOps simulation is waiting for automatic contract deployment: chain_id={chain_id} lane={lane} reason={reason}"
            ),
            Diagnostic::FloorUnfundable {
                quoted_fee,
                affordable,
                floor,
                base_fee,
            } => worker::console_log!(
                "in-band reimbursement cannot fund an includable outer fee: chain_id={chain_id} quoted_fee={quoted_fee} affordable={affordable} floor={floor} base_fee={base_fee}"
            ),
            Diagnostic::Repriced {
                quoted_fee,
                repriced_fee,
                base_fee,
                tip,
            } => worker::console_log!(
                "repriced the outer transaction to the signed in-band budget: chain_id={chain_id} quoted_fee={quoted_fee} repriced_fee={repriced_fee} base_fee={base_fee} tip={tip}"
            ),
            Diagnostic::SubmissionTierCap {
                tier,
                quoted_fee,
                cap,
                base_fee,
                tip,
                market_tip,
            } => worker::console_log!(
                "submitting at the client-requested speed: chain_id={chain_id} tier={} quoted_fee={quoted_fee} cap={cap} base_fee={base_fee} tip={tip} market_tip={market_tip}",
                tier.as_str()
            ),
            Diagnostic::HoldBudgetExhausted {
                hash,
                attempt,
                paid,
                required,
            } => worker::console_warn!(
                "in-band reimbursement stayed unaffordable for the whole hold budget: chain_id={chain_id} user_operation_hash={hash} attempt={attempt} paid={paid} required={required}"
            ),
            Diagnostic::SettlementRejected {
                hash,
                payment_asset,
                paid,
                required,
                stable_logs_valid,
            } => worker::console_warn!(
                "in-band settlement rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} payment_asset={payment_asset:?} paid={paid} required={required} stable_logs_valid={stable_logs_valid}"
            ),
            Diagnostic::TempoSettlementRejected {
                hash,
                paid,
                required,
                stable_logs_valid,
            } => worker::console_warn!(
                "Tempo pathUSD in-band settlement rejected UserOperation: chain_id={chain_id} user_operation_hash={hash} paid={paid} required={required} stable_logs_valid={stable_logs_valid}"
            ),
            Diagnostic::TopUpCapUsd { native_units } => worker::console_log!(
                "using USD-denominated relayer top-up cap: chain_id={chain_id} native_units={native_units}"
            ),
            Diagnostic::TopUpCapUnconvertible => worker::console_warn!(
                "could not convert USD relayer top-up cap to native units; using static cap: chain_id={chain_id}"
            ),
            Diagnostic::ExecutionDeferred { reason } => worker::console_warn!(
                "UserOperation lane execution deferred: chain_id={chain_id} lane={lane} error={reason}"
            ),
        }
    }
}

// --- verdict/reply conversions (docker engine arm mappings, verbatim) ---

fn operation_sim_verdict(
    verdict: SimulationVerdict<SimulationResult>,
) -> core_execution::OperationSimVerdict {
    match verdict {
        SimulationVerdict::Success(_) => core_execution::OperationSimVerdict::Success,
        SimulationVerdict::NonceMismatch => core_execution::OperationSimVerdict::NonceMismatch,
        SimulationVerdict::Rejected(reason) => core_execution::OperationSimVerdict::Rejected {
            reason: reason.to_string(),
        },
        SimulationVerdict::Pending(reason) => core_execution::OperationSimVerdict::Pending {
            reason: reason.to_string(),
        },
        SimulationVerdict::Transient(reason) => core_execution::OperationSimVerdict::Transient {
            reason: reason.to_string(),
        },
    }
}

fn bundle_sim_verdict(
    verdict: SimulationVerdict<SimulationResult>,
) -> core_execution::BundleSimVerdict {
    match verdict {
        SimulationVerdict::Success(simulation) => {
            core_execution::BundleSimVerdict::Success(core_execution::BundleSimulationData {
                gas_used: simulation.gas_used,
                operation_gas_used: simulation
                    .events
                    .iter()
                    .map(|event| event.actual_gas_used)
                    .collect(),
                logs: simulation
                    .logs
                    .iter()
                    .map(|log| vela_relay_core::settlement::SettlementLog {
                        address: log.address,
                        topics: log.topics.clone(),
                        data: log.data.clone(),
                    })
                    .collect(),
            })
        }
        SimulationVerdict::NonceMismatch => core_execution::BundleSimVerdict::NonceMismatch,
        SimulationVerdict::Rejected(reason) => core_execution::BundleSimVerdict::Rejected {
            reason: reason.to_string(),
        },
        SimulationVerdict::Pending(reason) => core_execution::BundleSimVerdict::Pending {
            reason: reason.to_string(),
        },
        SimulationVerdict::Transient(reason) => core_execution::BundleSimVerdict::Transient {
            reason: reason.to_string(),
        },
    }
}

fn broadcast_reply(
    outcome: crate::arms::trusted::BroadcastOutcome,
) -> core_execution::BroadcastReply {
    use crate::arms::trusted::BroadcastOutcome;
    match outcome {
        BroadcastOutcome::Accepted(hash) => core_execution::BroadcastReply::Accepted {
            transaction_hash: hash,
        },
        BroadcastOutcome::Ambiguous(reason) => core_execution::BroadcastReply::Ambiguous { reason },
        BroadcastOutcome::Rejected(reason) => core_execution::BroadcastReply::Rejected { reason },
    }
}

/// Docker engine `transaction_context`: one five-call batch (estimate, block,
/// tip, nonce, balance) with the legacy-gas-price tip fallback; every error
/// string byte-identical.
async fn transaction_context(
    trusted: &TrustedRpcClient<'_>,
    chain_id: u64,
    relayer: Address,
    entry_point: Address,
    calldata: &[u8],
) -> std::result::Result<core_execution::TransactionContext, String> {
    let transaction = json!({
        "from": relayer.to_string(),
        "to": entry_point.to_string(),
        "data": format!("0x{}", hex::encode(calldata)),
    });
    let calls = [
        RpcBatchCall {
            method: "eth_estimateGas",
            params: json!([transaction]),
        },
        RpcBatchCall {
            method: "eth_getBlockByNumber",
            params: json!(["latest", false]),
        },
        RpcBatchCall {
            method: "eth_maxPriorityFeePerGas",
            params: json!([]),
        },
        RpcBatchCall {
            method: "eth_getTransactionCount",
            params: json!([relayer.to_string(), "pending"]),
        },
        RpcBatchCall {
            method: "eth_getBalance",
            params: json!([relayer.to_string(), "pending"]),
        },
    ];
    let responses = trusted
        .batch(chain_id, &calls)
        .await
        .map_err(|error| error.to_string())?;
    let estimated_gas = response_quantity(&responses, 0, "eth_estimateGas")?;
    let block = response_value(&responses, 1, "eth_getBlockByNumber")?;
    let base_fee = block
        .get("baseFeePerGas")
        .and_then(Value::as_str)
        .and_then(parse_quantity)
        .ok_or_else(|| "latest block has no EIP-1559 base fee".to_owned())?;
    // The market tip, by the one rule the quote shares
    // (`gas_math::market_tip`); `eth_gasPrice` only when the node named none.
    let max_priority_fee_per_gas = response_quantity_optional(&responses, 2);
    let legacy_gas_price = match max_priority_fee_per_gas {
        Some(_) => None,
        None => Some(
            trusted
                .call(chain_id, "eth_gasPrice", json!([]))
                .await
                .map_err(|error| error.to_string())?
                .as_str()
                .and_then(parse_quantity)
                .ok_or_else(|| "eth_gasPrice returned an invalid quantity".to_owned())?,
        ),
    };
    let tip =
        vela_relay_core::gas_math::market_tip(max_priority_fee_per_gas, legacy_gas_price, base_fee)
            .ok_or_else(|| "gas price is below the latest base fee".to_owned())?;
    let base_fee = u128::try_from(base_fee).map_err(|_| "base fee exceeds uint128".to_owned())?;
    let tip = u128::try_from(tip).map_err(|_| "priority fee exceeds uint128".to_owned())?;
    let max_fee_per_gas = vela_relay_core::gas_math::quoted_outer_fee(base_fee, tip)
        .ok_or_else(|| "EIP-1559 fee overflow".to_owned())?;
    let nonce = u64::try_from(response_quantity(&responses, 3, "eth_getTransactionCount")?)
        .map_err(|_| "relayer nonce exceeds uint64".to_owned())?;
    let relayer_balance = response_quantity(&responses, 4, "eth_getBalance")?;

    Ok(core_execution::TransactionContext {
        estimated_gas,
        base_fee_per_gas: base_fee,
        max_fee_per_gas,
        max_priority_fee_per_gas: tip,
        nonce,
        relayer_balance,
    })
}

/// Docker engine `market_usd_price` (Binance, 60 s cache, same error texts).
/// Pegged chains never reach this arm — the core decides the peg.
async fn market_usd_price(symbol: &str) -> std::result::Result<U256, String> {
    let symbol = symbol.trim().to_ascii_uppercase();
    if symbol.is_empty() || !symbol.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("native currency symbol is invalid".into());
    }
    let now = Date::now().as_millis();
    let cached = MARKET_PRICES.with(|prices| {
        prices
            .borrow()
            .get(&symbol)
            .filter(|(expires_at, _)| *expires_at > now)
            .map(|(_, price)| *price)
    });
    if let Some(price) = cached {
        return Ok(price);
    }
    let raw_price = market::binance_usdt_price(&symbol)
        .await
        .ok_or_else(|| "Binance native USD price request failed".to_owned())?;
    let price = vela_relay_core::settlement::parse_market_usd_price(&raw_price)
        .ok_or_else(|| "Binance native USD price is invalid".to_owned())?;
    MARKET_PRICES.with(|prices| {
        prices
            .borrow_mut()
            .insert(symbol, (now + BINANCE_PRICE_TTL_MS, price));
    });
    Ok(price)
}

/// Docker engine `SignBundle` arm: the same core signing math over the
/// vault-derived per-lane key. The key never enters the core (Constitution).
fn sign_bundle(
    context: &BatchContext<'_>,
    chain_id: u64,
    lane: u8,
    request: &core_execution::BundleSignRequest,
) -> Out {
    let Some(secret) = context.config.operator_secret.as_deref() else {
        return Out::Failed {
            message: "OPERATOR_SECRET is required for execution".into(),
        };
    };
    let key = match vault::derive_pool_relayer_secret_key(secret, lane as usize) {
        Ok(key) => key,
        Err(error) => {
            return Out::Failed {
                message: error.to_string(),
            };
        }
    };
    match vela_relay_core::signing::sign_eip1559(
        &key,
        vela_relay_core::signing::TransactionPlan {
            chain_id,
            nonce: request.nonce,
            gas_limit: request.gas_limit,
            max_fee_per_gas: request.max_fee_per_gas,
            max_priority_fee_per_gas: request.max_priority_fee_per_gas,
            to: request.entry_point,
            value: U256::ZERO,
            input: request.calldata.clone().into(),
        },
    ) {
        Ok(signed) => Out::Signed {
            signed: core_execution::SignedBundle {
                raw_transaction_hex: format!("0x{}", hex::encode(&signed.raw_transaction)),
                transaction_hash: signed.transaction_hash,
                nonce: signed.nonce,
            },
        },
        Err(error) => Out::Failed {
            message: error.to_string(),
        },
    }
}

/// Docker engine `tempo_transaction_context`: one four-call batch (latest
/// block, gas price, relayer nonce, relayer pathUSD balance); the base fee
/// falls back from the block header to `eth_gasPrice` to the pinned Tempo
/// constant, every error string byte-identical.
async fn tempo_transaction_context(
    trusted: &TrustedRpcClient<'_>,
    chain_id: u64,
    relayer: Address,
) -> std::result::Result<(U256, u64, U256), String> {
    use vela_relay_core::tempo;
    let calls = [
        RpcBatchCall {
            method: "eth_getBlockByNumber",
            params: json!(["latest", false]),
        },
        RpcBatchCall {
            method: "eth_gasPrice",
            params: json!([]),
        },
        RpcBatchCall {
            method: "eth_getTransactionCount",
            params: json!([relayer.to_string(), "pending"]),
        },
        RpcBatchCall {
            method: "eth_call",
            params: json!([{
                "to": tempo::PATH_USD.to_string(),
                "data": format!(
                    "0x{}",
                    hex::encode(tempo::path_usd_balance_calldata(relayer))
                ),
            }, "latest"]),
        },
    ];
    let responses = trusted
        .batch(chain_id, &calls)
        .await
        .map_err(|error| error.to_string())?;
    let base_fee_atto = response_value(&responses, 0, "Tempo latest block")?
        .get("baseFeePerGas")
        .and_then(Value::as_str)
        .and_then(parse_quantity)
        .or_else(|| response_quantity_optional(&responses, 1))
        .unwrap_or_else(|| U256::from(tempo::TEMPO_BASE_FEE_ATTO));
    let nonce = u64::try_from(response_quantity(&responses, 2, "Tempo relayer nonce")?)
        .map_err(|_| "Tempo relayer nonce exceeds uint64".to_owned())?;
    let relayer_path_usd_balance = response_abi_u256(&responses, 3, "Tempo pathUSD balance")?;
    Ok((base_fee_atto, nonce, relayer_path_usd_balance))
}

/// Docker engine `FetchTempoTreasuryContext` arm: treasury nonce, treasury
/// pathUSD balance, and the raw gas estimate of the exact pathUSD transfer
/// (the buffer is applied by the core).
async fn tempo_treasury_context(
    trusted: &TrustedRpcClient<'_>,
    chain_id: u64,
    treasury: Address,
    relayer: Address,
    transfer_amount: U256,
) -> std::result::Result<(u64, U256, u64), String> {
    use vela_relay_core::tempo;
    let transfer_calldata = tempo::path_usd_transfer_calldata(relayer, transfer_amount);
    let calls = [
        RpcBatchCall {
            method: "eth_getTransactionCount",
            params: json!([treasury.to_string(), "pending"]),
        },
        RpcBatchCall {
            method: "eth_call",
            params: json!([{
                "to": tempo::PATH_USD.to_string(),
                "data": format!(
                    "0x{}",
                    hex::encode(tempo::path_usd_balance_calldata(treasury))
                ),
            }, "latest"]),
        },
        RpcBatchCall {
            method: "eth_estimateGas",
            params: json!([{
                "from": treasury.to_string(),
                "to": tempo::PATH_USD.to_string(),
                "data": format!("0x{}", hex::encode(&transfer_calldata)),
                "feeToken": tempo::PATH_USD.to_string(),
            }, "latest"]),
        },
    ];
    let responses = trusted
        .batch(chain_id, &calls)
        .await
        .map_err(|error| error.to_string())?;
    let nonce = u64::try_from(response_quantity(&responses, 0, "Tempo treasury nonce")?)
        .map_err(|_| "Tempo treasury nonce exceeds uint64".to_owned())?;
    let balance = response_abi_u256(&responses, 1, "Tempo treasury pathUSD balance")?;
    let raw_gas_estimate = u64::try_from(response_quantity(
        &responses,
        2,
        "Tempo pathUSD top-up eth_estimateGas",
    )?)
    .map_err(|_| "Tempo pathUSD top-up gas estimate exceeds uint64".to_owned())?;
    Ok((nonce, balance, raw_gas_estimate))
}

/// Docker engine `SignTempoBundle` arm: the Tempo `0x76` handleOps envelope
/// over the vault-derived per-lane key (fee token pathUSD, tip 0).
fn sign_tempo_bundle(
    context: &BatchContext<'_>,
    chain_id: u64,
    lane: u8,
    request: &core_execution::TempoSignRequest,
) -> Out {
    let Some(secret) = context.config.operator_secret.as_deref() else {
        return Out::Failed {
            message: "OPERATOR_SECRET is required for execution".into(),
        };
    };
    let key = match vault::derive_pool_relayer_secret_key(secret, lane as usize) {
        Ok(key) => key,
        Err(error) => {
            return Out::Failed {
                message: error.to_string(),
            };
        }
    };
    match vela_relay_core::signing::sign_tempo(
        &key,
        vela_relay_core::signing::TempoTransactionPlan {
            chain_id,
            nonce: request.nonce,
            gas_limit: request.gas_limit,
            max_fee_per_gas: request.max_fee_per_gas,
            max_priority_fee_per_gas: 0,
            fee_token: vela_relay_core::tempo::PATH_USD,
            to: request.entry_point,
            input: request.calldata.clone().into(),
        },
    ) {
        Ok(signed) => Out::Signed {
            signed: core_execution::SignedBundle {
                raw_transaction_hex: format!("0x{}", hex::encode(&signed.raw_transaction)),
                transaction_hash: signed.transaction_hash,
                nonce: signed.nonce,
            },
        },
        Err(error) => Out::Failed {
            message: error.to_string(),
        },
    }
}

/// Docker engine `SignTreasuryTransfer` arm: a plain-value EIP-1559 transfer
/// from the treasury to this lane's relayer, gas pinned to the core's
/// `TOP_UP_GAS_LIMIT`.
fn sign_treasury_transfer(
    context: &BatchContext<'_>,
    chain_id: u64,
    request: &core_execution::TreasurySignRequest,
) -> Out {
    let Some(secret) = context.config.operator_secret.as_deref() else {
        return Out::Failed {
            message: "OPERATOR_SECRET is required for execution".into(),
        };
    };
    let key = match vault::derive_treasury_secret_key(secret) {
        Ok(key) => key,
        Err(error) => {
            return Out::Failed {
                message: error.to_string(),
            };
        }
    };
    match vela_relay_core::signing::sign_eip1559(
        &key,
        vela_relay_core::signing::TransactionPlan {
            chain_id,
            nonce: request.nonce,
            gas_limit: vela_relay_core::funding::TOP_UP_GAS_LIMIT,
            max_fee_per_gas: request.max_fee_per_gas,
            max_priority_fee_per_gas: request.max_priority_fee_per_gas,
            to: context.policy.relayer,
            value: request.amount,
            input: alloy::primitives::Bytes::new(),
        },
    ) {
        Ok(signed) => Out::Signed {
            signed: core_execution::SignedBundle {
                raw_transaction_hex: format!("0x{}", hex::encode(&signed.raw_transaction)),
                transaction_hash: signed.transaction_hash,
                nonce: signed.nonce,
            },
        },
        Err(error) => Out::Failed {
            message: error.to_string(),
        },
    }
}

/// Docker engine `SignTreasuryPathUsd` arm: the Tempo `0x76` pathUSD transfer
/// from the treasury to this lane's relayer (fee token pathUSD, tip 0).
fn sign_treasury_path_usd(
    context: &BatchContext<'_>,
    chain_id: u64,
    request: &core_execution::TempoTreasurySignRequest,
) -> Out {
    let Some(secret) = context.config.operator_secret.as_deref() else {
        return Out::Failed {
            message: "OPERATOR_SECRET is required for execution".into(),
        };
    };
    let key = match vault::derive_treasury_secret_key(secret) {
        Ok(key) => key,
        Err(error) => {
            return Out::Failed {
                message: error.to_string(),
            };
        }
    };
    match vela_relay_core::signing::sign_tempo(
        &key,
        vela_relay_core::signing::TempoTransactionPlan {
            chain_id,
            nonce: request.nonce,
            gas_limit: request.gas_limit,
            max_fee_per_gas: request.max_fee_per_gas,
            max_priority_fee_per_gas: 0,
            fee_token: vela_relay_core::tempo::PATH_USD,
            to: vela_relay_core::tempo::PATH_USD,
            input: vela_relay_core::tempo::path_usd_transfer_calldata(
                context.policy.relayer,
                request.amount,
            ),
        },
    ) {
        Ok(signed) => Out::Signed {
            signed: core_execution::SignedBundle {
                raw_transaction_hex: format!("0x{}", hex::encode(&signed.raw_transaction)),
                transaction_hash: signed.transaction_hash,
                nonce: signed.nonce,
            },
        },
        Err(error) => Out::Failed {
            message: error.to_string(),
        },
    }
}

/// Docker engine `transaction_is_known`: the expected hash must be observable.
async fn transaction_is_known(
    trusted: &TrustedRpcClient<'_>,
    chain_id: u64,
    expected_hash: &str,
) -> bool {
    match trusted
        .call(chain_id, "eth_getTransactionByHash", json!([expected_hash]))
        .await
    {
        Ok(Value::Object(transaction)) => transaction
            .get("hash")
            .and_then(Value::as_str)
            .is_some_and(|hash| hash.eq_ignore_ascii_case(expected_hash)),
        Ok(_) => false,
        Err(error) => {
            worker::console_warn!(
                "could not confirm ambiguous transaction broadcast: chain_id={chain_id} transaction_hash={expected_hash} error={error}"
            );
            false
        }
    }
}

/// Docker engine `bundle_nonce_is_stale`: errors keep the intent (false).
async fn bundle_nonce_is_stale(context: &BatchContext<'_>, intent: &PreparedBundleIntent) -> bool {
    let Some(relayer) = relayer_address_for_lane(context, intent.lane) else {
        return false;
    };
    match context
        .trusted
        .call(
            intent.chain_id,
            "eth_getTransactionCount",
            json!([relayer.to_string(), "latest"]),
        )
        .await
        .ok()
        .and_then(|value| value.as_str().and_then(parse_quantity))
        .and_then(|nonce| u64::try_from(nonce).ok())
    {
        Some(latest_nonce) => latest_nonce > intent.nonce,
        None => false,
    }
}

/// The docker engine indexes its pre-derived key table; here the pool address
/// is derived on demand from the operator secret (chain-agnostic, vault).
fn relayer_address_for_lane(context: &BatchContext<'_>, lane: u8) -> Option<Address> {
    let secret = context.config.operator_secret.as_deref()?;
    vault::derive_pool_relayer_address(secret, lane as usize)
        .ok()?
        .parse()
        .ok()
}

/// The docker store's `receipt_patch`, byte-identical field set: status,
/// transactionHash, admitted, the full receipt, blockHash/blockNumber lifted
/// from it, and the member's UserOperationEvent when one exists.
fn receipt_patch(
    status: &str,
    transaction_hash: &str,
    receipt: &Value,
    event: Option<&vela_relay_core::task::UserOperationEvent>,
) -> Value {
    let mut patch = json!({
        "status": status,
        "transactionHash": transaction_hash,
        "admitted": true,
        "receipt": receipt,
        "blockHash": receipt.get("blockHash"),
        "blockNumber": receipt.get("blockNumber"),
    });
    if let Some(event) = event {
        patch["event"] = serde_json::to_value(event).unwrap_or(Value::Null);
    }
    patch
}

fn tempo_chain_assets(config: &CfConfig) -> core_execution::ResolvedChainAssets {
    core_execution::ResolvedChainAssets {
        assets: vela_relay_core::settlement::ChainAssetConfig {
            native_decimals: vela_relay_core::tempo::PATH_USD_DECIMALS,
            settlement_markup_bps: config.settlement_markup_bps,
            stablecoins: std::collections::BTreeMap::from([(
                vela_relay_core::tempo::PATH_USD,
                vela_relay_core::settlement::StablecoinConfig {
                    symbol: vela_relay_core::tempo::PATH_USD_SYMBOL.into(),
                    decimals: vela_relay_core::tempo::PATH_USD_DECIMALS,
                },
            )]),
        },
        native_symbol: vela_relay_core::tempo::PATH_USD_SYMBOL.into(),
    }
}
