//! Simulation orchestration over the trusted transport: the docker shell's
//! three-tier order (`eth_simulateV1` → deployed Pimlico `eth_call` →
//! `debug_traceCall`) with every interpretation rule taken from
//! `vela_relay_core::simulation`. On a chain known or recently proven to lack
//! `eth_simulateV1` (core `simulate_v1_turn`) that tier is asked last, and only
//! for what the other two could not decide.
//!
//! Declared delta (contracts/platform-bindings.md): this shell does not
//! auto-deploy the Pimlico simulation pair — deployment is the docker
//! treasury's job, and the CREATE2 addresses are a pure function of the shared
//! treasury, so contracts deployed there are found `Ready` here. A `Missing`
//! pair falls through to `debug_traceCall`; the `Pending` verdict (and its
//! deployment-wait diagnostics) is never produced on this shell.

use alloy::primitives::{Address, B256, U256};
use serde_json::{Value, json};
use vela_relay_core::abi::{
    PackedOperation, handle_ops_calldata, pimlico_simulate_handle_op_calldata,
};
use vela_relay_core::simulation::{
    PimlicoSimulationContracts, SIMULATE_V1, SimulateV1Turn, SimulatedUserOperation,
    SimulationResult, SimulationVerdict, debug_trace_params, parse_simulation,
    parse_trace_simulation, parse_u256, pimlico_contracts_for_treasury,
    revert_reports_nonce_mismatch, simulate_params, simulate_v1_turn,
};

use super::trusted::{RpcBatchCall, RpcError, TrustedRpcClient};

enum PimlicoContractAvailability {
    Ready(PimlicoSimulationContracts),
    Missing,
    Unavailable,
}

/// Runs every candidate in isolation in one JSON-RPC HTTP batch (docker
/// `simulate_individually` minus the deployment hook).
pub async fn simulate_individually(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    relayer: Address,
    beneficiary: Address,
    operations: &[PackedOperation],
    hashes: &[B256],
) -> Vec<SimulationVerdict<SimulationResult>> {
    debug_assert_eq!(operations.len(), hashes.len());
    let every = (0..operations.len()).collect::<Vec<_>>();
    let turn = simulate_v1_turn(chain_id, rpc.lacks_method(chain_id, SIMULATE_V1));
    let mut verdicts = match turn {
        SimulateV1Turn::First => {
            simulate_v1_individually(
                rpc,
                chain_id,
                entry_point,
                relayer,
                beneficiary,
                operations,
                hashes,
                &every,
            )
            .await
        }
        SimulateV1Turn::Last => every
            .iter()
            .map(|_| SimulationVerdict::Transient("individual simulation method unavailable"))
            .collect(),
    };
    simulate_individually_without_v1(
        rpc,
        chain_id,
        entry_point,
        relayer,
        beneficiary,
        operations,
        hashes,
        &mut verdicts,
    )
    .await;
    if turn == SimulateV1Turn::Last {
        let undecided = transient_indexes(&verdicts);
        if !undecided.is_empty() {
            let last = simulate_v1_individually(
                rpc,
                chain_id,
                entry_point,
                relayer,
                beneficiary,
                operations,
                hashes,
                &undecided,
            )
            .await;
            for (index, verdict) in undecided.into_iter().zip(last) {
                if !matches!(verdict, SimulationVerdict::Transient(_)) {
                    verdicts[index] = verdict;
                }
            }
        }
    }
    verdicts
}

fn transient_indexes(verdicts: &[SimulationVerdict<SimulationResult>]) -> Vec<usize> {
    verdicts
        .iter()
        .enumerate()
        .filter_map(|(index, verdict)| {
            matches!(verdict, SimulationVerdict::Transient(_)).then_some(index)
        })
        .collect()
}

