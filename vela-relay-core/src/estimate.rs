//! `eth_estimateUserOperationGas` computation: simulation planning (calldata,
//! state overrides, vendored EntryPoint simulations bytecode), validation
//! decoding, revert-reason extraction, and every gas rule. Moved from the
//! docker shell's handler (spec 002) so both shells estimate identically; the
//! shells perform exactly two JSON-RPC calls and hand the outcomes in as data.

use serde_json::{Map, Value, json};

use crate::wire::{
    EstimatableUserOperation, EstimatableUserOperationV0_7, RpcError, StateOverrideSet,
    UserOperationGasEstimate,
};

const SIMULATE_VALIDATION_SELECTOR: [u8; 4] = [0xc3, 0xbc, 0xe0, 0x09];
const SIMULATION_VERIFICATION_GAS_LIMIT: u128 = 500_000;
const TEMPO_SIMULATION_VERIFICATION_GAS_LIMIT: u128 = 8_000_000;
const SIMULATION_CALL_GAS_LIMIT: u128 = 1_000_000;
const DEFAULT_CALL_GAS_LIMIT: u128 = 200_000;
const MIN_CALL_GAS_LIMIT: u128 = 50_000;
const MIN_VERIFICATION_GAS_LIMIT: u128 = 100_000;
const MIN_PAYMASTER_GAS_LIMIT: u128 = 100_000;
const SIMULATION_SENDER_BALANCE: &str = "0x56bc75e2d63100000";
/// EntryPoint v0.7's `SenderCreator`, the only caller an account factory sees
/// when the EntryPoint deploys an account (created by the EntryPoint at its
/// canonical address, so the same on every chain).
const SENDER_CREATOR_V07: &str = "0xefc2c1444ebcc4db75e7613d20c6a62ff67a167c";
/// A transaction's intrinsic gas, before calldata.
const TRANSACTION_BASE_GAS: u128 = 21_000;
/// The outer gas a one-operation `handleOps` spends beyond its intrinsic gas,
/// its calldata, the operation's validation (`preOpGas`) and its execution
/// (`eth_estimateGas` of the call, less that call's own intrinsic gas): the
/// EntryPoint's loop, events and beneficiary transfer.
///
/// Measured, not derived. Replaying the relay's two estimate calls at the
/// parent block of four mined Vela operations on Ethereum mainnet left
/// 1,249 / 2,368 / 3,511 gas unexplained for three deployed Safe sends
/// (0x7132ee31…, 0xe42fb6b9…, 0x22cbfbd6…) and −930 for a first operation
/// that deployed its Safe (0x86795d08…, 5,034,866 gas); the standalone
/// estimate's own slack (cold accesses the EntryPoint has already warmed, the
/// 63/64 forwarding headroom) absorbs most of the EntryPoint's overhead.
/// 10,000 covers the largest with room for a dummy signature cheaper in
/// calldata than the real one (Vela's costs 3,060 gas against a real 3,792).
const ENTRY_POINT_OVERHEAD_GAS: u128 = 10_000;

const ENTRY_POINT_SIMULATIONS_BYTECODE: &str =
    include_str!("entry_point_simulations_v07_bytecode.txt");

/// A JSON-RPC error from a simulation upstream, in the shape both shells'
/// transports produce.
#[derive(Debug)]
pub struct SimulationRevert {
    pub code: Option<i64>,
    pub message: String,
    pub data: Option<Value>,
}

/// The two ways a simulation call can fail; the shells map their transport
/// errors into this and the core decides what each means.
#[derive(Debug)]
pub enum SimulationCallError {
    Reverted(SimulationRevert),
    Unavailable,
}

/// The shells' `eth_estimateGas` outcome for the execution phase.
pub enum CallGasSource {
    /// `callData` was empty: no execution call is needed.
    NotNeeded,
    Estimated(Value),
    /// The `eth_simulateV1` answer to [`EstimatePlan::deployed_execution_params`]:
    /// a counterfactual account deployed, then its `callData` run, with the
    /// gas each used.
    Simulated(Value),
    Reverted(SimulationRevert),
    Unavailable,
}

pub struct EstimateOutcome {
    pub estimate: UserOperationGasEstimate,
    /// `Some(fallback)` when estimation was unavailable and the conservative
    /// fallback was used — the shell logs its historical warn with it.
    pub fallback_call_gas: Option<u128>,
}

pub struct EstimatePlan {
    chain_id: u64,
    operation: SimulationUserOperation,
    simulation_call_data: Vec<u8>,
    validation_params: Value,
    execution_params: Option<Value>,
    deployed_execution_params: Option<Value>,
}

impl EstimatePlan {
    /// Params for the first call: `eth_call` of `simulateValidation` against
    /// the EntryPoint with the simulations bytecode override.
    pub fn validation_params(&self) -> &Value {
        &self.validation_params
    }

    /// Params for the second call (`eth_estimateGas`), absent when `callData`
    /// is empty.
    pub fn execution_params(&self) -> Option<&Value> {
        self.execution_params.as_ref()
    }

    /// Params for an `eth_simulateV1` the shell asks INSTEAD of the
    /// `eth_estimateGas` above when the sender is not deployed yet, falling
    /// back to that `eth_estimateGas` only when no endpoint performs it.
    ///
    /// `eth_estimateGas` of a call to an address with no code measures a
    /// plain transfer, not the account's execution: a counterfactual Safe's
    /// first send was estimated at the 50,000 floor while it used 52,804, and
    /// a first operation that registered a backup at about 30,000 while it
    /// used 4,315,038. The simulation deploys the account the way the
    /// EntryPoint would (the factory, called by the `SenderCreator`) and then
    /// runs the `callData` from the EntryPoint in the same block, so the
    /// execution is measured with the account's code in place.
    ///
    /// `None` when the sender is deployed, when there is no `callData`, or on
    /// a chain known to lack the method
    /// ([`crate::simulation::chain_lacks_simulate_v1`]).
    pub fn deployed_execution_params(&self) -> Option<&Value> {
        self.deployed_execution_params.as_ref()
    }
}

