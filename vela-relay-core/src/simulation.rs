//! Simulation interpretation: the rules that turn raw `eth_simulateV1`,
//! Pimlico `eth_call`, and `debug_traceCall` responses into bundle-membership
//! verdicts. Both shells run the same three-tier orchestration over their own
//! transports; every parse, nonce-mismatch classification, verdict reason
//! string, and CREATE2 address derivation lives here so the shells cannot
//! drift (Constitution I).

use std::str::FromStr;

use alloy::primitives::{Address, B256, Bytes, U256, address, b256, keccak256};
use serde_json::{Value, json};

/// The first simulation tier's method.
pub const SIMULATE_V1: &str = "eth_simulateV1";

/// Chains whose node software has no `eth_simulateV1`, known before any walk:
/// Avalanche C-Chain and its Fuji testnet run AvalancheGo's C-Chain client,
/// which does not implement the method. On 2026-10-08 every one of the 28
/// endpoints the directory lists for 43114 answered -32601 or not at all, and
/// asking each of them in turn, twice a pass, was most of the minute an AVAX
/// send took (vela-wallet #464).
///
/// Listing a chain here only moves the method to the end of the walk: if the
/// chain gains it, a simulation that the other tiers could not run still asks.
pub fn chain_lacks_simulate_v1(chain_id: u64) -> bool {
    matches!(chain_id, 43_114 | 43_113)
}

/// When a simulation asks for `eth_simulateV1` relative to the other tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulateV1Turn {
    /// Before the Pimlico `eth_call` and `debug_traceCall` tiers, as always.
    First,
    /// After them, and only for what they could not decide. A chain known or
    /// recently proven to lack the method skips the walk that would find no
    /// one serving it, and never loses a verdict to a stale belief.
    Last,
}

/// `remembered_missing`: whether a walk within
/// [`crate::rpc_walk::MISSING_METHOD_MEMORY_MS`] proved the chain's endpoints
/// lack the method (the shell's [`crate::rpc_walk::MissingMethods`]).
pub fn simulate_v1_turn(chain_id: u64, remembered_missing: bool) -> SimulateV1Turn {
    if remembered_missing || chain_lacks_simulate_v1(chain_id) {
        SimulateV1Turn::Last
    } else {
        SimulateV1Turn::First
    }
}

pub const DETERMINISTIC_DEPLOYER: Address = address!("4e59b44847b379578588920ca78fbf26c0b4956c");
pub const PIMLICO_SIMULATIONS_INIT_CODE_HASH: B256 =
    b256!("6d2eb1ee903947960a7faf13c49dc4b9deb468b3c7a6d19863c4d9b2bffd78d1");
pub const ENTRY_POINT_SIMULATIONS_V07_INIT_CODE_HASH: B256 =
    b256!("5ec5a546872ae8196c7627fe2a5c89e0a23614070cbd474dbce5e97b82d40c94");

#[derive(Clone, Copy, Debug)]
pub struct PimlicoSimulationContracts {
    pub pimlico: Address,
    pub entry_point_v07: Address,
}

#[derive(Clone, Debug)]
pub struct SimulatedLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

#[derive(Clone, Debug)]
pub struct SimulatedUserOperation {
    pub user_operation_hash: B256,
    pub success: bool,
    pub actual_gas_used: U256,
}

#[derive(Clone, Debug)]
pub struct SimulationResult {
    pub gas_used: U256,
    pub events: Vec<SimulatedUserOperation>,
    pub logs: Vec<SimulatedLog>,
    /// Whether this is a full execution of the `handleOps` call — its status,
    /// its gas and every log (`eth_simulateV1`, `debug_traceCall`) — rather
    /// than a stand-in that proves less: the Pimlico `eth_call` (gas 0, no
    /// logs) or the bundle's `eth_estimateGas` (no logs).
    pub full_execution: bool,
}

