//! Shared business vocabulary for a relayed UserOperation.
//!
//! The shell re-exports these types under its historical paths; wire names and
//! serde shapes are frozen (see `specs/001-crux-core-split/contracts/`).

use serde::{Deserialize, Serialize};

/// Lifecycle status of a UserOperation as stored and as exposed over RPC.
///
/// `NotFound` is an API-only value: responses use it for unknown hashes, but a
/// stored record never carries it. The transition rules between the stored
/// states live in [`crate::lifecycle`], the single authoritative table.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserOperationStatus {
    NotFound,
    Queued,
    NotSubmitted,
    Submitted,
    Rejected,
    Included,
    Failed,
}

impl UserOperationStatus {
    /// A terminal status accepts same-status field merges but no transitions.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Rejected | Self::Included | Self::Failed)
    }

    /// Whether the executor may treat this operation as durably settled for
    /// consumer-offset purposes. Broader than [`Self::is_terminal`]:
    /// `Submitted` is durable (the bundle intent survives crashes) but not
    /// terminal (receipts still move it to `Included`/`Failed`).
    pub fn is_durable(self) -> bool {
        matches!(
            self,
            Self::Submitted | Self::Rejected | Self::Included | Self::Failed
        )
    }
}

// ---- Wire vocabulary (moved from the shell's RPC types and store; serde
// shapes and field names are frozen — see contracts/external-api.md) ----