/// Validates the request and builds both simulation calls, exactly as the
/// docker handler did.
pub fn plan(
    chain_id: u64,
    user_operation: EstimatableUserOperation,
    entry_point: &str,
    state_overrides: Option<&StateOverrideSet>,
) -> Result<EstimatePlan, RpcError> {
    // Estimation simulates the UserOperation only. In-band reimbursement
    // admission checks belong exclusively to eth_sendUserOperation.
    if !crate::admission::entry_point_is_supported(entry_point) {
        return Err(RpcError::invalid_params("unsupported EntryPoint"));
    }

    let operation = SimulationUserOperation::try_from((chain_id, user_operation))?;
    let simulation_call_data = operation.simulate_validation_calldata();
    let simulation_overrides =
        state_overrides_for_simulation(state_overrides, &operation.sender, entry_point);
    let validation_params = json!([
        { "to": entry_point, "data": bytes_to_hex(&simulation_call_data) },
        "latest",
        simulation_overrides,
    ]);

    let execution_params = if operation.call_data.is_empty() {
        None
    } else {
        let execution_overrides = state_overrides_for_execution(state_overrides, &operation.sender);
        Some(json!([
            {
                "from": entry_point,
                "to": format_address(operation.sender),
                "data": bytes_to_hex(&operation.call_data),
            },
            "latest",
            execution_overrides,
        ]))
    };
    let deployed_execution_params = (!operation.call_data.is_empty()
        && operation.init_code.len() >= 20
        && !crate::simulation::chain_lacks_simulate_v1(chain_id))
    .then(|| {
        let (factory, factory_data) = operation.init_code.split_at(20);
        json!([
            {
                "blockStateCalls": [{
                    "stateOverrides": state_overrides_for_execution(state_overrides, &operation.sender),
                    "calls": [
                        {
                            "from": SENDER_CREATOR_V07,
                            "to": bytes_to_hex(factory),
                            "data": bytes_to_hex(factory_data),
                        },
                        {
                            "from": entry_point,
                            "to": format_address(operation.sender),
                            "data": bytes_to_hex(&operation.call_data),
                        },
                    ],
                }],
                "validation": false,
                "traceTransfers": false,
            },
            "latest",
        ])
    });

    Ok(EstimatePlan {
        chain_id,
        operation,
        simulation_call_data,
        validation_params,
        execution_params,
        deployed_execution_params,
    })
}

/// Applies every gas rule to the two call outcomes.
///
/// `terms` are the executor's billing terms; their gas buffer prices
/// `settlementGas`, which is
/// returned only on a chain whose executor bills the gas a bundle uses
/// ([`crate::cost::SettlementGasRule::Measured`]) and only when the execution
/// was measured. See [`settlement_gas`].
pub fn finish(
    plan: &EstimatePlan,
    validation_value: &Value,
    call_gas: CallGasSource,
    terms: &crate::cost::BillingTerms,
) -> Result<EstimateOutcome, RpcError> {
    let pre_op_gas = parse_validation_pre_op_gas(validation_value)?;
    let undeployed = !plan.operation.init_code.is_empty();
    let call_data_gas = calldata_gas(&plan.operation.call_data);

    let mut fallback_call_gas = None;
    // The call gas LIMIT, and the gas the execution is measured to use (when
    // it is): a standalone call's gas, less the intrinsic gas it paid as a
    // transaction of its own, which inside `handleOps` the outer transaction
    // pays once.
    let (call_gas_limit, execution_gas) = match call_gas {
        CallGasSource::NotNeeded => (0, Some(0)),
        CallGasSource::Estimated(result) => {
            let gas = parse_quantity(
                result
                    .as_str()
                    .ok_or_else(RpcError::estimation_unavailable)?,
                "eth_estimateGas result",
            )?;
            (
                with_percent_buffer(gas, 150).max(MIN_CALL_GAS_LIMIT),
                // A sender with no code measured a plain transfer.
                (!undeployed).then(|| gas.saturating_sub(TRANSACTION_BASE_GAS + call_data_gas)),
            )
        }
        CallGasSource::Simulated(result) => {
            let gas = parse_deployed_execution_gas(&result)?;
            (
                with_percent_buffer(gas, 150).max(MIN_CALL_GAS_LIMIT),
                Some(gas.saturating_sub(TRANSACTION_BASE_GAS + call_data_gas)),
            )
        }
        CallGasSource::Reverted(error) => return Err(simulation_revert_error(error)),
        CallGasSource::Unavailable => {
            let fallback = plan
                .operation
                .call_gas_limit
                .unwrap_or(DEFAULT_CALL_GAS_LIMIT);
            fallback_call_gas = Some(fallback);
            (fallback.max(MIN_CALL_GAS_LIMIT), None)
        }
    };

    let verification_gas_limit =
        with_percent_buffer(pre_op_gas, 150).max(MIN_VERIFICATION_GAS_LIMIT);
    let (paymaster_verification_gas_limit, paymaster_post_op_gas_limit) =
        if plan.operation.has_paymaster {
            (
                (verification_gas_limit / 2).max(MIN_PAYMASTER_GAS_LIMIT),
                MIN_PAYMASTER_GAS_LIMIT,
            )
        } else {
            (0, 0)
        };

    let pre_verification_gas = plan
        .operation
        .pre_verification_gas(&plan.simulation_call_data);
    let declared = verification_gas_limit
        .saturating_add(call_gas_limit)
        .saturating_add(pre_verification_gas);
    let settlement_gas = (crate::cost::settlement_gas_rule(plan.chain_id)
        == crate::cost::SettlementGasRule::Measured)
        .then_some(execution_gas)
        .flatten()
        .and_then(|execution_gas| {
            settlement_gas(
                plan,
                pre_op_gas,
                execution_gas,
                [verification_gas_limit, call_gas_limit, pre_verification_gas],
                terms,
            )
        })
        // Never above the limits it is returned beside.
        .map(|gas| gas.min(declared));
    Ok(EstimateOutcome {
        estimate: UserOperationGasEstimate {
            pre_verification_gas: quantity(pre_verification_gas),
            verification_gas_limit: quantity(verification_gas_limit),
            call_gas_limit: quantity(call_gas_limit),
            paymaster_verification_gas_limit: quantity(paymaster_verification_gas_limit),
            paymaster_post_op_gas_limit: quantity(paymaster_post_op_gas_limit),
            settlement_gas: settlement_gas.map(quantity),
        },
        fallback_call_gas,
    })
}

/// The gas the executor will bill this operation for, predicted from the two
/// estimate calls: the gas a one-operation `handleOps` carrying it uses,
/// through [`crate::cost::buffered_gas`] — the executor's own rule over its
/// own measurement (`docs/fees.md` §1).
///
/// ```text
/// used = 21,000 + calldata gas of handleOps([op], beneficiary)
///      + preOpGas                      (simulateValidation: validation, and deployment)
///      + execution                     (the call's gas less its own intrinsic gas)
///      + ENTRY_POINT_OVERHEAD_GAS
/// ```
///
/// The `handleOps` calldata is encoded with the limits this estimate returns,
/// the signature the request carried and an all-`0xff` beneficiary (the most
/// a 20-byte address can cost). Predicted this way the four mined operations
/// in [`ENTRY_POINT_OVERHEAD_GAS`] come out 0.2–6% above the gas they used.
/// `None` on overflow.
fn settlement_gas(
    plan: &EstimatePlan,
    pre_op_gas: u128,
    execution_gas: u128,
    [verification_gas_limit, call_gas_limit, pre_verification_gas]: [u128; 3],
    terms: &crate::cost::BillingTerms,
) -> Option<u128> {
    let handle_ops = plan.operation.handle_ops_calldata(
        verification_gas_limit,
        call_gas_limit,
        pre_verification_gas,
    );
    let used = TRANSACTION_BASE_GAS
        .checked_add(calldata_gas(&handle_ops))?
        .checked_add(pre_op_gas)?
        .checked_add(execution_gas)?
        .checked_add(ENTRY_POINT_OVERHEAD_GAS)?;
    u128::try_from(crate::cost::buffered_gas(
        alloy::primitives::U256::from(used),
        terms.gas_buffer_bps,
        terms.fixed_gas_buffer,
    )?)
    .ok()
}