#[derive(Debug)]
pub enum SimulationVerdict<T> {
    Success(T),
    NonceMismatch,
    Rejected(&'static str),
    Pending(&'static str),
    Transient(&'static str),
}

/// The Alto simulation pair a treasury deploys through the canonical CREATE2
/// deployer: the addresses are a pure function of the treasury, so every shell
/// sharing an operator secret probes (and reuses) the same deployments.
pub fn pimlico_contracts_for_treasury(treasury: Address) -> PimlicoSimulationContracts {
    let salt = keccak256(treasury.as_slice());
    PimlicoSimulationContracts {
        pimlico: create2_address(
            DETERMINISTIC_DEPLOYER,
            salt,
            PIMLICO_SIMULATIONS_INIT_CODE_HASH,
        ),
        entry_point_v07: create2_address(
            DETERMINISTIC_DEPLOYER,
            salt,
            ENTRY_POINT_SIMULATIONS_V07_INIT_CODE_HASH,
        ),
    }
}

pub fn create2_address(deployer: Address, salt: B256, init_code_hash: B256) -> Address {
    let mut preimage = Vec::with_capacity(85);
    preimage.push(0xff);
    preimage.extend_from_slice(deployer.as_slice());
    preimage.extend_from_slice(salt.as_slice());
    preimage.extend_from_slice(init_code_hash.as_slice());
    let hash = keccak256(preimage);
    Address::from_slice(&hash.as_slice()[12..])
}

pub fn revert_reports_nonce_mismatch(message: &str, data: Option<&str>) -> bool {
    let message = message.to_ascii_lowercase();
    if message.contains("aa25") || message.contains("invalid account nonce") {
        return true;
    }
    let Some(data) = data.and_then(|data| data.strip_prefix("0x")) else {
        return false;
    };
    let Ok(bytes) = hex::decode(data) else {
        return false;
    };
    bytes
        .windows("aa25".len())
        .any(|window| window.eq_ignore_ascii_case(b"aa25"))
        || bytes
            .windows("invalid account nonce".len())
            .any(|window| window.eq_ignore_ascii_case(b"invalid account nonce"))
}

pub fn simulate_params(from: Address, entry_point: Address, calldata: Bytes) -> Value {
    json!([
        {
            "blockStateCalls": [{
                "calls": [{
                    "from": from.to_string(),
                    "to": entry_point.to_string(),
                    "data": format!("0x{}", hex::encode(calldata)),
                }]
            }],
            "validation": false,
            "traceTransfers": false,
        },
        "latest"
    ])
}

pub fn debug_trace_params(from: Address, entry_point: Address, calldata: Bytes) -> Value {
    json!([
        {
            "from": from.to_string(),
            "to": entry_point.to_string(),
            "data": format!("0x{}", hex::encode(calldata)),
        },
        "latest",
        {
            "tracer": "callTracer",
            "tracerConfig": { "withLog": true },
        }
    ])
}

pub fn parse_simulation(
    value: Value,
    entry_point: Address,
    expected_hashes: &[B256],
) -> SimulationVerdict<SimulationResult> {
    let Some(call) = value
        .get(0)
        .and_then(|block| block.get("calls"))
        .and_then(Value::as_array)
        .and_then(|calls| calls.first())
    else {
        return SimulationVerdict::Transient("simulation response has no call result");
    };

    let status = call
        .get("status")
        .and_then(Value::as_str)
        .and_then(parse_u256);
    match status {
        Some(status) if status == U256::from(1u8) => {}
        Some(status) if status.is_zero() && call_reports_nonce_mismatch(call) => {
            return SimulationVerdict::NonceMismatch;
        }
        Some(status) if status.is_zero() => {
            return SimulationVerdict::Rejected("handleOps reverted during simulation");
        }
        _ => return SimulationVerdict::Transient("simulation returned an invalid call status"),
    }

    let Some(gas_used) = call
        .get("gasUsed")
        .and_then(Value::as_str)
        .and_then(parse_u256)
    else {
        return SimulationVerdict::Transient("simulation response has no gasUsed");
    };
    let Some(raw_logs) = call.get("logs").and_then(Value::as_array) else {
        return SimulationVerdict::Transient("simulation response has no logs");
    };
    let logs = match raw_logs.iter().map(parse_log).collect::<Option<Vec<_>>>() {
        Some(logs) => logs,
        None => return SimulationVerdict::Transient("simulation returned malformed logs"),
    };
    simulation_from_logs(gas_used, logs, entry_point, expected_hashes)
}

pub fn parse_trace_simulation(
    value: Value,
    entry_point: Address,
    expected_hashes: &[B256],
) -> SimulationVerdict<SimulationResult> {
    if trace_reports_failure(&value) {
        return if trace_reports_nonce_mismatch(&value) {
            SimulationVerdict::NonceMismatch
        } else {
            SimulationVerdict::Rejected("handleOps reverted during debug trace simulation")
        };
    }
    let Some(gas_used) = value
        .get("gasUsed")
        .and_then(Value::as_str)
        .and_then(parse_u256)
    else {
        return SimulationVerdict::Transient("debug trace response has no gasUsed");
    };
    let Some(logs) = trace_logs(&value) else {
        return SimulationVerdict::Transient("debug trace returned malformed logs");
    };
    simulation_from_logs(gas_used, logs, entry_point, expected_hashes)
}

fn trace_reports_failure(trace: &Value) -> bool {
    trace
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|error| !error.is_empty())
        || trace
            .get("revertReason")
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty())
}

fn trace_reports_nonce_mismatch(trace: &Value) -> bool {
    ["error", "revertReason"]
        .into_iter()
        .filter_map(|field| trace.get(field).and_then(Value::as_str))
        .any(|message| {
            let message = message.to_ascii_lowercase();
            message.contains("aa25") || message.contains("invalid account nonce")
        })
}

fn trace_logs(trace: &Value) -> Option<Vec<SimulatedLog>> {
    let own_logs = match trace.get("logs") {
        Some(logs) => logs
            .as_array()?
            .iter()
            .map(parse_log)
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    let child_logs = match trace.get("calls") {
        Some(calls) => calls
            .as_array()?
            .iter()
            .map(trace_logs)
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    Some(
        own_logs
            .into_iter()
            .chain(child_logs.into_iter().flatten())
            .collect(),
    )
}

fn simulation_from_logs(
    gas_used: U256,
    logs: Vec<SimulatedLog>,
    entry_point: Address,
    expected_hashes: &[B256],
) -> SimulationVerdict<SimulationResult> {
    let event_signature =
        keccak256(b"UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)");
    let events = logs
        .iter()
        .filter(|log| log.address == entry_point && log.topics.first() == Some(&event_signature))
        .map(parse_user_operation_event)
        .collect::<Option<Vec<_>>>();
    let Some(events) = events else {
        return SimulationVerdict::Transient("simulation returned malformed UserOperationEvent");
    };

    if events.len() != expected_hashes.len()
        || events
            .iter()
            .zip(expected_hashes)
            .any(|(event, expected)| event.user_operation_hash != *expected)
    {
        return SimulationVerdict::Rejected(
            "simulation did not emit the expected UserOperationEvent",
        );
    }
    if events.iter().any(|event| !event.success) {
        return SimulationVerdict::Rejected("UserOperation execution reverted during simulation");
    }

    SimulationVerdict::Success(SimulationResult {
        gas_used,
        events,
        logs,
        full_execution: true,
    })
}

fn call_reports_nonce_mismatch(call: &Value) -> bool {
    let message = call
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    message.contains("aa25") || message.contains("invalid account nonce")
}

fn parse_log(value: &Value) -> Option<SimulatedLog> {
    let address = Address::from_str(value.get("address")?.as_str()?).ok()?;
    let topics = value
        .get("topics")?
        .as_array()?
        .iter()
        .map(|topic| B256::from_str(topic.as_str()?).ok())
        .collect::<Option<Vec<_>>>()?;
    let data = parse_bytes(value.get("data")?.as_str()?)?;
    Some(SimulatedLog {
        address,
        topics,
        data,
    })
}

fn parse_user_operation_event(log: &SimulatedLog) -> Option<SimulatedUserOperation> {
    let user_operation_hash = *log.topics.get(1)?;
    if log.data.len() < 4 * 32 {
        return None;
    }
    let success = parse_word(&log.data, 1)?;
    if success > U256::from(1) {
        return None;
    }
    // Word 2 is actualGasCost. In-band operations intentionally declare zero EntryPoint fees,
    // so the executor prices its outer transaction independently. Still decode it to require a
    // canonical complete event before using word 3.
    let _actual_gas_cost = parse_word(&log.data, 2)?;
    Some(SimulatedUserOperation {
        user_operation_hash,
        success: success == U256::from(1),
        actual_gas_used: parse_word(&log.data, 3)?,
    })
}

fn parse_word(data: &[u8], index: usize) -> Option<U256> {
    let start = index.checked_mul(32)?;
    let bytes: [u8; 32] = data.get(start..start + 32)?.try_into().ok()?;
    Some(U256::from_be_bytes(bytes))
}

pub fn parse_u256(value: &str) -> Option<U256> {
    U256::from_str_radix(value.strip_prefix("0x")?, 16).ok()
}

fn parse_bytes(value: &str) -> Option<Bytes> {
    let value = value.strip_prefix("0x")?;
    if !value.len().is_multiple_of(2) {
        return None;
    }
    hex::decode(value).ok().map(Into::into)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256};
    use serde_json::json;

    use super::{
        DETERMINISTIC_DEPLOYER, ENTRY_POINT_SIMULATIONS_V07_INIT_CODE_HASH,
        PIMLICO_SIMULATIONS_INIT_CODE_HASH, SimulationVerdict, create2_address, parse_simulation,
        parse_trace_simulation, revert_reports_nonce_mismatch,
    };

    #[test]
    fn ignores_forged_events_from_non_entry_point_addresses() {
        let hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let signature = alloy::primitives::keccak256(
            b"UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)",
        );
        let word = |value: u64| format!("{value:064x}");
        let response = json!([{
            "calls": [{
                "status": "0x1",
                "gasUsed": "0x100",
                "logs": [{
                    "address": "0x2222222222222222222222222222222222222222",
                    "topics": [signature.to_string(), hash.to_string()],
                    "data": format!("0x{}{}{}{}", word(0), word(1), word(10), word(9)),
                }]
            }]
        }]);

        assert!(matches!(
            parse_simulation(
                response,
                address!("1111111111111111111111111111111111111111"),
                &[hash]
            ),
            SimulationVerdict::Rejected(_)
        ));
    }

    #[test]
    fn accepts_only_an_explicit_success_call_status() {
        let hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let signature = alloy::primitives::keccak256(
            b"UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)",
        );
        let word = |value: u64| format!("{value:064x}");
        let response = |status: Option<&str>| {
            let mut call = json!({
                "gasUsed": "0x100",
                "logs": [{
                    "address": "0x1111111111111111111111111111111111111111",
                    "topics": [signature.to_string(), hash.to_string()],
                    "data": format!("0x{}{}{}{}", word(0), word(1), word(10), word(9)),
                }]
            });
            if let Some(status) = status {
                call["status"] = json!(status);
            }
            json!([{ "calls": [call] }])
        };
        let entry_point = address!("1111111111111111111111111111111111111111");

        assert!(matches!(
            parse_simulation(response(Some("0x1")), entry_point, &[hash]),
            SimulationVerdict::Success(_)
        ));
        assert!(matches!(
            parse_simulation(response(Some("0x0")), entry_point, &[hash]),
            SimulationVerdict::Rejected(_)
        ));
        for invalid in [None, Some("0x2"), Some("invalid")] {
            assert!(matches!(
                parse_simulation(response(invalid), entry_point, &[hash]),
                SimulationVerdict::Transient(_)
            ));
        }
    }

    #[test]
    fn classifies_only_explicit_account_nonce_errors_for_follow_up() {
        let entry_point = address!("1111111111111111111111111111111111111111");
        let hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        for message in [
            "FailedOp(0, AA25 invalid account nonce)",
            "Invalid Account Nonce",
        ] {
            let response = json!([{
                "calls": [{
                    "status": "0x0",
                    "error": { "message": message }
                }]
            }]);
            assert!(matches!(
                parse_simulation(response, entry_point, &[hash]),
                SimulationVerdict::NonceMismatch
            ));
        }

        let unrelated = json!([{
            "calls": [{
                "status": "0x0",
                "error": { "message": "AA24 signature error" }
            }]
        }]);
        assert!(matches!(
            parse_simulation(unrelated, entry_point, &[hash]),
            SimulationVerdict::Rejected(_)
        ));
    }

    #[test]
    fn accepts_a_nested_entry_point_event_from_debug_trace_call() {
        let entry_point = address!("1111111111111111111111111111111111111111");
        let hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let signature = alloy::primitives::keccak256(
            b"UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)",
        );
        let word = |value: u64| format!("{value:064x}");
        let trace = json!({
            "type": "CALL",
            "gasUsed": "0x120",
            "calls": [{
                "type": "CALL",
                "logs": [{
                    "address": entry_point.to_string(),
                    "topics": [signature.to_string(), hash.to_string()],
                    "data": format!("0x{}{}{}{}", word(0), word(1), word(10), word(9)),
                }]
            }]
        });

        assert!(matches!(
            parse_trace_simulation(trace, entry_point, &[hash]),
            SimulationVerdict::Success(_)
        ));
    }

    #[test]
    fn avalanche_asks_for_simulate_v1_last_and_so_does_a_chain_proven_to_lack_it() {
        use super::{SimulateV1Turn, simulate_v1_turn};
        assert_eq!(simulate_v1_turn(43_114, false), SimulateV1Turn::Last);
        assert_eq!(simulate_v1_turn(43_113, false), SimulateV1Turn::Last);
        // Polygon, Base, Arbitrum: publicnode serves it (measured 2026-10-08).
        for chain_id in [137, 8_453, 42_161] {
            assert_eq!(simulate_v1_turn(chain_id, false), SimulateV1Turn::First);
            assert_eq!(simulate_v1_turn(chain_id, true), SimulateV1Turn::Last);
        }
    }

    #[test]
    fn derives_the_deployed_monad_pimlico_addresses_from_the_treasury_salt() {
        let treasury = address!("ee2cca98ecbff34663591a925968fa4db5a1f0dd");
        let salt = alloy::primitives::keccak256(treasury.as_slice());
        assert_eq!(
            create2_address(
                DETERMINISTIC_DEPLOYER,
                salt,
                PIMLICO_SIMULATIONS_INIT_CODE_HASH
            ),
            address!("002ea30f431a34736439e98275b10350112de6ae")
        );
        assert_eq!(
            create2_address(
                DETERMINISTIC_DEPLOYER,
                salt,
                ENTRY_POINT_SIMULATIONS_V07_INIT_CODE_HASH,
            ),
            address!("e050ef4de2109ecded19dcc3d3f2120121d47ec5")
        );
    }

    #[test]
    fn recognizes_a_nonce_mismatch_embedded_in_pimlico_revert_data() {
        let error_data = "0x65c8fd4d0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000194141323520696e76616c6964206163636f756e74206e6f6e636500000000000000";
        assert!(revert_reports_nonce_mismatch(
            "execution reverted",
            Some(error_data)
        ));
        assert!(!revert_reports_nonce_mismatch(
            "execution reverted",
            Some("0x65c8fd4d")
        ));
    }
}
