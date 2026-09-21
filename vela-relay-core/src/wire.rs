//! The frozen JSON-RPC wire vocabulary, shared by every shell.
//!
//! Response bytes ARE contract (`specs/001-crux-core-split/contracts/
//! external-api.md`): both the docker shell and the Cloudflare shell parse
//! requests and render responses through this module, so the two deployments
//! cannot drift apart byte-wise (spec 002, FR-002). The module is pure serde —
//! no transport, no IO. Moved from the docker shell's `src/app/rpc/types.rs`
//! plus the envelope validation that lived in `src/app/rpc/mod.rs`; the shell
//! re-exports everything under its historical paths.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::gas_math::SubmissionTier;
pub use crate::task::{
    Address, Eip7702Authorization, HexData, Quantity, TransactionHash, UserOperation,
};

pub type BlockHash = String;
pub type UserOperationHash = String;

#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default = "empty_params")]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct RpcResponse<T> {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl<T> RpcResponse<T> {
    pub fn result(id: Value, result: T) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn parse_error(details: String) -> Self {
        Self::new(-32700, "parse error", Some(Value::String(details)))
    }

    pub fn invalid_request(details: impl Into<String>) -> Self {
        Self::new(
            -32600,
            "invalid request",
            Some(Value::String(details.into())),
        )
    }

    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self::new(
            -32601,
            "method not found",
            Some(Value::String(method.into())),
        )
    }

    pub fn invalid_params(details: impl Into<String>) -> Self {
        Self::new(
            -32602,
            "invalid params",
            Some(Value::String(details.into())),
        )
    }

    pub fn backend_unavailable() -> Self {
        Self::new(-32000, "bundler backend is not configured", None)
    }

    pub fn gas_price_unavailable() -> Self {
        Self::new(-32000, "all gas price RPC sources failed", None)
    }

    pub fn gas_price_timeout() -> Self {
        Self::new(-32000, "gas price RPC request timed out", None)
    }

    pub fn in_band_gas_quote_unavailable() -> Self {
        Self::new(-32000, "in-band gas quote is temporarily unavailable", None)
    }

    pub fn user_operation_queue_unavailable() -> Self {
        Self::new(
            -32000,
            "UserOperation queue is temporarily unavailable",
            None,
        )
    }

    pub fn user_operation_status_store_unavailable() -> Self {
        Self::new(
            -32000,
            "UserOperation status store is temporarily unavailable",
            None,
        )
    }

    pub fn user_operation_rejected(details: impl Into<String>) -> Self {
        Self::new(
            -32500,
            "UserOperation simulation failed",
            Some(Value::String(details.into())),
        )
    }

    pub fn estimation_unavailable() -> Self {
        Self::new(
            -32000,
            "UserOperation simulation is temporarily unavailable",
            None,
        )
    }

    pub fn mempool_full() -> Self {
        Self::new(-32000, "bundler mempool is full", None)
    }

    fn new(code: i32, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}

/// Parse and version-check one request envelope. `Err` carries the exact
/// error response the old shell rendered inline (parse error with a null id;
/// version refusal echoing the request id).
#[allow(
    clippy::result_large_err,
    reason = "The Err IS the rendered refusal response; envelope handling happens once per request and the value is immediately serialized, so boxing would add noise for no measurable win."
)]
pub fn parse_envelope(body: &[u8]) -> Result<RpcRequest, RpcResponse<Value>> {
    let request = serde_json::from_slice::<RpcRequest>(body).map_err(|error| {
        RpcResponse::error(Value::Null, RpcError::parse_error(error.to_string()))
    })?;

    if request.jsonrpc != "2.0" {
        return Err(RpcResponse::error(
            request.id,
            RpcError::invalid_request("`jsonrpc` must be \"2.0\""),
        ));
    }

    Ok(request)
}

#[derive(Clone, Copy, Debug)]
pub enum RpcMethod {
    SendUserOperation,
    EstimateUserOperationGas,
    GetUserOperationReceipt,
    GetUserOperationByHash,
    SupportedEntryPoints,
    GetUserOperationGasPrice,
    GetUserOperationStatus,
    GetInBandGasQuote,
}