/// The execution gas an `eth_simulateV1` of [deploy, execute] reports for its
/// second call. A deployment that did not succeed answers nothing usable; an
/// execution that reverted is the operation's own revert.
fn parse_deployed_execution_gas(value: &Value) -> Result<u128, RpcError> {
    let calls = value
        .get(0)
        .and_then(|block| block.get("calls"))
        .and_then(Value::as_array)
        .filter(|calls| calls.len() == 2)
        .ok_or_else(RpcError::estimation_unavailable)?;
    let succeeded = |call: &Value| call.get("status").and_then(Value::as_str) == Some("0x1");
    if !succeeded(&calls[0]) {
        return Err(RpcError::estimation_unavailable());
    }
    if !succeeded(&calls[1]) {
        let error = calls[1].get("error");
        return Err(simulation_revert_error(SimulationRevert {
            code: error
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            message: error
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("execution reverted")
                .to_owned(),
            data: calls[1]
                .get("returnData")
                .or_else(|| error.and_then(|error| error.get("data")))
                .cloned(),
        }));
    }
    parse_quantity(
        calls[1]
            .get("gasUsed")
            .and_then(Value::as_str)
            .ok_or_else(RpcError::estimation_unavailable)?,
        "eth_simulateV1 gasUsed",
    )
    .map_err(|_| RpcError::estimation_unavailable())
}

fn calldata_gas(bytes: &[u8]) -> u128 {
    bytes
        .iter()
        .fold(0u128, |total, byte| total + if *byte == 0 { 4 } else { 16 })
}

/// Maps a validation-call failure to the frozen response.
pub fn simulation_error(error: SimulationCallError) -> RpcError {
    match error {
        SimulationCallError::Reverted(error) => simulation_revert_error(error),
        SimulationCallError::Unavailable => RpcError::estimation_unavailable(),
    }
}

fn simulation_revert_error(error: SimulationRevert) -> RpcError {
    let reason = revert_reason(&error).unwrap_or_else(|| {
        let code = error
            .code
            .map(|code| format!(" (RPC code {code})"))
            .unwrap_or_default();
        format!("UserOperation validation reverted{code}: {}", error.message)
    });
    RpcError::user_operation_rejected(reason)
}

/// Same classification both shells' transports apply: a JSON-RPC error is a
/// contract revert (stop failing over, surface it) only when it looks like
/// execution output.
pub fn is_execution_revert(error: &SimulationRevert) -> bool {
    error.code == Some(3)
        || error
            .message
            .to_ascii_lowercase()
            .contains("execution reverted")
        || error
            .message
            .to_ascii_lowercase()
            .contains("execution error")
}

fn parse_validation_pre_op_gas(value: &Value) -> Result<u128, RpcError> {
    let encoded = value
        .as_str()
        .ok_or_else(RpcError::estimation_unavailable)?;
    let encoded = decode_hex_str(encoded).map_err(|()| RpcError::estimation_unavailable())?;
    // `abi.encode(ValidationResult)`: word 0 is the offset of the tuple, and
    // the tuple's FIRST head word is not `preOpGas` but the offset (relative
    // to the tuple) of `ReturnInfo`, which is itself dynamic because it ends
    // in `bytes paymasterContext`. `preOpGas` is ReturnInfo's first word.
    let tuple = read_usize_word(&encoded, 0).ok_or_else(RpcError::estimation_unavailable)?;
    let return_info = read_usize_word(&encoded, tuple)
        .and_then(|relative| tuple.checked_add(relative))
        .ok_or_else(RpcError::estimation_unavailable)?;
    read_u128_word(&encoded, return_info).ok_or_else(RpcError::estimation_unavailable)
}

/// Extract revert bytes from the error shapes used by common EVM RPC providers.
///
/// Geth usually places them in `error.data`, while gateway providers commonly nest them or
/// append them to `error.message`. The data is contract output, not a URL or credential.
pub fn revert_reason(error: &SimulationRevert) -> Option<String> {
    let data = error
        .data
        .as_ref()
        .and_then(|data| find_revert_data(data, 0))
        .or_else(|| find_revert_data_in_message(&error.message))?;
    let bytes = decode_hex_str(&data).ok()?;

    decode_revert_bytes(&bytes).or_else(|| {
        let selector = bytes
            .get(..4)
            .map(bytes_to_hex)
            .unwrap_or_else(|| "0x".into());
        Some(format!(
            "Unknown EVM custom error {selector} ({} bytes of revert data)",
            bytes.len()
        ))
    })
}

fn find_revert_data(value: &Value, depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }

    match value {
        Value::String(value) => valid_revert_data(value),
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_revert_data(value, depth + 1)),
        Value::Object(values) => ["data", "revertData", "originalError", "error"]
            .into_iter()
            .filter_map(|key| values.get(key))
            .find_map(|value| find_revert_data(value, depth + 1)),
        _ => None,
    }
}

fn find_revert_data_in_message(message: &str) -> Option<String> {
    message
        .match_indices("0x")
        .filter_map(|(index, _)| {
            let hex = message[index + 2..]
                .bytes()
                .take_while(u8::is_ascii_hexdigit)
                .count();
            valid_revert_data(&message[index..index + 2 + hex])
        })
        .max_by_key(|data| data.len())
}

fn valid_revert_data(value: &str) -> Option<String> {
    let value = value.strip_prefix("0x")?;
    (value.len() >= 8
        && value.len().is_multiple_of(2)
        && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then(|| format!("0x{value}"))
}

fn decode_revert_bytes(bytes: &[u8]) -> Option<String> {
    const ERROR_STRING_SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
    const PANIC_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];
    const FAILED_OP_SELECTOR: [u8; 4] = [0x22, 0x02, 0x66, 0xb6];
    const FAILED_OP_WITH_REVERT_SELECTOR: [u8; 4] = [0x65, 0xc8, 0xfd, 0x4d];
    const CALL_PHASE_REVERTED_SELECTOR: [u8; 4] = [0x46, 0x2c, 0x71, 0xb2];
    const SAFE_EXECUTION_FAILED_SELECTOR: [u8; 4] = [0xac, 0xfd, 0xb4, 0x44];

    let selector = bytes.get(..4)?;
    if selector == ERROR_STRING_SELECTOR {
        return abi_string(bytes, 4, 4);
    }
    if selector == PANIC_SELECTOR {
        return read_u128_word(bytes, 4).map(panic_reason);
    }
    if selector == FAILED_OP_SELECTOR {
        let operation_index = read_u128_word(bytes, 4)?;
        let reason = abi_string(bytes, 4, 36)?;
        return Some(format!("FailedOp({operation_index}): {reason}"));
    }
    if selector == FAILED_OP_WITH_REVERT_SELECTOR {
        let operation_index = read_u128_word(bytes, 4)?;
        let reason = abi_string(bytes, 4, 36)?;
        let inner = abi_bytes(bytes, 4, 68)
            .and_then(decode_revert_bytes)
            .unwrap_or_else(|| "inner call reverted".into());
        return Some(format!(
            "FailedOpWithRevert({operation_index}): {reason}; {inner}"
        ));
    }
    if selector == CALL_PHASE_REVERTED_SELECTOR {
        return abi_bytes(bytes, 4, 4)
            .and_then(decode_revert_bytes)
            .map(|reason| format!("CallPhaseReverted: {reason}"));
    }
    if selector == SAFE_EXECUTION_FAILED_SELECTOR {
        return Some("Safe execution failed: the target call in executeUserOp reverted".into());
    }

    None
}

