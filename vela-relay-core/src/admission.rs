//! The two-phase UserOperation admission program.
//!
//! One `Core<AdmissionApp>` drives one `eth_sendUserOperation` request:
//! validation (structure, zero in-band fees, minimum reimbursement), the
//! durable Redis record, the Iggy append, and the admitted mark. The shell
//! executes storage/queue/directory operations and renders the settled
//! outcome to a JSON-RPC response. All refusal messages are byte-frozen wire
//! contract.
//!
//! Crash-window policy is deliberate and unchanged: a queue failure after the
//! record was created keeps the unadmitted record for recovery — no operation
//! exists to delete an admission.

use alloy::primitives::keccak256;
use crux_core::{App, Command, macros::effect};
use serde_json::{Value, json};

use crate::{
    broadcast::parse_hex_bytes,
    settlement::{MIN_NATIVE_FRACTION_DECIMALS, minimum_amount},
    task::{QueuedUserOperation, StoredUserOperation, UserOperation, UserOperationV0_7},
    tempo,
};

/// EntryPoint contracts this relay accepts (v0.7).
pub const SUPPORTED_ENTRY_POINTS: &[&str] = &["0x0000000071727De22E5E9d8BAf0edAc6f37da032"];

pub fn entry_point_is_supported(entry_point: &str) -> bool {
    SUPPORTED_ENTRY_POINTS
        .iter()
        .any(|supported| supported.eq_ignore_ascii_case(entry_point))
}

const ZERO_GAS_FEE: u128 = 0;

pub const CONFLICT_MESSAGE: &str = "this UserOperation hash is already queued with a different operation payload; resubmit the original operation fields or wait for the existing record to reach a durable outcome";

#[derive(Debug, PartialEq)]
pub enum AdmissionOperation {
    /// Chain-directory settlement assets (native decimals + stablecoin list).
    LoadSettlementAssets,
    FetchTokenDecimals {
        token: String,
    },
    CreateQueued {
        operation: QueuedUserOperation,
    },
    LoadExisting {
        hash: String,
    },
    /// `retry` marks the re-append of an incomplete earlier admission.
    Enqueue {
        envelope: Value,
        retry: bool,
    },
    MarkAdmitted {
        hash: String,
    },
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "Shell results are constructed once and consumed immediately in-process; boxing the record-bearing variant would add noise at each decision site."
)]
pub enum AdmissionResult {
    Assets {
        native_decimals: u32,
        stablecoins: Vec<String>,
    },
    AssetsUnavailable,
    Decimals {
        decimals: u32,
    },
    DecimalsUnavailable,
    Created {
        created: bool,
    },
    Record {
        record: Option<StoredUserOperation>,
    },
    Enqueued,
    QueueUnavailable,
    Marked {
        marked: bool,
    },
    StoreFailed,
}

impl crux_core::capability::Operation for AdmissionOperation {
    type Output = AdmissionResult;
}

#[effect]
pub enum AdmissionEffect {
    Work(AdmissionOperation),
}

/// The settled outcome; the shell renders it to today's exact JSON-RPC
/// responses and logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionOutcome {
    Accepted {
        user_operation_hash: String,
        sender_hex: String,
        /// Echoed for the shell's accepted log line, verbatim from the request.
        entry_point: String,
    },
    /// Idempotent duplicate: the record already reached the durable queue.
    AlreadyQueued {
        user_operation_hash: String,
    },
    Conflict {
        user_operation_hash: String,
        existing_chain_id: u64,
        existing_entry_point: String,
    },
    Invalid {
        message: String,
    },
    Rejected {
        message: String,
    },
    EstimationUnavailable,
    BackendUnavailable,
    StoreUnavailable,
    QueueUnavailable,
}

#[derive(Debug)]
pub enum AdmissionEvent {
    /// The shell supplies the chain context and derived settlement recipient
    /// so the program stays deterministic.
    Submit(Box<SubmitRequest>),
    Settled(AdmissionOutcome),
}

#[derive(Debug)]
pub struct SubmitRequest {
    pub chain_id: u64,
    pub entry_point: String,
    pub user_operation: UserOperation,
    pub settlement_recipient: Option<String>,
    /// The submission speed named by the optional third RPC parameter. It is
    /// not part of what the user signed, so it never touches the userOpHash,
    /// the admission fingerprint or the durable record — it rides the queue
    /// envelope to the executor, which is the only thing that acts on it.
    pub submission_tier: Option<crate::gas_math::SubmissionTier>,
}

#[derive(Default)]
pub struct AdmissionModel {
    outcome: Option<AdmissionOutcome>,
}

pub struct AdmissionViewModel {
    pub outcome: Option<AdmissionOutcome>,
}

#[derive(Default)]
pub struct AdmissionApp;

impl App for AdmissionApp {
    type Event = AdmissionEvent;
    type Model = AdmissionModel;
    type ViewModel = AdmissionViewModel;
    type Effect = AdmissionEffect;

    fn update(
        &self,
        event: Self::Event,
        model: &mut Self::Model,
    ) -> Command<Self::Effect, Self::Event> {
        match event {
            AdmissionEvent::Submit(request) => Command::new(|ctx| async move {
                let outcome = match drive_admission(&ctx, *request).await {
                    Ok(outcome) | Err(outcome) => outcome,
                };
                ctx.send_event(AdmissionEvent::Settled(outcome));
            }),
            AdmissionEvent::Settled(outcome) => {
                model.outcome = Some(outcome);
                Command::done()
            }
        }
    }

    fn view(&self, model: &Self::Model) -> Self::ViewModel {
        AdmissionViewModel {
            outcome: model.outcome.clone(),
        }
    }
}

type Ctx = crux_core::command::CommandContext<AdmissionEffect, AdmissionEvent>;
type Flow<T> = Result<T, AdmissionOutcome>;

async fn request(ctx: &Ctx, operation: AdmissionOperation) -> AdmissionResult {
    ctx.request_from_shell(operation).await
}