/// The `eth_simulateV1` tier for the candidates at `indexes`, one verdict per
/// index; what the walk showed about the method goes into the chain's memory.
#[allow(clippy::too_many_arguments)]
async fn simulate_v1_individually(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    relayer: Address,
    beneficiary: Address,
    operations: &[PackedOperation],
    hashes: &[B256],
    indexes: &[usize],
) -> Vec<SimulationVerdict<SimulationResult>> {
    let calls = indexes
        .iter()
        .map(|index| RpcBatchCall {
            method: SIMULATE_V1,
            params: simulate_params(
                relayer,
                entry_point,
                handle_ops_calldata(
                    std::slice::from_ref(&operations[*index].packed),
                    beneficiary,
                ),
            ),
        })
        .collect::<Vec<_>>();
    let (responses, answers) = rpc.batch_walk(chain_id, &calls).await;
    rpc.note_method_walk(chain_id, SIMULATE_V1, &answers);
    match responses {
        Ok(responses) => responses
            .into_iter()
            .zip(indexes)
            .map(|(response, index)| match response {
                Ok(value) => parse_simulation(value, entry_point, &[hashes[*index]]),
                Err(RpcError::Reverted { .. }) => {
                    // `eth_simulateV1` reports a real call verdict inside `result`. A top-level
                    // error means the RPC could not perform the method, even if its message says
                    // revert.
                    SimulationVerdict::Transient("individual simulation method unavailable")
                }
                Err(_) => SimulationVerdict::Transient("individual simulation RPC unavailable"),
            })
            .collect(),
        Err(_) => indexes
            .iter()
            .map(|_| SimulationVerdict::Transient("individual simulation RPC unavailable"))
            .collect(),
    }
}

/// The Pimlico `eth_call` and `debug_traceCall` tiers for every candidate
/// still undecided (`Transient`).
#[allow(clippy::too_many_arguments)]
async fn simulate_individually_without_v1(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    relayer: Address,
    beneficiary: Address,
    operations: &[PackedOperation],
    hashes: &[B256],
    verdicts: &mut [SimulationVerdict<SimulationResult>],
) {
    let fallback_indexes = transient_indexes(verdicts);
    if fallback_indexes.is_empty() {
        return;
    }
    let pimlico_contracts = match pimlico_contracts(rpc, chain_id, beneficiary).await {
        PimlicoContractAvailability::Ready(contracts) => Some(contracts),
        PimlicoContractAvailability::Missing | PimlicoContractAvailability::Unavailable => None,
    };
    let mut trace_indexes = Vec::new();
    for index in fallback_indexes {
        let verdict = simulate_with_pimlico(
            rpc,
            chain_id,
            entry_point,
            &operations[index],
            hashes[index],
            pimlico_contracts,
        )
        .await;
        if matches!(verdict, SimulationVerdict::Transient(_)) {
            trace_indexes.push(index);
        } else {
            verdicts[index] = verdict;
        }
    }
    if trace_indexes.is_empty() {
        return;
    }
    let trace_calls = trace_indexes
        .iter()
        .map(|index| RpcBatchCall {
            method: "debug_traceCall",
            params: debug_trace_params(
                relayer,
                entry_point,
                handle_ops_calldata(&[operations[*index].packed.clone()], beneficiary),
            ),
        })
        .collect::<Vec<_>>();
    let trace_responses = rpc.batch(chain_id, &trace_calls).await;
    for (position, index) in trace_indexes.into_iter().enumerate() {
        verdicts[index] = match trace_responses
            .as_ref()
            .ok()
            .and_then(|responses| responses.get(position))
        {
            Some(Ok(value)) => parse_trace_simulation(value.clone(), entry_point, &[hashes[index]]),
            _ => SimulationVerdict::Transient(
                "no trusted executor RPC supports eth_simulateV1, deployed Pimlico eth_call, or debug_traceCall",
            ),
        };
    }
}