fn abi_string(bytes: &[u8], arguments_start: usize, offset_position: usize) -> Option<String> {
    String::from_utf8(abi_bytes(bytes, arguments_start, offset_position)?.to_vec()).ok()
}

fn abi_bytes(bytes: &[u8], arguments_start: usize, offset_position: usize) -> Option<&[u8]> {
    let offset = read_usize_word(bytes, offset_position)?;
    let start = arguments_start.checked_add(offset)?;
    let length = read_usize_word(bytes, start)?;
    bytes.get(start.checked_add(32)?..start.checked_add(32)?.checked_add(length)?)
}

fn panic_reason(code: u128) -> String {
    let description = match code {
        0x01 => "assertion failed",
        0x11 => "arithmetic overflow or underflow",
        0x12 => "division or modulo by zero",
        0x21 => "invalid enum conversion",
        0x22 => "invalid storage byte array",
        0x31 => "empty array pop",
        0x32 => "array index out of bounds",
        0x41 => "memory allocation overflow",
        0x51 => "invalid internal function",
        _ => return format!("Solidity panic 0x{code:x}"),
    };
    format!("Solidity panic 0x{code:x}: {description}")
}

fn state_overrides_for_simulation(
    user_overrides: Option<&StateOverrideSet>,
    sender: &[u8; 20],
    entry_point: &str,
) -> Value {
    let mut overrides = serialized_overrides(user_overrides);
    let object = overrides
        .as_object_mut()
        .expect("state overrides must serialize as an object");

    let mut sender_override = take_address_override(object, sender);
    sender_override.insert(
        "balance".into(),
        Value::String(SIMULATION_SENDER_BALANCE.into()),
    );
    object.insert(format_address(*sender), Value::Object(sender_override));

    let entry_point = entry_point.to_ascii_lowercase();
    let mut entry_point_override = take_string_address_override(object, &entry_point);
    entry_point_override.insert(
        "code".into(),
        Value::String(ENTRY_POINT_SIMULATIONS_BYTECODE.trim().into()),
    );
    object.insert(entry_point, Value::Object(entry_point_override));
    overrides
}

fn state_overrides_for_execution(
    user_overrides: Option<&StateOverrideSet>,
    sender: &[u8; 20],
) -> Value {
    let mut overrides = serialized_overrides(user_overrides);
    let object = overrides
        .as_object_mut()
        .expect("state overrides must serialize as an object");
    let mut sender_override = take_address_override(object, sender);
    sender_override.insert(
        "balance".into(),
        Value::String(SIMULATION_SENDER_BALANCE.into()),
    );
    object.insert(format_address(*sender), Value::Object(sender_override));
    overrides
}

fn serialized_overrides(overrides: Option<&StateOverrideSet>) -> Value {
    overrides
        .map(|overrides| serde_json::to_value(overrides).expect("state overrides must serialize"))
        .unwrap_or_else(|| Value::Object(Map::new()))
}

fn take_address_override(
    object: &mut Map<String, Value>,
    address: &[u8; 20],
) -> Map<String, Value> {
    take_string_address_override(object, &format_address(*address))
}

