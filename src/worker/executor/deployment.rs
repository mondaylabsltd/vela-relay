use std::{str::FromStr, sync::Arc, time::Duration};

use alloy::primitives::{Address, Bytes, U256, keccak256};
use serde_json::{Value, json};

use crate::{
    app::{PreparedSimulationDeploymentIntent, UserOperationStatusStore},
    utils::tempo,
};

use super::{
    receipt::receipt_succeeded,
    rpc::{BroadcastOutcome, RpcBatchCall, TrustedRpcClient},
    simulation::{DETERMINISTIC_DEPLOYER, PimlicoSimulationContracts},
    transaction::{TransactionPlan, sign_eip1559},
};

const PIMLICO_ARTIFACT_NAME: &str = "PimlicoSimulations";
const ENTRY_POINT_V07_ARTIFACT_NAME: &str = "EntryPointSimulations07";
const PIMLICO_ARTIFACT_JSON: &str = include_str!("../../../contracts/alto/PimlicoSimulations.json");
const ENTRY_POINT_V07_ARTIFACT_JSON: &str =
    include_str!("../../../contracts/alto/EntryPointSimulations07.json");
const DEPLOYMENT_GAS_BUFFER_BPS: u64 = 12_000;

#[derive(Clone)]
pub(super) struct SimulationContractDeployer {
    rpc: TrustedRpcClient,
    store: UserOperationStatusStore,
    treasury_key: Arc<k256::SecretKey>,
    treasury_address: Address,
    lease_ttl: Duration,
    treasury_floor_wei: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SimulationDeploymentState {
    Ready,
    Pending,
    Unavailable,
}

struct DeploymentTarget {
    name: &'static str,
    address: Address,
    data: Bytes,
}

impl SimulationContractDeployer {
    pub(super) fn new(
        rpc: TrustedRpcClient,
        store: UserOperationStatusStore,
        treasury_key: Arc<k256::SecretKey>,
        treasury_address: Address,
        lease_ttl: Duration,
        treasury_floor_wei: u128,
    ) -> Self {
        Self {
            rpc,
            store,
            treasury_key,
            treasury_address,
            lease_ttl,
            treasury_floor_wei,
        }
    }

    /// Lazily deploys the pair only after eth_simulateV1 was unavailable and code was observed
    /// missing. Deployments use the same per-chain treasury lease as float funding, preserving
    /// transaction nonce ordering across every executor lane and replica.
    pub(super) async fn ensure(
        &self,
        chain_id: u64,
        contracts: PimlicoSimulationContracts,
    ) -> SimulationDeploymentState {
        if tempo::is_tempo_chain(chain_id) {
            // Tempo has no native coin and these artifacts are normal EIP-1559 deployments.
            // Keep its native simulation path explicit instead of submitting an invalid envelope.
            return SimulationDeploymentState::Unavailable;
        }
        let scope = format!("treasury:{chain_id}");
        let token = format!(
            "simulation-deployment:{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let Ok(acquired) = self
            .store
            .acquire_lease(&scope, &token, self.lease_ttl)
            .await
        else {
            tracing::warn!(
                chain_id,
                "could not acquire Redis lease for simulation deployment"
            );
            return SimulationDeploymentState::Pending;
        };
        if !acquired {
            return SimulationDeploymentState::Pending;
        }
        let state = self
            .ensure_locked(chain_id, contracts, &scope, &token)
            .await;
        if let Err(error) = self.store.release_lease(&scope, &token).await {
            tracing::warn!(chain_id, %error, "could not release simulation deployment lease");
        }
        state
    }

    async fn ensure_locked(
        &self,
        chain_id: u64,
        contracts: PimlicoSimulationContracts,
        scope: &str,
        token: &str,
    ) -> SimulationDeploymentState {
        match self
            .store
            .get_prepared_simulation_deployment_intent(chain_id)
            .await
        {
            Ok(Some(intent)) => match self.resume(chain_id, &intent).await {
                SimulationDeploymentState::Ready => {}
                state => return state,
            },
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(chain_id, %error, "could not load simulation deployment outbox");
                return SimulationDeploymentState::Unavailable;
            }
        }

        let targets = match deployment_targets(self.treasury_address, contracts) {
            Ok(targets) => targets,
            Err(error) => {
                tracing::error!(chain_id, %error, "vendored simulation artifact is invalid");
                return SimulationDeploymentState::Unavailable;
            }
        };
        let calls = [
            RpcBatchCall {
                method: "eth_getCode",
                params: json!([DETERMINISTIC_DEPLOYER.to_string(), "latest"]),
            },
            RpcBatchCall {
                method: "eth_getCode",
                params: json!([targets[0].address.to_string(), "latest"]),
            },
            RpcBatchCall {
                method: "eth_getCode",
                params: json!([targets[1].address.to_string(), "latest"]),
            },
        ];
        let Ok(responses) = self.rpc.batch(chain_id, &calls).await else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(codes) = responses
            .iter()
            .map(|response| response.as_ref().ok().and_then(Value::as_str))
            .collect::<Option<Vec<_>>>()
        else {
            return SimulationDeploymentState::Unavailable;
        };
        if codes[0] == "0x" {
            tracing::warn!(
                chain_id,
                deployer = %DETERMINISTIC_DEPLOYER,
                "canonical CREATE2 deployer is absent; cannot auto-deploy simulation contracts"
            );
            return SimulationDeploymentState::Unavailable;
        }
        let Some(target) = targets
            .into_iter()
            .zip(codes.into_iter().skip(1))
            .find_map(|(target, code)| (code == "0x").then_some(target))
        else {
            return SimulationDeploymentState::Ready;
        };

        if !self.renew(scope, token, chain_id).await {
            return SimulationDeploymentState::Pending;
        }
        self.prepare_and_broadcast(chain_id, target, scope, token)
            .await
    }