pub type Address = String;
pub type HexData = String;
pub type Quantity = String;
pub type TransactionHash = String;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserOperation {
    V0_7(Box<UserOperationV0_7>),
    V0_6(Box<UserOperationV0_6>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserOperationV0_7 {
    pub sender: Address,
    pub nonce: Quantity,
    pub factory: Option<Address>,
    pub factory_data: Option<HexData>,
    pub call_data: HexData,
    pub call_gas_limit: Quantity,
    pub verification_gas_limit: Quantity,
    pub pre_verification_gas: Quantity,
    pub max_fee_per_gas: Quantity,
    pub max_priority_fee_per_gas: Quantity,
    pub paymaster: Option<Address>,
    pub paymaster_verification_gas_limit: Option<Quantity>,
    pub paymaster_post_op_gas_limit: Option<Quantity>,
    pub paymaster_data: Option<HexData>,
    pub signature: HexData,
    pub eip7702_auth: Option<Eip7702Authorization>,
    /// Tempo extension: the token used by the outer `0x76` transaction. It is deliberately
    /// outside ERC-4337's packed hash, but must survive queue persistence verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_token: Option<Address>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserOperationV0_6 {
    pub sender: Address,
    pub nonce: Quantity,
    pub init_code: HexData,
    pub call_data: HexData,
    pub call_gas_limit: Quantity,
    pub verification_gas_limit: Quantity,
    pub pre_verification_gas: Quantity,
    pub max_fee_per_gas: Quantity,
    pub max_priority_fee_per_gas: Quantity,
    pub paymaster_and_data: HexData,
    pub signature: HexData,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Eip7702Authorization {
    pub chain_id: Quantity,
    pub address: Address,
    pub nonce: Quantity,
    pub y_parity: Quantity,
    pub r: Quantity,
    pub s: Quantity,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperationEvent {
    pub user_operation_hash: String,
    pub success: bool,
    pub actual_gas_cost: String,
    pub actual_gas_used: String,
}

// ---- Queue and store vocabulary (moved from the shell; serde shapes and
// field names are frozen) ----

use serde_json::Value;

/// The validated queue envelope plus its deterministic relayer lane and Iggy position.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoutedUserOperation {
    pub schema_version: u32,
    pub user_operation_hash: String,
    pub chain_id: u64,
    pub entry_point: String,
    pub user_operation: Value,
    pub sender: String,
    pub lane: u8,
    pub stream: String,
    pub partition_id: u32,
    pub offset: u64,
    /// The submission speed the client named on `eth_sendUserOperation`, if it
    /// named one.
    ///
    /// It rides the envelope rather than the UserOperation for two reasons:
    /// the operation's serde shape is `deny_unknown_fields` ERC-4337 wire and
    /// its bytes are hashed, and the speed is a preference about how the RELAY
    /// submits, not a term of the operation the user signed. `skip_serializing`
    /// keeps an untiered envelope byte-identical to the ones already in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_tier: Option<crate::gas_math::SubmissionTier>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct QueuedUserOperation {
    pub user_operation_hash: String,
    pub chain_id: u64,
    pub entry_point: String,
    pub user_operation: UserOperation,
}

/// Redis-backed lifecycle state for an accepted UserOperation.
///
/// `admitted` is an internal two-phase marker: a `queued` record is created before the Iggy
/// append, then marked admitted only after Iggy acknowledges it. It is never exposed via RPC.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredUserOperation {
    pub status: UserOperationStatus,
    pub transaction_hash: Option<TransactionHash>,
    pub chain_id: u64,
    /// Decimal text used by Redis Lua because cjson numbers cannot represent every u64 exactly.
    #[serde(default)]
    pub chain_id_text: String,
    pub entry_point: String,
    pub user_operation: UserOperation,
    pub admitted: bool,
    #[serde(default)]
    pub next_receipt_check_at_ms: u64,
    pub block_hash: Option<String>,
    pub block_number: Option<String>,
    pub receipt: Option<Value>,
    pub event: Option<UserOperationEvent>,
    /// The last executor diagnostic. It explains either a pending retry or a terminal local
    /// rejection (for example an insufficient in-band reimbursement).
    #[serde(default)]
    pub last_executor_stage: Option<String>,
    #[serde(default)]
    pub last_executor_error: Option<String>,
    #[serde(default)]
    pub last_executor_attempt_at_ms: Option<u64>,
}

/// Bounds an operator-facing diagnostic before it is stored: external error
/// bodies are untrusted input and clients poll the record directly. Shared by
/// both shells (docker `truncate_diagnostic` moved here, spec 002).
pub fn truncate_diagnostic(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .take_while(|(index, character)| {
            index.saturating_add(character.len_utf8()) <= limit.saturating_sub(3)
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    format!("{}...", &value[..end])
}

/// The initial stored record for a freshly queued operation — the shape both
/// shells' create-if-absent writes persist (docker Redis SETNX, RecordDO
/// put-if-absent). Frozen alongside the record shape itself.
pub fn queued_record(operation: QueuedUserOperation, admitted: bool) -> StoredUserOperation {
    StoredUserOperation {
        status: UserOperationStatus::Queued,
        transaction_hash: None,
        chain_id: operation.chain_id,
        chain_id_text: operation.chain_id.to_string(),
        entry_point: operation.entry_point,
        user_operation: operation.user_operation,
        admitted,
        next_receipt_check_at_ms: 0,
        block_hash: None,
        block_number: None,
        receipt: None,
        event: None,
        last_executor_stage: None,
        last_executor_error: None,
        last_executor_attempt_at_ms: None,
    }
}

/// A fully signed outer transaction persisted before its first broadcast.
///
/// One intent may exist for a `(chain_id, lane)` pair. If a worker dies after broadcasting but
/// before updating UserOperation status, its successor loads and rebroadcasts this exact byte
/// sequence instead of allocating another nonce.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedBundleIntent {
    pub chain_id: u64,
    pub lane: u8,
    pub entry_point: String,
    pub raw_transaction: String,
    pub transaction_hash: String,
    pub nonce: u64,
    pub user_operation_hashes: Vec<String>,
}

/// A signed treasury transfer persisted before broadcast. Only one funding transaction may be
/// outstanding per chain, which serializes the treasury nonce across all relayer lanes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedFundingIntent {
    pub chain_id: u64,
    pub relayer: String,
    pub amount_wei: u128,
    pub raw_transaction: String,
    pub transaction_hash: String,
    pub nonce: u64,
}