fn take_string_address_override(
    object: &mut Map<String, Value>,
    address: &str,
) -> Map<String, Value> {
    let key = object
        .keys()
        .find(|key| key.eq_ignore_ascii_case(address))
        .cloned();
    key.and_then(|key| object.remove(&key))
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

#[derive(Debug)]
struct SimulationUserOperation {
    sender: [u8; 20],
    nonce: [u8; 32],
    init_code: Vec<u8>,
    call_data: Vec<u8>,
    call_gas_limit: Option<u128>,
    verification_gas_limit: u128,
    has_paymaster: bool,
    paymaster_and_data: Vec<u8>,
    signature: Vec<u8>,
    has_eip7702_auth: bool,
}

impl TryFrom<(u64, EstimatableUserOperation)> for SimulationUserOperation {
    type Error = RpcError;

    fn try_from(value: (u64, EstimatableUserOperation)) -> Result<Self, Self::Error> {
        let (chain_id, operation) = value;
        match operation {
            EstimatableUserOperation::V0_7(operation) => Self::from_v0_7(chain_id, operation),
            EstimatableUserOperation::V0_6(_) => Err(RpcError::invalid_params(
                "the configured EntryPoint requires an unpacked v0.7 UserOperation",
            )),
        }
    }
}

impl SimulationUserOperation {
    fn from_v0_7(
        chain_id: u64,
        operation: Box<EstimatableUserOperationV0_7>,
    ) -> Result<Self, RpcError> {
        let sender = address(&operation.sender, "sender")?;
        let nonce = quantity_word(&operation.nonce, "nonce")?;
        let call_data = bytes(&operation.call_data, "callData")?;
        let signature = bytes(
            operation
                .signature
                .as_deref()
                .ok_or_else(|| RpcError::invalid_params("signature is required for estimation"))?,
            "signature",
        )?;

        let init_code = match (operation.factory, operation.factory_data) {
            (Some(factory), factory_data) => {
                let mut init_code = address(&factory, "factory")?.to_vec();
                init_code.extend(bytes(
                    factory_data.as_deref().unwrap_or("0x"),
                    "factoryData",
                )?);
                init_code
            }
            (None, Some(factory_data)) if factory_data != "0x" => {
                return Err(RpcError::invalid_params("factoryData requires factory"));
            }
            (None, _) => Vec::new(),
        };

        let call_gas_limit =
            optional_quantity(operation.call_gas_limit.as_deref(), "callGasLimit")?;
        let default_verification_gas = if crate::tempo::is_tempo_chain(chain_id) {
            TEMPO_SIMULATION_VERIFICATION_GAS_LIMIT
        } else {
            SIMULATION_VERIFICATION_GAS_LIMIT
        };
        let verification_gas_limit = optional_quantity(
            operation.verification_gas_limit.as_deref(),
            "verificationGasLimit",
        )?
        .filter(|gas| *gas > 0)
        .unwrap_or(default_verification_gas);
        let max_fee_per_gas =
            optional_quantity(operation.max_fee_per_gas.as_deref(), "maxFeePerGas")?.unwrap_or(0);
        let max_priority_fee_per_gas = optional_quantity(
            operation.max_priority_fee_per_gas.as_deref(),
            "maxPriorityFeePerGas",
        )?
        .unwrap_or(0);
        if max_fee_per_gas != 0 || max_priority_fee_per_gas != 0 {
            return Err(RpcError::invalid_params(
                "maxFeePerGas and maxPriorityFeePerGas must both be 0x0",
            ));
        }

        let (has_paymaster, paymaster_and_data) = match operation.paymaster {
            Some(paymaster) => {
                let mut value = address(&paymaster, "paymaster")?.to_vec();
                value.extend(uint128_word(
                    optional_quantity(
                        operation.paymaster_verification_gas_limit.as_deref(),
                        "paymasterVerificationGasLimit",
                    )?
                    .unwrap_or(0),
                ));
                value.extend(uint128_word(
                    optional_quantity(
                        operation.paymaster_post_op_gas_limit.as_deref(),
                        "paymasterPostOpGasLimit",
                    )?
                    .unwrap_or(0),
                ));
                value.extend(bytes(
                    operation.paymaster_data.as_deref().unwrap_or("0x"),
                    "paymasterData",
                )?);
                (true, value)
            }
            None => (false, Vec::new()),
        };

        Ok(Self {
            sender,
            nonce,
            init_code,
            call_data,
            call_gas_limit,
            verification_gas_limit,
            has_paymaster,
            paymaster_and_data,
            signature,
            has_eip7702_auth: operation.eip7702_auth.is_some(),
        })
    }

    fn simulate_validation_calldata(&self) -> Vec<u8> {
        let call_gas_limit = self
            .call_gas_limit
            .filter(|gas| *gas > 0)
            .unwrap_or(SIMULATION_CALL_GAS_LIMIT);
        let account_gas_limits = [
            uint128_word(self.verification_gas_limit),
            uint128_word(call_gas_limit),
        ]
        .concat();
        // All supported chains settle bundler fees in-band. The EntryPoint native-prefund
        // fields are signed and must remain zero for both estimation and submission.
        let gas_fees = [uint128_word(0), uint128_word(0)].concat();
        let mut output = SIMULATE_VALIDATION_SELECTOR.to_vec();
        output.extend(usize_word(32));
        output.extend(self.encode_user_operation_tuple(&account_gas_limits, 0, &gas_fees));
        output
    }

    /// `handleOps([this operation], 0xff…ff)` as the executor would send it,
    /// carrying the given limits — what the outer transaction's calldata
    /// costs is part of the gas it uses.
    fn handle_ops_calldata(
        &self,
        verification_gas_limit: u128,
        call_gas_limit: u128,
        pre_verification_gas: u128,
    ) -> Vec<u8> {
        const HANDLE_OPS_SELECTOR: [u8; 4] = [0x76, 0x5e, 0x82, 0x7f];
        let account_gas_limits = [
            uint128_word(verification_gas_limit),
            uint128_word(call_gas_limit),
        ]
        .concat();
        let gas_fees = [uint128_word(0), uint128_word(0)].concat();
        let mut output = HANDLE_OPS_SELECTOR.to_vec();
        output.extend(usize_word(64));
        output.extend(address_word([0xff; 20]));
        output.extend(usize_word(1));
        output.extend(usize_word(32));
        output.extend(self.encode_user_operation_tuple(
            &account_gas_limits,
            pre_verification_gas,
            &gas_fees,
        ));
        output
    }

    fn encode_user_operation_tuple(
        &self,
        account_gas_limits: &[u8],
        pre_verification_gas: u128,
        gas_fees: &[u8],
    ) -> Vec<u8> {
        const HEAD_SIZE: usize = 9 * 32;
        let mut tail = Vec::new();
        let mut offsets = [0usize; 4];
        for (index, value) in [
            self.init_code.as_slice(),
            self.call_data.as_slice(),
            self.paymaster_and_data.as_slice(),
            self.signature.as_slice(),
        ]
        .into_iter()
        .enumerate()
        {
            offsets[index] = HEAD_SIZE + tail.len();
            tail.extend(dynamic_bytes(value));
        }

        let mut head = Vec::with_capacity(HEAD_SIZE);
        head.extend(address_word(self.sender));
        head.extend(self.nonce);
        head.extend(usize_word(offsets[0]));
        head.extend(usize_word(offsets[1]));
        head.extend(account_gas_limits);
        head.extend(uint256_word(pre_verification_gas));
        head.extend(gas_fees);
        head.extend(usize_word(offsets[2]));
        head.extend(usize_word(offsets[3]));
        head.extend(tail);
        head
    }

    fn pre_verification_gas(&self, simulation_call_data: &[u8]) -> u128 {
        let calldata_gas = calldata_gas(simulation_call_data);
        let auth_gas = if self.has_eip7702_auth { 25_000 } else { 0 };
        let base = 21_000 + 50_000 + 10_000 + 3_000 + calldata_gas + auth_gas;
        base + (base / 10).max(5_000)
    }
}

fn dynamic_bytes(value: &[u8]) -> Vec<u8> {
    let mut encoded = usize_word(value.len()).to_vec();
    encoded.extend(value);
    let padding = (32 - value.len() % 32) % 32;
    encoded.extend(std::iter::repeat_n(0, padding));
    encoded
}

fn address(value: &str, field: &str) -> Result<[u8; 20], RpcError> {
    crate::quote::address(value)
        .ok_or_else(|| RpcError::invalid_params(format!("{field} must be a 20-byte address")))
}

fn bytes(value: &str, field: &str) -> Result<Vec<u8>, RpcError> {
    decode_hex_str(value)
        .map_err(|()| RpcError::invalid_params(format!("{field} must be 0x-prefixed hex data")))
}

fn decode_hex_str(value: &str) -> Result<Vec<u8>, ()> {
    let value = value.strip_prefix("0x").ok_or(())?;
    hex::decode(value).map_err(|_| ())
}

fn optional_quantity(value: Option<&str>, field: &str) -> Result<Option<u128>, RpcError> {
    value.map(|value| parse_quantity(value, field)).transpose()
}

fn parse_quantity(value: &str, field: &str) -> Result<u128, RpcError> {
    let value = value.strip_prefix("0x").ok_or_else(|| {
        RpcError::invalid_params(format!("{field} must be a 0x-prefixed quantity"))
    })?;
    if value.is_empty() || value.len() > 32 {
        return Err(RpcError::invalid_params(format!("invalid {field}")));
    }
    u128::from_str_radix(value, 16)
        .map_err(|_| RpcError::invalid_params(format!("invalid {field}")))
}

fn quantity_word(value: &str, field: &str) -> Result<[u8; 32], RpcError> {
    let value = value.strip_prefix("0x").ok_or_else(|| {
        RpcError::invalid_params(format!("{field} must be a 0x-prefixed quantity"))
    })?;
    if value.is_empty() || value.len() > 64 {
        return Err(RpcError::invalid_params(format!("invalid {field}")));
    }
    let value = if value.len() % 2 == 0 {
        value.to_owned()
    } else {
        format!("0{value}")
    };
    let bytes = decode_hex_str(&format!("0x{value}"))
        .map_err(|()| RpcError::invalid_params(format!("invalid {field}")))?;
    let mut word = [0; 32];
    let offset = word.len() - bytes.len();
    word[offset..].copy_from_slice(&bytes);
    Ok(word)
}

fn address_word(address: [u8; 20]) -> [u8; 32] {
    let mut word = [0; 32];
    word[12..].copy_from_slice(&address);
    word
}

fn uint128_word(value: u128) -> [u8; 16] {
    value.to_be_bytes()
}

fn uint256_word(value: u128) -> [u8; 32] {
    let mut word = [0; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

fn usize_word(value: usize) -> [u8; 32] {
    let mut word = [0; 32];
    word[24..].copy_from_slice(&(value as u64).to_be_bytes());
    word
}

fn read_u128_word(data: &[u8], offset: usize) -> Option<u128> {
    let word = data.get(offset..offset.checked_add(32)?)?;
    if word.get(..16)?.iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u128::from_be_bytes(word.get(16..)?.try_into().ok()?))
}

fn read_usize_word(data: &[u8], offset: usize) -> Option<usize> {
    let word = data.get(offset..offset.checked_add(32)?)?;
    if word.get(..24)?.iter().any(|byte| *byte != 0) {
        return None;
    }
    let value = u64::from_be_bytes(word.get(24..)?.try_into().ok()?);
    value.try_into().ok()
}

fn format_address(address: [u8; 20]) -> String {
    format!("0x{}", hex::encode(address))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn quantity(value: u128) -> String {
    format!("0x{value:x}")
}

fn with_percent_buffer(value: u128, percentage: u128) -> u128 {
    value.saturating_mul(percentage).saturating_add(99) / 100
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        SimulationRevert, SimulationUserOperation, parse_validation_pre_op_gas, quantity_word,
        revert_reason, with_percent_buffer,
    };
    use crate::wire::{EstimatableUserOperation, EstimatableUserOperationV0_7};

    fn operation_with_fees(
        max_fee: Option<&str>,
        max_priority: Option<&str>,
        gas_limits: Option<&str>,
    ) -> EstimatableUserOperation {
        EstimatableUserOperation::V0_7(Box::new(EstimatableUserOperationV0_7 {
            sender: "0x1111111111111111111111111111111111111111".into(),
            nonce: "0x0".into(),
            factory: None,
            factory_data: None,
            call_data: "0x".into(),
            call_gas_limit: gas_limits.map(String::from),
            verification_gas_limit: gas_limits.map(String::from),
            pre_verification_gas: gas_limits.map(String::from),
            max_fee_per_gas: max_fee.map(String::from),
            max_priority_fee_per_gas: max_priority.map(String::from),
            paymaster: None,
            paymaster_verification_gas_limit: None,
            paymaster_post_op_gas_limit: None,
            paymaster_data: None,
            signature: Some("0x1234".into()),
            eip7702_auth: None,
            fee_token: None,
        }))
    }

    #[test]
    fn rejects_nonzero_fee_fields() {
        let operation = operation_with_fees(Some("0x1234"), Some("0x56"), Some("0x0"));

        let error = SimulationUserOperation::try_from((1, operation)).unwrap_err();

        assert_eq!(error.code, -32602);
        assert_eq!(
            error.data,
            Some(json!(
                "maxFeePerGas and maxPriorityFeePerGas must both be 0x0"
            ))
        );
    }

    #[test]
    fn preserves_zero_fee_fields_on_every_chain() {
        let operation = operation_with_fees(None, None, Some("0x0"));

        let calldata = SimulationUserOperation::try_from((1, operation))
            .unwrap()
            .simulate_validation_calldata();
        let tuple_start = 4 + 32;
        let gas_fees_start = tuple_start + 6 * 32;

        assert_eq!(&calldata[gas_fees_start..gas_fees_start + 32], &[0; 32]);
    }

    #[test]
    fn estimation_does_not_require_an_in_band_reimbursement() {
        let operation = operation_with_fees(Some("0x0"), Some("0x0"), None);

        assert!(SimulationUserOperation::try_from((1, operation)).is_ok());
    }

    /// Real `simulateValidation` answers from Ethereum mainnet (block
    /// ~26,149,240, 2026-10-08 UTC) for a Vela Safe's ETH send: a deployed
    /// Safe, and a counterfactual one whose operation carries the factory.
    const MAINNET_VALIDATION_DEPLOYED: &str = "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000140000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f1d200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000";
    const MAINNET_VALIDATION_UNDEPLOYED: &str = "0x000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000001400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000064a2300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn decodes_pre_op_gas_from_validation_return_data() {
        // The old reader returned 320 — the offset of ReturnInfo inside the
        // tuple — for both, and the 100,000 floor hid it.
        assert_eq!(
            parse_validation_pre_op_gas(&json!(MAINNET_VALIDATION_DEPLOYED)).unwrap(),
            61_906
        );
        assert_eq!(
            parse_validation_pre_op_gas(&json!(MAINNET_VALIDATION_UNDEPLOYED)).unwrap(),
            412_195
        );
    }

    #[test]
    fn a_validation_answer_that_ends_early_is_no_estimate() {
        // The tuple offset is read, but the ReturnInfo it points at is past
        // the end of the data: refuse rather than read a neighbouring word.
        let truncated = &MAINNET_VALIDATION_DEPLOYED[..2 + 2 * 64 * 2];
        assert_eq!(
            parse_validation_pre_op_gas(&json!(truncated))
                .unwrap_err()
                .code,
            super::RpcError::estimation_unavailable().code
        );
    }

    #[test]
    fn the_verification_limit_is_the_measured_validation_gas_with_its_buffer() {
        let plan = super::plan(
            1,
            operation_with_fees(None, None, None),
            "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
            None,
        )
        .unwrap();
        let limit = |validation: &str| {
            super::finish(
                &plan,
                &json!(validation),
                super::CallGasSource::NotNeeded,
                &crate::cost::BillingTerms::default(),
            )
            .unwrap()
            .estimate
            .verification_gas_limit
        };
        // 1.5 × 61,906 = 92,859 is under the 100,000 floor.
        assert_eq!(limit(MAINNET_VALIDATION_DEPLOYED), "0x186a0");
        // 1.5 × 412,195 = 618,293 (rounded up). It was the 100,000 floor.
        assert_eq!(
            limit(MAINNET_VALIDATION_UNDEPLOYED),
            format!("0x{:x}", 618_293)
        );
    }

    /// The callData of the 2026-10-02 Ethereum send (tx 0x7132ee31…, Safe
    /// 0x88cC…6894, nonce 1): an ETH transfer plus the in-band fee leg.
    const SEND_OF_2026_10_02_CALL_DATA: &str = "0x7bb3742800000000000000000000000038869bf66a61cf6bdb996a6ae40d5853fd43b52600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000001048d80ff0a000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000aa0014fb1fb21751e29f7ec48dc450017552e3d1ea5c0000000000000000000000000000000000000000000000000006f9678b6057b00000000000000000000000000000000000000000000000000000000000000000003e59292e18417f814112f731e7163534c6d2fe3c00000000000000000000000000000000000000000000000000027ef383a5daea00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    /// The dummy signature vela-wallet estimates with: a WebAuthn signature
    /// of the real length whose r and s are 1.
    const VELA_DUMMY_SIGNATURE: &str = "0x00000000000000000000000000000000000000000000000094a4f6affbd8975951142c3999aeab7ecee555c20000000000000000000000000000000000000000000000000000000000000041000000000000000000000000000000000000000000000000000000000000000140000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000025a69533717b230610f14ea657c0bd8231dd6fc7b7108f1215a874fbb1d14df34905000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000032226f726967696e223a2268747470733a2f2f67657476656c612e617070222c2263726f73734f726967696e223a66616c73650000000000000000000000000000";

    fn estimation_request(
        sender: &str,
        nonce: &str,
        factory: Option<(&str, &str)>,
        call_data: &str,
    ) -> EstimatableUserOperation {
        EstimatableUserOperation::V0_7(Box::new(EstimatableUserOperationV0_7 {
            sender: sender.into(),
            nonce: nonce.into(),
            factory: factory.map(|(factory, _)| factory.into()),
            factory_data: factory.map(|(_, data)| data.into()),
            call_data: call_data.into(),
            call_gas_limit: None,
            verification_gas_limit: None,
            pre_verification_gas: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            paymaster: None,
            paymaster_verification_gas_limit: None,
            paymaster_post_op_gas_limit: None,
            paymaster_data: None,
            signature: Some(VELA_DUMMY_SIGNATURE.into()),
            eip7702_auth: None,
            fee_token: None,
        }))
    }

    const ENTRY_POINT: &str = "0x0000000071727De22E5E9d8BAf0edAc6f37da032";

    fn quantity_of(value: &str) -> u128 {
        u128::from_str_radix(value.trim_start_matches("0x"), 16).unwrap()
    }

    /// A `simulateValidation` answer carrying `pre_op_gas`, in the mainnet
    /// answer's exact layout.
    fn validation_answer(pre_op_gas: u128) -> serde_json::Value {
        let mut bytes = hex::decode(&MAINNET_VALIDATION_DEPLOYED[2..]).unwrap();
        bytes[352..384].copy_from_slice(&super::uint256_word(pre_op_gas));
        json!(format!("0x{}", hex::encode(bytes)))
    }

    /// The executor bills `buffered_gas` of the bundle's measured gas
    /// (`cost::settlement_gas_allocations`); a wallet paying for
    /// `settlementGas` must never pay for less.
    fn executor_bills(measured: u128) -> u128 {
        let terms = crate::cost::BillingTerms::default();
        crate::cost::buffered_gas(
            alloy::primitives::U256::from(measured),
            terms.gas_buffer_bps,
            terms.fixed_gas_buffer,
        )
        .unwrap()
        .to::<u128>()
    }

    /// The relay's two estimate calls replayed at the parent block of the
    /// 2026-10-02 send (archive state): simulateValidation answered
    /// preOpGas 61,906 and eth_estimateGas of the execution 76,669. The
    /// mined bundle used 146,824 gas, which the executor now bills as
    /// 198,848; the estimate's settlementGas is 208,070 — 4.6% above, never
    /// below — while the limits it returns stay estimate-based.
    #[test]
    fn the_settlement_gas_of_a_mined_ethereum_send_covers_what_the_executor_billed() {
        let plan = super::plan(
            1,
            estimation_request(
                "0x88cca0eedbf2c4426110bbfc998f048689266894",
                "0x1",
                None,
                SEND_OF_2026_10_02_CALL_DATA,
            ),
            ENTRY_POINT,
            None,
        )
        .unwrap();
        assert!(plan.deployed_execution_params().is_none());
        let estimate = super::finish(
            &plan,
            &json!(MAINNET_VALIDATION_DEPLOYED),
            super::CallGasSource::Estimated(json!("0x12b7d")),
            &crate::cost::BillingTerms::default(),
        )
        .unwrap()
        .estimate;
        assert_eq!(quantity_of(&estimate.verification_gas_limit), 100_000);
        assert_eq!(quantity_of(&estimate.call_gas_limit), 115_004);
        assert_eq!(quantity_of(&estimate.pre_verification_gas), 101_692);
        let settlement = quantity_of(estimate.settlement_gas.as_deref().unwrap());
        assert_eq!(settlement, 208_070);
        assert_eq!(executor_bills(146_824), 198_848);
        assert!(settlement >= executor_bills(146_824));
        // It prices the gas used, not the limits: 316,696 of them.
        assert!(settlement < 100_000 + 115_004 + 101_692);
        // And it is the same rule over any buffer the operator configures.
        let unbuffered = super::finish(
            &plan,
            &json!(MAINNET_VALIDATION_DEPLOYED),
            super::CallGasSource::Estimated(json!("0x12b7d")),
            &crate::cost::BillingTerms {
                gas_buffer_bps: 0,
                fixed_gas_buffer: 0,
                ..crate::cost::BillingTerms::default()
            },
        )
        .unwrap()
        .estimate;
        // 21,000 + 9,132 (handleOps calldata) + 61,906 + 52,805 + 10,000.
        assert_eq!(
            quantity_of(unbuffered.settlement_gas.as_deref().unwrap()),
            154_843
        );
    }

    /// A counterfactual Safe's first operation that registered a passkey
    /// backup (tx 0x86795d08…, 5,034,866 gas). `eth_estimateGas` of a call to
    /// an address with no code measures a plain transfer, so the execution is
    /// simulated with the Safe deployed first; replayed at the parent block,
    /// that answered 4,374,542 gas for the execution and simulateValidation
    /// 642,246 for validation and deployment.
    #[test]
    fn an_undeployed_safe_is_measured_with_its_code_in_place() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "testdata/registry_backup_first_operation.json"
        ))
        .unwrap();
        let text = |key: &str| fixture[key].as_str().unwrap().to_owned();
        let request = || {
            estimation_request(
                &text("sender"),
                "0x0",
                Some((&text("factory"), &text("factoryData"))),
                &text("callData"),
            )
        };
        let plan = super::plan(1, request(), ENTRY_POINT, None).unwrap();
        let params = plan.deployed_execution_params().unwrap();
        let calls = &params[0]["blockStateCalls"][0]["calls"];
        assert_eq!(calls[0]["from"], json!(super::SENDER_CREATOR_V07));
        assert_eq!(calls[0]["to"], json!(text("factory")));
        assert_eq!(calls[0]["data"], json!(text("factoryData")));
        assert_eq!(calls[1]["from"], json!(ENTRY_POINT));
        assert_eq!(calls[1]["data"], json!(text("callData")));
        // Avalanche has no eth_simulateV1: its estimate keeps eth_estimateGas.
        assert!(
            super::plan(43_114, request(), ENTRY_POINT, None)
                .unwrap()
                .deployed_execution_params()
                .is_none()
        );

        let simulated = json!([{ "calls": [
            { "status": "0x1", "gasUsed": "0x5a1c6", "returnData": "0x", "logs": [] },
            { "status": "0x1", "gasUsed": format!("0x{:x}", 4_374_542), "returnData": "0x", "logs": [] },
        ] }]);
        let estimate = super::finish(
            &plan,
            &validation_answer(642_246),
            super::CallGasSource::Simulated(simulated),
            &crate::cost::BillingTerms::default(),
        )
        .unwrap()
        .estimate;
        // The limits: 1.5 × each measurement, so the operation cannot run out.
        assert_eq!(quantity_of(&estimate.verification_gas_limit), 963_369);
        assert_eq!(quantity_of(&estimate.call_gas_limit), 6_561_813);
        let settlement = quantity_of(estimate.settlement_gas.as_deref().unwrap());
        assert_eq!(settlement, 5_831_838);
        assert!(settlement >= executor_bills(5_034_866));
        assert!(settlement * 1_000 <= executor_bills(5_034_866) * 1_005);

        // Measured as a plain transfer, the same operation names no
        // settlementGas: the wallet prices the limits instead.
        let blind = super::finish(
            &plan,
            &validation_answer(642_246),
            super::CallGasSource::Estimated(json!("0x7530")),
            &crate::cost::BillingTerms::default(),
        )
        .unwrap()
        .estimate;
        assert_eq!(blind.settlement_gas, None);
    }

    #[test]
    fn settlement_gas_is_returned_only_where_the_executor_bills_measured_gas() {
        let estimate_on = |chain_id: u64, call_gas: super::CallGasSource| {
            let plan = super::plan(
                chain_id,
                estimation_request(
                    "0x88cca0eedbf2c4426110bbfc998f048689266894",
                    "0x1",
                    None,
                    SEND_OF_2026_10_02_CALL_DATA,
                ),
                ENTRY_POINT,
                None,
            )
            .unwrap();
            super::finish(
                &plan,
                &json!(MAINNET_VALIDATION_DEPLOYED),
                call_gas,
                &crate::cost::BillingTerms::default(),
            )
            .unwrap()
        };
        let estimated = || super::CallGasSource::Estimated(json!("0x12b7d"));
        for chain_id in [1, 11_155_111, 100, 137, 56] {
            assert!(
                estimate_on(chain_id, estimated())
                    .estimate
                    .settlement_gas
                    .is_some()
            );
        }
        // Arbitrum, Base, Optimism, Avalanche, an unlisted chain: the
        // executor bills the outer limit there, so there is nothing smaller
        // to promise.
        for chain_id in [42_161, 8_453, 10, 43_114, 123_456_789] {
            assert_eq!(
                estimate_on(chain_id, estimated()).estimate.settlement_gas,
                None
            );
        }
        // Nothing measured, nothing promised — and the response omits the field.
        let fallback = estimate_on(1, super::CallGasSource::Unavailable);
        assert_eq!(fallback.estimate.settlement_gas, None);
        assert!(
            serde_json::to_value(&fallback.estimate)
                .unwrap()
                .get("settlementGas")
                .is_none()
        );
        let measured = estimate_on(1, estimated());
        assert_eq!(
            serde_json::to_value(&measured.estimate).unwrap()["settlementGas"],
            json!(format!("0x{:x}", 208_070))
        );
    }

    #[test]
    fn a_simulated_deployment_that_fails_or_an_execution_that_reverts_is_no_estimate() {
        let plan = super::plan(
            1,
            estimation_request(
                "0x88cca0eedbf2c4426110bbfc998f048689266894",
                "0x0",
                Some(("0x4e1dcf7ad4e460cfd30791ccc4f9c8a4f820ec67", "0x1688f0b9")),
                SEND_OF_2026_10_02_CALL_DATA,
            ),
            ENTRY_POINT,
            None,
        )
        .unwrap();
        let finish = |answer: serde_json::Value| {
            super::finish(
                &plan,
                &json!(MAINNET_VALIDATION_UNDEPLOYED),
                super::CallGasSource::Simulated(answer),
                &crate::cost::BillingTerms::default(),
            )
        };
        let failed_deployment = finish(json!([{ "calls": [
            { "status": "0x0", "gasUsed": "0x5208" },
            { "status": "0x1", "gasUsed": "0x5208" },
        ] }]));
        assert_eq!(
            failed_deployment.err().unwrap().code,
            super::RpcError::estimation_unavailable().code
        );
        let reverted = finish(json!([{ "calls": [
            { "status": "0x1", "gasUsed": "0x5208" },
            { "status": "0x0", "gasUsed": "0x5208", "returnData": "0xacfdb444",
              "error": { "code": 3, "message": "execution reverted" } },
        ] }]))
        .err()
        .unwrap();
        assert_eq!(
            reverted.data,
            Some(json!(
                "Safe execution failed: the target call in executeUserOp reverted"
            ))
        );
    }

    #[test]
    fn decodes_entry_point_failed_op_reasons() {
        let error = SimulationRevert {
            code: Some(3),
            message: "execution reverted".into(),
            data: Some(json!({
                "originalError": {
                    "data": "0x220266b600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000001a4141323520696e76616c6964206163636f756e74206e6f6e6365000000000000"
                }
            })),
        };

        assert_eq!(
            revert_reason(&error),
            Some("FailedOp(0): AA25 invalid account nonce".into())
        );
    }

    #[test]
    fn extracts_revert_data_embedded_in_the_rpc_error_message() {
        let error = SimulationRevert {
            code: Some(3),
            message: "execution reverted: 0x4e487b710000000000000000000000000000000000000000000000000000000000000011".into(),
            data: None,
        };

        assert_eq!(
            revert_reason(&error),
            Some("Solidity panic 0x11: arithmetic overflow or underflow".into())
        );
    }

    #[test]
    fn identifies_safe_execution_failed() {
        let error = SimulationRevert {
            code: Some(3),
            message: "execution reverted".into(),
            data: Some(json!("0xacfdb444")),
        };

        assert_eq!(
            revert_reason(&error),
            Some("Safe execution failed: the target call in executeUserOp reverted".into())
        );
    }

    #[test]
    fn pads_gas_limits_upward() {
        assert_eq!(with_percent_buffer(101, 150), 152);
        assert_eq!(
            quantity_word("0x1234", "nonce").unwrap()[30..],
            [0x12, 0x34]
        );
    }
}