    async fn prepare_and_broadcast(
        &self,
        chain_id: u64,
        target: DeploymentTarget,
        scope: &str,
        token: &str,
    ) -> SimulationDeploymentState {
        let estimate_call = json!({
            "from": self.treasury_address.to_string(),
            "to": DETERMINISTIC_DEPLOYER.to_string(),
            "data": format!("0x{}", hex::encode(&target.data)),
            "value": "0x0",
        });
        let calls = [
            RpcBatchCall {
                method: "eth_estimateGas",
                params: json!([estimate_call]),
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
                params: json!([self.treasury_address.to_string(), "pending"]),
            },
            RpcBatchCall {
                method: "eth_getBalance",
                params: json!([self.treasury_address.to_string(), "pending"]),
            },
        ];
        let Ok(responses) = self.rpc.batch(chain_id, &calls).await else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(estimated_gas) = response_quantity(&responses, 0) else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(block) = response_value(&responses, 1) else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(base_fee) = block
            .get("baseFeePerGas")
            .and_then(Value::as_str)
            .and_then(parse_quantity)
        else {
            return SimulationDeploymentState::Unavailable;
        };
        // The same market-tip rule the quote and the bundle executor share.
        let max_priority_fee_per_gas = response_quantity(&responses, 2);
        let legacy_gas_price = match max_priority_fee_per_gas {
            Some(_) => None,
            None => {
                let Ok(gas_price) = self.rpc.call(chain_id, "eth_gasPrice", json!([])).await else {
                    return SimulationDeploymentState::Unavailable;
                };
                let Some(gas_price) = gas_price.as_str().and_then(parse_quantity) else {
                    return SimulationDeploymentState::Unavailable;
                };
                Some(gas_price)
            }
        };
        let Some(tip) = vela_relay_core::gas_math::market_tip(
            max_priority_fee_per_gas,
            legacy_gas_price,
            base_fee,
        ) else {
            return SimulationDeploymentState::Unavailable;
        };
        let (Ok(estimated_gas), Ok(base_fee), Ok(tip), Some(nonce), Some(balance)) = (
            u64::try_from(estimated_gas),
            u128::try_from(base_fee),
            u128::try_from(tip),
            response_quantity(&responses, 3).and_then(|value| u64::try_from(value).ok()),
            response_quantity(&responses, 4),
        ) else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(gas_limit) = estimated_gas
            .checked_mul(DEPLOYMENT_GAS_BUFFER_BPS)
            .map(|value| value / 10_000)
        else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(max_fee_per_gas) = base_fee.checked_mul(2).and_then(|fee| fee.checked_add(tip))
        else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(max_cost) = U256::from(gas_limit).checked_mul(U256::from(max_fee_per_gas)) else {
            return SimulationDeploymentState::Unavailable;
        };
        let Some(required_treasury) = max_cost.checked_add(U256::from(self.treasury_floor_wei))
        else {
            return SimulationDeploymentState::Unavailable;
        };
        if balance < required_treasury {
            tracing::warn!(
                chain_id,
                contract = target.name,
                treasury_balance = %balance,
                required_treasury = %required_treasury,
                "treasury balance is insufficient to deploy simulation contract"
            );
            return SimulationDeploymentState::Unavailable;
        }
        if !self.renew(scope, token, chain_id).await {
            return SimulationDeploymentState::Pending;
        }
        let Ok(signed) = sign_eip1559(
            &self.treasury_key,
            TransactionPlan {
                chain_id,
                nonce,
                gas_limit,
                max_fee_per_gas,
                max_priority_fee_per_gas: tip,
                to: DETERMINISTIC_DEPLOYER,
                value: U256::ZERO,
                input: target.data,
            },
        ) else {
            return SimulationDeploymentState::Unavailable;
        };
        let intent = PreparedSimulationDeploymentIntent {
            chain_id,
            contract: target.name.to_owned(),
            contract_address: target.address.to_string(),
            raw_transaction: format!("0x{}", hex::encode(&signed.raw_transaction)),
            transaction_hash: signed.transaction_hash,
            nonce: signed.nonce,
        };
        match self
            .store
            .save_prepared_simulation_deployment_intent(&intent)
            .await
        {
            Ok(true) => {}
            Ok(false) => return SimulationDeploymentState::Pending,
            Err(error) => {
                tracing::warn!(chain_id, %error, "could not save simulation deployment outbox");
                return SimulationDeploymentState::Unavailable;
            }
        }
        self.broadcast(chain_id, &intent).await;
        tracing::info!(
            chain_id,
            contract = %intent.contract,
            contract_address = %intent.contract_address,
            transaction_hash = %intent.transaction_hash,
            "submitted automatic simulation-contract deployment"
        );
        SimulationDeploymentState::Pending
    }