/// The whole admission program; `Err` is the early-settlement channel.
async fn drive_admission(ctx: &Ctx, submit: SubmitRequest) -> Flow<AdmissionOutcome> {
    let SubmitRequest {
        chain_id,
        entry_point,
        user_operation,
        settlement_recipient,
        submission_tier,
    } = submit;

    if !entry_point_is_supported(&entry_point) {
        return Err(AdmissionOutcome::Invalid {
            message: "unsupported EntryPoint".into(),
        });
    }
    let prepared = PreparedUserOperation::try_from(user_operation)
        .map_err(|message| AdmissionOutcome::Invalid { message })?;
    validate_in_band_submission(ctx, chain_id, settlement_recipient, &prepared.operation).await?;

    let entry_point_address = parse_address_field(&entry_point, "entryPoint")
        .map_err(|message| AdmissionOutcome::Invalid { message })?;
    let user_operation_hash = prepared.user_operation_hash(entry_point_address, chain_id);

    let created = match request(
        ctx,
        AdmissionOperation::CreateQueued {
            operation: QueuedUserOperation {
                user_operation_hash: user_operation_hash.clone(),
                chain_id,
                entry_point: entry_point.clone(),
                user_operation: prepared.operation.clone(),
            },
        },
    )
    .await
    {
        AdmissionResult::Created { created } => created,
        // The shell reports a missing queue backend here, BEFORE the record
        // exists — preserving the historical ordering where the queue handle
        // was checked ahead of the durable write.
        AdmissionResult::QueueUnavailable => return Err(AdmissionOutcome::QueueUnavailable),
        _ => return Err(AdmissionOutcome::StoreUnavailable),
    };

    let mut retry = false;
    if !created {
        let existing = match request(
            ctx,
            AdmissionOperation::LoadExisting {
                hash: user_operation_hash.clone(),
            },
        )
        .await
        {
            AdmissionResult::Record { record } => record,
            _ => return Err(AdmissionOutcome::StoreUnavailable),
        };
        // The one-hour admission record expired between SET NX and GET. A
        // later request can create it again, but this request no longer owns
        // a record it can safely finalize.
        let Some(existing) = existing else {
            return Err(AdmissionOutcome::StoreUnavailable);
        };
        match existing_admission_action(&existing, chain_id, &entry_point, &prepared.operation) {
            ExistingAdmissionAction::Conflict => {
                return Err(AdmissionOutcome::Conflict {
                    user_operation_hash,
                    existing_chain_id: existing.chain_id,
                    existing_entry_point: existing.entry_point,
                });
            }
            ExistingAdmissionAction::AlreadyAdmitted => {
                return Err(AdmissionOutcome::AlreadyQueued {
                    user_operation_hash,
                });
            }
            ExistingAdmissionAction::RetryAppend => retry = true,
        }
    }

    let mut envelope = json!({
        "schemaVersion": 1,
        "userOperationHash": user_operation_hash,
        "chainId": chain_id,
        "entryPoint": entry_point,
        "userOperation": prepared.operation,
    });
    // Only a named speed adds a key. An envelope from a client that named none
    // is byte-for-byte the envelope this relay has always appended, so nothing
    // already in the queue — or any consumer reading it — changes shape.
    if let Some(tier) = submission_tier {
        envelope["submissionTier"] = Value::String(tier.as_str().to_owned());
    }
    match request(ctx, AdmissionOperation::Enqueue { envelope, retry }).await {
        AdmissionResult::Enqueued => {}
        // A timeout or transport error can happen after Iggy durably appended
        // the message but before its acknowledgement reached us. Deleting the
        // Redis half here would create an executable orphan; the unadmitted
        // record is kept for recovery. There is deliberately no operation in
        // this vocabulary that could remove it.
        _ => return Err(AdmissionOutcome::QueueUnavailable),
    }

    match request(
        ctx,
        AdmissionOperation::MarkAdmitted {
            hash: user_operation_hash.clone(),
        },
    )
    .await
    {
        AdmissionResult::Marked { marked: true } => Ok(AdmissionOutcome::Accepted {
            user_operation_hash,
            sender_hex: prepared.sender_hex(),
            entry_point,
        }),
        _ => Err(AdmissionOutcome::StoreUnavailable),
    }
}

async fn validate_in_band_submission(
    ctx: &Ctx,
    chain_id: u64,
    settlement_recipient: Option<String>,
    user_operation: &UserOperation,
) -> Flow<()> {
    let (call_data, max_fee_per_gas, max_priority_fee_per_gas, fee_token) = match user_operation {
        UserOperation::V0_7(operation) => (
            operation.call_data.as_str(),
            operation.max_fee_per_gas.as_str(),
            operation.max_priority_fee_per_gas.as_str(),
            operation.fee_token.as_deref(),
        ),
        UserOperation::V0_6(_) => {
            return Err(AdmissionOutcome::Invalid {
                message: "the configured EntryPoint requires an unpacked v0.7 UserOperation".into(),
            });
        }
    };

    let invalid = |message: &str| AdmissionOutcome::Invalid {
        message: message.into(),
    };
    if quantity(max_fee_per_gas, "maxFeePerGas")
        .map_err(|message| AdmissionOutcome::Invalid { message })?
        != ZERO_GAS_FEE
        || quantity(max_priority_fee_per_gas, "maxPriorityFeePerGas")
            .map_err(|message| AdmissionOutcome::Invalid { message })?
            != ZERO_GAS_FEE
    {
        return Err(invalid(
            "in-band UserOperations must set maxFeePerGas and maxPriorityFeePerGas to 0x0",
        ));
    }

    let Some(recipient) = settlement_recipient else {
        return Err(AdmissionOutcome::BackendUnavailable);
    };

    if tempo::is_tempo_chain(chain_id) {
        // `parse_reimbursement` normalizes token-map keys to lowercase while
        // Alloy renders this all-numeric address with an EIP-55 uppercase
        // character. Use one canonical form for both the allowlist and lookup
        // so a valid pathUSD payment is not read as zero.
        let path_usd = tempo::PATH_USD.to_string().to_ascii_lowercase();
        if fee_token.is_some_and(|token| !token.eq_ignore_ascii_case(&path_usd)) {
            return Err(invalid("Tempo currently accepts pathUSD as the feeToken"));
        }
        let reimbursement =
            string_reimbursement(call_data, &recipient, std::iter::once(path_usd.clone()));
        let paid = reimbursement
            .stablecoins
            .get(&path_usd)
            .copied()
            .unwrap_or_default();
        let minimum = 10u128.pow(tempo::PATH_USD_DECIMALS - 2);
        if paid >= minimum {
            return Ok(());
        }
        return Err(AdmissionOutcome::Rejected {
            message:
                "Tempo UserOperation must reimburse the settlement recipient with at least 0.01 pathUSD"
                    .into(),
        });
    }

    let (native_decimals, stablecoins) =
        match request(ctx, AdmissionOperation::LoadSettlementAssets).await {
            AdmissionResult::Assets {
                native_decimals,
                stablecoins,
            } => (native_decimals, stablecoins),
            _ => return Err(AdmissionOutcome::EstimationUnavailable),
        };
    let reimbursement = string_reimbursement(call_data, &recipient, stablecoins.into_iter());
    let native_minimum = minimum_amount(native_decimals, MIN_NATIVE_FRACTION_DECIMALS)
        .ok()
        .and_then(|value| u128::try_from(value).ok())
        .ok_or(AdmissionOutcome::EstimationUnavailable)?;
    if reimbursement.native >= native_minimum {
        return Ok(());
    }
    for (token, amount) in reimbursement.stablecoins {
        let decimals = match request(ctx, AdmissionOperation::FetchTokenDecimals { token }).await {
            AdmissionResult::Decimals { decimals } => decimals,
            _ => return Err(AdmissionOutcome::EstimationUnavailable),
        };
        let minimum = crate::settlement::minimum_amount(
            decimals,
            crate::settlement::MIN_STABLE_FRACTION_DECIMALS,
        )
        .ok()
        .and_then(|value| u128::try_from(value).ok())
        .ok_or(AdmissionOutcome::EstimationUnavailable)?;
        if amount >= minimum {
            return Ok(());
        }
    }

    Err(AdmissionOutcome::Rejected {
        message: "in-band UserOperation must reimburse the settlement recipient with at least 0.00001 native coin or 0.01 of an allowlisted stablecoin".into(),
    })
}