/// The final full-bundle proof (docker `simulate_bundle` minus the deployment
/// hook).
pub async fn simulate_bundle(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    relayer: Address,
    beneficiary: Address,
    operations: &[PackedOperation],
    hashes: &[B256],
) -> SimulationVerdict<SimulationResult> {
    let calldata = handle_ops_calldata(
        &operations
            .iter()
            .map(|operation| operation.packed.clone())
            .collect::<Vec<_>>(),
        beneficiary,
    );
    let turn = simulate_v1_turn(chain_id, rpc.lacks_method(chain_id, SIMULATE_V1));
    if turn == SimulateV1Turn::First
        && let Some(verdict) = simulate_v1_bundle(
            rpc,
            chain_id,
            entry_point,
            relayer,
            calldata.clone(),
            hashes,
        )
        .await
    {
        return verdict;
    }
    let verdict = simulate_bundle_without_v1(
        rpc,
        chain_id,
        entry_point,
        relayer,
        beneficiary,
        calldata.clone(),
        hashes,
    )
    .await;
    if turn == SimulateV1Turn::Last
        && matches!(verdict, SimulationVerdict::Transient(_))
        && let Some(last) =
            simulate_v1_bundle(rpc, chain_id, entry_point, relayer, calldata, hashes).await
        && !matches!(last, SimulationVerdict::Transient(_))
    {
        return last;
    }
    verdict
}

/// The `eth_simulateV1` tier for the whole bundle: `None` when no endpoint
/// performed it. What the walk showed goes into the chain's memory.
async fn simulate_v1_bundle(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    relayer: Address,
    calldata: alloy::primitives::Bytes,
    hashes: &[B256],
) -> Option<SimulationVerdict<SimulationResult>> {
    let (response, answers) = rpc
        .call_walk(
            chain_id,
            SIMULATE_V1,
            simulate_params(relayer, entry_point, calldata),
        )
        .await;
    rpc.note_method_walk(chain_id, SIMULATE_V1, &answers);
    response
        .ok()
        .map(|value| parse_simulation(value, entry_point, hashes))
}

/// The Pimlico `eth_call` (as `eth_estimateGas` of the bundle) and
/// `debug_traceCall` tiers for the whole bundle.
async fn simulate_bundle_without_v1(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    relayer: Address,
    beneficiary: Address,
    calldata: alloy::primitives::Bytes,
    hashes: &[B256],
) -> SimulationVerdict<SimulationResult> {
    let contracts = match pimlico_contracts(rpc, chain_id, beneficiary).await {
        PimlicoContractAvailability::Ready(contracts) => Some(contracts),
        PimlicoContractAvailability::Missing | PimlicoContractAvailability::Unavailable => None,
    };
    if let Some(contracts) = contracts {
        let verdict = simulate_bundle_with_eth_call(
            rpc,
            chain_id,
            entry_point,
            relayer,
            calldata.clone(),
            hashes,
            contracts,
        )
        .await;
        if !matches!(verdict, SimulationVerdict::Transient(_)) {
            return verdict;
        }
    }
    match rpc
        .call(
            chain_id,
            "debug_traceCall",
            debug_trace_params(relayer, entry_point, calldata),
        )
        .await
    {
        Ok(value) => parse_trace_simulation(value, entry_point, hashes),
        Err(_) => SimulationVerdict::Transient(
            "no trusted executor RPC supports eth_simulateV1, deployed Pimlico eth_call, or debug_traceCall",
        ),
    }
}

async fn pimlico_contracts(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    treasury: Address,
) -> PimlicoContractAvailability {
    let contracts = pimlico_contracts_for_treasury(treasury);
    let calls = [
        RpcBatchCall {
            method: "eth_getCode",
            params: json!([contracts.pimlico.to_string(), "latest"]),
        },
        RpcBatchCall {
            method: "eth_getCode",
            params: json!([contracts.entry_point_v07.to_string(), "latest"]),
        },
    ];
    let Ok(responses) = rpc.batch(chain_id, &calls).await else {
        return PimlicoContractAvailability::Unavailable;
    };
    let Some(all_deployed) = responses
        .iter()
        .map(|response| response.as_ref().ok().and_then(Value::as_str))
        .collect::<Option<Vec<_>>>()
        .map(|codes| codes.into_iter().all(|code| code != "0x"))
    else {
        return PimlicoContractAvailability::Unavailable;
    };
    if all_deployed {
        PimlicoContractAvailability::Ready(contracts)
    } else {
        PimlicoContractAvailability::Missing
    }
}

