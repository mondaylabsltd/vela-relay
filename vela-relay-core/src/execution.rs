//! The per-lane batch execution program.
//!
//! One `Core<ExecutionApp>` drives one consumed lane batch from routed
//! envelopes to a per-operation resolution vector. Every business decision —
//! triage, deduplication, simulation-verdict handling, the settlement gate,
//! funding readiness, the sign/persist/broadcast/mark sequence — is made
//! here; the shell executes the requested operations against Redis, the
//! chain, and Telegram and reports what happened as data.
//!
//! Three nested pipelines are still modeled as composite operations executed
//! by the shell (`ResumeBundleIntent`, `ResolveNonceMismatches`,
//! `EnsureRelayerFunded`, plus the whole Tempo bundle tail and
//! `BroadcastBundle`): their internal decisions migrate here in a follow-up
//! (tasks T034); the operations' *outcomes* already flow through this
//! program's decisions.

use std::collections::HashSet;

use alloy::primitives::{Address, B256, U256};
use crux_core::{App, Command, macros::effect};
use serde_json::Value;

use crate::{
    abi::{PackedOperation, handle_ops_calldata, user_operation_hash},
    cost::allocate_bundle_gas,
    funding::{
        NATIVE_TOP_UP_USD_CAP, native_amount_for_usd_cap, native_top_up_reserve,
        plan_native_top_up, plan_tempo_top_up, treasury_affordable_top_up,
    },
    gas_math::SubmissionTier,
    hold::{HoldDecision, decide_hold},
    receipt::receipt_succeeded,
    settlement::{
        ChainAssetConfig, FeeContext, SettlementDecision, SettlementLog, decide_settlement,
        has_stablecoin_payment, settlement_rejection_reason, verify_stable_transfer_logs,
    },
    task::{
        PreparedBundleIntent, PreparedFundingIntent, QueuedUserOperation, RoutedUserOperation,
        StoredUserOperation, UserOperation, UserOperationStatus,
    },
    vault::relayer_index_for_sender,
};

/// Validated policy values the shell injects per batch (from its config).
#[derive(Clone, Debug)]
pub struct ExecutionPolicy {
    pub pool_width: usize,
    pub max_bundle_operations: usize,
    pub gas_buffer_bps: u64,
    pub fixed_gas_buffer: u64,
    pub settlement_inclusion_floor_bps: u64,
    pub settlement_hold_max_attempts: u32,
    /// Static fallback per-transfer top-up cap in wei, used when no market
    /// price is available.
    pub top_up_max_wei: u128,
    /// Whether this chain settles through Tempo's pathUSD `0x76` path.
    pub is_tempo: bool,
    /// The settlement recipient / treasury address (derived by the shell).
    pub treasury: Address,
    /// This lane's relayer address (derived by the shell).
    pub relayer: Address,
    // Native relayer-float policy (validated config).
    pub relayer_float_cost_multiplier: u64,
    pub relayer_float_target_wei: u128,
    pub relayer_float_min_wei: u128,
    pub treasury_floor_wei: u128,
}

/// Everything the program needs to know about one lane batch at start.
#[derive(Debug)]
pub struct StartBatch {
    pub operations: Vec<RoutedUserOperation>,
    pub policy: ExecutionPolicy,
    /// Shell-generated lane lease token (nondeterministic input).
    pub lease_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemResolution {
    /// A durable outcome was reached; the consumer offset may pass this item.
    Durable,
    /// No durable outcome; the message must be redelivered.
    Failed { reason: String },
}

/// Chain asset metadata as resolved by the shell (directory or Tempo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedChainAssets {
    pub assets: ChainAssetConfig,
    pub native_symbol: String,
}

/// Mirror of the shell's per-operation simulation verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationSimVerdict {
    Success,
    NonceMismatch,
    Rejected {
        reason: String,
    },
    /// Waiting for automatic simulation-contract deployment.
    Pending {
        reason: String,
    },
    Transient {
        reason: String,
    },
}

/// Mirror of the shell's whole-bundle simulation verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleSimVerdict {
    Success(BundleSimulationData),
    NonceMismatch,
    Rejected { reason: String },
    Pending { reason: String },
    Transient { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSimulationData {
    pub gas_used: U256,
    /// `actual_gas_used` per surviving operation, in bundle order.
    pub operation_gas_used: Vec<U256>,
    /// Logs from the exact final handleOps simulation (settlement evidence).
    pub logs: Vec<SettlementLog>,
}

/// Fee/nonce/balance context for the outer transaction, fetched by the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionContext {
    pub estimated_gas: U256,
    pub base_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub nonce: u64,
    pub relayer_balance: U256,
}

/// The signed outer transaction as produced by the shell's keystore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedBundle {
    pub raw_transaction_hex: String,
    pub transaction_hash: String,
    pub nonce: u64,
}

/// A native treasury → relayer transfer the shell signs with the treasury key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreasurySignRequest {
    pub nonce: u64,
    pub amount: U256,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// A pathUSD treasury → relayer transfer the shell signs with the treasury key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TempoTreasurySignRequest {
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub amount: U256,
}

/// The Tempo `0x76` plan the shell signs (fee token is always pathUSD, tip 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TempoSignRequest {
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub entry_point: Address,
    pub calldata: Vec<u8>,
}

/// The plan the shell signs; keys never cross into the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleSignRequest {
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub entry_point: Address,
    pub calldata: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub enum ExecutionOperation {
    // --- triage ---
    CheckChainSupported,
    LoadChainAssets,
    LoadRecords {
        hashes: Vec<String>,
    },
    DeadLetterRouted {
        index: usize,
        reason: String,
    },
    RestoreQueued {
        index: usize,
        queued: QueuedUserOperation,
    },
    ReloadRecord {
        hash: String,
    },
    MarkAdmitted {
        hash: String,
    },
    MarkRejected {
        hash: String,
        cause: RejectionCause,
    },
    MarkRejectedWithReason {
        hash: String,
        stage: &'static str,
        reason: String,
    },
    /// Park item `index` in the durable delayed inbox (post-increment attempt
    /// count comes back). `cause` carries the business context for the
    /// shell's diagnostics.
    DeferOperation {
        index: usize,
        cause: DeferCause,
    },
    // --- diagnostics (best-effort writes; Telegram policy decided here) ---
    RecordDeferred {
        hash: String,
        stage: &'static str,
        reason: String,
    },
    NotifyIssue {
        hash: String,
        stage: &'static str,
        reason: String,
    },
    /// An operator-facing log line whose data exists only inside a core
    /// decision; the shell renders it with the historical tracing level,
    /// message, and field names.
    EmitDiagnostic {
        diagnostic: ExecutionDiagnostic,
    },
    // --- leases ---
    AcquireLaneLease,
    EnsureLaneLease,
    // --- prepared intent ---
    LoadPreparedBundle,
    /// Composite: audit replay, clear or rebroadcast as today (T034 splits it).
    ResumeBundleIntent {
        intent: PreparedBundleIntent,
    },
    // --- simulation ---
    SimulateIndividually {
        entry_point: Address,
        operations: Vec<(B256, PackedOperation)>,
    },
    /// Batch `EntryPoint.getNonce` probe for AA25 mismatches; one `Option`
    /// per probe (`None` = undecodable response).
    FetchAccountNonces {
        entry_point: Address,
        probes: Vec<(Address, U256)>,
    },
    SimulateBundle {
        entry_point: Address,
        operations: Vec<(B256, PackedOperation)>,
    },
    // --- outer transaction ---
    FetchTransactionContext {
        entry_point: Address,
        calldata: Vec<u8>,
    },
    FetchMarketPrice,
    // --- treasury funding (sequenced here) ---
    AcquireTreasuryLease,
    EnsureTreasuryLease,
    ReleaseTreasuryLease,
    LoadPreparedFunding,
    SaveFundingIntent {
        intent: PreparedFundingIntent,
    },
    ClearFundingIntent {
        transaction_hash: String,
    },
    FetchTreasuryContext,
    /// Tempo variant: also estimates the pathUSD transfer's gas (raw, the
    /// buffer is applied here).
    FetchTempoTreasuryContext {
        transfer_amount: U256,
    },
    SignTreasuryTransfer {
        request: TreasurySignRequest,
    },
    SignTreasuryPathUsd {
        request: TempoTreasurySignRequest,
    },
    AcquireReceiptProbe {
        transaction_hash: String,
    },
    FetchTransactionReceipt {
        transaction_hash: String,
    },
    RecordTreasuryShortfall {
        treasury_balance: U256,
        required_treasury: U256,
        requested: U256,
        minimum: U256,
        top_up_gas_cost: U256,
    },
    RecordTempoTreasuryShortfall {
        treasury_balance: U256,
        required_treasury: U256,
        top_up: U256,
        top_up_gas_limit: u64,
        top_up_gas_cost: U256,
    },
    RecordPartialTopUp {
        requested: U256,
        submitted: U256,
        minimum: U256,
    },
    RecordFundingSubmitted {
        amount: U256,
        transaction_hash: String,
        tempo: bool,
    },
    RecordUnprovenFunding {
        transaction_hash: String,
        ambiguous: bool,
        reason: String,
    },
    NoteFundingReceipt {
        intent: PreparedFundingIntent,
        success: bool,
    },
    SignBundle {
        request: BundleSignRequest,
    },
    SavePreparedBundle {
        intent: PreparedBundleIntent,
    },
    // --- broadcast (sequenced here; judgement in `crate::broadcast`) ---
    CheckBroadcastSeen {
        transaction_hash: String,
    },
    BroadcastRaw {
        raw_transaction: Vec<u8>,
        transaction_hash: String,
    },
    RememberBroadcast {
        transaction_hash: String,
    },
    ForgetBroadcast {
        transaction_hash: String,
    },
    ProbeTransactionKnown {
        transaction_hash: String,
    },
    ProbeStaleNonce {
        intent: PreparedBundleIntent,
    },
    ClearStaleIntent {
        intent: PreparedBundleIntent,
        reason: String,
    },
    /// An unproven broadcast is being retained for retry; the shell records
    /// the appropriate diagnostic.
    RecordUnprovenBroadcast {
        transaction_hash: String,
        ambiguous: bool,
        reason: String,
    },
    MarkBundleSubmitted {
        intent: PreparedBundleIntent,
        gas_limit: u64,
    },
    /// Tempo fee/nonce/pathUSD-balance context for the outer `0x76`.
    FetchTempoContext,
    SignTempoBundle {
        request: TempoSignRequest,
    },
}

/// Why a triaged or simulated operation is being rejected — carried on the
/// operation so the shell can emit its historical diagnostics.
#[derive(Debug, PartialEq)]
pub enum RejectionCause {
    InvalidQueuedPayload {
        reason: &'static str,
    },
    SimulationRejected {
        reason: String,
    },
    /// The account nonce has already been consumed on-chain.
    StaleNonce {
        user_nonce: U256,
        onchain_nonce: U256,
    },
    /// A Tempo wallet extension requested a fee token other than pathUSD.
    UnsupportedTempoFeeToken {
        fee_token: Option<Address>,
    },
}

/// Why an item is entering the durable delayed inbox.
#[derive(Debug, PartialEq)]
pub enum DeferCause {
    /// Waiting for the market to fit the signed reimbursement (US2 hold).
    AffordableMarketHold,
    /// A keyed nonce ahead of the account's on-chain nonce.
    FutureNonce {
        user_nonce: U256,
        onchain_nonce: U256,
    },
}

/// The historical operator log lines of the old engine whose data lives only
/// in core decisions. One variant per line; the shell's `EmitDiagnostic` arm
/// carries the byte-identical message and field names.
#[derive(Debug, PartialEq)]
pub enum ExecutionDiagnostic {
    /// info: "single-operation simulation is waiting for automatic contract deployment"
    SimulationDeploymentWait { hash: String, reason: String },
    /// warn: "single-operation simulation unavailable"
    SimulationUnavailable { hash: String, reason: String },
    /// warn: "final handleOps simulation rejected bundle"
    BundleSimulationRejected { reason: String },
    /// warn: "final handleOps simulation reported an account nonce mismatch"
    BundleSimulationNonceMismatch,
    /// info: "final handleOps simulation is waiting for automatic contract deployment"
    BundleSimulationDeploymentWait { reason: String },
    /// info: "in-band reimbursement cannot fund an includable outer fee"
    FloorUnfundable {
        quoted_fee: u128,
        affordable: u128,
        floor: u128,
        base_fee: u128,
    },
    /// info: "repriced the outer transaction to the signed in-band budget"
    Repriced {
        quoted_fee: u128,
        repriced_fee: u128,
        base_fee: u128,
        tip: u128,
    },
    /// info: "submitting at the client-requested speed"
    ///
    /// `cap` is what the tier resolved to after the clamps, so `cap` below
    /// `base_fee_bps × base + tip` is the reimbursement or the inclusion floor
    /// speaking — the one line that explains why a paid-for speed was not
    /// delivered verbatim.
    ///
    /// `tip` is the tip actually signed (`tip_bps × market_tip`) and
    /// `market_tip` the raw reading it was scaled from. Both are logged
    /// because a tier that moved the cap and not the tip is the defect this
    /// diagnostic exists to make visible: `tip == market_tip` on anything but
    /// `slow` means the scaling was lost.
    SubmissionTierCap {
        tier: SubmissionTier,
        quoted_fee: u128,
        cap: u128,
        base_fee: u128,
        tip: u128,
        market_tip: u128,
    },
    /// warn: "in-band reimbursement stayed unaffordable for the whole hold budget"
    HoldBudgetExhausted {
        hash: String,
        attempt: u32,
        paid: U256,
        required: U256,
    },
    /// warn: "in-band settlement rejected UserOperation"
    SettlementRejected {
        hash: String,
        payment_asset: Option<Address>,
        paid: U256,
        required: U256,
        stable_logs_valid: bool,
    },
    /// warn: "Tempo pathUSD in-band settlement rejected UserOperation"
    TempoSettlementRejected {
        hash: String,
        paid: U256,
        required: U256,
        stable_logs_valid: bool,
    },
    /// debug: "using USD-denominated relayer top-up cap"
    TopUpCapUsd { native_units: U256 },
    /// warn: "could not convert USD relayer top-up cap to native units; using static cap"
    TopUpCapUnconvertible,
    /// warn: "UserOperation lane execution deferred"
    ExecutionDeferred { reason: String },
}

/// Mirror of the node's raw-broadcast reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BroadcastReply {
    Accepted { transaction_hash: String },
    Ambiguous { reason: String },
    Rejected { reason: String },
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "Shell results are constructed once and consumed immediately in-process; boxing every record-bearing variant would add noise at each decision site."
)]
pub enum ExecutionOutcome {
    Supported {
        supported: bool,
    },
    Assets {
        resolved: ResolvedChainAssets,
    },
    AssetsUnavailable {
        reason: String,
    },
    Records {
        records: Vec<Option<StoredUserOperation>>,
    },
    Record {
        record: Option<StoredUserOperation>,
    },
    Persisted {
        persisted: bool,
    },
    Marked {
        marked: bool,
    },
    Deferred {
        attempt: u32,
    },
    Done,
    LeaseAcquired {
        acquired: bool,
    },
    LeaseHeld {
        held: bool,
    },
    Intent {
        intent: Option<PreparedBundleIntent>,
    },
    Resumed {
        known_outcome: bool,
    },
    OperationVerdicts {
        verdicts: Vec<OperationSimVerdict>,
    },
    AccountNonces {
        nonces: Vec<Option<U256>>,
    },
    BundleVerdict {
        verdict: BundleSimVerdict,
    },
    Context {
        context: TransactionContext,
    },
    Price {
        price: U256,
    },
    FundingIntent {
        intent: Option<PreparedFundingIntent>,
    },
    TreasuryContext {
        nonce: u64,
        balance: U256,
    },
    TempoTreasuryContext {
        nonce: u64,
        balance: U256,
        raw_gas_estimate: u64,
    },
    Receipt {
        receipt: Option<Value>,
    },
    Signed {
        signed: SignedBundle,
    },
    Saved {
        saved: bool,
    },
    Seen {
        seen: bool,
    },
    Sent {
        reply: BroadcastReply,
    },
    Known {
        known: bool,
    },
    Stale {
        stale: bool,
    },
    Indexed {
        indexed: usize,
    },
    TempoContext {
        base_fee_atto: U256,
        nonce: u64,
        relayer_path_usd_balance: U256,
    },
    /// Any infrastructure failure, folded to its display text; the program
    /// decides what it means at each step.
    Failed {
        message: String,
    },
    /// The shell's lease heartbeat failed while this operation was pending:
    /// the operation was NOT executed. Mirrors the old engine's mid-await
    /// pipeline abort — [`request`] converts it into the batch-fatal `Err`
    /// channel so the program unwinds without taking further effects.
    Interrupted {
        reason: String,
    },
}