/// String-facing reimbursement view (the RPC layer's historical semantics:
/// saturating `u128`, lowercase token keys, malformed input reads as unpaid).
pub struct StringReimbursement {
    pub native: u128,
    pub stablecoins: std::collections::BTreeMap<String, u128>,
}

pub fn string_reimbursement(
    call_data: &str,
    recipient: &str,
    stablecoin_allowlist: impl Iterator<Item = String>,
) -> StringReimbursement {
    let empty = StringReimbursement {
        native: 0,
        stablecoins: std::collections::BTreeMap::new(),
    };
    let Ok(recipient) = parse_raw_address(recipient) else {
        return empty;
    };
    let allowlist = stablecoin_allowlist
        .filter_map(|address| parse_raw_address(&address).ok())
        .map(alloy::primitives::Address::from)
        .collect::<std::collections::BTreeSet<_>>();
    let Some(call_data) = parse_hex_bytes(call_data) else {
        return empty;
    };
    match crate::settlement::parse_reimbursement(
        &call_data,
        alloy::primitives::Address::from(recipient),
        &allowlist,
    ) {
        Ok(reimbursement) => StringReimbursement {
            native: u128::try_from(reimbursement.native).unwrap_or(u128::MAX),
            stablecoins: reimbursement
                .stablecoins
                .into_iter()
                .map(|(token, amount)| {
                    (
                        format!("0x{}", hex::encode(token.as_slice())),
                        u128::try_from(amount).unwrap_or(u128::MAX),
                    )
                })
                .collect(),
        },
        Err(_) => empty,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExistingAdmissionAction {
    AlreadyAdmitted,
    RetryAppend,
    Conflict,
}

pub fn existing_admission_action(
    existing: &StoredUserOperation,
    chain_id: u64,
    entry_point: &str,
    user_operation: &UserOperation,
) -> ExistingAdmissionAction {
    // The EntryPoint hash is calculated from binary fields, whereas JSON-RPC permits harmless
    // spelling changes such as `0x01` versus `0x1`. Compare the parsed field representation so
    // an idempotent retry is not rejected merely because a wallet normalized hex differently.
    //
    // ERC-4337 deliberately excludes `signature` from userOpHash. Wallets can therefore produce
    // a fresh signature for the same operation (for example after reconnecting a session) while
    // polling or retrying `eth_sendUserOperation`. Treat that retry as idempotent: the already
    // admitted record remains the canonical queue payload, and no second message is appended.
    let operation_matches = admission_fingerprint(&existing.user_operation)
        .zip(admission_fingerprint(user_operation))
        .is_some_and(|(existing, submitted)| existing == submitted);
    let matches = existing.chain_id == chain_id
        && existing.entry_point.eq_ignore_ascii_case(entry_point)
        && operation_matches;
    match (matches, existing.admitted) {
        (true, true) => ExistingAdmissionAction::AlreadyAdmitted,
        (true, false) => ExistingAdmissionAction::RetryAppend,
        (false, _) => ExistingAdmissionAction::Conflict,
    }
}

#[derive(Eq, PartialEq)]
pub struct AdmissionFingerprint {
    sender: [u8; 20],
    nonce: [u8; 32],
    init_code: Vec<u8>,
    call_data: Vec<u8>,
    account_gas_limits: [u8; 32],
    pre_verification_gas: [u8; 32],
    gas_fees: [u8; 32],
    paymaster_and_data: Vec<u8>,
    fee_token: Option<[u8; 20]>,
}

pub fn admission_fingerprint(operation: &UserOperation) -> Option<AdmissionFingerprint> {
    let UserOperation::V0_7(operation) = operation else {
        return None;
    };
    let prepared = PreparedUserOperation::try_from(UserOperation::V0_7(operation.clone())).ok()?;
    let fee_token = operation
        .fee_token
        .as_deref()
        .map(|fee_token| parse_address_field(fee_token, "feeToken"))
        .transpose()
        .ok()?;
    Some(AdmissionFingerprint {
        sender: prepared.sender,
        nonce: prepared.nonce,
        init_code: prepared.init_code,
        call_data: prepared.call_data,
        account_gas_limits: prepared.account_gas_limits,
        pre_verification_gas: prepared.pre_verification_gas,
        gas_fees: prepared.gas_fees,
        paymaster_and_data: prepared.paymaster_and_data,
        fee_token,
    })
}

/// A structurally validated v0.7 UserOperation with the packed byte fields
/// its EntryPoint hash is computed from. Refusal messages are wire contract.
#[derive(Debug)]
pub struct PreparedUserOperation {
    pub operation: UserOperation,
    sender: [u8; 20],
    nonce: [u8; 32],
    init_code: Vec<u8>,
    call_data: Vec<u8>,
    account_gas_limits: [u8; 32],
    pre_verification_gas: [u8; 32],
    gas_fees: [u8; 32],
    paymaster_and_data: Vec<u8>,
}

impl TryFrom<UserOperation> for PreparedUserOperation {
    type Error = String;

    fn try_from(operation: UserOperation) -> Result<Self, Self::Error> {
        let UserOperation::V0_7(operation) = operation else {
            return Err("the configured EntryPoint requires an unpacked v0.7 UserOperation".into());
        };
        Self::from_v0_7(operation)
    }
}

impl PreparedUserOperation {
    fn from_v0_7(operation: Box<UserOperationV0_7>) -> Result<Self, String> {
        if operation.eip7702_auth.is_some() {
            return Err("eip7702Auth is not enabled for in-band UserOperations".into());
        }

        let sender = parse_address_field(&operation.sender, "sender")?;
        let nonce = uint256(&operation.nonce, "nonce")?;
        let call_data = bytes_field(&operation.call_data, "callData")?;
        let signature = bytes_field(&operation.signature, "signature")?;
        if signature.is_empty() {
            return Err("signature is required".into());
        }

        let init_code = match (
            operation.factory.as_deref(),
            operation.factory_data.as_deref(),
        ) {
            (Some(factory), factory_data) => {
                let mut init_code = parse_address_field(factory, "factory")?.to_vec();
                init_code.extend(bytes_field(factory_data.unwrap_or("0x"), "factoryData")?);
                init_code
            }
            (None, Some(factory_data)) if factory_data != "0x" => {
                return Err("factoryData requires factory".into());
            }
            (None, _) => Vec::new(),
        };

        let call_gas_limit = nonzero_uint128(&operation.call_gas_limit, "callGasLimit")?;
        let verification_gas_limit =
            nonzero_uint128(&operation.verification_gas_limit, "verificationGasLimit")?;
        let pre_verification_gas =
            nonzero_uint256(&operation.pre_verification_gas, "preVerificationGas")?;
        let max_fee_per_gas = quantity(&operation.max_fee_per_gas, "maxFeePerGas")?;
        let max_priority_fee_per_gas =
            quantity(&operation.max_priority_fee_per_gas, "maxPriorityFeePerGas")?;
        if max_fee_per_gas != ZERO_GAS_FEE || max_priority_fee_per_gas != ZERO_GAS_FEE {
            return Err(
                "in-band UserOperations must set maxFeePerGas and maxPriorityFeePerGas to 0x0"
                    .into(),
            );
        }

        let paymaster_and_data = paymaster_and_data(&operation)?;
        let account_gas_limits = packed_uint128(verification_gas_limit, call_gas_limit);
        let gas_fees = packed_uint128(max_priority_fee_per_gas, max_fee_per_gas);

        Ok(Self {
            operation: UserOperation::V0_7(operation),
            sender,
            nonce,
            init_code,
            call_data,
            account_gas_limits,
            pre_verification_gas,
            gas_fees,
            paymaster_and_data,
        })
    }

    pub fn user_operation_hash(&self, entry_point: [u8; 20], chain_id: u64) -> String {
        let mut packed = Vec::with_capacity(8 * 32);
        packed.extend(address_word(self.sender));
        packed.extend(self.nonce);
        packed.extend(keccak256(&self.init_code));
        packed.extend(keccak256(&self.call_data));
        packed.extend(self.account_gas_limits);
        packed.extend(self.pre_verification_gas);
        packed.extend(self.gas_fees);
        packed.extend(keccak256(&self.paymaster_and_data));

        let mut envelope = Vec::with_capacity(3 * 32);
        envelope.extend(keccak256(&packed));
        envelope.extend(address_word(entry_point));
        envelope.extend(uint256_from_u64(chain_id));
        format!("0x{}", hex::encode(keccak256(&envelope)))
    }

    pub fn sender_hex(&self) -> String {
        format!("0x{}", hex::encode(self.sender))
    }
}

fn paymaster_and_data(operation: &UserOperationV0_7) -> Result<Vec<u8>, String> {
    let Some(paymaster) = operation.paymaster.as_deref() else {
        if operation.paymaster_verification_gas_limit.is_some()
            || operation.paymaster_post_op_gas_limit.is_some()
            || operation
                .paymaster_data
                .as_deref()
                .is_some_and(|data| data != "0x")
        {
            return Err("paymaster fields require paymaster".into());
        }
        return Ok(Vec::new());
    };

    let verification_gas_limit = operation
        .paymaster_verification_gas_limit
        .as_deref()
        .ok_or("paymasterVerificationGasLimit is required when paymaster is set")?;
    let verification_gas_limit = quantity(verification_gas_limit, "paymasterVerificationGasLimit")?;
    let post_op_gas_limit = operation
        .paymaster_post_op_gas_limit
        .as_deref()
        .map(|value| quantity(value, "paymasterPostOpGasLimit"))
        .transpose()?
        .unwrap_or_default();

    let mut value = parse_address_field(paymaster, "paymaster")?.to_vec();
    value.extend(verification_gas_limit.to_be_bytes());
    value.extend(post_op_gas_limit.to_be_bytes());
    value.extend(bytes_field(
        operation.paymaster_data.as_deref().unwrap_or("0x"),
        "paymasterData",
    )?);
    Ok(value)
}

fn parse_raw_address(value: &str) -> Result<[u8; 20], ()> {
    let value = value.strip_prefix("0x").ok_or(())?;
    if value.len() != 40 {
        return Err(());
    }
    let bytes = parse_hex_bytes(&format!("0x{value}")).ok_or(())?;
    bytes.as_ref().try_into().map_err(|_| ())
}

fn parse_address_field(value: &str, field: &str) -> Result<[u8; 20], String> {
    parse_raw_address(value).map_err(|_| format!("{field} must be a 20-byte address"))
}

fn bytes_field(value: &str, field: &str) -> Result<Vec<u8>, String> {
    parse_hex_bytes(value)
        .map(|bytes| bytes.to_vec())
        .ok_or_else(|| format!("{field} must be 0x-prefixed hex data"))
}

fn quantity(value: &str, field: &str) -> Result<u128, String> {
    let value = value
        .strip_prefix("0x")
        .ok_or_else(|| format!("{field} must be a 0x-prefixed quantity"))?;
    if value.is_empty() || value.len() > 32 {
        return Err(format!("invalid {field}"));
    }
    u128::from_str_radix(value, 16).map_err(|_| format!("invalid {field}"))
}

fn nonzero_uint128(value: &str, field: &str) -> Result<u128, String> {
    let value = quantity(value, field)?;
    (value != 0)
        .then_some(value)
        .ok_or_else(|| format!("{field} must be greater than zero"))
}

fn uint256(value: &str, field: &str) -> Result<[u8; 32], String> {
    let value = value
        .strip_prefix("0x")
        .ok_or_else(|| format!("{field} must be a 0x-prefixed quantity"))?;
    if value.is_empty() || value.len() > 64 {
        return Err(format!("invalid {field}"));
    }

    let padded = if value.len() % 2 == 0 {
        value.to_owned()
    } else {
        format!("0{value}")
    };
    let bytes =
        parse_hex_bytes(&format!("0x{padded}")).ok_or_else(|| format!("invalid {field}"))?;
    let mut word = [0; 32];
    word[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(word)
}

fn nonzero_uint256(value: &str, field: &str) -> Result<[u8; 32], String> {
    let value = uint256(value, field)?;
    value
        .iter()
        .any(|byte| *byte != 0)
        .then_some(value)
        .ok_or_else(|| format!("{field} must be greater than zero"))
}

fn packed_uint128(high: u128, low: u128) -> [u8; 32] {
    let mut value = [0; 32];
    value[..16].copy_from_slice(&high.to_be_bytes());
    value[16..].copy_from_slice(&low.to_be_bytes());
    value
}

fn address_word(value: [u8; 20]) -> [u8; 32] {
    let mut word = [0; 32];
    word[12..].copy_from_slice(&value);
    word
}

fn uint256_from_u64(value: u64) -> [u8; 32] {
    let mut word = [0; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    word
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crux_core::{Core, Request};
    use serde_json::json;

    use super::{
        AdmissionApp, AdmissionEvent, AdmissionOperation, AdmissionOutcome, AdmissionResult,
        ExistingAdmissionAction, PreparedUserOperation, SubmitRequest, existing_admission_action,
        parse_address_field, quantity,
    };
    use crate::task::{
        QueuedUserOperation, StoredUserOperation, UserOperation, UserOperationStatus,
        UserOperationV0_7,
    };

    const ENTRY_POINT: &str = "0x0000000071727De22E5E9d8BAf0edAc6f37da032";
    const LOCAL_POLICY_CHAIN: u64 = 9_999_999_991;
    const RECIPIENT: &str = "0x1111111111111111111111111111111111111111";

    fn user_operation() -> UserOperation {
        UserOperation::V0_7(Box::new(UserOperationV0_7 {
            sender: "0x1111111111111111111111111111111111111111".into(),
            nonce: "0x0".into(),
            factory: None,
            factory_data: None,
            call_data: "0x1234".into(),
            call_gas_limit: "0x5208".into(),
            verification_gas_limit: "0x10000".into(),
            pre_verification_gas: "0x1000".into(),
            max_fee_per_gas: "0x0".into(),
            max_priority_fee_per_gas: "0x0".into(),
            paymaster: None,
            paymaster_verification_gas_limit: None,
            paymaster_post_op_gas_limit: None,
            paymaster_data: None,
            signature: "0x1234".into(),
            eip7702_auth: None,
            fee_token: None,
        }))
    }

    #[test]
    fn calculates_the_entry_point_v07_user_operation_hash() {
        let prepared = PreparedUserOperation::try_from(user_operation()).unwrap();
        let entry_point =
            parse_address_field("0x0000000071727De22E5E9d8BAf0edAc6f37da032", "entryPoint")
                .unwrap();

        assert_eq!(
            prepared.user_operation_hash(entry_point, 1),
            "0xdd4a4e34a55b5ea9cd7bfbbe15c570fe6fd8893c2c809e72ac2077de91e1e257"
        );
    }

    #[test]
    fn rejects_native_prefund_fee_fields() {
        let mut operation = match user_operation() {
            UserOperation::V0_7(operation) => operation,
            UserOperation::V0_6(_) => unreachable!(),
        };
        operation.max_fee_per_gas = "0x1".into();

        let error = PreparedUserOperation::try_from(UserOperation::V0_7(operation)).unwrap_err();
        assert_eq!(
            error,
            "in-band UserOperations must set maxFeePerGas and maxPriorityFeePerGas to 0x0"
        );
    }

    #[test]
    fn validates_gas_quantities_and_zero_fee_forms() {
        assert_eq!(quantity("0x000", "maxFeePerGas").unwrap(), 0);
        assert!(quantity("0x", "maxFeePerGas").is_err());
    }

    #[test]
    fn canonicalizes_path_usd_for_reimbursement_lookup() {
        assert_eq!(
            crate::tempo::PATH_USD.to_string().to_ascii_lowercase(),
            "0x20c0000000000000000000000000000000000000"
        );
    }

    #[test]
    fn retries_only_an_exact_unadmitted_redis_record() {
        let operation = user_operation();
        let mut stored = stored_admission(operation.clone(), false);

        assert_eq!(
            existing_admission_action(
                &stored,
                LOCAL_POLICY_CHAIN,
                &ENTRY_POINT.to_ascii_lowercase(),
                &operation,
            ),
            ExistingAdmissionAction::RetryAppend
        );

        stored.admitted = true;
        assert_eq!(
            existing_admission_action(&stored, LOCAL_POLICY_CHAIN, ENTRY_POINT, &operation),
            ExistingAdmissionAction::AlreadyAdmitted
        );

        stored.admitted = false;
        assert_eq!(
            existing_admission_action(
                &stored,
                LOCAL_POLICY_CHAIN,
                "0x1111111111111111111111111111111111111111",
                &operation,
            ),
            ExistingAdmissionAction::Conflict
        );

        stored.chain_id += 1;
        assert_eq!(
            existing_admission_action(&stored, LOCAL_POLICY_CHAIN, ENTRY_POINT, &operation),
            ExistingAdmissionAction::Conflict
        );

        stored.chain_id = LOCAL_POLICY_CHAIN;
        let mut different_operation = match operation {
            UserOperation::V0_7(operation) => operation,
            UserOperation::V0_6(_) => unreachable!(),
        };
        different_operation.nonce = "0x1".into();
        assert_eq!(
            existing_admission_action(
                &stored,
                LOCAL_POLICY_CHAIN,
                ENTRY_POINT,
                &UserOperation::V0_7(different_operation),
            ),
            ExistingAdmissionAction::Conflict
        );
    }

    #[test]
    fn retries_a_semantically_identical_user_operation_with_normalized_hex() {
        let operation = user_operation();
        let stored = stored_admission(operation.clone(), true);
        let mut normalized = match operation {
            UserOperation::V0_7(operation) => operation,
            UserOperation::V0_6(_) => unreachable!(),
        };
        normalized.nonce = "0x00".into();
        normalized.call_gas_limit = "0x05208".into();
        normalized.verification_gas_limit = "0x010000".into();
        normalized.pre_verification_gas = "0x01000".into();
        normalized.max_fee_per_gas = "0x000".into();
        normalized.max_priority_fee_per_gas = "0x0000".into();

        assert_eq!(
            existing_admission_action(
                &stored,
                LOCAL_POLICY_CHAIN,
                ENTRY_POINT,
                &UserOperation::V0_7(normalized),
            ),
            ExistingAdmissionAction::AlreadyAdmitted
        );
    }

    #[test]
    fn retries_a_queued_user_operation_with_a_refreshed_signature() {
        let operation = user_operation();
        let stored = stored_admission(operation.clone(), true);
        let mut refreshed = match operation {
            UserOperation::V0_7(operation) => operation,
            UserOperation::V0_6(_) => unreachable!(),
        };
        refreshed.signature = "0x12345678".into();

        assert_eq!(
            existing_admission_action(
                &stored,
                LOCAL_POLICY_CHAIN,
                ENTRY_POINT,
                &UserOperation::V0_7(refreshed),
            ),
            ExistingAdmissionAction::AlreadyAdmitted
        );
    }

    fn stored_admission(user_operation: UserOperation, admitted: bool) -> StoredUserOperation {
        StoredUserOperation {
            status: UserOperationStatus::Queued,
            transaction_hash: None,
            chain_id: LOCAL_POLICY_CHAIN,
            chain_id_text: LOCAL_POLICY_CHAIN.to_string(),
            entry_point: ENTRY_POINT.into(),
            user_operation,
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

    // ----- Driver-based program walks -----

    struct Driver {
        core: Core<AdmissionApp>,
        queue: VecDeque<Request<AdmissionOperation>>,
    }

    impl Driver {
        fn submit(request: SubmitRequest) -> Self {
            let core: Core<AdmissionApp> = Core::new();
            let effects = core.process_event(AdmissionEvent::Submit(Box::new(request)));
            let mut driver = Self {
                core,
                queue: VecDeque::new(),
            };
            driver.absorb(effects);
            driver
        }

        fn absorb(&mut self, effects: Vec<super::AdmissionEffect>) {
            for effect in effects {
                let super::AdmissionEffect::Work(request) = effect;
                self.queue.push_back(request);
            }
            assert!(
                self.queue.len() <= 1,
                "the admission program must be strictly sequential"
            );
        }

        fn step(&mut self, expected: AdmissionOperation, result: AdmissionResult) {
            let mut request = self
                .queue
                .pop_front()
                .unwrap_or_else(|| panic!("no operation in flight; expected {expected:?}"));
            assert_eq!(request.operation, expected);
            let effects = self
                .core
                .resolve(&mut request, result)
                .expect("resolve must succeed");
            self.absorb(effects);
        }

        fn assert_settled(&self, expected: AdmissionOutcome) {
            assert!(self.queue.is_empty(), "no operation may remain in flight");
            assert_eq!(self.core.view().outcome, Some(expected));
        }
    }

    /// Safe executeUserOp → MultiSend(delegatecall) paying `amount` native to
    /// the recipient.
    fn native_payment_call_data(amount: u128) -> String {
        fn word_u128(value: u128) -> Vec<u8> {
            let mut word = vec![0; 16];
            word.extend(value.to_be_bytes());
            word
        }
        let mut packed = Vec::new();
        packed.push(0);
        packed.extend(&super::parse_raw_address(RECIPIENT).unwrap());
        packed.extend(word_u128(amount));
        packed.extend(word_u128(0));

        let mut multisend = vec![0x8d, 0x80, 0xff, 0x0a];
        multisend.extend(word_u128(32));
        multisend.extend(word_u128(packed.len() as u128));
        multisend.extend(packed);
        let padding = (32 - multisend.len() % 32) % 32;
        multisend.resize(multisend.len() + padding, 0);

        let mut call_data = vec![0x7b, 0xb3, 0x74, 0x28];
        let mut trusted = vec![0u8; 12];
        trusted.extend(
            super::parse_raw_address("0x38869bf66a61cf6bdb996a6ae40d5853fd43b526").unwrap(),
        );
        call_data.extend(trusted);
        call_data.extend(word_u128(0));
        call_data.extend(word_u128(128));
        call_data.extend(word_u128(1));
        call_data.extend(word_u128(multisend.len() as u128));
        call_data.extend(multisend);
        format!("0x{}", hex::encode(call_data))
    }

    fn paying_operation(amount: u128) -> UserOperation {
        let UserOperation::V0_7(mut operation) = user_operation() else {
            unreachable!()
        };
        operation.call_data = native_payment_call_data(amount);
        UserOperation::V0_7(operation)
    }

    fn submit(operation: UserOperation) -> SubmitRequest {
        submit_at(operation, None)
    }

    fn submit_at(
        operation: UserOperation,
        submission_tier: Option<crate::gas_math::SubmissionTier>,
    ) -> SubmitRequest {
        SubmitRequest {
            chain_id: LOCAL_POLICY_CHAIN,
            entry_point: ENTRY_POINT.into(),
            user_operation: operation,
            settlement_recipient: Some(RECIPIENT.into()),
            submission_tier,
        }
    }

    fn expected_hash(operation: &UserOperation) -> String {
        let prepared = PreparedUserOperation::try_from(operation.clone()).unwrap();
        prepared.user_operation_hash(
            parse_address_field(ENTRY_POINT, "entryPoint").unwrap(),
            LOCAL_POLICY_CHAIN,
        )
    }

    #[test]
    fn a_paying_operation_walks_create_enqueue_admit() {
        // 18 native decimals → minimum 10^13; pay exactly the minimum.
        let operation = paying_operation(10_000_000_000_000);
        let hash = expected_hash(&operation);
        let mut driver = Driver::submit(submit(operation.clone()));

        driver.step(
            AdmissionOperation::LoadSettlementAssets,
            AdmissionResult::Assets {
                native_decimals: 18,
                stablecoins: Vec::new(),
            },
        );
        driver.step(
            AdmissionOperation::CreateQueued {
                operation: QueuedUserOperation {
                    user_operation_hash: hash.clone(),
                    chain_id: LOCAL_POLICY_CHAIN,
                    entry_point: ENTRY_POINT.into(),
                    user_operation: operation.clone(),
                },
            },
            AdmissionResult::Created { created: true },
        );
        driver.step(
            AdmissionOperation::Enqueue {
                envelope: json!({
                    "schemaVersion": 1,
                    "userOperationHash": hash,
                    "chainId": LOCAL_POLICY_CHAIN,
                    "entryPoint": ENTRY_POINT,
                    "userOperation": operation,
                }),
                retry: false,
            },
            AdmissionResult::Enqueued,
        );
        driver.step(
            AdmissionOperation::MarkAdmitted { hash: hash.clone() },
            AdmissionResult::Marked { marked: true },
        );
        driver.assert_settled(AdmissionOutcome::Accepted {
            user_operation_hash: hash,
            sender_hex: "0x1111111111111111111111111111111111111111".into(),
            entry_point: ENTRY_POINT.into(),
        });
    }

    #[test]
    fn a_named_speed_rides_the_envelope_without_touching_what_the_user_signed() {
        use crate::gas_math::SubmissionTier;
        let operation = paying_operation(10_000_000_000_000);
        // The same operation, so the same hash: naming a speed is a request
        // about how the RELAY submits, not a term of what the user signed, and
        // it must never move the userOpHash or the durable record.
        let hash = expected_hash(&operation);
        let mut driver = Driver::submit(submit_at(operation.clone(), Some(SubmissionTier::Fast)));

        driver.step(
            AdmissionOperation::LoadSettlementAssets,
            AdmissionResult::Assets {
                native_decimals: 18,
                stablecoins: Vec::new(),
            },
        );
        driver.step(
            AdmissionOperation::CreateQueued {
                operation: QueuedUserOperation {
                    user_operation_hash: hash.clone(),
                    chain_id: LOCAL_POLICY_CHAIN,
                    entry_point: ENTRY_POINT.into(),
                    user_operation: operation.clone(),
                },
            },
            AdmissionResult::Created { created: true },
        );
        // One extra key, and only when a speed was named — the untiered
        // envelope pinned by the test above stays byte-for-byte what it was.
        driver.step(
            AdmissionOperation::Enqueue {
                envelope: json!({
                    "schemaVersion": 1,
                    "userOperationHash": hash,
                    "chainId": LOCAL_POLICY_CHAIN,
                    "entryPoint": ENTRY_POINT,
                    "userOperation": operation,
                    "submissionTier": "fast",
                }),
                retry: false,
            },
            AdmissionResult::Enqueued,
        );
        driver.step(
            AdmissionOperation::MarkAdmitted { hash: hash.clone() },
            AdmissionResult::Marked { marked: true },
        );
        driver.assert_settled(AdmissionOutcome::Accepted {
            user_operation_hash: hash,
            sender_hex: "0x1111111111111111111111111111111111111111".into(),
            entry_point: ENTRY_POINT.into(),
        });
    }

    #[test]
    fn a_duplicate_admitted_record_is_idempotent() {
        let operation = paying_operation(10_000_000_000_000);
        let hash = expected_hash(&operation);
        let mut driver = Driver::submit(submit(operation.clone()));
        driver.step(
            AdmissionOperation::LoadSettlementAssets,
            AdmissionResult::Assets {
                native_decimals: 18,
                stablecoins: Vec::new(),
            },
        );
        driver.step(
            AdmissionOperation::CreateQueued {
                operation: QueuedUserOperation {
                    user_operation_hash: hash.clone(),
                    chain_id: LOCAL_POLICY_CHAIN,
                    entry_point: ENTRY_POINT.into(),
                    user_operation: operation.clone(),
                },
            },
            AdmissionResult::Created { created: false },
        );
        let mut stored = stored_admission(operation, true);
        stored.user_operation = paying_operation(10_000_000_000_000);
        driver.step(
            AdmissionOperation::LoadExisting { hash: hash.clone() },
            AdmissionResult::Record {
                record: Some(stored),
            },
        );
        driver.assert_settled(AdmissionOutcome::AlreadyQueued {
            user_operation_hash: hash,
        });
    }

    #[test]
    fn a_conflicting_record_refuses_resubmission() {
        let operation = paying_operation(10_000_000_000_000);
        let hash = expected_hash(&operation);
        let mut driver = Driver::submit(submit(operation.clone()));
        driver.step(
            AdmissionOperation::LoadSettlementAssets,
            AdmissionResult::Assets {
                native_decimals: 18,
                stablecoins: Vec::new(),
            },
        );
        driver.step(
            AdmissionOperation::CreateQueued {
                operation: QueuedUserOperation {
                    user_operation_hash: hash.clone(),
                    chain_id: LOCAL_POLICY_CHAIN,
                    entry_point: ENTRY_POINT.into(),
                    user_operation: operation.clone(),
                },
            },
            AdmissionResult::Created { created: false },
        );
        // A different stored payload under the same hash: refuse.
        driver.step(
            AdmissionOperation::LoadExisting { hash: hash.clone() },
            AdmissionResult::Record {
                record: Some(stored_admission(user_operation(), false)),
            },
        );
        driver.assert_settled(AdmissionOutcome::Conflict {
            user_operation_hash: hash,
            existing_chain_id: LOCAL_POLICY_CHAIN,
            existing_entry_point: ENTRY_POINT.into(),
        });
    }

    #[test]
    fn a_queue_outage_after_the_record_keeps_the_crash_window_semantics() {
        let operation = paying_operation(10_000_000_000_000);
        let hash = expected_hash(&operation);
        let mut driver = Driver::submit(submit(operation.clone()));
        driver.step(
            AdmissionOperation::LoadSettlementAssets,
            AdmissionResult::Assets {
                native_decimals: 18,
                stablecoins: Vec::new(),
            },
        );
        driver.step(
            AdmissionOperation::CreateQueued {
                operation: QueuedUserOperation {
                    user_operation_hash: hash.clone(),
                    chain_id: LOCAL_POLICY_CHAIN,
                    entry_point: ENTRY_POINT.into(),
                    user_operation: operation.clone(),
                },
            },
            AdmissionResult::Created { created: true },
        );
        driver.step(
            AdmissionOperation::Enqueue {
                envelope: json!({
                    "schemaVersion": 1,
                    "userOperationHash": hash,
                    "chainId": LOCAL_POLICY_CHAIN,
                    "entryPoint": ENTRY_POINT,
                    "userOperation": operation,
                }),
                retry: false,
            },
            AdmissionResult::QueueUnavailable,
        );
        // The record is deliberately retained; the vocabulary has no way to
        // delete it, and the program settles without further operations.
        driver.assert_settled(AdmissionOutcome::QueueUnavailable);
    }

    #[test]
    fn an_underpaying_operation_is_rejected_without_touching_the_store() {
        let operation = paying_operation(1);
        let mut driver = Driver::submit(submit(operation));
        driver.step(
            AdmissionOperation::LoadSettlementAssets,
            AdmissionResult::Assets {
                native_decimals: 18,
                stablecoins: Vec::new(),
            },
        );
        driver.assert_settled(AdmissionOutcome::Rejected {
            message: "in-band UserOperation must reimburse the settlement recipient with at least 0.00001 native coin or 0.01 of an allowlisted stablecoin".into(),
        });
    }

    #[test]
    fn nonzero_fees_are_refused_before_any_operation() {
        let UserOperation::V0_7(mut operation) = user_operation() else {
            unreachable!()
        };
        operation.max_fee_per_gas = "0x1".into();
        let driver = Driver::submit(submit(UserOperation::V0_7(operation)));
        driver.assert_settled(AdmissionOutcome::Invalid {
            message: "in-band UserOperations must set maxFeePerGas and maxPriorityFeePerGas to 0x0"
                .into(),
        });
    }
}

#[cfg(test)]
mod string_reimbursement_tests {
    use super::string_reimbursement;

    const TRUSTED_MULTISEND: &str = "0x38869bf66a61cf6bdb996a6ae40d5853fd43b526";
    const RECIPIENT: &str = "0x1111111111111111111111111111111111111111";
    const STABLECOIN: &str = "0x2222222222222222222222222222222222222222";

    #[test]
    fn counts_only_native_and_allowlisted_stablecoin_legs_to_the_recipient() {
        let call_data = encode_safe_multisend(&[
            Entry::native(RECIPIENT, 10_000_000_000_000),
            Entry::erc20(STABLECOIN, RECIPIENT, 10_000),
            Entry::erc20(
                "0x3333333333333333333333333333333333333333",
                RECIPIENT,
                99_999,
            ),
        ]);

        let reimbursement =
            string_reimbursement(&call_data, RECIPIENT, [STABLECOIN.to_owned()].into_iter());

        assert_eq!(reimbursement.native, 10_000_000_000_000);
        assert_eq!(reimbursement.stablecoins[STABLECOIN], 10_000);
        assert_eq!(reimbursement.stablecoins.len(), 1);
    }

    #[test]
    fn rejects_transfer_shaped_data_without_a_delegatecall_to_trusted_multisend() {
        let call_data = encode_safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, 10_000)]);
        let tampered = call_data.replacen("01", "00", 1);

        let reimbursement =
            string_reimbursement(&tampered, RECIPIENT, [STABLECOIN.to_owned()].into_iter());

        assert_eq!(reimbursement.native, 0);
        assert!(reimbursement.stablecoins.is_empty());
    }

    struct Entry {
        to: &'static str,
        value: u128,
        data: Vec<u8>,
    }

    impl Entry {
        fn native(to: &'static str, value: u128) -> Self {
            Self {
                to,
                value,
                data: Vec::new(),
            }
        }

        fn erc20(token: &'static str, recipient: &'static str, amount: u128) -> Self {
            let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
            data.extend(word_address(recipient));
            data.extend(word_u128(amount));
            Self {
                to: token,
                value: 0,
                data,
            }
        }
    }

    fn encode_safe_multisend(entries: &[Entry]) -> String {
        let mut packed = Vec::new();
        for entry in entries {
            packed.push(0);
            packed.extend(address(entry.to));
            packed.extend(word_u128(entry.value));
            packed.extend(word_u128(entry.data.len() as u128));
            packed.extend(&entry.data);
        }

        let mut multisend = vec![0x8d, 0x80, 0xff, 0x0a];
        multisend.extend(word_u128(32));
        multisend.extend(word_u128(packed.len() as u128));
        multisend.extend(packed);
        let padding = (32 - multisend.len() % 32) % 32;
        multisend.resize(multisend.len() + padding, 0);

        let mut call_data = vec![0x7b, 0xb3, 0x74, 0x28];
        call_data.extend(word_address(TRUSTED_MULTISEND));
        call_data.extend(word_u128(0));
        call_data.extend(word_u128(128));
        call_data.extend(word_u128(1));
        call_data.extend(word_u128(multisend.len() as u128));
        call_data.extend(multisend);
        format!("0x{}", encode_hex(&call_data))
    }

    fn address(value: &'static str) -> Vec<u8> {
        super::parse_raw_address(value).unwrap().to_vec()
    }

    fn word_address(value: &'static str) -> Vec<u8> {
        let mut word = vec![0; 12];
        word.extend(address(value));
        word
    }

    fn word_u128(value: u128) -> Vec<u8> {
        let mut word = vec![0; 16];
        word.extend(value.to_be_bytes());
        word
    }

    fn encode_hex(value: &[u8]) -> String {
        const TABLE: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(value.len() * 2);
        for byte in value {
            output.push(TABLE[(byte >> 4) as usize] as char);
            output.push(TABLE[(byte & 0x0f) as usize] as char);
        }
        output
    }
}