async fn simulate_with_pimlico(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    operation: &PackedOperation,
    hash: B256,
    contracts: Option<PimlicoSimulationContracts>,
) -> SimulationVerdict<SimulationResult> {
    let Some(contracts) = contracts else {
        return SimulationVerdict::Transient(
            "no trusted executor RPC supports eth_simulateV1, debug_traceCall, or deployed Pimlico simulations",
        );
    };
    let data =
        pimlico_simulate_handle_op_calldata(contracts.entry_point_v07, entry_point, operation);
    match rpc
        .simulate(
            chain_id,
            "eth_call",
            json!([{
                "to": contracts.pimlico.to_string(),
                "data": format!("0x{}", hex::encode(data)),
            }, "latest"]),
        )
        .await
    {
        // `simulateHandleOp` reverts for an invalid EntryPoint validation or account call. It has
        // no logs by design, but individual verdicts are used only to decide bundle membership.
        Ok(_) => SimulationVerdict::Success(SimulationResult {
            gas_used: U256::ZERO,
            events: vec![SimulatedUserOperation {
                user_operation_hash: hash,
                success: true,
                actual_gas_used: U256::ZERO,
            }],
            logs: Vec::new(),
        }),
        Err(RpcError::Reverted { message, data }) => {
            if revert_reports_nonce_mismatch(&message, data.as_deref()) {
                SimulationVerdict::NonceMismatch
            } else {
                SimulationVerdict::Rejected(
                    "Pimlico eth_call simulation reverted during EntryPoint validation or execution",
                )
            }
        }
        Err(_) => SimulationVerdict::Transient(
            "Pimlico eth_call simulation is unavailable on trusted executor RPCs",
        ),
    }
}

async fn simulate_bundle_with_eth_call(
    rpc: &TrustedRpcClient<'_>,
    chain_id: u64,
    entry_point: Address,
    relayer: Address,
    calldata: alloy::primitives::Bytes,
    hashes: &[B256],
    _contracts: PimlicoSimulationContracts,
) -> SimulationVerdict<SimulationResult> {
    // The individual Pimlico calls have already proven validation and account execution. The
    // standard `eth_estimateGas` here executes the exact final `handleOps` bundle, catching
    // inter-operation state conflicts without requiring a debug namespace.
    match rpc
        .simulate(
            chain_id,
            "eth_estimateGas",
            json!([{
                "from": relayer.to_string(),
                "to": entry_point.to_string(),
                "data": format!("0x{}", hex::encode(calldata)),
            }, "latest"]),
        )
        .await
    {
        Ok(value) => match value.as_str().and_then(parse_u256) {
            Some(gas_used) => SimulationVerdict::Success(SimulationResult {
                gas_used,
                // `eth_estimateGas` does not return logs or per-operation gas. Preserve each
                // expected hash so allocation charges the full outer estimate evenly rather than
                // crediting an unverified operation.
                events: hashes
                    .iter()
                    .copied()
                    .map(|user_operation_hash| SimulatedUserOperation {
                        user_operation_hash,
                        success: true,
                        actual_gas_used: U256::ZERO,
                    })
                    .collect(),
                logs: Vec::new(),
            }),
            None => SimulationVerdict::Transient(
                "Pimlico eth_call fallback returned an invalid eth_estimateGas quantity",
            ),
        },
        Err(RpcError::Reverted { .. }) => SimulationVerdict::Rejected(
            "final handleOps bundle reverted during eth_estimateGas fallback",
        ),
        Err(_) => SimulationVerdict::Transient(
            "Pimlico eth_call fallback could not estimate the final handleOps bundle",
        ),
    }
}