impl crux_core::capability::Operation for ExecutionOperation {
    type Output = ExecutionOutcome;
}

#[effect]
pub enum ExecutionEffect {
    Work(ExecutionOperation),
}

#[derive(Debug)]
pub enum ExecutionEvent {
    Start(Box<StartBatch>),
    Settled(Vec<ItemResolution>),
}

#[derive(Default)]
pub struct ExecutionModel {
    outcome: Option<Vec<ItemResolution>>,
}

pub struct ExecutionViewModel {
    pub outcome: Option<Vec<ItemResolution>>,
}

#[derive(Default)]
pub struct ExecutionApp;

impl App for ExecutionApp {
    type Event = ExecutionEvent;
    type Model = ExecutionModel;
    type ViewModel = ExecutionViewModel;
    type Effect = ExecutionEffect;

    fn update(
        &self,
        event: Self::Event,
        model: &mut Self::Model,
    ) -> Command<Self::Effect, Self::Event> {
        match event {
            ExecutionEvent::Start(start) => Command::new(|ctx| async move {
                let outcome = drive_batch(&ctx, *start).await;
                ctx.send_event(ExecutionEvent::Settled(outcome));
            }),
            ExecutionEvent::Settled(outcome) => {
                model.outcome = Some(outcome);
                Command::done()
            }
        }
    }

    fn view(&self, model: &Self::Model) -> Self::ViewModel {
        ExecutionViewModel {
            outcome: model.outcome.clone(),
        }
    }
}

type Ctx = crux_core::command::CommandContext<ExecutionEffect, ExecutionEvent>;

/// A leased-pipeline operation. `Err` is a lease-heartbeat interrupt: the
/// operation did not run, and the whole batch unwinds through the transient
/// `Err(reason)` channel — the same disposition the old engine reached by
/// dropping the pipeline future mid-await. Infrastructure failures are NOT
/// errors here: they arrive as `Ok(Failed { .. })` result data and each step
/// keeps deciding their meaning.
async fn request(ctx: &Ctx, operation: ExecutionOperation) -> Result<ExecutionOutcome, String> {
    match ctx.request_from_shell(operation).await {
        ExecutionOutcome::Interrupted { reason } => Err(reason),
        outcome => Ok(outcome),
    }
}

/// A triage or bookkeeping operation the shell answers even while a lease
/// interrupt is pending: everything before the lane lease exists (no
/// heartbeat yet), plus deferral diagnostics, Telegram, and log emission —
/// which the old engine likewise performed after aborting the pipeline.
async fn request_unfenced(ctx: &Ctx, operation: ExecutionOperation) -> ExecutionOutcome {
    ctx.request_from_shell(operation).await
}

async fn emit_diagnostic(ctx: &Ctx, diagnostic: ExecutionDiagnostic) {
    let _ = request_unfenced(ctx, ExecutionOperation::EmitDiagnostic { diagnostic }).await;
}

/// Whether an executor deferral is operator-actionable. A lease held by
/// another worker, a freshly submitted funding/deployment transaction, and an
/// in-budget market hold are expected hand-offs; Telegram is reserved for
/// work that is actually blocked. The allowlist (not a denylist) is the
/// contract: a newly introduced stage stays silent until deliberately added.
pub fn should_notify_executor_deferred(stage: &str) -> bool {
    matches!(
        stage,
        "rpc" | "assets" | "simulation" | "bundle_simulation" | "broadcast" | "execution"
    )
}

/// Validation of one routed envelope for durable-payload restoration.
/// (Formerly the shell's `queued_operation_from_routed`.)
pub fn queued_operation_from_routed(
    routed: &RoutedUserOperation,
    pool_width: usize,
) -> Result<QueuedUserOperation, &'static str> {
    let operation = serde_json::from_value::<UserOperation>(routed.user_operation.clone())
        .map_err(|_| "queue UserOperation payload is not canonical v0.7 JSON")?;
    if serde_json::to_value(&operation).ok().as_ref() != Some(&routed.user_operation) {
        return Err("queue UserOperation payload is not canonical JSON");
    }
    let entry_point = parse_address(&routed.entry_point, "queue EntryPoint address is invalid")?;
    let hash = parse_hash(
        &routed.user_operation_hash,
        "queue UserOperation hash is invalid",
    )?;
    let packed =
        PackedOperation::try_from(&operation).map_err(|_| "could not pack queue UserOperation")?;
    if packed.has_eip7702_authorization {
        return Err("EIP-7702 UserOperations are not enabled in the executor");
    }
    if !packed
        .sender
        .to_string()
        .eq_ignore_ascii_case(&routed.sender)
    {
        return Err("queue sender does not match UserOperation sender");
    }
    if relayer_index_for_sender(&routed.sender, pool_width) != routed.lane as usize {
        return Err("sender route does not match relayer lane");
    }
    if user_operation_hash(&packed, entry_point, routed.chain_id) != hash {
        return Err("queue UserOperation hash does not match immutable payload");
    }
    Ok(QueuedUserOperation {
        user_operation_hash: routed.user_operation_hash.to_ascii_lowercase(),
        chain_id: routed.chain_id,
        entry_point: routed.entry_point.clone(),
        user_operation: operation,
    })
}

/// A record admitted for execution, packed and cross-checked against its
/// envelope. (Formerly the shell's `candidate_from_record`.)
pub struct Candidate {
    pub result_index: usize,
    pub hash: B256,
    pub hash_string: String,
    pub entry_point: Address,
    pub packed: PackedOperation,
    /// The submission speed this operation's client named, carried over from
    /// its queue envelope.
    pub submission_tier: Option<SubmissionTier>,
}

pub fn candidate_from_record(
    result_index: usize,
    routed: &RoutedUserOperation,
    record: &StoredUserOperation,
    pool_width: usize,
) -> Result<Candidate, &'static str> {
    let hash = parse_hash(
        &routed.user_operation_hash,
        "queue UserOperation hash is invalid",
    )?;
    let entry_point = parse_address(&routed.entry_point, "queue EntryPoint address is invalid")?;
    if relayer_index_for_sender(&routed.sender, pool_width) != routed.lane as usize {
        return Err("sender route does not match relayer lane");
    }
    let packed = PackedOperation::try_from(&record.user_operation)
        .map_err(|_| "could not pack queued UserOperation")?;
    if packed.has_eip7702_authorization {
        return Err("EIP-7702 UserOperations are not enabled in the executor");
    }
    if !packed
        .sender
        .to_string()
        .eq_ignore_ascii_case(&routed.sender)
    {
        return Err("queue sender does not match UserOperation sender");
    }
    if user_operation_hash(&packed, entry_point, routed.chain_id) != hash {
        return Err("queue UserOperation hash does not match immutable payload");
    }
    Ok(Candidate {
        result_index,
        hash,
        hash_string: routed.user_operation_hash.to_ascii_lowercase(),
        entry_point,
        packed,
        submission_tier: routed.submission_tier,
    })
}

/// The speed one outer transaction submits at, given what its operations asked
/// for.
///
/// `None` — nobody asked — is the guarantee that a bundle of ordinary
/// operations never touches the tier machinery at all.
///
/// Otherwise the bundle takes the FASTEST speed any of its operations named,
/// and an operation that named none counts as `Standard`, because standard IS
/// the pace the relay keeps on its own. So a neighbour can never slow an
/// operation below today's cap, and the operation that paid for speed gets it.
/// A higher cap costs nothing on the chain — it charges `base + tip` whatever
/// the cap says — and the settlement clamp downstream still holds the cap to
/// what the WEAKEST payer in the bundle funds, so a fast neighbour can never
/// price a slow one out of its own transaction.
pub fn bundle_submission_tier(candidates: &[Candidate]) -> Option<SubmissionTier> {
    candidates
        .iter()
        .any(|candidate| candidate.submission_tier.is_some())
        .then(|| {
            candidates
                .iter()
                .map(|candidate| candidate.submission_tier.unwrap_or_default())
                .max()
                .unwrap_or_default()
        })
}