impl RpcMethod {
    pub fn parse(value: &str) -> Result<Self, RpcError> {
        match value {
            "eth_sendUserOperation" => Ok(Self::SendUserOperation),
            "eth_estimateUserOperationGas" => Ok(Self::EstimateUserOperationGas),
            "eth_getUserOperationReceipt" => Ok(Self::GetUserOperationReceipt),
            "eth_getUserOperationByHash" => Ok(Self::GetUserOperationByHash),
            "eth_supportedEntryPoints" => Ok(Self::SupportedEntryPoints),
            "pimlico_getUserOperationGasPrice" => Ok(Self::GetUserOperationGasPrice),
            "pimlico_getUserOperationStatus" => Ok(Self::GetUserOperationStatus),
            "vela_getInBandGasQuote" => Ok(Self::GetInBandGasQuote),
            _ => Err(RpcError::method_not_found(value)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SendUserOperation => "eth_sendUserOperation",
            Self::EstimateUserOperationGas => "eth_estimateUserOperationGas",
            Self::GetUserOperationReceipt => "eth_getUserOperationReceipt",
            Self::GetUserOperationByHash => "eth_getUserOperationByHash",
            Self::SupportedEntryPoints => "eth_supportedEntryPoints",
            Self::GetUserOperationGasPrice => "pimlico_getUserOperationGasPrice",
            Self::GetUserOperationStatus => "pimlico_getUserOperationStatus",
            Self::GetInBandGasQuote => "vela_getInBandGasQuote",
        }
    }
}

/// Method + params validation exactly as the old shell's `validate_call`:
/// the method must be known and its params must deserialize (send/estimate/
/// status/byHash/receipt/quote) or be an empty array (supportedEntryPoints,
/// gasPrice), all BEFORE any handler runs.
pub fn validate_call(method: &str, params: Value) -> Result<RpcMethod, RpcError> {
    let method = RpcMethod::parse(method)?;

    match method {
        RpcMethod::SendUserOperation => parse_params::<SendUserOperationParams>(params)?,
        RpcMethod::EstimateUserOperationGas => {
            parse_params::<EstimateUserOperationGasParams>(params)?;
        }
        RpcMethod::GetUserOperationReceipt => {
            parse_params::<GetUserOperationReceiptParams>(params)?;
        }
        RpcMethod::GetUserOperationByHash => {
            parse_params::<GetUserOperationByHashParams>(params)?;
        }
        RpcMethod::SupportedEntryPoints | RpcMethod::GetUserOperationGasPrice => {
            validate_empty_params(params)?;
        }
        RpcMethod::GetUserOperationStatus => {
            parse_params::<GetUserOperationStatusParams>(params)?;
        }
        RpcMethod::GetInBandGasQuote => {
            parse_params::<GetInBandGasQuoteParams>(params)?;
        }
    }

    Ok(method)
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<(), RpcError> {
    serde_json::from_value::<T>(params)
        .map(|_| ())
        .map_err(|error| RpcError::invalid_params(error.to_string()))
}

fn validate_empty_params(params: Value) -> Result<(), RpcError> {
    match params {
        Value::Array(values) if values.is_empty() => Ok(()),
        _ => Err(RpcError::invalid_params("expected an empty parameter list")),
    }
}

pub trait RpcMethodSpec {
    type Params;
    type Result;

    const METHOD: &'static str;
}

pub struct SendUserOperation;

impl RpcMethodSpec for SendUserOperation {
    type Params = SendUserOperationParams;
    type Result = UserOperationHash;

    const METHOD: &'static str = "eth_sendUserOperation";
}

pub struct EstimateUserOperationGas;

impl RpcMethodSpec for EstimateUserOperationGas {
    type Params = EstimateUserOperationGasParams;
    type Result = UserOperationGasEstimate;

    const METHOD: &'static str = "eth_estimateUserOperationGas";
}

pub struct GetUserOperationReceipt;

impl RpcMethodSpec for GetUserOperationReceipt {
    type Params = GetUserOperationReceiptParams;
    type Result = Option<UserOperationReceipt>;

    const METHOD: &'static str = "eth_getUserOperationReceipt";
}

pub struct GetUserOperationByHash;

impl RpcMethodSpec for GetUserOperationByHash {
    type Params = GetUserOperationByHashParams;
    type Result = Option<UserOperationByHash>;

    const METHOD: &'static str = "eth_getUserOperationByHash";
}

pub struct SupportedEntryPoints;

impl RpcMethodSpec for SupportedEntryPoints {
    type Params = NoParams;
    type Result = Vec<Address>;

    const METHOD: &'static str = "eth_supportedEntryPoints";
}

pub struct GetUserOperationGasPrice;

impl RpcMethodSpec for GetUserOperationGasPrice {
    type Params = NoParams;
    type Result = UserOperationGasPrice;

    const METHOD: &'static str = "pimlico_getUserOperationGasPrice";
}

pub struct GetUserOperationStatus;

impl RpcMethodSpec for GetUserOperationStatus {
    type Params = GetUserOperationStatusParams;
    type Result = UserOperationStatus;

    const METHOD: &'static str = "pimlico_getUserOperationStatus";
}

pub struct GetInBandGasQuote;

impl RpcMethodSpec for GetInBandGasQuote {
    type Params = GetInBandGasQuoteParams;
    type Result = Vec<InBandGasQuote>;

    const METHOD: &'static str = "vela_getInBandGasQuote";
}

pub struct NoParams;

/// `[userOperation, entryPoint]`, plus an optional third element naming the
/// submission speed (`"slow" | "standard" | "fast"`).
///
/// The third element is a NAME, never a wei amount: the relay resolves it
/// against the base fee it reads at submit time, so a quote that went stale
/// between signing and inclusion can never set the price. Omitting it is the
/// whole of today's wire and keeps today's behaviour exactly; a name this
/// relay does not know fails `validate_call` with `invalid params` before any
/// handler runs, rather than silently becoming a speed nobody asked for.
#[derive(Debug, Deserialize)]
pub struct SendUserOperationParams(
    pub UserOperation,
    pub Address,
    #[serde(default)] pub Option<SubmissionTier>,
);

#[derive(Clone, Debug, Deserialize)]
pub struct EstimateUserOperationGasParams(
    pub EstimatableUserOperation,
    pub Address,
    #[serde(default)] pub Option<StateOverrideSet>,
);

#[derive(Debug, Deserialize)]
pub struct GetUserOperationReceiptParams(pub [UserOperationHash; 1]);

#[derive(Debug, Deserialize)]
pub struct GetUserOperationByHashParams(pub [UserOperationHash; 1]);

#[derive(Debug, Deserialize)]
pub struct GetUserOperationStatusParams(pub [UserOperationHash; 1]);

#[derive(Debug, Deserialize)]
pub struct GetInBandGasQuoteParams(pub [InBandGasQuoteRequest; 1]);

impl GetInBandGasQuoteParams {
    pub fn safe_address(self) -> Address {
        self.0
            .into_iter()
            .next()
            .expect("a one-element params array always contains a request")
            .safe_address
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InBandGasQuoteRequest {
    pub safe_address: Address,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum EstimatableUserOperation {
    V0_7(Box<EstimatableUserOperationV0_7>),
    V0_6(Box<EstimatableUserOperationV0_6>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EstimatableUserOperationV0_7 {
    pub sender: Address,
    pub nonce: Quantity,
    pub factory: Option<Address>,
    pub factory_data: Option<HexData>,
    pub call_data: HexData,
    pub call_gas_limit: Option<Quantity>,
    pub verification_gas_limit: Option<Quantity>,
    pub pre_verification_gas: Option<Quantity>,
    pub max_fee_per_gas: Option<Quantity>,
    pub max_priority_fee_per_gas: Option<Quantity>,
    pub paymaster: Option<Address>,
    pub paymaster_verification_gas_limit: Option<Quantity>,
    pub paymaster_post_op_gas_limit: Option<Quantity>,
    pub paymaster_data: Option<HexData>,
    pub signature: Option<HexData>,
    pub eip7702_auth: Option<Eip7702Authorization>,
    /// Accepted during Tempo gas estimation so clients can use the same request shape as submit.
    #[serde(default)]
    pub fee_token: Option<Address>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EstimatableUserOperationV0_6 {
    pub sender: Address,
    pub nonce: Quantity,
    pub init_code: HexData,
    pub call_data: HexData,
    pub call_gas_limit: Option<Quantity>,
    pub verification_gas_limit: Option<Quantity>,
    pub pre_verification_gas: Option<Quantity>,
    pub max_fee_per_gas: Option<Quantity>,
    pub max_priority_fee_per_gas: Option<Quantity>,
    pub paymaster_and_data: Option<HexData>,
    pub signature: Option<HexData>,
}

pub type StateOverrideSet = BTreeMap<Address, StateOverride>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateOverride {
    pub balance: Option<Quantity>,
    pub nonce: Option<Quantity>,
    pub code: Option<HexData>,
    pub state: Option<BTreeMap<HexData, HexData>>,
    pub state_diff: Option<BTreeMap<HexData, HexData>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationGasEstimate {
    pub pre_verification_gas: Quantity,
    pub verification_gas_limit: Quantity,
    pub call_gas_limit: Quantity,
    pub paymaster_verification_gas_limit: Quantity,
    pub paymaster_post_op_gas_limit: Quantity,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationByHash {
    pub user_operation: UserOperation,
    pub entry_point: Address,
    pub block_number: Option<Quantity>,
    pub block_hash: Option<BlockHash>,
    pub transaction_hash: Option<TransactionHash>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationReceipt {
    pub user_op_hash: UserOperationHash,
    pub entry_point: Address,
    pub sender: Address,
    pub nonce: Quantity,
    pub paymaster: Option<Address>,
    pub actual_gas_cost: Quantity,
    pub actual_gas_used: Quantity,
    pub success: bool,
    pub reason: HexData,
    pub logs: Vec<Log>,
    pub receipt: TransactionReceipt,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Log {
    pub address: Address,
    pub topics: Vec<HexData>,
    pub data: HexData,
    pub block_number: Option<Quantity>,
    pub transaction_hash: Option<TransactionHash>,
    pub transaction_index: Option<Quantity>,
    pub block_hash: Option<BlockHash>,
    pub log_index: Option<Quantity>,
    pub removed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionReceipt {
    pub transaction_hash: TransactionHash,
    pub transaction_index: Quantity,
    pub block_hash: Option<BlockHash>,
    pub block_number: Option<Quantity>,
    pub from: Address,
    pub to: Option<Address>,
    pub cumulative_gas_used: Quantity,
    pub gas_used: Quantity,
    pub contract_address: Option<Address>,
    pub logs: Vec<Log>,
    pub logs_bloom: HexData,
    pub status: Option<Quantity>,
    pub effective_gas_price: Option<Quantity>,
    #[serde(rename = "type")]
    pub transaction_type: Option<Quantity>,
}

#[derive(Debug, Serialize)]
pub struct UserOperationGasPrice {
    pub slow: GasPriceTier,
    pub standard: GasPriceTier,
    pub fast: GasPriceTier,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InBandGasQuote {
    pub recipient: Address,
    pub asset: InBandGasQuoteAsset,
    pub fee_token: Option<Address>,
    pub decimals: u32,
    pub symbol: String,
    pub balance: Quantity,
    pub usd_price: Option<String>,
    pub usd_balance: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InBandGasQuoteAsset {
    Native,
    Erc20,
}

/// One tier of `pimlico_getUserOperationGasPrice`. The last two fields are
/// the Vela extension the in-band fee contract is built on
/// (`docs/fees.md` §2b): a generic bundler omits them, this relay never does.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GasPriceTier {
    /// The cap the relay will submit this tier at.
    pub max_fee_per_gas: Quantity,
    pub max_priority_fee_per_gas: Quantity,
    /// `R`: what the client's in-band reimbursement must be priced against.
    /// Absent, vela-core silently falls back to its own chain measurement and
    /// every tier costs the same — the defect this field exists to close.
    pub network_fee_per_gas: Quantity,
    /// `maxFeePerGas − networkFeePerGas`, the inclusion headroom above the
    /// reimbursement basis. Reported so no client has to infer it.
    pub relayer_fee_per_gas: Quantity,
}

pub use crate::task::UserOperationStatus as UserOperationStatusKind;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserOperationStatus {
    pub status: UserOperationStatusKind,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: Option<TransactionHash>,
    /// The executor stage that last deferred or locally rejected the operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_executor_stage: Option<String>,
    /// A bounded, operator-safe reason for the last deferred or rejected executor attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_executor_error: Option<String>,
    /// Unix timestamp in milliseconds for the last deferred or rejected executor attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_executor_attempt_at_ms: Option<u64>,
}

fn empty_params() -> Value {
    Value::Array(Vec::new())
}

/// The status read model: which stored fields the status RPC exposes.
/// Executor diagnostics are shown only while the operation is still pending
/// or locally rejected — never once it is on-chain.
pub fn rpc_status(record: &crate::task::StoredUserOperation) -> UserOperationStatus {
    let exposes_executor_diagnostic = matches!(
        record.status,
        UserOperationStatusKind::Queued
            | UserOperationStatusKind::NotSubmitted
            | UserOperationStatusKind::Rejected
    );
    UserOperationStatus {
        status: record.status,
        transaction_hash: record.transaction_hash.clone(),
        last_executor_stage: exposes_executor_diagnostic
            .then(|| record.last_executor_stage.clone())
            .flatten(),
        last_executor_error: exposes_executor_diagnostic
            .then(|| record.last_executor_error.clone())
            .flatten(),
        last_executor_attempt_at_ms: exposes_executor_diagnostic
            .then_some(record.last_executor_attempt_at_ms)
            .flatten(),
    }
}

/// The ERC-7769 receipt read model over a stored record: `Some` only for
/// `included` operations and on-chain rejections whose event reports failure,
/// both requiring the persisted event + outer receipt.
pub fn receipt_response(
    user_operation_hash: &str,
    record: &crate::task::StoredUserOperation,
) -> Option<Value> {
    let event = record.event.as_ref()?;
    let receipt = record.receipt.as_ref()?;
    match record.status {
        UserOperationStatusKind::Included => {}
        UserOperationStatusKind::Rejected if !event.success => {}
        _ => return None,
    }
    let (sender, nonce, paymaster) = match &record.user_operation {
        UserOperation::V0_7(user_operation) => (
            user_operation.sender.clone(),
            user_operation.nonce.clone(),
            user_operation.paymaster.clone(),
        ),
        UserOperation::V0_6(user_operation) => (
            user_operation.sender.clone(),
            user_operation.nonce.clone(),
            paymaster_from_v06(&user_operation.paymaster_and_data),
        ),
    };
    // ERC-7769 exposes the logs associated with the UserOperation. The persisted outer receipt is
    // the authoritative chain snapshot; returning its logs matches the existing vela-bundler
    // formatter and is strictly more useful than silently returning an empty list.
    let logs = receipt
        .get("logs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    Some(serde_json::json!({
        "userOpHash": user_operation_hash,
        "entryPoint": record.entry_point,
        "sender": sender,
        "nonce": nonce,
        "paymaster": paymaster,
        "actualGasCost": event.actual_gas_cost,
        "actualGasUsed": event.actual_gas_used,
        "success": event.success,
        "reason": "0x",
        "logs": logs,
        "receipt": receipt,
    }))
}

fn paymaster_from_v06(paymaster_and_data: &str) -> Option<String> {
    let value = paymaster_and_data.strip_prefix("0x")?;
    (value.len() >= 40).then(|| format!("0x{}", &value[..40]))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{RpcError, RpcResponse, parse_envelope, validate_call};

    fn bytes<T: serde::Serialize>(response: &RpcResponse<T>) -> String {
        serde_json::to_string(response).expect("wire responses always serialize")
    }

    // Golden vectors below are production bytes captured by the 001 replay
    // battery against the docker deployment (2026-08-28); the wire module must
    // reproduce them exactly.

    #[test]
    fn renders_a_result_envelope_byte_identically() {
        let response = RpcResponse::result(
            json!(1),
            json!(["0x0000000071727De22E5E9d8BAf0edAc6f37da032"]),
        );
        assert_eq!(
            bytes(&response),
            r#"{"jsonrpc":"2.0","id":1,"result":["0x0000000071727De22E5E9d8BAf0edAc6f37da032"]}"#
        );
    }

    #[test]
    fn refuses_an_unknown_method_byte_identically() {
        let error = validate_call("eth_chainId", json!([])).expect_err("unknown method");
        assert_eq!(
            bytes(&RpcResponse::<Value>::error(json!(2), error)),
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found","data":"eth_chainId"}}"#
        );
    }

    /// A minimal, structurally valid in-band v0.7 operation. Only the shape
    /// matters here: these tests are about the params array around it.
    fn user_operation() -> Value {
        json!({
            "sender": "0x1111111111111111111111111111111111111111",
            "nonce": "0x0",
            "factory": null,
            "factoryData": null,
            "callData": "0x",
            "callGasLimit": "0x1",
            "verificationGasLimit": "0x1",
            "preVerificationGas": "0x1",
            "maxFeePerGas": "0x0",
            "maxPriorityFeePerGas": "0x0",
            "paymaster": null,
            "paymasterVerificationGasLimit": null,
            "paymasterPostOpGasLimit": null,
            "paymasterData": null,
            "signature": "0x1234",
            "eip7702Auth": null
        })
    }

    #[test]
    fn a_send_may_name_a_submission_speed_and_every_client_today_may_omit_it() {
        use super::{SendUserOperationParams, SubmissionTier};
        let entry_point = "0x0000000071727De22E5E9d8BAf0edAc6f37da032";

        // The wire every client in the field speaks: two elements, no tier.
        // This must keep working untouched, and must read as "named nothing"
        // rather than as any default.
        let params: SendUserOperationParams =
            serde_json::from_value(json!([user_operation(), entry_point])).unwrap();
        assert_eq!(params.2, None);
        assert!(
            validate_call(
                "eth_sendUserOperation",
                json!([user_operation(), entry_point])
            )
            .is_ok()
        );

        // And the new wire: a third element naming the speed.
        for (name, tier) in [
            ("slow", SubmissionTier::Slow),
            ("standard", SubmissionTier::Standard),
            ("fast", SubmissionTier::Fast),
        ] {
            let params: SendUserOperationParams =
                serde_json::from_value(json!([user_operation(), entry_point, name])).unwrap();
            assert_eq!(params.2, Some(tier));
            assert!(
                validate_call(
                    "eth_sendUserOperation",
                    json!([user_operation(), entry_point, name])
                )
                .is_ok(),
                "{name}"
            );
        }
        // An explicit null is "named nothing", not a parse failure.
        let params: SendUserOperationParams =
            serde_json::from_value(json!([user_operation(), entry_point, null])).unwrap();
        assert_eq!(params.2, None);
    }

    #[test]
    fn refuses_a_submission_speed_this_relay_does_not_know() {
        // A name the relay cannot price must fail the request before any
        // handler runs — never fall back to a speed nobody asked for, and
        // never let the client name a wei amount instead of a speed.
        let entry_point = "0x0000000071727De22E5E9d8BAf0edAc6f37da032";
        let error = validate_call(
            "eth_sendUserOperation",
            json!([user_operation(), entry_point, "turbo"]),
        )
        .expect_err("unknown tier");
        assert_eq!(
            bytes(&RpcResponse::<Value>::error(json!(9), error)),
            r#"{"jsonrpc":"2.0","id":9,"error":{"code":-32602,"message":"invalid params","data":"unknown variant `turbo`, expected one of `slow`, `standard`, `fast`"}}"#
        );
        assert!(
            validate_call(
                "eth_sendUserOperation",
                json!([user_operation(), entry_point, 1_000_000_000u64]),
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_a_malformed_envelope_with_a_null_id() {
        let response = parse_envelope(br#"{"jsonrpc":"2.0", broken"#).expect_err("malformed");
        assert_eq!(
            bytes(&response),
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error","data":"key must be a string at line 1 column 19"}}"#
        );
    }

    #[test]
    fn refuses_a_wrong_jsonrpc_version_echoing_the_id() {
        let body = br#"{"jsonrpc":"1.0","id":3,"method":"eth_supportedEntryPoints","params":[]}"#;
        let response = parse_envelope(body).expect_err("wrong version");
        assert_eq!(
            bytes(&response),
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32600,"message":"invalid request","data":"`jsonrpc` must be \"2.0\""}}"#
        );
    }

    #[test]
    fn renders_invalid_params_and_rejection_envelopes_byte_identically() {
        let invalid = RpcResponse::<Value>::error(
            json!(4),
            RpcError::invalid_params(
                "in-band UserOperations must set maxFeePerGas and maxPriorityFeePerGas to 0x0",
            ),
        );
        assert_eq!(
            bytes(&invalid),
            r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"invalid params","data":"in-band UserOperations must set maxFeePerGas and maxPriorityFeePerGas to 0x0"}}"#
        );

        let rejected = RpcResponse::<Value>::error(
            json!(6),
            RpcError::user_operation_rejected(
                "in-band UserOperation must reimburse the settlement recipient with at least \
                 0.00001 native coin or 0.01 of an allowlisted stablecoin",
            ),
        );
        assert_eq!(
            bytes(&rejected),
            r#"{"jsonrpc":"2.0","id":6,"error":{"code":-32500,"message":"UserOperation simulation failed","data":"in-band UserOperation must reimburse the settlement recipient with at least 0.00001 native coin or 0.01 of an allowlisted stablecoin"}}"#
        );
    }

    #[test]
    fn validates_params_before_any_handler_runs() {
        assert!(validate_call("eth_supportedEntryPoints", json!([])).is_ok());
        let error = validate_call("eth_supportedEntryPoints", json!([1])).expect_err("non-empty");
        assert_eq!(
            bytes(&RpcResponse::<Value>::error(Value::Null, error)),
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32602,"message":"invalid params","data":"expected an empty parameter list"}}"#
        );
        assert!(validate_call("pimlico_getUserOperationStatus", json!(["0xab"])).is_ok());
        assert!(validate_call("pimlico_getUserOperationStatus", json!([])).is_err());
    }
}