    async fn resume(
        &self,
        chain_id: u64,
        intent: &PreparedSimulationDeploymentIntent,
    ) -> SimulationDeploymentState {
        self.broadcast(chain_id, intent).await;
        let Ok(receipt) = self
            .rpc
            .call(
                chain_id,
                "eth_getTransactionReceipt",
                json!([intent.transaction_hash]),
            )
            .await
        else {
            return SimulationDeploymentState::Pending;
        };
        if receipt.is_null() {
            return SimulationDeploymentState::Pending;
        }
        let Some(success) = receipt_succeeded(&receipt) else {
            tracing::warn!(
                chain_id,
                contract = %intent.contract,
                transaction_hash = %intent.transaction_hash,
                "simulation deployment receipt has an invalid status"
            );
            return SimulationDeploymentState::Pending;
        };
        if !success {
            if let Err(error) = self
                .store
                .clear_prepared_simulation_deployment_intent(chain_id, &intent.transaction_hash)
                .await
            {
                tracing::warn!(chain_id, %error, "could not clear reverted simulation deployment outbox");
            }
            tracing::error!(
                chain_id,
                contract = %intent.contract,
                contract_address = %intent.contract_address,
                transaction_hash = %intent.transaction_hash,
                "automatic simulation-contract deployment reverted"
            );
            return SimulationDeploymentState::Unavailable;
        }
        let Ok(address) = Address::from_str(&intent.contract_address) else {
            return SimulationDeploymentState::Unavailable;
        };
        let Ok(code) = self
            .rpc
            .call(
                chain_id,
                "eth_getCode",
                json!([address.to_string(), "latest"]),
            )
            .await
        else {
            return SimulationDeploymentState::Pending;
        };
        if code.as_str().is_none_or(|code| code == "0x") {
            tracing::error!(
                chain_id,
                contract = %intent.contract,
                contract_address = %intent.contract_address,
                transaction_hash = %intent.transaction_hash,
                "simulation deployment succeeded but contract code is absent"
            );
            return SimulationDeploymentState::Unavailable;
        }
        if let Err(error) = self
            .store
            .clear_prepared_simulation_deployment_intent(chain_id, &intent.transaction_hash)
            .await
        {
            tracing::warn!(chain_id, %error, "could not clear confirmed simulation deployment outbox");
            return SimulationDeploymentState::Pending;
        }
        tracing::info!(
            chain_id,
            contract = %intent.contract,
            contract_address = %intent.contract_address,
            transaction_hash = %intent.transaction_hash,
            "automatic simulation-contract deployment included"
        );
        SimulationDeploymentState::Ready
    }