/// Whether the queue envelope still matches the admitted record.
pub fn queue_record_matches(routed: &RoutedUserOperation, record: &StoredUserOperation) -> bool {
    record.chain_id == routed.chain_id
        && record.entry_point.eq_ignore_ascii_case(&routed.entry_point)
        && serde_json::to_value(&record.user_operation).ok().as_ref()
            == Some(&routed.user_operation)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionAction {
    Execute,
    Recover,
    DeadLetter,
}

pub fn admission_action(admitted: bool, envelope_matches: bool) -> AdmissionAction {
    match (admitted, envelope_matches) {
        (_, false) => AdmissionAction::DeadLetter,
        (false, true) => AdmissionAction::Recover,
        (true, true) => AdmissionAction::Execute,
    }
}

fn parse_address(value: &str, error: &'static str) -> Result<Address, &'static str> {
    value.parse::<Address>().map_err(|_| error)
}

fn parse_hash(value: &str, error: &'static str) -> Result<B256, &'static str> {
    value.parse::<B256>().map_err(|_| error)
}

struct Results {
    slots: Vec<Option<ItemResolution>>,
}

impl Results {
    fn new(len: usize) -> Self {
        Self {
            slots: vec![None; len],
        }
    }

    fn durable(&mut self, index: usize) {
        self.slots[index] = Some(ItemResolution::Durable);
    }

    fn failed(&mut self, index: usize, reason: impl Into<String>) {
        self.slots[index] = Some(ItemResolution::Failed {
            reason: reason.into(),
        });
    }

    fn is_settled(&self, index: usize) -> bool {
        self.slots[index].is_some()
    }

    fn finish(self, default_reason: &str) -> Vec<ItemResolution> {
        self.slots
            .into_iter()
            .map(|slot| {
                slot.unwrap_or(ItemResolution::Failed {
                    reason: default_reason.to_owned(),
                })
            })
            .collect()
    }

    fn all_failed(len: usize, reason: &str) -> Vec<ItemResolution> {
        (0..len)
            .map(|_| ItemResolution::Failed {
                reason: reason.to_owned(),
            })
            .collect()
    }
}

/// Best-effort deferral diagnostic plus the Telegram policy. Mirrors the
/// shell's old `record_executor_deferred`.
async fn record_deferred(ctx: &Ctx, hash: &str, stage: &'static str, reason: &str) {
    let _ = request_unfenced(
        ctx,
        ExecutionOperation::RecordDeferred {
            hash: hash.to_owned(),
            stage,
            reason: reason.to_owned(),
        },
    )
    .await;
    if should_notify_executor_deferred(stage) {
        let _ = request_unfenced(
            ctx,
            ExecutionOperation::NotifyIssue {
                hash: hash.to_owned(),
                stage,
                reason: reason.to_owned(),
            },
        )
        .await;
    }
}

/// Deferral diagnostics for every unresolved routed operation. Mirrors
/// `record_routed_deferred`.
async fn record_routed_deferred(
    ctx: &Ctx,
    operations: &[RoutedUserOperation],
    results: Option<&Results>,
    stage: &'static str,
    reason: &str,
) {
    for (index, operation) in operations.iter().enumerate() {
        if results.is_some_and(|results| results.is_settled(index)) {
            continue;
        }
        record_deferred(ctx, &operation.user_operation_hash, stage, reason).await;
    }
}

async fn record_candidates_deferred(
    ctx: &Ctx,
    candidates: &[Candidate],
    stage: &'static str,
    reason: &str,
) {
    for candidate in candidates {
        record_deferred(ctx, &candidate.hash_string, stage, reason).await;
    }
}

const DEFERRED_FINISH: &str = "UserOperation execution was deferred";
const NO_OUTCOME_FINISH: &str = "no durable executor outcome";

/// The whole lane-batch program. Sequential: at most one operation is ever in
/// flight (the Driver tests assert this invariant).
async fn drive_batch(ctx: &Ctx, start: StartBatch) -> Vec<ItemResolution> {
    let StartBatch {
        operations, policy, ..
    } = &start;
    if operations.is_empty() {
        return Vec::new();
    }
    let chain_id = operations[0].chain_id;
    let lane = operations[0].lane;
    if operations
        .iter()
        .any(|operation| operation.chain_id != chain_id || operation.lane != lane)
    {
        return Results::all_failed(
            operations.len(),
            "consumer returned a mixed chain/lane batch",
        );
    }

    match request_unfenced(ctx, ExecutionOperation::CheckChainSupported).await {
        ExecutionOutcome::Supported { supported: true } => {}
        _ => {
            let reason = "chain has no trusted executor RPC";
            record_routed_deferred(ctx, operations, None, "rpc", reason).await;
            return Results::all_failed(operations.len(), reason);
        }
    }
    let chain_assets = match request_unfenced(ctx, ExecutionOperation::LoadChainAssets).await {
        ExecutionOutcome::Assets { resolved } => resolved,
        ExecutionOutcome::AssetsUnavailable { reason }
        | ExecutionOutcome::Failed { message: reason } => {
            record_routed_deferred(ctx, operations, None, "assets", &reason).await;
            return Results::all_failed(operations.len(), &reason);
        }
        _ => return Results::all_failed(operations.len(), "unexpected shell response"),
    };

    let hashes = operations
        .iter()
        .map(|operation| operation.user_operation_hash.clone())
        .collect::<Vec<_>>();
    let records = match request_unfenced(ctx, ExecutionOperation::LoadRecords { hashes }).await {
        ExecutionOutcome::Records { records } => records,
        ExecutionOutcome::Failed { message } => {
            return Results::all_failed(operations.len(), &message);
        }
        _ => return Results::all_failed(operations.len(), "unexpected shell response"),
    };

    let mut results = Results::new(operations.len());
    let mut candidates = Vec::new();
    for (index, (routed, record)) in operations.iter().zip(records).enumerate() {
        let mut record = match record {
            Some(record) => record,
            None => {
                let queued = match queued_operation_from_routed(routed, policy.pool_width) {
                    Ok(queued) => queued,
                    Err(reason) => {
                        match request_unfenced(
                            ctx,
                            ExecutionOperation::DeadLetterRouted {
                                index,
                                reason: reason.to_owned(),
                            },
                        )
                        .await
                        {
                            ExecutionOutcome::Persisted { persisted: true } => {
                                results.durable(index);
                            }
                            _ => {
                                results.failed(index, "could not persist invalid queue message");
                            }
                        }
                        continue;
                    }
                };
                match request_unfenced(ctx, ExecutionOperation::RestoreQueued { index, queued })
                    .await
                {
                    ExecutionOutcome::Done => {}
                    ExecutionOutcome::Failed { message } => {
                        results.failed(index, &message);
                        continue;
                    }
                    _ => {
                        results.failed(index, "unexpected shell response");
                        continue;
                    }
                }
                match request_unfenced(
                    ctx,
                    ExecutionOperation::ReloadRecord {
                        hash: routed.user_operation_hash.clone(),
                    },
                )
                .await
                {
                    ExecutionOutcome::Record {
                        record: Some(record),
                    } => record,
                    ExecutionOutcome::Record { record: None } => {
                        results.failed(
                            index,
                            "rebuilt UserOperation status disappeared before execution",
                        );
                        continue;
                    }
                    ExecutionOutcome::Failed { message } => {
                        results.failed(index, &message);
                        continue;
                    }
                    _ => {
                        results.failed(index, "unexpected shell response");
                        continue;
                    }
                }
            }
        };
        if record.status.is_durable() {
            results.durable(index);
            continue;
        }
        match admission_action(record.admitted, queue_record_matches(routed, &record)) {
            AdmissionAction::DeadLetter => {
                match request_unfenced(
                    ctx,
                    ExecutionOperation::DeadLetterRouted {
                        index,
                        reason: "Iggy envelope does not match Redis admission".to_owned(),
                    },
                )
                .await
                {
                    ExecutionOutcome::Persisted { persisted: true } => results.durable(index),
                    _ => results.failed(index, "could not persist mismatched queue message"),
                }
                continue;
            }
            AdmissionAction::Recover => {
                match request_unfenced(
                    ctx,
                    ExecutionOperation::MarkAdmitted {
                        hash: routed.user_operation_hash.clone(),
                    },
                )
                .await
                {
                    ExecutionOutcome::Marked { marked: true } => record.admitted = true,
                    ExecutionOutcome::Marked { marked: false } => {
                        results.failed(index, "could not recover expired UserOperation admission");
                        continue;
                    }
                    ExecutionOutcome::Failed { message } => {
                        results.failed(index, &message);
                        continue;
                    }
                    _ => {
                        results.failed(index, "unexpected shell response");
                        continue;
                    }
                }
            }
            AdmissionAction::Execute => {}
        }
        match candidate_from_record(index, routed, &record, policy.pool_width) {
            Ok(candidate) => candidates.push(candidate),
            Err(reason) => {
                match request_unfenced(
                    ctx,
                    ExecutionOperation::MarkRejected {
                        hash: routed.user_operation_hash.clone(),
                        cause: RejectionCause::InvalidQueuedPayload { reason },
                    },
                )
                .await
                {
                    ExecutionOutcome::Failed { message } => results.failed(index, &message),
                    _ => results.durable(index),
                }
            }
        }
    }
    if candidates.is_empty() {
        return results.finish(NO_OUTCOME_FINISH);
    }

    // Never put two nonces from the same sender into one outer transaction.
    let mut unique_hashes = HashSet::new();
    candidates.retain(|candidate| unique_hashes.insert(candidate.hash));
    let mut senders = HashSet::new();
    candidates.retain(|candidate| {
        senders.insert(candidate.packed.sender) && candidate.result_index < operations.len()
    });
    candidates.truncate(policy.max_bundle_operations);

    match request_unfenced(ctx, ExecutionOperation::AcquireLaneLease).await {
        ExecutionOutcome::LeaseAcquired { acquired: true } => {}
        _ => {
            record_routed_deferred(
                ctx,
                operations,
                Some(&results),
                "lease",
                "relayer lane is currently owned by another worker",
            )
            .await;
            return results.finish("relayer lane is owned by another worker");
        }
    }

    match execute_with_lane_lease(ctx, &start, chain_assets, candidates, &mut results).await {
        Ok(()) => results.finish(DEFERRED_FINISH),
        Err(reason) => {
            record_routed_deferred(ctx, &start.operations, Some(&results), "execution", &reason)
                .await;
            emit_diagnostic(ctx, ExecutionDiagnostic::ExecutionDeferred { reason }).await;
            results.finish(DEFERRED_FINISH)
        }
    }
}

/// The leased execution pipeline. `Err(reason)` is the transient-deferral
/// channel (the old `ExecutorItemError` path), not a failure of the program.
async fn execute_with_lane_lease(
    ctx: &Ctx,
    start: &StartBatch,
    chain_assets: ResolvedChainAssets,
    mut candidates: Vec<Candidate>,
    results: &mut Results,
) -> Result<(), String> {
    let policy = &start.policy;

    match request(ctx, ExecutionOperation::LoadPreparedBundle).await? {
        ExecutionOutcome::Intent { intent: None } => {}
        ExecutionOutcome::Intent {
            intent: Some(intent),
        } => {
            let known = match request(
                ctx,
                ExecutionOperation::ResumeBundleIntent {
                    intent: intent.clone(),
                },
            )
            .await?
            {
                ExecutionOutcome::Resumed { known_outcome } => known_outcome,
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            };
            if known {
                for candidate in candidates {
                    if intent
                        .user_operation_hashes
                        .iter()
                        .any(|hash| hash.eq_ignore_ascii_case(&candidate.hash_string))
                    {
                        results.durable(candidate.result_index);
                    }
                }
            }
            return Ok(());
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }

    let entry_point = candidates[0].entry_point;
    if candidates
        .iter()
        .any(|candidate| candidate.entry_point != entry_point)
    {
        return Err("one lane batch contains multiple EntryPoints".to_owned());
    }

    let verdicts = match request(
        ctx,
        ExecutionOperation::SimulateIndividually {
            entry_point,
            operations: candidates
                .iter()
                .map(|candidate| (candidate.hash, candidate.packed.clone()))
                .collect(),
        },
    )
    .await?
    {
        ExecutionOutcome::OperationVerdicts { verdicts } => verdicts,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    if verdicts.len() != candidates.len() {
        return Err("simulation verdicts do not match candidates".to_owned());
    }

    let mut survivors = Vec::new();
    let mut nonce_mismatches = Vec::new();
    for (candidate, verdict) in candidates.drain(..).zip(verdicts) {
        match verdict {
            OperationSimVerdict::Success => survivors.push(candidate),
            OperationSimVerdict::NonceMismatch => nonce_mismatches.push(candidate),
            OperationSimVerdict::Rejected { reason } => {
                // A store failure here defers the whole lane batch: nothing
                // may broadcast while a rejection is only half-recorded.
                if let ExecutionOutcome::Failed { message } = request(
                    ctx,
                    ExecutionOperation::MarkRejected {
                        hash: candidate.hash_string.clone(),
                        cause: RejectionCause::SimulationRejected {
                            reason: reason.clone(),
                        },
                    },
                )
                .await?
                {
                    return Err(message);
                }
                let _ = request_unfenced(
                    ctx,
                    ExecutionOperation::NotifyIssue {
                        hash: candidate.hash_string.clone(),
                        stage: "simulation",
                        reason: reason.clone(),
                    },
                )
                .await;
                results.durable(candidate.result_index);
            }
            OperationSimVerdict::Pending { reason } => {
                record_deferred(
                    ctx,
                    &candidate.hash_string,
                    "simulation_deployment",
                    &reason,
                )
                .await;
                emit_diagnostic(
                    ctx,
                    ExecutionDiagnostic::SimulationDeploymentWait {
                        hash: candidate.hash_string.clone(),
                        reason,
                    },
                )
                .await;
            }
            OperationSimVerdict::Transient { reason } => {
                record_deferred(ctx, &candidate.hash_string, "simulation", &reason).await;
                emit_diagnostic(
                    ctx,
                    ExecutionDiagnostic::SimulationUnavailable {
                        hash: candidate.hash_string.clone(),
                        reason,
                    },
                )
                .await;
            }
        }
    }
    if !nonce_mismatches.is_empty() {
        resolve_nonce_mismatches(ctx, entry_point, &nonce_mismatches, results).await?;
    }
    if survivors.is_empty() {
        return Ok(());
    }
    ensure_lane_lease(ctx).await?;

    // If a multi-op bundle has a state interaction that does not exist in
    // isolated simulation, fall back to the first op. Later ops stay queued
    // instead of poisoning the whole handleOps transaction.
    let mut bundle_verdict = simulate_bundle(ctx, entry_point, &survivors).await?;
    if matches!(
        bundle_verdict,
        BundleSimVerdict::Rejected { .. } | BundleSimVerdict::NonceMismatch
    ) && survivors.len() > 1
    {
        survivors.truncate(1);
        bundle_verdict = simulate_bundle(ctx, entry_point, &survivors).await?;
    }
    let bundle_simulation = match bundle_verdict {
        BundleSimVerdict::Success(simulation) => simulation,
        BundleSimVerdict::Rejected { reason } => {
            record_candidates_deferred(ctx, &survivors, "bundle_simulation", &reason).await;
            emit_diagnostic(
                ctx,
                ExecutionDiagnostic::BundleSimulationRejected { reason },
            )
            .await;
            return Ok(());
        }
        BundleSimVerdict::NonceMismatch => {
            record_candidates_deferred(
                ctx,
                &survivors,
                "bundle_simulation",
                "final handleOps simulation reported an account nonce mismatch",
            )
            .await;
            emit_diagnostic(ctx, ExecutionDiagnostic::BundleSimulationNonceMismatch).await;
            return Ok(());
        }
        BundleSimVerdict::Pending { reason } => {
            record_candidates_deferred(ctx, &survivors, "simulation_deployment", &reason).await;
            emit_diagnostic(
                ctx,
                ExecutionDiagnostic::BundleSimulationDeploymentWait { reason },
            )
            .await;
            return Ok(());
        }
        BundleSimVerdict::Transient { reason } => return Err(reason),
    };
    ensure_lane_lease(ctx).await?;

    if policy.is_tempo {
        return execute_tempo_bundle(
            ctx,
            start,
            &chain_assets,
            entry_point,
            survivors,
            bundle_simulation,
            results,
        )
        .await;
    }

    let calldata = handle_ops_calldata(
        &survivors
            .iter()
            .map(|candidate| candidate.packed.packed.clone())
            .collect::<Vec<_>>(),
        start_treasury(start),
    );
    let mut context = match request(
        ctx,
        ExecutionOperation::FetchTransactionContext {
            entry_point,
            calldata: calldata.to_vec(),
        },
    )
    .await?
    {
        ExecutionOutcome::Context { context } => context,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let allocations = allocate_bundle_gas(
        bundle_simulation.gas_used,
        context.estimated_gas,
        &bundle_simulation.operation_gas_used,
        policy.gas_buffer_bps,
        policy.fixed_gas_buffer,
    )
    .ok_or_else(|| "bundle gas allocation overflow".to_owned())?;

    // --- settlement gate (US3 verdict + US2 hold, composed here) ---
    let call_datas = survivors
        .iter()
        .map(|candidate| candidate.packed.call_data.as_ref())
        .collect::<Vec<&[u8]>>();
    let treasury = start_treasury(start);
    let chain_id = start.operations[0].chain_id;
    let native_usd_price = if has_stablecoin_payment(treasury, &chain_assets.assets, &call_datas) {
        // xDAI is USD-pegged: Gnosis settlement never consults the market.
        match crate::settlement::pegged_native_usd_price(chain_id) {
            Some(price) => Some(price),
            None => match request(ctx, ExecutionOperation::FetchMarketPrice).await? {
                ExecutionOutcome::Price { price } => Some(price),
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            },
        }
    } else {
        None
    };
    let mut fees = FeeContext {
        quoted_fee_per_gas: context.max_fee_per_gas,
        base_fee_per_gas: context.base_fee_per_gas,
        max_priority_fee_per_gas: context.max_priority_fee_per_gas,
        inclusion_floor_bps: policy.settlement_inclusion_floor_bps,
        requested_tier: bundle_submission_tier(&survivors),
    };
    // A client may name how fast it wants its operation in. Resolving the name
    // here, against the base fee and tip the shell just read, is what makes a
    // stale quote harmless: the client never names a price, only a speed. When
    // it named nothing this returns `None` and the fees below are the ones the
    // executor has always used.
    //
    // BOTH levers are assigned. The cap alone buys resilience to a base-fee
    // spike; the tip is what a block builder orders by, so assigning only the
    // cap — as this did — left every tier with the same effective tip and made
    // `fast` cost more for no priority at all.
    let market_tip = fees.max_priority_fee_per_gas;
    if let Some(tier) = fees.requested_tier
        && let Some(outer) = crate::settlement::decide_submission_fees(
            treasury,
            &chain_assets.assets,
            &call_datas,
            &allocations,
            native_usd_price,
            &fees,
        )
        .map_err(|error| error.to_string())?
    {
        emit_diagnostic(
            ctx,
            ExecutionDiagnostic::SubmissionTierCap {
                tier,
                quoted_fee: fees.quoted_fee_per_gas,
                cap: outer.max_fee_per_gas,
                base_fee: fees.base_fee_per_gas,
                tip: outer.max_priority_fee_per_gas,
                market_tip,
            },
        )
        .await;
        fees.quoted_fee_per_gas = outer.max_fee_per_gas;
        // The floor `decide_settlement` may reprice down to is computed from
        // this tip, so carrying it into `fees` is what stops a reprice giving
        // back the priority the client paid for.
        fees.max_priority_fee_per_gas = outer.max_priority_fee_per_gas;
        context.max_fee_per_gas = outer.max_fee_per_gas;
        context.max_priority_fee_per_gas = outer.max_priority_fee_per_gas;
    }
    let settlement = match decide_settlement(
        treasury,
        &chain_assets.assets,
        &call_datas,
        &allocations,
        native_usd_price,
        &fees,
    )
    .map_err(|error| error.to_string())?
    {
        SettlementDecision::KeepQuote { evaluation } => evaluation,
        SettlementDecision::FloorUnfundable {
            evaluation,
            affordable,
            floor,
        } => {
            emit_diagnostic(
                ctx,
                ExecutionDiagnostic::FloorUnfundable {
                    quoted_fee: fees.quoted_fee_per_gas,
                    affordable,
                    floor,
                    base_fee: fees.base_fee_per_gas,
                },
            )
            .await;
            evaluation
        }
        SettlementDecision::Reprice {
            fee_per_gas,
            evaluation,
        } => {
            emit_diagnostic(
                ctx,
                ExecutionDiagnostic::Repriced {
                    quoted_fee: fees.quoted_fee_per_gas,
                    repriced_fee: fee_per_gas,
                    base_fee: fees.base_fee_per_gas,
                    tip: fees.max_priority_fee_per_gas,
                },
            )
            .await;
            context.max_fee_per_gas = fee_per_gas;
            evaluation
        }
    };

    let mut rejected_any = false;
    for (candidate, evaluation) in survivors.iter().zip(&settlement.operations) {
        let stable_logs_valid = verify_stable_transfer_logs(
            &evaluation.reimbursement,
            candidate.packed.sender,
            treasury,
            &bundle_simulation.logs,
        );
        if evaluation.accepted() && stable_logs_valid {
            continue;
        }
        if evaluation.is_shortfall() && stable_logs_valid {
            // Hold: park in the delayed inbox, judge the budget on the
            // post-increment attempt.
            if let ExecutionOutcome::Deferred { attempt } = request(
                ctx,
                ExecutionOperation::DeferOperation {
                    index: candidate.result_index,
                    cause: DeferCause::AffordableMarketHold,
                },
            )
            .await?
            {
                match decide_hold(
                    attempt,
                    policy.settlement_hold_max_attempts,
                    evaluation.paid_amount,
                    evaluation.required_amount,
                ) {
                    HoldDecision::Hold { reason } => {
                        record_deferred(
                            ctx,
                            &candidate.hash_string,
                            "in_band_settlement_hold",
                            &reason,
                        )
                        .await;
                        results.durable(candidate.result_index);
                        rejected_any = true;
                        continue;
                    }
                    HoldDecision::RejectBudgetExhausted => {
                        emit_diagnostic(
                            ctx,
                            ExecutionDiagnostic::HoldBudgetExhausted {
                                hash: candidate.hash_string.clone(),
                                attempt,
                                paid: evaluation.paid_amount,
                                required: evaluation.required_amount,
                            },
                        )
                        .await;
                    }
                }
            }
        }
        let reason = settlement_rejection_reason(
            evaluation.paid_amount,
            evaluation.required_amount,
            stable_logs_valid,
        );
        if let ExecutionOutcome::Failed { message } = request(
            ctx,
            ExecutionOperation::MarkRejectedWithReason {
                hash: candidate.hash_string.clone(),
                stage: "in_band_settlement",
                reason: reason.clone(),
            },
        )
        .await?
        {
            return Err(message);
        }
        results.durable(candidate.result_index);
        rejected_any = true;
        emit_diagnostic(
            ctx,
            ExecutionDiagnostic::SettlementRejected {
                hash: candidate.hash_string.clone(),
                payment_asset: evaluation.payment_asset,
                paid: evaluation.paid_amount,
                required: evaluation.required_amount,
                stable_logs_valid,
            },
        )
        .await;
        let _ = request_unfenced(
            ctx,
            ExecutionOperation::NotifyIssue {
                hash: candidate.hash_string.clone(),
                stage: "in_band_settlement",
                reason,
            },
        )
        .await;
    }
    if rejected_any {
        // Reassemble on the next queue delivery so the cost allocation and
        // aggregate estimate never include a rejected payer.
        return Ok(());
    }

    let gas_limit = allocations
        .iter()
        .try_fold(U256::ZERO, |sum, gas| sum.checked_add(*gas))
        .ok_or_else(|| "bundle gas limit overflow".to_owned())?;
    let gas_limit =
        u64::try_from(gas_limit).map_err(|_| "bundle gas limit exceeds uint64".to_owned())?;
    let prefund = U256::from(gas_limit)
        .checked_mul(U256::from(context.max_fee_per_gas))
        .ok_or_else(|| "bundle prefund overflow".to_owned())?;

    // Per-transfer top-up cap: USD-denominated when a price exists (pegged on
    // Gnosis, otherwise from the market), failing open to the static wei cap.
    let top_up_price = match crate::settlement::pegged_native_usd_price(chain_id) {
        Some(price) => Some(price),
        None => match request(ctx, ExecutionOperation::FetchMarketPrice).await? {
            ExecutionOutcome::Price { price } => Some(price),
            _ => None,
        },
    };
    let top_up_max = match top_up_price {
        // No usable market price: silently keep the operator's static cap.
        None => U256::from(policy.top_up_max_wei),
        Some(price) => match native_amount_for_usd_cap(
            chain_assets.assets.native_decimals,
            price,
            NATIVE_TOP_UP_USD_CAP,
        ) {
            Some(cap) => {
                emit_diagnostic(ctx, ExecutionDiagnostic::TopUpCapUsd { native_units: cap }).await;
                cap
            }
            None => {
                emit_diagnostic(ctx, ExecutionDiagnostic::TopUpCapUnconvertible).await;
                U256::from(policy.top_up_max_wei)
            }
        },
    };

    // The current bundle takes precedence over filling the relayer float.
    if context.relayer_balance < prefund
        && !ensure_native_funding(
            ctx,
            policy,
            chain_id,
            context.relayer_balance,
            prefund,
            context.max_fee_per_gas,
            context.max_priority_fee_per_gas,
            top_up_max,
        )
        .await?
    {
        record_candidates_deferred(
            ctx,
            &survivors,
            "funding",
            "waiting for relayer funding transaction confirmation",
        )
        .await;
        return Ok(());
    }
    ensure_lane_lease(ctx).await?;

    // The one invariant every path above must leave true: the cap covers the
    // base fee plus the whole signed tip. Below `base + tip` a builder sees
    // only `cap − base` of priority, so the tier the client paid for would be
    // silently truncated; below `base` the transaction is not includable at
    // all, and geth refuses `maxPriorityFeePerGas > maxFeePerGas` outright.
    // Every producer of these two numbers — the untiered quote, the tier
    // resolution and the reprice — is bounded to satisfy it, so this is
    // unreachable; it fails the batch rather than sign a transaction that
    // cannot deliver what was charged for.
    let outer_fee = crate::gas_math::OuterFee {
        max_fee_per_gas: context.max_fee_per_gas,
        max_priority_fee_per_gas: context.max_priority_fee_per_gas,
    };
    if !outer_fee.delivers_full_tip_at(context.base_fee_per_gas) {
        return Err(format!(
            "outer fee cap {} cannot carry the {} tip at base fee {}",
            context.max_fee_per_gas, context.max_priority_fee_per_gas, context.base_fee_per_gas
        ));
    }

    let signed = match request(
        ctx,
        ExecutionOperation::SignBundle {
            request: BundleSignRequest {
                nonce: context.nonce,
                gas_limit,
                max_fee_per_gas: outer_fee.max_fee_per_gas,
                max_priority_fee_per_gas: outer_fee.max_priority_fee_per_gas,
                entry_point,
                calldata: calldata.to_vec(),
            },
        },
    )
    .await?
    {
        ExecutionOutcome::Signed { signed } => signed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let intent = PreparedBundleIntent {
        chain_id: start.operations[0].chain_id,
        lane: start.operations[0].lane,
        entry_point: entry_point.to_string(),
        raw_transaction: signed.raw_transaction_hex.clone(),
        transaction_hash: signed.transaction_hash.clone(),
        nonce: signed.nonce,
        user_operation_hashes: survivors
            .iter()
            .map(|candidate| candidate.hash_string.clone())
            .collect(),
    };
    ensure_lane_lease(ctx).await?;
    match request(
        ctx,
        ExecutionOperation::SavePreparedBundle {
            intent: intent.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Saved { saved: true } => {}
        ExecutionOutcome::Saved { saved: false } => {
            // Raced: another writer holds an intent; resume that one instead.
            let existing = match request(ctx, ExecutionOperation::LoadPreparedBundle).await? {
                ExecutionOutcome::Intent {
                    intent: Some(existing),
                } => existing,
                ExecutionOutcome::Intent { intent: None } => {
                    return Err("prepared bundle raced and disappeared".to_owned());
                }
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            };
            match request(
                ctx,
                ExecutionOperation::ResumeBundleIntent { intent: existing },
            )
            .await?
            {
                ExecutionOutcome::Resumed { .. } => {}
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            }
            return Ok(());
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    if !broadcast_bundle(ctx, &intent).await? {
        record_candidates_deferred(
            ctx,
            &survivors,
            "broadcast",
            "signed handleOps transaction awaits broadcast confirmation",
        )
        .await;
        return Ok(());
    }
    let indexed = match request(
        ctx,
        ExecutionOperation::MarkBundleSubmitted {
            intent: intent.clone(),
            gas_limit,
        },
    )
    .await?
    {
        ExecutionOutcome::Indexed { indexed } => indexed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    if indexed != intent.user_operation_hashes.len() {
        return Err("not every signed UserOperation entered submitted state".to_owned());
    }
    for candidate in survivors {
        results.durable(candidate.result_index);
    }
    Ok(())
}

/// The Tempo `0x76` tail: fee-token gate, pathUSD settlement gate, funding,
/// sign, persist, broadcast, mark — the pathUSD twin of the EIP-1559 tail.
async fn execute_tempo_bundle(
    ctx: &Ctx,
    start: &StartBatch,
    chain_assets: &ResolvedChainAssets,
    entry_point: Address,
    survivors: Vec<Candidate>,
    simulation: BundleSimulationData,
    results: &mut Results,
) -> Result<(), String> {
    let treasury = start_treasury(start);
    let chain_id = start.operations[0].chain_id;
    let lane = start.operations[0].lane;

    // A generic token here would make the treasury's pathUSD float unable to
    // replenish the relayer. Accept the wallet extension only when it agrees
    // with the protocol default; omitted `feeToken` canonically means pathUSD.
    if let Some(candidate) = survivors.iter().find(|candidate| {
        candidate
            .packed
            .fee_token
            .is_some_and(|fee_token| fee_token != crate::tempo::PATH_USD)
    }) {
        match request(
            ctx,
            ExecutionOperation::MarkRejected {
                hash: candidate.hash_string.clone(),
                cause: RejectionCause::UnsupportedTempoFeeToken {
                    fee_token: candidate.packed.fee_token,
                },
            },
        )
        .await?
        {
            ExecutionOutcome::Failed { message } => return Err(message),
            _ => results.durable(candidate.result_index),
        }
        return Ok(());
    }

    let calldata = handle_ops_calldata(
        &survivors
            .iter()
            .map(|candidate| candidate.packed.packed.clone())
            .collect::<Vec<_>>(),
        treasury,
    );
    let context = match request(ctx, ExecutionOperation::FetchTempoContext).await? {
        ExecutionOutcome::TempoContext {
            base_fee_atto,
            nonce,
            relayer_path_usd_balance,
        } => (base_fee_atto, nonce, relayer_path_usd_balance),
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let (base_fee_atto, nonce, relayer_path_usd_balance) = context;
    let allocations = allocate_bundle_gas(
        simulation.gas_used,
        simulation.gas_used,
        &simulation.operation_gas_used,
        0,
        crate::tempo::TEMPO_COST_BUFFER_GAS,
    )
    .ok_or_else(|| "Tempo bundle gas allocation overflow".to_owned())?;
    let costs = allocations
        .iter()
        .map(|gas| {
            crate::tempo::tempo_cost_in_path_usd(*gas, base_fee_atto)
                .ok_or_else(|| "Tempo pathUSD cost overflow".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let allowlist = std::collections::BTreeSet::from([crate::tempo::PATH_USD]);
    let mut rejected_any = false;
    for (candidate, cost) in survivors.iter().zip(&costs) {
        let reimbursement = crate::settlement::parse_reimbursement(
            candidate.packed.call_data.as_ref(),
            treasury,
            &allowlist,
        );
        let (paid, stable_logs_valid) = match reimbursement {
            Ok(reimbursement) => (
                reimbursement
                    .stablecoins
                    .get(&crate::tempo::PATH_USD)
                    .copied()
                    .unwrap_or_default(),
                verify_stable_transfer_logs(
                    &reimbursement,
                    candidate.packed.sender,
                    treasury,
                    &simulation.logs,
                ),
            ),
            Err(_) => (U256::ZERO, false),
        };
        let required =
            crate::settlement::marked_tempo_cost(*cost, chain_assets.assets.settlement_markup_bps)
                .ok_or_else(|| "Tempo settlement markup overflow".to_owned())?;
        if paid < required || !stable_logs_valid {
            let reason = settlement_rejection_reason(paid, required, stable_logs_valid);
            if let ExecutionOutcome::Failed { message } = request(
                ctx,
                ExecutionOperation::MarkRejectedWithReason {
                    hash: candidate.hash_string.clone(),
                    stage: "in_band_settlement",
                    reason: reason.clone(),
                },
            )
            .await?
            {
                return Err(message);
            }
            results.durable(candidate.result_index);
            rejected_any = true;
            emit_diagnostic(
                ctx,
                ExecutionDiagnostic::TempoSettlementRejected {
                    hash: candidate.hash_string.clone(),
                    paid,
                    required,
                    stable_logs_valid,
                },
            )
            .await;
            let _ = request_unfenced(
                ctx,
                ExecutionOperation::NotifyIssue {
                    hash: candidate.hash_string.clone(),
                    stage: "in_band_settlement",
                    reason,
                },
            )
            .await;
        }
    }
    if rejected_any {
        return Ok(());
    }

    let gas_limit = crate::tempo::tempo_handle_ops_gas_limit(
        &survivors
            .iter()
            .map(|candidate| &candidate.packed)
            .collect::<Vec<_>>(),
    )
    .map_err(str::to_owned)?;
    let outer_max_fee = crate::tempo::tempo_outer_max_fee(base_fee_atto).map_err(str::to_owned)?;
    let required_prefund =
        crate::tempo::tempo_cost_in_path_usd(U256::from(gas_limit), U256::from(outer_max_fee))
            .ok_or_else(|| "Tempo pathUSD cost overflow".to_owned())?;
    // The current bundle takes precedence over filling the pathUSD float.
    if relayer_path_usd_balance < required_prefund.max(U256::from(crate::tempo::TEMPO_FLOAT_MIN))
        && !ensure_tempo_funding(
            ctx,
            &start.policy,
            chain_id,
            relayer_path_usd_balance,
            required_prefund,
            outer_max_fee,
        )
        .await?
    {
        record_candidates_deferred(
            ctx,
            &survivors,
            "funding",
            "waiting for relayer pathUSD funding transaction confirmation",
        )
        .await;
        return Ok(());
    }
    ensure_lane_lease(ctx).await?;

    let signed = match request(
        ctx,
        ExecutionOperation::SignTempoBundle {
            request: TempoSignRequest {
                nonce,
                gas_limit,
                max_fee_per_gas: outer_max_fee,
                entry_point,
                calldata: calldata.to_vec(),
            },
        },
    )
    .await?
    {
        ExecutionOutcome::Signed { signed } => signed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let intent = PreparedBundleIntent {
        chain_id,
        lane,
        entry_point: entry_point.to_string(),
        raw_transaction: signed.raw_transaction_hex.clone(),
        transaction_hash: signed.transaction_hash.clone(),
        nonce: signed.nonce,
        user_operation_hashes: survivors
            .iter()
            .map(|candidate| candidate.hash_string.clone())
            .collect(),
    };
    ensure_lane_lease(ctx).await?;
    match request(
        ctx,
        ExecutionOperation::SavePreparedBundle {
            intent: intent.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Saved { saved: true } => {}
        ExecutionOutcome::Saved { saved: false } => {
            let existing = match request(ctx, ExecutionOperation::LoadPreparedBundle).await? {
                ExecutionOutcome::Intent {
                    intent: Some(existing),
                } => existing,
                ExecutionOutcome::Intent { intent: None } => {
                    return Err("prepared Tempo bundle raced and disappeared".to_owned());
                }
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            };
            match request(
                ctx,
                ExecutionOperation::ResumeBundleIntent { intent: existing },
            )
            .await?
            {
                ExecutionOutcome::Resumed { .. } => {}
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            }
            return Ok(());
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    if !broadcast_bundle(ctx, &intent).await? {
        record_candidates_deferred(
            ctx,
            &survivors,
            "broadcast",
            "signed Tempo handleOps transaction awaits broadcast confirmation",
        )
        .await;
        return Ok(());
    }
    let indexed = match request(
        ctx,
        ExecutionOperation::MarkBundleSubmitted {
            intent: intent.clone(),
            gas_limit,
        },
    )
    .await?
    {
        ExecutionOutcome::Indexed { indexed } => indexed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    if indexed != intent.user_operation_hashes.len() {
        return Err("not every signed Tempo UserOperation entered submitted state".to_owned());
    }
    for candidate in survivors {
        results.durable(candidate.result_index);
    }
    Ok(())
}

/// Classification of a prepared bundle's members against their lifecycle
/// records, deciding whether a persisted outbox may be replayed, marked, or
/// cleared. Errors are integrity refusals with byte-frozen texts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BundleReplayAudit {
    pub active: usize,
    pub awaiting_submission: usize,
    pub terminal: usize,
    pub expired: usize,
}

pub fn audit_bundle_replay(
    intent: &PreparedBundleIntent,
    records: &[Option<StoredUserOperation>],
) -> Result<BundleReplayAudit, String> {
    if records.len() != intent.user_operation_hashes.len() {
        return Err("Redis returned incomplete prepared bundle membership".to_owned());
    }

    let mut audit = BundleReplayAudit::default();
    for (hash, record) in intent.user_operation_hashes.iter().zip(records) {
        let Some(record) = record else {
            audit.expired += 1;
            continue;
        };
        if record.chain_id != intent.chain_id
            || !record.entry_point.eq_ignore_ascii_case(&intent.entry_point)
        {
            return Err(format!(
                "prepared bundle member {hash} no longer matches its chain and EntryPoint"
            ));
        }

        match record.status {
            UserOperationStatus::Queued | UserOperationStatus::NotSubmitted => {
                if !record.admitted {
                    return Err(format!(
                        "prepared bundle member {hash} is no longer admitted"
                    ));
                }
                audit.active += 1;
                audit.awaiting_submission += 1;
            }
            UserOperationStatus::Submitted => {
                if !record
                    .transaction_hash
                    .as_ref()
                    .is_some_and(|transaction_hash| {
                        transaction_hash.eq_ignore_ascii_case(&intent.transaction_hash)
                    })
                {
                    return Err(format!(
                        "prepared bundle member {hash} belongs to another transaction"
                    ));
                }
                audit.active += 1;
            }
            UserOperationStatus::Rejected
            | UserOperationStatus::Included
            | UserOperationStatus::Failed => audit.terminal += 1,
            UserOperationStatus::NotFound => {
                return Err(format!(
                    "prepared bundle member {hash} has an invalid stored status"
                ));
            }
        }
    }
    Ok(audit)
}

/// Distinguishes a future keyed nonce (durable defer) from a stale nonce
/// (durable reject). Called only for explicit AA25 simulation failures.
async fn resolve_nonce_mismatches(
    ctx: &Ctx,
    entry_point: Address,
    mismatches: &[Candidate],
    results: &mut Results,
) -> Result<(), String> {
    const LOOKUP_UNAVAILABLE: &str = "account nonce lookup is temporarily unavailable";
    let nonces = match request(
        ctx,
        ExecutionOperation::FetchAccountNonces {
            entry_point,
            probes: mismatches
                .iter()
                .map(|candidate| (candidate.packed.sender, candidate.packed.packed.nonce))
                .collect(),
        },
    )
    .await?
    {
        ExecutionOutcome::AccountNonces { nonces } if nonces.len() == mismatches.len() => nonces,
        _ => {
            for candidate in mismatches {
                results.failed(candidate.result_index, LOOKUP_UNAVAILABLE);
            }
            return Ok(());
        }
    };
    for (candidate, onchain_nonce) in mismatches.iter().zip(nonces) {
        let Some(onchain_nonce) = onchain_nonce else {
            results.failed(candidate.result_index, LOOKUP_UNAVAILABLE);
            continue;
        };
        let user_nonce = candidate.packed.packed.nonce;
        if user_nonce > onchain_nonce {
            // Redis takes a complete immutable copy; a durable item result
            // lets Iggy advance past this nonce without losing at-least-once
            // execution.
            match request(
                ctx,
                ExecutionOperation::DeferOperation {
                    index: candidate.result_index,
                    cause: DeferCause::FutureNonce {
                        user_nonce,
                        onchain_nonce,
                    },
                },
            )
            .await?
            {
                ExecutionOutcome::Deferred { .. } => results.durable(candidate.result_index),
                _ => results.failed(
                    candidate.result_index,
                    "could not persist future UserOperation",
                ),
            }
            continue;
        }
        match request(
            ctx,
            ExecutionOperation::MarkRejected {
                hash: candidate.hash_string.clone(),
                cause: RejectionCause::StaleNonce {
                    user_nonce,
                    onchain_nonce,
                },
            },
        )
        .await?
        {
            ExecutionOutcome::Failed { .. } => results.failed(
                candidate.result_index,
                "could not persist stale nonce rejection",
            ),
            _ => results.durable(candidate.result_index),
        }
    }
    Ok(())
}

/// Native relayer funding under the treasury lease: prepared-intent resume,
/// the float plan, affordability, sign/persist, and the funding broadcast.
/// Returns whether the relayer is ready (`false` = a top-up is pending).
#[allow(clippy::too_many_arguments)]
async fn ensure_native_funding(
    ctx: &Ctx,
    policy: &ExecutionPolicy,
    chain_id: u64,
    relayer_balance: U256,
    required_prefund: U256,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    top_up_max: U256,
) -> Result<bool, String> {
    match request(ctx, ExecutionOperation::AcquireTreasuryLease).await? {
        ExecutionOutcome::LeaseAcquired { acquired: true } => {}
        // A store failure is not "another funder owns the lease": it defers
        // the whole batch through the transient channel, as the old engine's
        // acquire error did.
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Ok(false),
    }
    let result = native_funding_locked(
        ctx,
        policy,
        chain_id,
        relayer_balance,
        required_prefund,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        top_up_max,
    )
    .await;
    let _ = request_unfenced(ctx, ExecutionOperation::ReleaseTreasuryLease).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn native_funding_locked(
    ctx: &Ctx,
    policy: &ExecutionPolicy,
    chain_id: u64,
    relayer_balance: U256,
    required_prefund: U256,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    top_up_max: U256,
) -> Result<bool, String> {
    match request(ctx, ExecutionOperation::LoadPreparedFunding).await? {
        ExecutionOutcome::FundingIntent { intent: None } => {}
        ExecutionOutcome::FundingIntent {
            intent: Some(intent),
        } => {
            resume_funding(ctx, &intent).await?;
            return Ok(false);
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    let plan = plan_native_top_up(
        relayer_balance,
        required_prefund,
        policy.relayer_float_cost_multiplier,
        policy.relayer_float_target_wei,
        policy.relayer_float_min_wei,
        top_up_max,
    )
    .map_err(|error| error.to_string())?;
    let (nonce, treasury_balance) =
        match request(ctx, ExecutionOperation::FetchTreasuryContext).await? {
            ExecutionOutcome::TreasuryContext { nonce, balance } => (nonce, balance),
            ExecutionOutcome::Failed { message } => return Err(message),
            _ => return Err("unexpected shell response".to_owned()),
        };
    let protected_treasury = native_top_up_reserve(max_fee_per_gas, policy.treasury_floor_wei)
        .map_err(|error| error.to_string())?;
    let top_up_gas_cost = protected_treasury - U256::from(policy.treasury_floor_wei);
    // If the treasury can satisfy this bundle but not the preferred float,
    // make a partial top-up. The next bundle will replenish the float when
    // more treasury funds arrive.
    let Some(amount) = treasury_affordable_top_up(
        plan.amount_capped,
        plan.deficit,
        treasury_balance,
        protected_treasury,
    ) else {
        let required_treasury = plan
            .deficit
            .checked_add(protected_treasury)
            .ok_or_else(|| "treasury balance requirement overflow".to_owned())?;
        let _ = request(
            ctx,
            ExecutionOperation::RecordTreasuryShortfall {
                treasury_balance,
                required_treasury,
                requested: plan.amount_capped,
                minimum: plan.deficit,
                top_up_gas_cost,
            },
        )
        .await?;
        return Err(
            "treasury balance cannot cover the current UserOperation prefund, top-up gas, and reserve floor"
                .to_owned(),
        );
    };
    if amount < plan.amount_capped {
        let _ = request(
            ctx,
            ExecutionOperation::RecordPartialTopUp {
                requested: plan.amount_capped,
                submitted: amount,
                minimum: plan.deficit,
            },
        )
        .await?;
    }
    let amount_u128 =
        u128::try_from(amount).map_err(|_| "top-up amount exceeds uint128".to_owned())?;

    ensure_treasury_lease(ctx).await?;
    let signed = match request(
        ctx,
        ExecutionOperation::SignTreasuryTransfer {
            request: TreasurySignRequest {
                nonce,
                amount,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            },
        },
    )
    .await?
    {
        ExecutionOutcome::Signed { signed } => signed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let intent = PreparedFundingIntent {
        chain_id,
        relayer: policy.relayer.to_string(),
        amount_wei: amount_u128,
        raw_transaction: signed.raw_transaction_hex.clone(),
        transaction_hash: signed.transaction_hash.clone(),
        nonce: signed.nonce,
    };
    ensure_treasury_lease(ctx).await?;
    match request(
        ctx,
        ExecutionOperation::SaveFundingIntent {
            intent: intent.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Saved { saved: true } => {}
        ExecutionOutcome::Saved { saved: false } => {
            match request(ctx, ExecutionOperation::LoadPreparedFunding).await? {
                ExecutionOutcome::FundingIntent {
                    intent: Some(existing),
                } => {
                    resume_funding(ctx, &existing).await?;
                    return Ok(false);
                }
                ExecutionOutcome::FundingIntent { intent: None } => {
                    return Err("another treasury relayer top-up is pending".to_owned());
                }
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            }
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    broadcast_funding(ctx, &intent).await?;
    let _ = request(
        ctx,
        ExecutionOperation::RecordFundingSubmitted {
            amount,
            transaction_hash: intent.transaction_hash.clone(),
            tempo: false,
        },
    )
    .await?;
    Ok(false)
}

/// Tempo pathUSD variant: flat float target, all-or-nothing affordability.
async fn ensure_tempo_funding(
    ctx: &Ctx,
    policy: &ExecutionPolicy,
    chain_id: u64,
    relayer_path_usd_balance: U256,
    required_prefund: U256,
    outer_max_fee: u128,
) -> Result<bool, String> {
    match request(ctx, ExecutionOperation::AcquireTreasuryLease).await? {
        ExecutionOutcome::LeaseAcquired { acquired: true } => {}
        // A store failure is not "another funder owns the lease": it defers
        // the whole batch through the transient channel, as the old engine's
        // acquire error did.
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Ok(false),
    }
    let result = tempo_funding_locked(
        ctx,
        policy,
        chain_id,
        relayer_path_usd_balance,
        required_prefund,
        outer_max_fee,
    )
    .await;
    let _ = request_unfenced(ctx, ExecutionOperation::ReleaseTreasuryLease).await;
    result
}

async fn tempo_funding_locked(
    ctx: &Ctx,
    policy: &ExecutionPolicy,
    chain_id: u64,
    relayer_path_usd_balance: U256,
    required_prefund: U256,
    outer_max_fee: u128,
) -> Result<bool, String> {
    match request(ctx, ExecutionOperation::LoadPreparedFunding).await? {
        ExecutionOutcome::FundingIntent { intent: None } => {}
        ExecutionOutcome::FundingIntent {
            intent: Some(intent),
        } => {
            resume_funding(ctx, &intent).await?;
            return Ok(false);
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    let Some(amount) = plan_tempo_top_up(relayer_path_usd_balance, required_prefund)
        .map_err(|_| "Tempo relayer funding amount underflow".to_owned())?
    else {
        return Ok(true);
    };
    let amount_u128 =
        u128::try_from(amount).map_err(|_| "Tempo relayer top-up exceeds uint128".to_owned())?;
    let (nonce, treasury_balance, raw_gas_estimate) = match request(
        ctx,
        ExecutionOperation::FetchTempoTreasuryContext {
            transfer_amount: amount,
        },
    )
    .await?
    {
        ExecutionOutcome::TempoTreasuryContext {
            nonce,
            balance,
            raw_gas_estimate,
        } => (nonce, balance, raw_gas_estimate),
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let top_up_gas_limit = crate::tempo::buffered_top_up_gas_limit(raw_gas_estimate)
        .ok_or_else(|| "Tempo pathUSD top-up gas buffer overflow".to_owned())?;
    let top_up_gas_cost = crate::tempo::tempo_cost_in_path_usd(
        U256::from(top_up_gas_limit),
        U256::from(outer_max_fee),
    )
    .ok_or_else(|| "Tempo pathUSD cost overflow".to_owned())?;
    let required_treasury = amount
        .checked_add(top_up_gas_cost)
        .and_then(|value| value.checked_add(U256::from(crate::tempo::TEMPO_TREASURY_FLOOR)))
        .ok_or_else(|| "Tempo treasury balance requirement overflow".to_owned())?;
    if treasury_balance < required_treasury {
        let _ = request(
            ctx,
            ExecutionOperation::RecordTempoTreasuryShortfall {
                treasury_balance,
                required_treasury,
                top_up: amount,
                top_up_gas_limit,
                top_up_gas_cost,
            },
        )
        .await?;
        return Err(
            "Tempo treasury pathUSD is below top-up amount, gas, and reserve floor".to_owned(),
        );
    }

    ensure_treasury_lease(ctx).await?;
    let signed = match request(
        ctx,
        ExecutionOperation::SignTreasuryPathUsd {
            request: TempoTreasurySignRequest {
                nonce,
                gas_limit: top_up_gas_limit,
                max_fee_per_gas: outer_max_fee,
                amount,
            },
        },
    )
    .await?
    {
        ExecutionOutcome::Signed { signed } => signed,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let intent = PreparedFundingIntent {
        chain_id,
        relayer: policy.relayer.to_string(),
        amount_wei: amount_u128,
        raw_transaction: signed.raw_transaction_hex.clone(),
        transaction_hash: signed.transaction_hash.clone(),
        nonce: signed.nonce,
    };
    ensure_treasury_lease(ctx).await?;
    match request(
        ctx,
        ExecutionOperation::SaveFundingIntent {
            intent: intent.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Saved { saved: true } => {}
        ExecutionOutcome::Saved { saved: false } => {
            match request(ctx, ExecutionOperation::LoadPreparedFunding).await? {
                ExecutionOutcome::FundingIntent {
                    intent: Some(existing),
                } => {
                    resume_funding(ctx, &existing).await?;
                    return Ok(false);
                }
                ExecutionOutcome::FundingIntent { intent: None } => {
                    return Err("another Tempo treasury relayer top-up is pending".to_owned());
                }
                ExecutionOutcome::Failed { message } => return Err(message),
                _ => return Err("unexpected shell response".to_owned()),
            }
        }
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    broadcast_funding(ctx, &intent).await?;
    let _ = request(
        ctx,
        ExecutionOperation::RecordFundingSubmitted {
            amount,
            transaction_hash: intent.transaction_hash.clone(),
            tempo: true,
        },
    )
    .await?;
    Ok(false)
}

async fn ensure_treasury_lease(ctx: &Ctx) -> Result<(), String> {
    match request(ctx, ExecutionOperation::EnsureTreasuryLease).await? {
        ExecutionOutcome::LeaseHeld { held: true } => Ok(()),
        ExecutionOutcome::LeaseHeld { held: false } => Err("executor lease was lost".to_owned()),
        ExecutionOutcome::Failed { message } => Err(message),
        _ => Err("unexpected shell response".to_owned()),
    }
}

/// The funding-transfer broadcast: like the bundle broadcast but without a
/// stale-nonce path — the treasury nonce is serialized by its lease.
async fn broadcast_funding(ctx: &Ctx, intent: &PreparedFundingIntent) -> Result<(), String> {
    crate::broadcast::validate_raw_transaction(&intent.raw_transaction, &intent.transaction_hash)
        .map_err(|error| error.to_string())?;
    match request(
        ctx,
        ExecutionOperation::CheckBroadcastSeen {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Seen { seen: true } => return Ok(()),
        ExecutionOutcome::Seen { seen: false } => {}
        _ => return Err("unexpected shell response".to_owned()),
    }
    let raw = crate::broadcast::parse_hex_bytes(&intent.raw_transaction)
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default();
    let reply = match request(
        ctx,
        ExecutionOperation::BroadcastRaw {
            raw_transaction: raw,
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Sent { reply } => reply,
        ExecutionOutcome::Failed { message } => {
            forget_funding_broadcast(ctx, intent).await;
            return Err(message);
        }
        _ => return Err("unexpected shell response".to_owned()),
    };
    match reply {
        BroadcastReply::Accepted { transaction_hash }
            if transaction_hash.eq_ignore_ascii_case(&intent.transaction_hash) =>
        {
            let _ = request_unfenced(
                ctx,
                ExecutionOperation::RememberBroadcast {
                    transaction_hash: intent.transaction_hash.clone(),
                },
            )
            .await;
            Ok(())
        }
        BroadcastReply::Accepted { .. } => {
            forget_funding_broadcast(ctx, intent).await;
            Err("RPC returned a different funding transaction hash".to_owned())
        }
        BroadcastReply::Ambiguous { reason } => {
            forget_funding_broadcast(ctx, intent).await;
            let _ = request(
                ctx,
                ExecutionOperation::RecordUnprovenFunding {
                    transaction_hash: intent.transaction_hash.clone(),
                    ambiguous: true,
                    reason,
                },
            )
            .await?;
            Ok(())
        }
        BroadcastReply::Rejected { reason } => {
            forget_funding_broadcast(ctx, intent).await;
            let known = match request(
                ctx,
                ExecutionOperation::ProbeTransactionKnown {
                    transaction_hash: intent.transaction_hash.clone(),
                },
            )
            .await?
            {
                ExecutionOutcome::Known { known } => known,
                _ => return Err("unexpected shell response".to_owned()),
            };
            if known {
                let _ = request_unfenced(
                    ctx,
                    ExecutionOperation::RememberBroadcast {
                        transaction_hash: intent.transaction_hash.clone(),
                    },
                )
                .await;
            } else {
                let _ = request(
                    ctx,
                    ExecutionOperation::RecordUnprovenFunding {
                        transaction_hash: intent.transaction_hash.clone(),
                        ambiguous: false,
                        reason,
                    },
                )
                .await?;
            }
            Ok(())
        }
    }
}

async fn forget_funding_broadcast(ctx: &Ctx, intent: &PreparedFundingIntent) {
    let _ = request_unfenced(
        ctx,
        ExecutionOperation::ForgetBroadcast {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await;
}

/// Rebroadcast a persisted funding transfer and, behind a receipt-probe
/// claim, settle it: clear on inclusion, error on revert.
async fn resume_funding(ctx: &Ctx, intent: &PreparedFundingIntent) -> Result<(), String> {
    broadcast_funding(ctx, intent).await?;
    match request(
        ctx,
        ExecutionOperation::AcquireReceiptProbe {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::LeaseAcquired { acquired: true } => {}
        ExecutionOutcome::LeaseAcquired { acquired: false } => return Ok(()),
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    }
    let receipt = match request(
        ctx,
        ExecutionOperation::FetchTransactionReceipt {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Receipt { receipt } => receipt,
        ExecutionOutcome::Failed { message } => return Err(message),
        _ => return Err("unexpected shell response".to_owned()),
    };
    let Some(receipt) = receipt else {
        return Ok(());
    };
    let Some(success) = receipt_succeeded(&receipt) else {
        return Err("funding transaction receipt has invalid status".to_owned());
    };
    if let ExecutionOutcome::Failed { message } = request(
        ctx,
        ExecutionOperation::ClearFundingIntent {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        return Err(message);
    }
    forget_funding_broadcast(ctx, intent).await;
    let _ = request(
        ctx,
        ExecutionOperation::NoteFundingReceipt {
            intent: intent.clone(),
            success,
        },
    )
    .await?;
    if !success {
        return Err(format!(
            "treasury relayer top-up transaction reverted: {}",
            intent.transaction_hash
        ));
    }
    Ok(())
}

/// The broadcast sequence for a freshly signed bundle intent: cache probe,
/// send, and — for unproven outcomes — the observability and stale-nonce
/// probes judged by `crate::broadcast::resolve_unproven_broadcast`. Returns
/// whether the transaction is confirmed observable; `Err` is the transient
/// deferral channel.
async fn broadcast_bundle(ctx: &Ctx, intent: &PreparedBundleIntent) -> Result<bool, String> {
    crate::broadcast::validate_raw_transaction(&intent.raw_transaction, &intent.transaction_hash)
        .map_err(|error| error.to_string())?;
    match request(
        ctx,
        ExecutionOperation::CheckBroadcastSeen {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Seen { seen: true } => return Ok(true),
        ExecutionOutcome::Seen { seen: false } => {}
        _ => return Err("unexpected shell response".to_owned()),
    }
    let raw = crate::broadcast::parse_hex_bytes(&intent.raw_transaction)
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default();
    let reply = match request(
        ctx,
        ExecutionOperation::BroadcastRaw {
            raw_transaction: raw,
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Sent { reply } => reply,
        ExecutionOutcome::Failed { message } => {
            forget_broadcast(ctx, intent).await;
            return Err(message);
        }
        _ => return Err("unexpected shell response".to_owned()),
    };
    let (ambiguous, reason) = match reply {
        BroadcastReply::Accepted { transaction_hash }
            if transaction_hash.eq_ignore_ascii_case(&intent.transaction_hash) =>
        {
            remember_broadcast(ctx, intent).await;
            return Ok(true);
        }
        BroadcastReply::Accepted { .. } => {
            forget_broadcast(ctx, intent).await;
            return Err("RPC returned a transaction hash different from the signed bytes".into());
        }
        BroadcastReply::Ambiguous { reason } => (true, reason),
        BroadcastReply::Rejected { reason } => (false, reason),
    };
    forget_broadcast(ctx, intent).await;
    let known = match request(
        ctx,
        ExecutionOperation::ProbeTransactionKnown {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await?
    {
        ExecutionOutcome::Known { known } => known,
        _ => return Err("unexpected shell response".to_owned()),
    };
    match crate::broadcast::resolve_unproven_broadcast(&reason, known) {
        crate::broadcast::UnprovenBroadcast::Confirmed => {
            remember_broadcast(ctx, intent).await;
            Ok(true)
        }
        crate::broadcast::UnprovenBroadcast::CheckStaleNonce => {
            let stale = match request(
                ctx,
                ExecutionOperation::ProbeStaleNonce {
                    intent: intent.clone(),
                },
            )
            .await?
            {
                ExecutionOutcome::Stale { stale } => stale,
                _ => return Err("unexpected shell response".to_owned()),
            };
            if stale {
                if let ExecutionOutcome::Failed { message } = request(
                    ctx,
                    ExecutionOperation::ClearStaleIntent {
                        intent: intent.clone(),
                        reason: reason.clone(),
                    },
                )
                .await?
                {
                    return Err(message);
                }
                return Ok(false);
            }
            note_unproven(ctx, intent, ambiguous, &reason).await;
            Ok(false)
        }
        crate::broadcast::UnprovenBroadcast::RetainOutbox => {
            note_unproven(ctx, intent, ambiguous, &reason).await;
            Ok(false)
        }
    }
}

async fn remember_broadcast(ctx: &Ctx, intent: &PreparedBundleIntent) {
    let _ = request_unfenced(
        ctx,
        ExecutionOperation::RememberBroadcast {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await;
}

async fn forget_broadcast(ctx: &Ctx, intent: &PreparedBundleIntent) {
    let _ = request_unfenced(
        ctx,
        ExecutionOperation::ForgetBroadcast {
            transaction_hash: intent.transaction_hash.clone(),
        },
    )
    .await;
}

async fn note_unproven(ctx: &Ctx, intent: &PreparedBundleIntent, ambiguous: bool, reason: &str) {
    let _ = request_unfenced(
        ctx,
        ExecutionOperation::RecordUnprovenBroadcast {
            transaction_hash: intent.transaction_hash.clone(),
            ambiguous,
            reason: reason.to_owned(),
        },
    )
    .await;
}

async fn ensure_lane_lease(ctx: &Ctx) -> Result<(), String> {
    match request(ctx, ExecutionOperation::EnsureLaneLease).await? {
        ExecutionOutcome::LeaseHeld { held: true } => Ok(()),
        ExecutionOutcome::LeaseHeld { held: false } => Err("executor lease was lost".to_owned()),
        ExecutionOutcome::Failed { message } => Err(message),
        _ => Err("unexpected shell response".to_owned()),
    }
}

async fn simulate_bundle(
    ctx: &Ctx,
    entry_point: Address,
    survivors: &[Candidate],
) -> Result<BundleSimVerdict, String> {
    match request(
        ctx,
        ExecutionOperation::SimulateBundle {
            entry_point,
            operations: survivors
                .iter()
                .map(|candidate| (candidate.hash, candidate.packed.clone()))
                .collect(),
        },
    )
    .await?
    {
        ExecutionOutcome::BundleVerdict { verdict } => Ok(verdict),
        ExecutionOutcome::Failed { message } => Err(message),
        _ => Err("unexpected shell response".to_owned()),
    }
}

fn start_treasury(start: &StartBatch) -> Address {
    start.policy.treasury
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};

    use alloy::primitives::{Address, U256, address};
    use crux_core::{Core, Request};
    use serde_json::Value;

    use super::{
        BroadcastReply, BundleSimVerdict, BundleSimulationData, Candidate, DeferCause,
        ExecutionApp, ExecutionDiagnostic, ExecutionEvent, ExecutionOperation, ExecutionOutcome,
        ExecutionPolicy, ItemResolution, OperationSimVerdict, ResolvedChainAssets, SignedBundle,
        StartBatch, SubmissionTier, TransactionContext, bundle_submission_tier,
        candidate_from_record,
    };
    use crate::{
        abi::{PackedOperation, user_operation_hash},
        settlement::ChainAssetConfig,
        task::{
            RoutedUserOperation, StoredUserOperation, UserOperation, UserOperationStatus,
            UserOperationV0_7,
        },
        vault::relayer_index_for_sender,
    };

    const TREASURY: Address = address!("1111111111111111111111111111111111111111");
    const ENTRY_POINT: &str = "0x2222222222222222222222222222222222222222";
    const SENDER: &str = "0x00000000000000000000000000000000000000aa";
    const CHAIN_ID: u64 = 42_161;

    /// Scripts the shell side of the conversation, asserting the strictly
    /// sequential invariant: at most one operation is ever in flight.
    struct Driver {
        core: Core<ExecutionApp>,
        queue: VecDeque<Request<ExecutionOperation>>,
    }

    impl Driver {
        fn start(batch: StartBatch) -> Self {
            let core: Core<ExecutionApp> = Core::new();
            let effects = core.process_event(ExecutionEvent::Start(Box::new(batch)));
            let mut driver = Self {
                core,
                queue: VecDeque::new(),
            };
            driver.absorb(effects);
            driver
        }

        fn absorb(&mut self, effects: Vec<super::ExecutionEffect>) {
            for effect in effects {
                let super::ExecutionEffect::Work(request) = effect;
                self.queue.push_back(request);
            }
            assert!(
                self.queue.len() <= 1,
                "the batch program must be strictly sequential"
            );
        }

        fn step(&mut self, expected: ExecutionOperation, outcome: ExecutionOutcome) {
            let mut request = self
                .queue
                .pop_front()
                .unwrap_or_else(|| panic!("no operation in flight; expected {expected:?}"));
            assert_eq!(request.operation, expected);
            let effects = self
                .core
                .resolve(&mut request, outcome)
                .expect("resolve must succeed");
            self.absorb(effects);
        }

        fn assert_settled(&self, expected: &[ItemResolution]) {
            assert!(
                self.queue.is_empty(),
                "no operation may remain in flight, found {:?}",
                self.queue.front().map(|request| &request.operation)
            );
            assert_eq!(
                self.core.view().outcome.as_deref(),
                Some(expected),
                "batch must settle with the expected resolutions"
            );
        }
    }

    fn policy() -> ExecutionPolicy {
        ExecutionPolicy {
            pool_width: 10,
            max_bundle_operations: 4,
            gas_buffer_bps: 0,
            fixed_gas_buffer: 0,
            settlement_inclusion_floor_bps: 15_000,
            settlement_hold_max_attempts: 12,
            top_up_max_wei: 1_000_000,
            is_tempo: false,
            treasury: TREASURY,
            relayer: address!("9999999999999999999999999999999999999999"),
            relayer_float_cost_multiplier: 5,
            relayer_float_target_wei: 0,
            relayer_float_min_wei: 0,
            treasury_floor_wei: 0,
        }
    }

    fn assets() -> ResolvedChainAssets {
        ResolvedChainAssets {
            assets: ChainAssetConfig {
                native_decimals: 5,
                settlement_markup_bps: 14_000,
                stablecoins: BTreeMap::new(),
            },
            native_symbol: "ETH".into(),
        }
    }

    // --- calldata fixture: Safe executeUserOp -> MultiSend(delegatecall) ---

    fn word_u128(value: u128) -> Vec<u8> {
        let mut word = vec![0; 16];
        word.extend(value.to_be_bytes());
        word
    }

    fn native_payment_calldata(amount: u128) -> Vec<u8> {
        let mut packed = Vec::new();
        packed.push(0); // CALL
        packed.extend(TREASURY.as_slice());
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
        trusted.extend(address!("38869bf66a61cf6bdb996a6ae40d5853fd43b526").as_slice());
        call_data.extend(trusted);
        call_data.extend(word_u128(0));
        call_data.extend(word_u128(128));
        call_data.extend(word_u128(1));
        call_data.extend(word_u128(multisend.len() as u128));
        call_data.extend(multisend);
        call_data
    }

    fn user_op(paid: u128) -> UserOperationV0_7 {
        user_op_with_nonce(paid, "0x0")
    }

    fn user_op_with_nonce(paid: u128, nonce: &str) -> UserOperationV0_7 {
        UserOperationV0_7 {
            sender: SENDER.into(),
            nonce: nonce.into(),
            factory: None,
            factory_data: None,
            call_data: format!("0x{}", hex::encode(native_payment_calldata(paid))),
            call_gas_limit: "0x64".into(),
            verification_gas_limit: "0x64".into(),
            pre_verification_gas: "0x0".into(),
            max_fee_per_gas: "0x0".into(),
            max_priority_fee_per_gas: "0x0".into(),
            paymaster: None,
            paymaster_verification_gas_limit: None,
            paymaster_post_op_gas_limit: None,
            paymaster_data: None,
            signature: "0x".into(),
            eip7702_auth: None,
            fee_token: None,
        }
    }

    struct Fixture {
        routed: RoutedUserOperation,
        record: StoredUserOperation,
        hash_string: String,
        packed: PackedOperation,
    }

    fn fixture(paid: u128) -> Fixture {
        fixture_with_nonce(paid, "0x0")
    }

    fn fixture_at(paid: u128, tier: SubmissionTier) -> Fixture {
        let mut fixture = fixture_with_nonce(paid, "0x0");
        fixture.routed.submission_tier = Some(tier);
        fixture
    }

    fn fixture_with_nonce(paid: u128, nonce: &str) -> Fixture {
        let operation = UserOperation::V0_7(Box::new(user_op_with_nonce(paid, nonce)));
        let packed = PackedOperation::try_from(&operation).expect("fixture packs");
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let hash = user_operation_hash(&packed, entry_point, CHAIN_ID);
        let hash_string = hash.to_string().to_ascii_lowercase();
        let lane = relayer_index_for_sender(SENDER, 10) as u8;
        let value: Value = serde_json::to_value(&operation).unwrap();
        let routed = RoutedUserOperation {
            schema_version: 1,
            user_operation_hash: hash_string.clone(),
            chain_id: CHAIN_ID,
            entry_point: ENTRY_POINT.into(),
            user_operation: value,
            sender: SENDER.into(),
            lane,
            stream: "chain-42161".into(),
            partition_id: 1,
            offset: 7,
            submission_tier: None,
        };
        let record = StoredUserOperation {
            status: UserOperationStatus::Queued,
            transaction_hash: None,
            chain_id: CHAIN_ID,
            chain_id_text: CHAIN_ID.to_string(),
            entry_point: ENTRY_POINT.into(),
            user_operation: operation,
            admitted: true,
            next_receipt_check_at_ms: 0,
            block_hash: None,
            block_number: None,
            receipt: None,
            event: None,
            last_executor_stage: None,
            last_executor_error: None,
            last_executor_attempt_at_ms: None,
        };
        Fixture {
            routed,
            record,
            hash_string,
            packed,
        }
    }

    fn start(operations: Vec<RoutedUserOperation>) -> StartBatch {
        StartBatch {
            operations,
            policy: policy(),
            lease_token: "lane-token-1".into(),
        }
    }

    fn sim_data() -> BundleSimulationData {
        BundleSimulationData {
            gas_used: U256::from(100u64),
            operation_gas_used: vec![U256::from(100u64)],
            logs: Vec::new(),
        }
    }

    fn context() -> TransactionContext {
        TransactionContext {
            estimated_gas: U256::from(100u64),
            base_fee_per_gas: 1,
            max_fee_per_gas: 2,
            max_priority_fee_per_gas: 0,
            nonce: 7,
            relayer_balance: U256::from(10_000u64),
        }
    }

    #[test]
    fn a_fully_funded_batch_walks_the_pipeline_to_submission() {
        // allocation 100 gas at the quoted fee 2 → cost 200; markup 1.4 →
        // required 280; the calldata pays exactly 280.
        let fixture = fixture(280);
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));

        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata: calldata.clone(),
            },
            ExecutionOutcome::Context { context: context() },
        );
        driver.step(
            ExecutionOperation::FetchMarketPrice,
            ExecutionOutcome::Failed {
                message: "Binance native USD price request failed".into(),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let signed_raw = [0x02u8, 0x01, 0x02, 0x03];
        let signed_hash = alloy::primitives::keccak256(signed_raw).to_string();
        driver.step(
            ExecutionOperation::SignBundle {
                request: super::BundleSignRequest {
                    nonce: 7,
                    gas_limit: 100,
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 0,
                    entry_point,
                    calldata,
                },
            },
            ExecutionOutcome::Signed {
                signed: SignedBundle {
                    raw_transaction_hex: "0x02010203".into(),
                    transaction_hash: signed_hash.clone(),
                    nonce: 7,
                },
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let intent = crate::task::PreparedBundleIntent {
            chain_id: CHAIN_ID,
            lane: fixture.routed.lane,
            entry_point: entry_point.to_string(),
            raw_transaction: "0x02010203".into(),
            transaction_hash: signed_hash.clone(),
            nonce: 7,
            user_operation_hashes: vec![fixture.hash_string.clone()],
        };
        driver.step(
            ExecutionOperation::SavePreparedBundle {
                intent: intent.clone(),
            },
            ExecutionOutcome::Saved { saved: true },
        );
        driver.step(
            ExecutionOperation::CheckBroadcastSeen {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Seen { seen: false },
        );
        driver.step(
            ExecutionOperation::BroadcastRaw {
                raw_transaction: signed_raw.to_vec(),
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Sent {
                reply: BroadcastReply::Accepted {
                    transaction_hash: signed_hash.to_ascii_uppercase(),
                },
            },
        );
        driver.step(
            ExecutionOperation::RememberBroadcast {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::MarkBundleSubmitted {
                intent,
                gas_limit: 100,
            },
            ExecutionOutcome::Indexed { indexed: 1 },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn a_client_named_speed_signs_above_the_pace_the_relay_keeps_on_its_own() {
        // The same batch as the test above, but the client asked for `fast`
        // and paid 420 instead of 280. Base fee 1, tip 0: the relay's own cap
        // is 2 and `fast` asks for 3×1 = 3, which 420 funds exactly
        // (300 gas cost × 1.4 = 420). So the outer transaction is signed at 3
        // — strictly faster than the 2 the untiered twin signs at, which is
        // the whole point of the feature and the direction a reported-tier
        // price would have got wrong.
        let fixture = fixture_at(420, SubmissionTier::Fast);
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));

        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata: calldata.clone(),
            },
            ExecutionOutcome::Context { context: context() },
        );
        // The one line an operator can read the decision off: the speed asked
        // for, the cap the relay would have used, and what it resolved to.
        driver.step(
            ExecutionOperation::EmitDiagnostic {
                diagnostic: ExecutionDiagnostic::SubmissionTierCap {
                    tier: SubmissionTier::Fast,
                    quoted_fee: 2,
                    cap: 3,
                    base_fee: 1,
                    // This market has no tip at all, so `2 ×` it is still 0
                    // and only the cap can differ. The tip-scaling half of a
                    // tier is exercised by
                    // `a_named_speed_signs_the_tiers_tip_not_the_market_one`.
                    tip: 0,
                    market_tip: 0,
                },
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::FetchMarketPrice,
            ExecutionOutcome::Failed {
                message: "Binance native USD price request failed".into(),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let signed_raw = [0x02u8, 0x01, 0x02, 0x03];
        let signed_hash = alloy::primitives::keccak256(signed_raw).to_string();
        driver.step(
            ExecutionOperation::SignBundle {
                request: super::BundleSignRequest {
                    nonce: 7,
                    gas_limit: 100,
                    // 3, not the 2 the untiered pipeline signs at.
                    max_fee_per_gas: 3,
                    max_priority_fee_per_gas: 0,
                    entry_point,
                    calldata,
                },
            },
            ExecutionOutcome::Signed {
                signed: SignedBundle {
                    raw_transaction_hex: "0x02010203".into(),
                    transaction_hash: signed_hash.clone(),
                    nonce: 7,
                },
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let intent = crate::task::PreparedBundleIntent {
            chain_id: CHAIN_ID,
            lane: fixture.routed.lane,
            entry_point: entry_point.to_string(),
            raw_transaction: "0x02010203".into(),
            transaction_hash: signed_hash.clone(),
            nonce: 7,
            user_operation_hashes: vec![fixture.hash_string.clone()],
        };
        driver.step(
            ExecutionOperation::SavePreparedBundle {
                intent: intent.clone(),
            },
            ExecutionOutcome::Saved { saved: true },
        );
        driver.step(
            ExecutionOperation::CheckBroadcastSeen {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Seen { seen: false },
        );
        driver.step(
            ExecutionOperation::BroadcastRaw {
                raw_transaction: signed_raw.to_vec(),
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Sent {
                reply: BroadcastReply::Accepted {
                    transaction_hash: signed_hash.to_ascii_uppercase(),
                },
            },
        );
        driver.step(
            ExecutionOperation::RememberBroadcast {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::MarkBundleSubmitted {
                intent,
                gas_limit: 100,
            },
            ExecutionOutcome::Indexed { indexed: 1 },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn a_named_speed_signs_the_tiers_tip_not_the_market_one() {
        // The defect, at the layer that caused it: the tier used to replace
        // only `context.max_fee_per_gas`, so the `BundleSignRequest` below
        // carried the raw market tip and the block builder — which orders by
        // `min(maxPriorityFeePerGas, maxFeePerGas − baseFee)` — saw exactly
        // what a `slow` send would have offered. This walks a real market
        // (base 100, market tip 40) through the whole pipeline and pins BOTH
        // numbers in the signed request.
        //
        // fast: cap = 3 × 100 + 2 × 40 = 380, tip = 80.
        // Funding: 100 gas at the untiered quote 240 costs 24_000, × 1.4 =
        // 33_600 required; 60_000 paid funds a 428 cap, so nothing clamps.
        let fixture = fixture_at(60_000, SubmissionTier::Fast);
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));

        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata: calldata.clone(),
            },
            ExecutionOutcome::Context {
                context: TransactionContext {
                    estimated_gas: U256::from(100u64),
                    base_fee_per_gas: 100,
                    max_fee_per_gas: 240, // the untiered 2 × 100 + 40
                    max_priority_fee_per_gas: 40,
                    nonce: 7,
                    relayer_balance: U256::from(1_000_000u64),
                },
            },
        );
        // The operator's one line: the tip actually signed beside the market
        // tip it was scaled from. `tip == market_tip` on anything but `slow`
        // would mean the scaling was lost again.
        driver.step(
            ExecutionOperation::EmitDiagnostic {
                diagnostic: ExecutionDiagnostic::SubmissionTierCap {
                    tier: SubmissionTier::Fast,
                    quoted_fee: 240,
                    cap: 380,
                    base_fee: 100,
                    tip: 80,
                    market_tip: 40,
                },
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::FetchMarketPrice,
            ExecutionOutcome::Failed {
                message: "Binance native USD price request failed".into(),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let signed_raw = [0x02u8, 0x01, 0x02, 0x03];
        let signed_hash = alloy::primitives::keccak256(signed_raw).to_string();
        driver.step(
            ExecutionOperation::SignBundle {
                request: super::BundleSignRequest {
                    nonce: 7,
                    gas_limit: 100,
                    // Both levers, not one. 380 = 3 × 100 + 80, and the tip
                    // is `fast`'s 80 — the market 40 would have bought the
                    // same block as `slow`.
                    max_fee_per_gas: 380,
                    max_priority_fee_per_gas: 80,
                    entry_point,
                    calldata,
                },
            },
            ExecutionOutcome::Signed {
                signed: SignedBundle {
                    raw_transaction_hex: "0x02010203".into(),
                    transaction_hash: signed_hash.clone(),
                    nonce: 7,
                },
            },
        );
        // `min(80, 380 − 100) = 80`: what the builder is paid, and the number
        // that was identical at every tier before this change.
        assert!(
            crate::gas_math::OuterFee {
                max_fee_per_gas: 380,
                max_priority_fee_per_gas: 80,
            }
            .delivers_full_tip_at(100)
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let intent = crate::task::PreparedBundleIntent {
            chain_id: CHAIN_ID,
            lane: fixture.routed.lane,
            entry_point: entry_point.to_string(),
            raw_transaction: "0x02010203".into(),
            transaction_hash: signed_hash.clone(),
            nonce: 7,
            user_operation_hashes: vec![fixture.hash_string.clone()],
        };
        driver.step(
            ExecutionOperation::SavePreparedBundle {
                intent: intent.clone(),
            },
            ExecutionOutcome::Saved { saved: true },
        );
        driver.step(
            ExecutionOperation::CheckBroadcastSeen {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Seen { seen: false },
        );
        driver.step(
            ExecutionOperation::BroadcastRaw {
                raw_transaction: signed_raw.to_vec(),
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Sent {
                reply: BroadcastReply::Accepted {
                    transaction_hash: signed_hash.to_ascii_uppercase(),
                },
            },
        );
        driver.step(
            ExecutionOperation::RememberBroadcast {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::MarkBundleSubmitted {
                intent,
                gas_limit: 100,
            },
            ExecutionOutcome::Indexed { indexed: 1 },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn a_bundle_takes_the_fastest_speed_asked_for_and_nothing_when_none_was() {
        // Nobody asked: the tier machinery never runs, which is the structural
        // form of "an existing client is untouched".
        let none = [fixture(280), fixture(280)];
        let candidates = |fixtures: &[Fixture]| -> Vec<Candidate> {
            fixtures
                .iter()
                .enumerate()
                .map(|(index, fixture)| {
                    candidate_from_record(index, &fixture.routed, &fixture.record, 10)
                        .expect("fixture is a valid candidate")
                })
                .collect()
        };
        assert_eq!(bundle_submission_tier(&candidates(&none)), None);

        // One op asked for `fast`, its neighbour asked for nothing. An
        // operation that named no speed already submits at `standard`, so the
        // bundle takes `fast` and neither op is worse off than it was alone.
        let mixed = [fixture_at(420, SubmissionTier::Fast), fixture(280)];
        assert_eq!(
            bundle_submission_tier(&candidates(&mixed)),
            Some(SubmissionTier::Fast)
        );

        // All three named, slowest first: the bundle is one transaction at one
        // price, and it takes the fastest speed anybody paid for.
        let named = [
            fixture_at(420, SubmissionTier::Slow),
            fixture_at(420, SubmissionTier::Standard),
        ];
        assert_eq!(
            bundle_submission_tier(&candidates(&named)),
            Some(SubmissionTier::Standard)
        );

        // And a lone `slow` really does ask for slow — the unnamed-is-standard
        // rule must not quietly promote it.
        let slow = [fixture_at(420, SubmissionTier::Slow)];
        assert_eq!(
            bundle_submission_tier(&candidates(&slow)),
            Some(SubmissionTier::Slow)
        );
    }

    #[test]
    fn a_store_outage_fails_every_item_without_touching_the_lane() {
        let fixture = fixture(280);
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Failed {
                message: "Redis command timed out".into(),
            },
        );
        driver.assert_settled(&[ItemResolution::Failed {
            reason: "Redis command timed out".into(),
        }]);
    }

    #[test]
    fn a_lost_lane_lease_defers_with_a_diagnostic_for_every_unresolved_item() {
        let fixture = fixture(280);
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: false },
        );
        // The "lease" stage is an expected hand-off: diagnostic, no Telegram.
        driver.step(
            ExecutionOperation::RecordDeferred {
                hash: fixture.hash_string.clone(),
                stage: "lease",
                reason: "relayer lane is currently owned by another worker".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Failed {
            reason: "relayer lane is owned by another worker".into(),
        }]);
    }

    #[test]
    fn an_already_durable_record_advances_without_execution() {
        let mut fixture = fixture(280);
        fixture.record.status = UserOperationStatus::Included;
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn an_unaffordable_market_holds_the_operation_without_paging() {
        // Paying 1 against a requirement of 280 is a shortfall: parked in the
        // delayed inbox, attempt 3 of 12 stays within budget. The hold is an
        // expected hand-off — the diagnostic is recorded but Telegram stays
        // quiet, exactly as the old engine's notify allowlist decided.
        let fixture = fixture(1);
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata,
            },
            ExecutionOutcome::Context { context: context() },
        );
        driver.step(
            ExecutionOperation::DeferOperation {
                index: 0,
                cause: DeferCause::AffordableMarketHold,
            },
            ExecutionOutcome::Deferred { attempt: 3 },
        );
        driver.step(
            ExecutionOperation::RecordDeferred {
                hash: fixture.hash_string.clone(),
                stage: "in_band_settlement_hold",
                reason: "waiting for network fees to fit the signed in-band reimbursement: \
                         paid=1, required=280, shortfall=279, attempt=3/12"
                    .into(),
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn a_heartbeat_interrupt_defers_the_batch_like_a_dropped_pipeline() {
        // A failed lease renewal answers the pending operation with
        // `Interrupted`: the operation must not run, and the batch settles
        // through the same "execution" deferral (diagnostic, Telegram, warn)
        // the old engine reached by dropping the pipeline future mid-await.
        let fixture = fixture(280);
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Interrupted {
                reason: "executor lease was lost".into(),
            },
        );
        driver.step(
            ExecutionOperation::RecordDeferred {
                hash: fixture.hash_string.clone(),
                stage: "execution",
                reason: "executor lease was lost".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::NotifyIssue {
                hash: fixture.hash_string.clone(),
                stage: "execution",
                reason: "executor lease was lost".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::EmitDiagnostic {
                diagnostic: ExecutionDiagnostic::ExecutionDeferred {
                    reason: "executor lease was lost".into(),
                },
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Failed {
            reason: "UserOperation execution was deferred".into(),
        }]);
    }

    #[test]
    fn an_exhausted_hold_budget_rejects_with_the_historical_warns() {
        // Attempt 13 of 12 exhausts the budget: the operation is rejected
        // with the frozen reason, and the shell is asked to emit the two
        // historical warns (budget exhausted + field-rich rejection) before
        // the Telegram notification.
        let fixture = fixture(1);
        let mut driver = walk_to_bundle_simulation(&fixture);
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata,
            },
            ExecutionOutcome::Context { context: context() },
        );
        driver.step(
            ExecutionOperation::DeferOperation {
                index: 0,
                cause: DeferCause::AffordableMarketHold,
            },
            ExecutionOutcome::Deferred { attempt: 13 },
        );
        driver.step(
            ExecutionOperation::EmitDiagnostic {
                diagnostic: ExecutionDiagnostic::HoldBudgetExhausted {
                    hash: fixture.hash_string.clone(),
                    attempt: 13,
                    paid: U256::from(1u64),
                    required: U256::from(280u64),
                },
            },
            ExecutionOutcome::Done,
        );
        let reason = "in-band reimbursement is below the required amount: \
                      paid=1, required=280, shortfall=279";
        driver.step(
            ExecutionOperation::MarkRejectedWithReason {
                hash: fixture.hash_string.clone(),
                stage: "in_band_settlement",
                reason: reason.into(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::EmitDiagnostic {
                diagnostic: ExecutionDiagnostic::SettlementRejected {
                    hash: fixture.hash_string.clone(),
                    payment_asset: None,
                    paid: U256::from(1u64),
                    required: U256::from(280u64),
                    stable_logs_valid: true,
                },
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::NotifyIssue {
                hash: fixture.hash_string.clone(),
                stage: "in_band_settlement",
                reason: reason.into(),
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    fn path_usd_payment_call_data(amount: u128) -> String {
        fn word_u128(value: u128) -> Vec<u8> {
            let mut word = vec![0; 16];
            word.extend(value.to_be_bytes());
            word
        }
        let path_usd = crate::tempo::PATH_USD;
        let mut transfer = vec![0xa9, 0x05, 0x9c, 0xbb];
        let mut to_word = vec![0u8; 12];
        to_word.extend(TREASURY.as_slice());
        transfer.extend(to_word);
        transfer.extend(word_u128(amount));

        let mut packed = Vec::new();
        packed.push(0);
        packed.extend(path_usd.as_slice());
        packed.extend(word_u128(0));
        packed.extend(word_u128(transfer.len() as u128));
        packed.extend(&transfer);

        let mut multisend = vec![0x8d, 0x80, 0xff, 0x0a];
        multisend.extend(word_u128(32));
        multisend.extend(word_u128(packed.len() as u128));
        multisend.extend(packed);
        let padding = (32 - multisend.len() % 32) % 32;
        multisend.resize(multisend.len() + padding, 0);

        let mut call_data = vec![0x7b, 0xb3, 0x74, 0x28];
        let mut trusted = vec![0u8; 12];
        trusted.extend(address!("38869bf66a61cf6bdb996a6ae40d5853fd43b526").as_slice());
        call_data.extend(trusted);
        call_data.extend(word_u128(0));
        call_data.extend(word_u128(128));
        call_data.extend(word_u128(1));
        call_data.extend(word_u128(multisend.len() as u128));
        call_data.extend(multisend);
        format!("0x{}", hex::encode(call_data))
    }

    fn path_usd_transfer_log(amount: u128) -> crate::settlement::SettlementLog {
        fn address_topic(address: Address) -> alloy::primitives::B256 {
            let mut word = [0u8; 32];
            word[12..].copy_from_slice(address.as_slice());
            alloy::primitives::B256::from(word)
        }
        let sender: Address = SENDER.parse().unwrap();
        crate::settlement::SettlementLog {
            address: crate::tempo::PATH_USD,
            topics: vec![
                alloy::primitives::keccak256(b"Transfer(address,address,uint256)"),
                address_topic(sender),
                address_topic(TREASURY),
            ],
            data: U256::from(amount).to_be_bytes::<32>().to_vec().into(),
        }
    }

    #[test]
    fn a_tempo_bundle_walks_the_path_usd_tail_to_submission() {
        // pathUSD payment 10_000 covers required = max(ceil(1.4 × 3600), 0.01
        // pathUSD) = 10_000; the relayer float (200k) already exceeds
        // max(prefund 3_307, TEMPO_FLOAT_MIN 100k), so no funding operation.
        let operation = {
            let mut op = user_op(280);
            op.call_data = path_usd_payment_call_data(10_000);
            UserOperation::V0_7(Box::new(op))
        };
        let packed = PackedOperation::try_from(&operation).expect("fixture packs");
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let tempo_chain = 4_217u64;
        let hash = user_operation_hash(&packed, entry_point, tempo_chain);
        let hash_string = hash.to_string().to_ascii_lowercase();
        let lane = relayer_index_for_sender(SENDER, 10) as u8;
        let routed = RoutedUserOperation {
            schema_version: 1,
            user_operation_hash: hash_string.clone(),
            chain_id: tempo_chain,
            entry_point: ENTRY_POINT.into(),
            user_operation: serde_json::to_value(&operation).unwrap(),
            sender: SENDER.into(),
            lane,
            stream: "chain-4217".into(),
            partition_id: 1,
            offset: 7,
            submission_tier: None,
        };
        let record = StoredUserOperation {
            status: UserOperationStatus::Queued,
            transaction_hash: None,
            chain_id: tempo_chain,
            chain_id_text: tempo_chain.to_string(),
            entry_point: ENTRY_POINT.into(),
            user_operation: operation,
            admitted: true,
            next_receipt_check_at_ms: 0,
            block_hash: None,
            block_number: None,
            receipt: None,
            event: None,
            last_executor_stage: None,
            last_executor_error: None,
            last_executor_attempt_at_ms: None,
        };
        let mut policy = policy();
        policy.is_tempo = true;
        let mut driver = Driver::start(StartBatch {
            operations: vec![routed],
            policy,
            lease_token: "lane-token-1".into(),
        });

        let tempo_assets = ResolvedChainAssets {
            assets: ChainAssetConfig {
                native_decimals: crate::tempo::PATH_USD_DECIMALS,
                settlement_markup_bps: 14_000,
                stablecoins: BTreeMap::from([(
                    crate::tempo::PATH_USD,
                    crate::settlement::StablecoinConfig {
                        symbol: crate::tempo::PATH_USD_SYMBOL.into(),
                        decimals: crate::tempo::PATH_USD_DECIMALS,
                    },
                )]),
            },
            native_symbol: crate::tempo::PATH_USD_SYMBOL.into(),
        };
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets {
                resolved: tempo_assets,
            },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(record)],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point,
                operations: vec![(hash, packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point,
                operations: vec![(hash, packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(BundleSimulationData {
                    gas_used: U256::from(100_000u64),
                    operation_gas_used: vec![U256::from(100_000u64)],
                    logs: vec![path_usd_transfer_log(10_000)],
                }),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::FetchTempoContext,
            ExecutionOutcome::TempoContext {
                base_fee_atto: U256::from(crate::tempo::TEMPO_BASE_FEE_ATTO),
                nonce: 7,
                relayer_path_usd_balance: U256::from(200_000u64),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let signed_raw = [0x76u8, 0x01, 0x02, 0x03];
        let signed_hash = alloy::primitives::keccak256(signed_raw).to_string();
        driver.step(
            ExecutionOperation::SignTempoBundle {
                request: super::TempoSignRequest {
                    nonce: 7,
                    gas_limit: 110_203,
                    max_fee_per_gas: 30_000_000_000,
                    entry_point,
                    calldata: crate::abi::handle_ops_calldata(
                        std::slice::from_ref(&packed.packed),
                        TREASURY,
                    )
                    .to_vec(),
                },
            },
            ExecutionOutcome::Signed {
                signed: SignedBundle {
                    raw_transaction_hex: "0x76010203".into(),
                    transaction_hash: signed_hash.clone(),
                    nonce: 7,
                },
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let intent = crate::task::PreparedBundleIntent {
            chain_id: tempo_chain,
            lane,
            entry_point: entry_point.to_string(),
            raw_transaction: "0x76010203".into(),
            transaction_hash: signed_hash.clone(),
            nonce: 7,
            user_operation_hashes: vec![hash_string.clone()],
        };
        driver.step(
            ExecutionOperation::SavePreparedBundle {
                intent: intent.clone(),
            },
            ExecutionOutcome::Saved { saved: true },
        );
        driver.step(
            ExecutionOperation::CheckBroadcastSeen {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Seen { seen: false },
        );
        driver.step(
            ExecutionOperation::BroadcastRaw {
                raw_transaction: signed_raw.to_vec(),
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Sent {
                reply: BroadcastReply::Accepted {
                    transaction_hash: signed_hash.clone(),
                },
            },
        );
        driver.step(
            ExecutionOperation::RememberBroadcast {
                transaction_hash: signed_hash,
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::MarkBundleSubmitted {
                intent,
                gas_limit: 110_203,
            },
            ExecutionOutcome::Indexed { indexed: 1 },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn an_underfunded_relayer_walks_the_treasury_top_up_and_defers() {
        // prefund = 100 gas x fee 2 = 200 > balance 100. Float plan: target
        // 200 x 5 = 1000, desired 900, deficit 100; treasury 100_000 covers
        // the full request after the 42_000 reserve.
        let fixture = fixture(280);
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let mut driver = walk_to_bundle_simulation(&fixture);
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata,
            },
            ExecutionOutcome::Context {
                context: TransactionContext {
                    relayer_balance: U256::from(100u64),
                    ..context()
                },
            },
        );
        driver.step(
            ExecutionOperation::FetchMarketPrice,
            ExecutionOutcome::Failed {
                message: "Binance native USD price request failed".into(),
            },
        );
        driver.step(
            ExecutionOperation::AcquireTreasuryLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedFunding,
            ExecutionOutcome::FundingIntent { intent: None },
        );
        driver.step(
            ExecutionOperation::FetchTreasuryContext,
            ExecutionOutcome::TreasuryContext {
                nonce: 3,
                balance: U256::from(100_000u64),
            },
        );
        driver.step(
            ExecutionOperation::EnsureTreasuryLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let funding_raw = [0x02u8, 0x09, 0x09];
        let funding_hash = alloy::primitives::keccak256(funding_raw).to_string();
        driver.step(
            ExecutionOperation::SignTreasuryTransfer {
                request: super::TreasurySignRequest {
                    nonce: 3,
                    amount: U256::from(900u64),
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 0,
                },
            },
            ExecutionOutcome::Signed {
                signed: SignedBundle {
                    raw_transaction_hex: "0x020909".into(),
                    transaction_hash: funding_hash.clone(),
                    nonce: 3,
                },
            },
        );
        driver.step(
            ExecutionOperation::EnsureTreasuryLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let funding_intent = crate::task::PreparedFundingIntent {
            chain_id: CHAIN_ID,
            relayer: policy().relayer.to_string(),
            amount_wei: 900,
            raw_transaction: "0x020909".into(),
            transaction_hash: funding_hash.clone(),
            nonce: 3,
        };
        driver.step(
            ExecutionOperation::SaveFundingIntent {
                intent: funding_intent,
            },
            ExecutionOutcome::Saved { saved: true },
        );
        driver.step(
            ExecutionOperation::CheckBroadcastSeen {
                transaction_hash: funding_hash.clone(),
            },
            ExecutionOutcome::Seen { seen: false },
        );
        driver.step(
            ExecutionOperation::BroadcastRaw {
                raw_transaction: funding_raw.to_vec(),
                transaction_hash: funding_hash.clone(),
            },
            ExecutionOutcome::Sent {
                reply: BroadcastReply::Accepted {
                    transaction_hash: funding_hash.clone(),
                },
            },
        );
        driver.step(
            ExecutionOperation::RememberBroadcast {
                transaction_hash: funding_hash.clone(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::RecordFundingSubmitted {
                amount: U256::from(900u64),
                transaction_hash: funding_hash,
                tempo: false,
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::ReleaseTreasuryLease,
            ExecutionOutcome::Done,
        );
        // "funding" is an expected hand-off: diagnostic, no Telegram.
        driver.step(
            ExecutionOperation::RecordDeferred {
                hash: fixture.hash_string.clone(),
                stage: "funding",
                reason: "waiting for relayer funding transaction confirmation".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Failed {
            reason: "UserOperation execution was deferred".into(),
        }]);
    }

    #[test]
    fn bundle_replay_audit_classifies_members_and_refuses_integrity_breaks() {
        use super::audit_bundle_replay;
        let fixture = fixture(280);
        let intent = crate::task::PreparedBundleIntent {
            chain_id: CHAIN_ID,
            lane: fixture.routed.lane,
            entry_point: ENTRY_POINT.into(),
            raw_transaction: "0x02".into(),
            transaction_hash: "0xbundle".into(),
            nonce: 1,
            user_operation_hashes: vec!["0x01".into(), "0x02".into(), "0x03".into(), "0x04".into()],
        };
        let mut queued = fixture.record.clone();
        queued.status = UserOperationStatus::Queued;
        let mut submitted = fixture.record.clone();
        submitted.status = UserOperationStatus::Submitted;
        submitted.transaction_hash = Some("0xBUNDLE".into());
        let mut included = fixture.record.clone();
        included.status = UserOperationStatus::Included;

        let audit = audit_bundle_replay(
            &intent,
            &[Some(queued.clone()), Some(submitted), Some(included), None],
        )
        .unwrap();
        assert_eq!(audit.active, 2);
        assert_eq!(audit.awaiting_submission, 1);
        assert_eq!(audit.terminal, 1);
        assert_eq!(audit.expired, 1);

        // Integrity refusals, byte-frozen.
        assert_eq!(
            audit_bundle_replay(&intent, &[Some(queued.clone())]).unwrap_err(),
            "Redis returned incomplete prepared bundle membership"
        );
        let mut unadmitted = queued.clone();
        unadmitted.admitted = false;
        assert_eq!(
            audit_bundle_replay(&intent, &[Some(unadmitted), None, None, None],).unwrap_err(),
            "prepared bundle member 0x01 is no longer admitted"
        );
        let mut foreign = fixture.record.clone();
        foreign.status = UserOperationStatus::Submitted;
        foreign.transaction_hash = Some("0xother".into());
        assert_eq!(
            audit_bundle_replay(&intent, &[Some(foreign), None, None, None]).unwrap_err(),
            "prepared bundle member 0x01 belongs to another transaction"
        );
        let mut moved = queued;
        moved.chain_id += 1;
        assert_eq!(
            audit_bundle_replay(&intent, &[Some(moved), None, None, None]).unwrap_err(),
            "prepared bundle member 0x01 no longer matches its chain and EntryPoint"
        );
    }

    /// Walks triage + simulation for a single candidate and returns the
    /// driver positioned right before the bundle simulation.
    fn walk_to_bundle_simulation(fixture: &Fixture) -> Driver {
        let mut driver = Driver::start(start(vec![fixture.routed.clone()]));
        driver.step(
            ExecutionOperation::CheckChainSupported,
            ExecutionOutcome::Supported { supported: true },
        );
        driver.step(
            ExecutionOperation::LoadChainAssets,
            ExecutionOutcome::Assets { resolved: assets() },
        );
        driver.step(
            ExecutionOperation::LoadRecords {
                hashes: vec![fixture.hash_string.clone()],
            },
            ExecutionOutcome::Records {
                records: vec![Some(fixture.record.clone())],
            },
        );
        driver.step(
            ExecutionOperation::AcquireLaneLease,
            ExecutionOutcome::LeaseAcquired { acquired: true },
        );
        driver.step(
            ExecutionOperation::LoadPreparedBundle,
            ExecutionOutcome::Intent { intent: None },
        );
        driver
    }

    #[test]
    fn a_future_account_nonce_defers_durably() {
        let fixture = fixture_with_nonce(280, "0x5");
        let mut driver = walk_to_bundle_simulation(&fixture);
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::NonceMismatch],
            },
        );
        driver.step(
            ExecutionOperation::FetchAccountNonces {
                entry_point: ENTRY_POINT.parse().unwrap(),
                probes: vec![(SENDER.parse().unwrap(), U256::from(5u64))],
            },
            ExecutionOutcome::AccountNonces {
                nonces: vec![Some(U256::from(3u64))],
            },
        );
        driver.step(
            ExecutionOperation::DeferOperation {
                index: 0,
                cause: DeferCause::FutureNonce {
                    user_nonce: U256::from(5u64),
                    onchain_nonce: U256::from(3u64),
                },
            },
            ExecutionOutcome::Deferred { attempt: 1 },
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn a_stale_account_nonce_rejects_durably() {
        let fixture = fixture_with_nonce(280, "0x5");
        let mut driver = walk_to_bundle_simulation(&fixture);
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point: ENTRY_POINT.parse().unwrap(),
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::NonceMismatch],
            },
        );
        driver.step(
            ExecutionOperation::FetchAccountNonces {
                entry_point: ENTRY_POINT.parse().unwrap(),
                probes: vec![(SENDER.parse().unwrap(), U256::from(5u64))],
            },
            ExecutionOutcome::AccountNonces {
                nonces: vec![Some(U256::from(9u64))],
            },
        );
        driver.step(
            ExecutionOperation::MarkRejected {
                hash: fixture.hash_string.clone(),
                cause: super::RejectionCause::StaleNonce {
                    user_nonce: U256::from(5u64),
                    onchain_nonce: U256::from(9u64),
                },
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Durable]);
    }

    #[test]
    fn a_nonce_too_low_rejection_with_a_stale_lane_clears_the_intent_and_defers() {
        // Walk the full pipeline to broadcast, then: rejected with
        // "nonce too low", not observable, lane nonce stale → clear intent,
        // retain nothing, defer the batch with broadcast diagnostics.
        let fixture = fixture(280);
        let entry_point: Address = ENTRY_POINT.parse().unwrap();
        let mut driver = walk_to_bundle_simulation(&fixture);
        driver.step(
            ExecutionOperation::SimulateIndividually {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::OperationVerdicts {
                verdicts: vec![OperationSimVerdict::Success],
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        driver.step(
            ExecutionOperation::SimulateBundle {
                entry_point,
                operations: vec![(fixture.hash_string.parse().unwrap(), fixture.packed.clone())],
            },
            ExecutionOutcome::BundleVerdict {
                verdict: BundleSimVerdict::Success(sim_data()),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let calldata =
            crate::abi::handle_ops_calldata(std::slice::from_ref(&fixture.packed.packed), TREASURY)
                .to_vec();
        driver.step(
            ExecutionOperation::FetchTransactionContext {
                entry_point,
                calldata: calldata.clone(),
            },
            ExecutionOutcome::Context { context: context() },
        );
        driver.step(
            ExecutionOperation::FetchMarketPrice,
            ExecutionOutcome::Failed {
                message: "Binance native USD price request failed".into(),
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let signed_raw = [0x02u8, 0x01, 0x02, 0x03];
        let signed_hash = alloy::primitives::keccak256(signed_raw).to_string();
        driver.step(
            ExecutionOperation::SignBundle {
                request: super::BundleSignRequest {
                    nonce: 7,
                    gas_limit: 100,
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 0,
                    entry_point,
                    calldata,
                },
            },
            ExecutionOutcome::Signed {
                signed: SignedBundle {
                    raw_transaction_hex: "0x02010203".into(),
                    transaction_hash: signed_hash.clone(),
                    nonce: 7,
                },
            },
        );
        driver.step(
            ExecutionOperation::EnsureLaneLease,
            ExecutionOutcome::LeaseHeld { held: true },
        );
        let intent = crate::task::PreparedBundleIntent {
            chain_id: CHAIN_ID,
            lane: fixture.routed.lane,
            entry_point: entry_point.to_string(),
            raw_transaction: "0x02010203".into(),
            transaction_hash: signed_hash.clone(),
            nonce: 7,
            user_operation_hashes: vec![fixture.hash_string.clone()],
        };
        driver.step(
            ExecutionOperation::SavePreparedBundle {
                intent: intent.clone(),
            },
            ExecutionOutcome::Saved { saved: true },
        );
        driver.step(
            ExecutionOperation::CheckBroadcastSeen {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Seen { seen: false },
        );
        driver.step(
            ExecutionOperation::BroadcastRaw {
                raw_transaction: signed_raw.to_vec(),
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Sent {
                reply: BroadcastReply::Rejected {
                    reason: "nonce too low: next nonce 8".into(),
                },
            },
        );
        driver.step(
            ExecutionOperation::ForgetBroadcast {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::ProbeTransactionKnown {
                transaction_hash: signed_hash.clone(),
            },
            ExecutionOutcome::Known { known: false },
        );
        driver.step(
            ExecutionOperation::ProbeStaleNonce {
                intent: intent.clone(),
            },
            ExecutionOutcome::Stale { stale: true },
        );
        driver.step(
            ExecutionOperation::ClearStaleIntent {
                intent,
                reason: "nonce too low: next nonce 8".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::RecordDeferred {
                hash: fixture.hash_string.clone(),
                stage: "broadcast",
                reason: "signed handleOps transaction awaits broadcast confirmation".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.step(
            ExecutionOperation::NotifyIssue {
                hash: fixture.hash_string.clone(),
                stage: "broadcast",
                reason: "signed handleOps transaction awaits broadcast confirmation".into(),
            },
            ExecutionOutcome::Done,
        );
        driver.assert_settled(&[ItemResolution::Failed {
            reason: "UserOperation execution was deferred".into(),
        }]);
    }
}

#[cfg(test)]
mod policy_tests {
    use super::{AdmissionAction, admission_action, should_notify_executor_deferred};

    #[test]
    fn recovers_only_a_matching_unadmitted_queue_record() {
        assert_eq!(admission_action(true, true), AdmissionAction::Execute);
        assert_eq!(admission_action(false, true), AdmissionAction::Recover);
        assert_eq!(admission_action(true, false), AdmissionAction::DeadLetter);
        assert_eq!(admission_action(false, false), AdmissionAction::DeadLetter);
    }

    #[test]
    fn alerts_only_for_executor_failures_not_expected_handoffs() {
        assert!(should_notify_executor_deferred("rpc"));
        assert!(should_notify_executor_deferred("assets"));
        assert!(should_notify_executor_deferred("execution"));
        assert!(should_notify_executor_deferred("simulation"));
        assert!(should_notify_executor_deferred("bundle_simulation"));
        assert!(should_notify_executor_deferred("broadcast"));
        assert!(!should_notify_executor_deferred("lease"));
        assert!(!should_notify_executor_deferred("funding"));
        assert!(!should_notify_executor_deferred("simulation_deployment"));
        // An in-budget market hold is an expected hand-off, not a page.
        assert!(!should_notify_executor_deferred("in_band_settlement_hold"));
        // Allowlist semantics: an unknown stage stays silent by default.
        assert!(!should_notify_executor_deferred("some_future_stage"));
    }
}