    async fn broadcast(&self, chain_id: u64, intent: &PreparedSimulationDeploymentIntent) {
        let Some(raw) = parse_raw_transaction(&intent.raw_transaction, &intent.transaction_hash)
        else {
            tracing::error!(
                chain_id,
                contract = %intent.contract,
                transaction_hash = %intent.transaction_hash,
                "stored simulation deployment transaction is invalid"
            );
            return;
        };
        match self.rpc.broadcast_raw_transaction(chain_id, &raw).await {
            Ok(BroadcastOutcome::Accepted(hash))
                if hash.eq_ignore_ascii_case(&intent.transaction_hash) => {}
            Ok(BroadcastOutcome::Accepted(hash)) => tracing::error!(
                chain_id,
                expected_transaction_hash = %intent.transaction_hash,
                returned_transaction_hash = %hash,
                "RPC returned a different simulation deployment transaction hash"
            ),
            Ok(BroadcastOutcome::Ambiguous(reason)) => tracing::debug!(
                chain_id,
                transaction_hash = %intent.transaction_hash,
                reason,
                "simulation deployment broadcast is ambiguous; retaining durable outbox"
            ),
            Ok(BroadcastOutcome::Rejected(reason)) => tracing::warn!(
                chain_id,
                transaction_hash = %intent.transaction_hash,
                reason,
                "simulation deployment broadcast was rejected; retaining durable outbox"
            ),
            Err(error) => tracing::warn!(
                chain_id,
                transaction_hash = %intent.transaction_hash,
                %error,
                "simulation deployment broadcast RPC failed; retaining durable outbox"
            ),
        }
    }

    async fn renew(&self, scope: &str, token: &str, chain_id: u64) -> bool {
        match self.store.renew_lease(scope, token, self.lease_ttl).await {
            Ok(true) => true,
            Ok(false) => {
                tracing::warn!(chain_id, "simulation deployment lease was lost");
                false
            }
            Err(error) => {
                tracing::warn!(chain_id, %error, "could not renew simulation deployment lease");
                false
            }
        }
    }
}

fn deployment_targets(
    treasury: Address,
    contracts: PimlicoSimulationContracts,
) -> Result<[DeploymentTarget; 2], &'static str> {
    let salt = keccak256(treasury.as_slice());
    let pimlico = artifact_init_code(PIMLICO_ARTIFACT_JSON)?;
    let entry_point_v07 = artifact_init_code(ENTRY_POINT_V07_ARTIFACT_JSON)?;
    Ok([
        DeploymentTarget {
            name: PIMLICO_ARTIFACT_NAME,
            address: contracts.pimlico,
            data: [salt.as_slice(), pimlico.as_slice()].concat().into(),
        },
        DeploymentTarget {
            name: ENTRY_POINT_V07_ARTIFACT_NAME,
            address: contracts.entry_point_v07,
            data: [salt.as_slice(), entry_point_v07.as_slice()]
                .concat()
                .into(),
        },
    ])
}

fn artifact_init_code(artifact: &str) -> Result<Vec<u8>, &'static str> {
    let artifact =
        serde_json::from_str::<Value>(artifact).map_err(|_| "artifact JSON is invalid")?;
    let bytecode = artifact
        .pointer("/bytecode/object")
        .and_then(Value::as_str)
        .ok_or("artifact has no bytecode.object")?;
    hex::decode(bytecode.strip_prefix("0x").unwrap_or(bytecode))
        .map_err(|_| "artifact bytecode is invalid hex")
}

fn response_value<'a>(
    responses: &'a [Result<Value, super::rpc::RpcError>],
    index: usize,
) -> Option<&'a Value> {
    responses.get(index)?.as_ref().ok()
}

fn response_quantity(
    responses: &[Result<Value, super::rpc::RpcError>],
    index: usize,
) -> Option<U256> {
    response_value(responses, index)?
        .as_str()
        .and_then(parse_quantity)
}

fn parse_quantity(value: &str) -> Option<U256> {
    let value = value.strip_prefix("0x")?;
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    U256::from_str_radix(value, 16).ok()
}

fn parse_raw_transaction(raw_transaction: &str, expected_hash: &str) -> Option<Vec<u8>> {
    let raw = hex::decode(raw_transaction.strip_prefix("0x")?).ok()?;
    (!raw.is_empty()
        && keccak256(&raw)
            .to_string()
            .eq_ignore_ascii_case(expected_hash))
    .then_some(raw)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::keccak256;

    use super::{ENTRY_POINT_V07_ARTIFACT_JSON, PIMLICO_ARTIFACT_JSON, artifact_init_code};
    use vela_relay_core::simulation::{
        ENTRY_POINT_SIMULATIONS_V07_INIT_CODE_HASH, PIMLICO_SIMULATIONS_INIT_CODE_HASH,
    };

    #[test]
    fn vendored_artifacts_match_the_deterministic_addresses() {
        assert_eq!(
            keccak256(artifact_init_code(PIMLICO_ARTIFACT_JSON).unwrap()),
            PIMLICO_SIMULATIONS_INIT_CODE_HASH
        );
        assert_eq!(
            keccak256(artifact_init_code(ENTRY_POINT_V07_ARTIFACT_JSON).unwrap()),
            ENTRY_POINT_SIMULATIONS_V07_INIT_CODE_HASH
        );
    }
}
