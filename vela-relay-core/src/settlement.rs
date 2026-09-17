//! In-band settlement vocabulary. The evaluation and repricing math migrates
//! here from the shell's executor (US3); today this module owns the
//! user-visible reason strings, which are byte-frozen: wallets poll them from
//! the status endpoint.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{Display, Formatter},
    str::FromStr,
};

use alloy::primitives::{Address, B256, Bytes, U256, address, aliases::U512, keccak256};

/// The diagnostic a held operation carries while it waits. Deliberately distinct from
/// [`settlement_rejection_reason`] — a wallet polling the status endpoint must be able to tell
/// "still going, waiting for gas to come down" from "this will never execute".
pub fn settlement_hold_reason(
    paid: U256,
    required: U256,
    attempt: u32,
    max_attempts: u32,
) -> String {
    format!(
        "waiting for network fees to fit the signed in-band reimbursement: paid={paid}, required={required}, shortfall={}, attempt={attempt}/{max_attempts}",
        required.saturating_sub(paid)
    )
}

pub fn settlement_rejection_reason(paid: U256, required: U256, stable_logs_valid: bool) -> String {
    if paid < required {
        format!(
            "in-band reimbursement is below the required amount: paid={paid}, required={required}, shortfall={}",
            required.saturating_sub(paid)
        )
    } else if !stable_logs_valid {
        "in-band reimbursement transfer logs do not prove payment to the settlement recipient"
            .into()
    } else {
        "in-band reimbursement was rejected".into()
    }
}

const EXECUTE_USER_OP_SELECTOR: [u8; 4] = [0x7b, 0xb3, 0x74, 0x28];
const MULTISEND_SELECTOR: [u8; 4] = [0x8d, 0x80, 0xff, 0x0a];
const ERC20_TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];
const TRUSTED_MULTISEND: Address = address!("38869bf66a61cf6bdb996a6ae40d5853fd43b526");

pub const MIN_NATIVE_FRACTION_DECIMALS: u32 = 5;
pub const MIN_STABLE_FRACTION_DECIMALS: u32 = 2;
pub const USD_PRICE_DECIMALS: u32 = 8;

/// Settlement assets loaded from the controlled chain directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainAssetConfig {
    pub native_decimals: u32,
    /// Required reimbursement ratio in basis points; 14_000 means 1.4x cost.
    pub settlement_markup_bps: u64,
    pub stablecoins: BTreeMap<Address, StablecoinConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StablecoinConfig {
    pub symbol: String,
    pub decimals: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct SettlementInput<'a> {
    pub call_data: &'a [u8],
    /// The exact native-token cost allocated to this operation, before markup.
    pub gas_native_cost: U256,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reimbursement {
    pub native: U256,
    pub stablecoins: BTreeMap<Address, U256>,
}

/// Minimal simulation-log view used to confirm that the statically parsed
/// stablecoin transfer was actually emitted by the allowlisted token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementRejection {
    MalformedCallData,
    ArithmeticOverflow,
    UnsupportedPaymentCombination,
    InsufficientPayment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementEvaluation {
    pub reimbursement: Reimbursement,
    pub gas_native_cost: U256,
    pub payment_asset: Option<Address>,
    pub paid_amount: U256,
    pub required_amount: U256,
    pub rejection: Option<SettlementRejection>,
}

impl SettlementEvaluation {
    pub fn accepted(&self) -> bool {
        self.rejection.is_none()
    }

    /// A shortfall is the one rejection a lower outer fee, or simply a calmer market, can still
    /// satisfy: the payment itself parsed and went to the right recipient, there just was not
    /// enough of it at the price this attempt quoted. Every other rejection is a property of the
    /// signed calldata and can never become payable.
    pub fn is_shortfall(&self) -> bool {
        matches!(
            self.rejection,
            Some(SettlementRejection::InsufficientPayment)
        )
    }

    /// What fraction of the required amount this operation actually paid, in basis points,
    /// saturating at `u64::MAX`. Asset-agnostic: `paid` and `required` are always in the same
    /// unit, and the requirement is linear in the outer fee, so this ratio applied to the quoted
    /// fee gives the fee this payer can afford. A zero requirement reads as fully paid.
    pub fn paid_ratio_bps(&self) -> u64 {
        if self.required_amount.is_zero() {
            return 10_000;
        }
        let scaled = widen_u256(self.paid_amount) * widen_u256(U256::from(10_000u64));
        let ratio = scaled / widen_u256(self.required_amount);
        u64::try_from(ratio).unwrap_or(u64::MAX)
    }
}

/// The outer fee cap a bundle's weakest payer can fund, given the fee this attempt quoted.
///
/// The quoted cap is `2 × base fee + tip`, which is inclusion headroom rather than cost — the
/// transaction only ever pays `base fee + tip`. So when a payer falls short of the requirement at
/// the quoted cap, repricing the bundle down to what they did pay keeps the relay's markup fully
/// intact: the reimbursement covers `markup × gas × new cap`, and the chain can never charge more
/// than `new cap`. Returns `None` when nothing is affordable.
pub fn affordable_fee_per_gas(
    quoted_max_fee_per_gas: u128,
    evaluations: &[SettlementEvaluation],
) -> Option<u128> {
    let weakest = evaluations
        .iter()
        .map(SettlementEvaluation::paid_ratio_bps)
        .min()?;
    let affordable = quoted_max_fee_per_gas.checked_mul(u128::from(weakest.min(10_000)))? / 10_000;
    (affordable > 0).then_some(affordable)
}

/// The lowest fee cap worth signing: `floor_bps` of the base fee, plus the tip. Below this the
/// transaction risks sitting unmined, and the executor has no fee-bump path to rescue it.
pub fn inclusion_floor_fee_per_gas(
    base_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    floor_bps: u64,
) -> Option<u128> {
    base_fee_per_gas
        .checked_mul(u128::from(floor_bps))
        .map(|scaled| scaled / 10_000)?
        .checked_add(max_priority_fee_per_gas)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchSettlementEvaluation {
    pub operations: Vec<SettlementEvaluation>,
}

impl BatchSettlementEvaluation {
    pub fn all_accepted(&self) -> bool {
        self.operations.iter().all(SettlementEvaluation::accepted)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementError {
    InvalidConfiguration(&'static str),
    ArithmeticOverflow,
    MissingNativeUsdPrice,
}

impl Display for SettlementError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => formatter.write_str(message),
            Self::ArithmeticOverflow => formatter.write_str("settlement arithmetic overflow"),
            Self::MissingNativeUsdPrice => formatter.write_str("missing Binance native USD price"),
        }
    }
}

impl std::error::Error for SettlementError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReimbursementParseError {
    MalformedCallData,
    ArithmeticOverflow,
}

/// Parse only transfers that the Safe actually executes through its canonical
/// `executeUserOp -> MultiSend(DELEGATECALL) -> CALL` path.
pub fn parse_reimbursement(
    call_data: &[u8],
    recipient: Address,
    stablecoin_allowlist: &BTreeSet<Address>,
) -> Result<Reimbursement, ReimbursementParseError> {
    let transactions = decode_multisend_transactions(call_data)
        .ok_or(ReimbursementParseError::MalformedCallData)?;
    let mut reimbursement = Reimbursement::default();
    let mut offset = 0usize;

    while offset < transactions.len() {
        let operation = *transactions
            .get(offset)
            .ok_or(ReimbursementParseError::MalformedCallData)?;
        let to = read_packed_address(transactions, offset + 1)
            .ok_or(ReimbursementParseError::MalformedCallData)?;
        let value = read_u256_word(transactions, offset + 21)
            .ok_or(ReimbursementParseError::MalformedCallData)?;
        let data_length = read_usize_word(transactions, offset + 53)
            .ok_or(ReimbursementParseError::MalformedCallData)?;
        let data_start = offset
            .checked_add(85)
            .ok_or(ReimbursementParseError::MalformedCallData)?;
        let data_end = data_start
            .checked_add(data_length)
            .ok_or(ReimbursementParseError::MalformedCallData)?;
        let data = transactions
            .get(data_start..data_end)
            .ok_or(ReimbursementParseError::MalformedCallData)?;

        // MultiSend operation 0 is CALL. A DELEGATECALL containing transfer-shaped
        // bytes is deliberately never credited as reimbursement.
        if operation == 0 {
            if to == recipient && !value.is_zero() {
                reimbursement.native = reimbursement
                    .native
                    .checked_add(value)
                    .ok_or(ReimbursementParseError::ArithmeticOverflow)?;
            }

            if stablecoin_allowlist.contains(&to)
                && let Some((transfer_recipient, amount)) = decode_erc20_transfer(data)
                && transfer_recipient == recipient
                && !amount.is_zero()
            {
                let total = reimbursement.stablecoins.entry(to).or_default();
                *total = total
                    .checked_add(amount)
                    .ok_or(ReimbursementParseError::ArithmeticOverflow)?;
            }
        }

        offset = data_end;
    }

    Ok(reimbursement)
}

/// Confirm stablecoin reimbursement against the logs from the exact final
/// `handleOps` bundle simulation. Logs from another token, sender, or recipient
/// never count. Native transfers do not have a standard log and are therefore
/// covered by successful final bundle simulation instead.
pub fn verify_stable_transfer_logs(
    reimbursement: &Reimbursement,
    sender: Address,
    recipient: Address,
    logs: &[SettlementLog],
) -> bool {
    let transfer_signature = keccak256(b"Transfer(address,address,uint256)");
    let sender_topic = B256::from(address_word(sender));
    let recipient_topic = B256::from(address_word(recipient));

    reimbursement.stablecoins.iter().all(|(token, expected)| {
        let mut actual = U256::ZERO;
        for log in logs {
            if log.address != *token
                || log.topics.len() != 3
                || log.topics[0] != transfer_signature
                || log.topics[1] != sender_topic
                || log.topics[2] != recipient_topic
                || log.data.len() != 32
            {
                continue;
            }

            let amount = U256::from_be_slice(&log.data);
            let Some(total) = actual.checked_add(amount) else {
                return false;
            };
            actual = total;
        }
        actual >= *expected
    })
}

/// Evaluate each operation independently. Surplus paid by one operation cannot
/// subsidize another operation in the same `handleOps` batch.
pub fn evaluate_batch(
    recipient: Address,
    config: &ChainAssetConfig,
    inputs: &[SettlementInput<'_>],
    native_usd_price: Option<U256>,
) -> Result<BatchSettlementEvaluation, SettlementError> {
    validate_config(config)?;
    let native_floor = minimum_amount(config.native_decimals, MIN_NATIVE_FRACTION_DECIMALS)?;
    let allowlist = config.stablecoins.keys().copied().collect::<BTreeSet<_>>();
    let mut operations = Vec::with_capacity(inputs.len());

    for input in inputs {
        let marked_cost = mul_div_ceil(
            input.gas_native_cost,
            U256::from(config.settlement_markup_bps),
            U256::from(10_000u64),
        )?;

        let reimbursement = match parse_reimbursement(input.call_data, recipient, &allowlist) {
            Ok(reimbursement) => reimbursement,
            Err(error) => {
                operations.push(rejected_parse(
                    input.gas_native_cost,
                    marked_cost,
                    native_floor,
                    error,
                ));
                continue;
            }
        };

        match evaluate_one(
            reimbursement,
            input.gas_native_cost,
            marked_cost,
            native_floor,
            config,
            native_usd_price,
        ) {
            Ok(evaluation) => operations.push(evaluation),
            Err(SettlementError::ArithmeticOverflow) => operations.push(SettlementEvaluation {
                reimbursement: Reimbursement::default(),
                gas_native_cost: input.gas_native_cost,
                payment_asset: None,
                paid_amount: U256::ZERO,
                required_amount: marked_cost.max(native_floor),
                rejection: Some(SettlementRejection::ArithmeticOverflow),
            }),
            Err(error) => return Err(error),
        }
    }

    Ok(BatchSettlementEvaluation { operations })
}

fn evaluate_one(
    reimbursement: Reimbursement,
    gas_native_cost: U256,
    marked_cost: U256,
    native_floor: U256,
    config: &ChainAssetConfig,
    native_usd_price: Option<U256>,
) -> Result<SettlementEvaluation, SettlementError> {
    let has_native = !reimbursement.native.is_zero();
    match (has_native, reimbursement.stablecoins.len()) {
        (true, 0) => {
            let required_amount = marked_cost.max(native_floor);
            let paid_amount = reimbursement.native;
            Ok(SettlementEvaluation {
                reimbursement,
                gas_native_cost,
                payment_asset: None,
                paid_amount,
                required_amount,
                rejection: (paid_amount < required_amount)
                    .then_some(SettlementRejection::InsufficientPayment),
            })
        }
        (false, 1) => {
            let (&token, &paid_amount) = reimbursement
                .stablecoins
                .first_key_value()
                .expect("stablecoin length is one");
            let asset =
                config
                    .stablecoins
                    .get(&token)
                    .ok_or(SettlementError::InvalidConfiguration(
                        "parsed stablecoin is not in the trusted asset policy",
                    ))?;
            let converted = native_to_usd_stable_ceil(
                marked_cost,
                config.native_decimals,
                native_usd_price.ok_or(SettlementError::MissingNativeUsdPrice)?,
                asset.decimals,
            )?;
            let stable_floor = minimum_amount(asset.decimals, MIN_STABLE_FRACTION_DECIMALS)?;
            let required_amount = converted.max(stable_floor);
            Ok(SettlementEvaluation {
                reimbursement,
                gas_native_cost,
                payment_asset: Some(token),
                paid_amount,
                required_amount,
                rejection: (paid_amount < required_amount)
                    .then_some(SettlementRejection::InsufficientPayment),
            })
        }
        (false, 0) => Ok(SettlementEvaluation {
            reimbursement,
            gas_native_cost,
            payment_asset: None,
            paid_amount: U256::ZERO,
            required_amount: marked_cost.max(native_floor),
            rejection: Some(SettlementRejection::InsufficientPayment),
        }),
        _ => Ok(SettlementEvaluation {
            reimbursement,
            gas_native_cost,
            payment_asset: None,
            paid_amount: U256::ZERO,
            required_amount: marked_cost.max(native_floor),
            rejection: Some(SettlementRejection::UnsupportedPaymentCombination),
        }),
    }
}

pub fn native_to_usd_stable_ceil(
    native_amount: U256,
    native_decimals: u32,
    native_usd_price: U256,
    stable_decimals: u32,
) -> Result<U256, SettlementError> {
    if native_usd_price.is_zero() {
        return Err(SettlementError::ArithmeticOverflow);
    }
    let denominator_exponent = native_decimals
        .checked_add(USD_PRICE_DECIMALS)
        .ok_or(SettlementError::ArithmeticOverflow)?;
    let numerator_scale =
        checked_pow10_u512(stable_decimals).ok_or(SettlementError::ArithmeticOverflow)?;
    let denominator_scale =
        checked_pow10_u512(denominator_exponent).ok_or(SettlementError::ArithmeticOverflow)?;
    let numerator = widen_u256(native_amount)
        .checked_mul(widen_u256(native_usd_price))
        .and_then(|value| value.checked_mul(numerator_scale))
        .ok_or(SettlementError::ArithmeticOverflow)?;
    let denominator = denominator_scale;
    if denominator.is_zero() {
        return Err(SettlementError::ArithmeticOverflow);
    }
    let quotient = numerator / denominator;
    let rounded = if numerator % denominator == U512::ZERO {
        quotient
    } else {
        quotient
            .checked_add(U512::ONE)
            .ok_or(SettlementError::ArithmeticOverflow)?
    };
    narrow_u512(rounded)
}

fn rejected_parse(
    gas_native_cost: U256,
    marked_cost: U256,
    native_floor: U256,
    error: ReimbursementParseError,
) -> SettlementEvaluation {
    SettlementEvaluation {
        reimbursement: Reimbursement::default(),
        gas_native_cost,
        payment_asset: None,
        paid_amount: U256::ZERO,
        required_amount: marked_cost.max(native_floor),
        rejection: Some(match error {
            ReimbursementParseError::MalformedCallData => SettlementRejection::MalformedCallData,
            ReimbursementParseError::ArithmeticOverflow => SettlementRejection::ArithmeticOverflow,
        }),
    }
}

fn validate_config(config: &ChainAssetConfig) -> Result<(), SettlementError> {
    if config.settlement_markup_bps < 10_000 {
        return Err(SettlementError::InvalidConfiguration(
            "settlement markup cannot be below 10_000 bps",
        ));
    }
    minimum_amount(config.native_decimals, MIN_NATIVE_FRACTION_DECIMALS)?;
    for asset in config.stablecoins.values() {
        minimum_amount(asset.decimals, MIN_STABLE_FRACTION_DECIMALS)?;
        if asset.symbol.trim().is_empty() {
            return Err(SettlementError::InvalidConfiguration(
                "stablecoin symbol cannot be empty",
            ));
        }
    }
    Ok(())
}

fn decode_multisend_transactions(call_data: &[u8]) -> Option<&[u8]> {
    if call_data.get(..4)? != EXECUTE_USER_OP_SELECTOR {
        return None;
    }
    let args = call_data.get(4..)?;
    let target = read_address_word(args, 0)?;
    let data_offset = read_usize_word(args, 64)?;
    let operation = read_u256_word(args, 96)?;
    if target != TRUSTED_MULTISEND || operation != U256::from(1u8) {
        return None;
    }

    let inner_data = read_dynamic_bytes(args, data_offset)?;
    if inner_data.get(..4)? != MULTISEND_SELECTOR {
        return None;
    }
    let inner_args = inner_data.get(4..)?;
    let transaction_offset = read_usize_word(inner_args, 0)?;
    read_dynamic_bytes(inner_args, transaction_offset)
}

fn decode_erc20_transfer(data: &[u8]) -> Option<(Address, U256)> {
    if data.len() != 68 || data.get(..4)? != ERC20_TRANSFER_SELECTOR {
        return None;
    }
    Some((read_address_word(data, 4)?, read_u256_word(data, 36)?))
}

fn read_dynamic_bytes(data: &[u8], offset: usize) -> Option<&[u8]> {
    if !offset.is_multiple_of(32) {
        return None;
    }
    let length = read_usize_word(data, offset)?;
    let start = offset.checked_add(32)?;
    let end = start.checked_add(length)?;
    data.get(start..end)
}

fn read_address_word(data: &[u8], offset: usize) -> Option<Address> {
    let word = data.get(offset..offset.checked_add(32)?)?;
    if word[..12].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(Address::from_slice(&word[12..]))
}

fn read_packed_address(data: &[u8], offset: usize) -> Option<Address> {
    Some(Address::from_slice(
        data.get(offset..offset.checked_add(20)?)?,
    ))
}

fn read_u256_word(data: &[u8], offset: usize) -> Option<U256> {
    Some(U256::from_be_slice(
        data.get(offset..offset.checked_add(32)?)?,
    ))
}

fn read_usize_word(data: &[u8], offset: usize) -> Option<usize> {
    let value = read_u256_word(data, offset)?;
    if value > U256::from(usize::MAX) {
        return None;
    }
    Some(value.to::<usize>())
}

fn address_word(address: Address) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    word
}

/// The smallest reimbursement worth accepting for an asset with `decimals`,
/// keeping `fraction_decimals` of user-visible precision. Shared by the RPC
/// validation/quoting path and the executor evaluation.
pub fn minimum_amount(decimals: u32, fraction_decimals: u32) -> Result<U256, SettlementError> {
    let exponent =
        decimals
            .checked_sub(fraction_decimals)
            .ok_or(SettlementError::InvalidConfiguration(
                "asset decimals are below the settlement floor",
            ))?;
    checked_pow10(exponent).ok_or(SettlementError::InvalidConfiguration(
        "asset decimals exceed U256 settlement arithmetic",
    ))
}

fn checked_pow10(exponent: u32) -> Option<U256> {
    let mut value = U256::ONE;
    for _ in 0..exponent {
        value = value.checked_mul(U256::from(10u8))?;
    }
    Some(value)
}

fn checked_pow10_u512(exponent: u32) -> Option<U512> {
    let mut value = U512::ONE;
    for _ in 0..exponent {
        value = value.checked_mul(U512::from(10u8))?;
    }
    Some(value)
}

fn mul_div_ceil(left: U256, right: U256, denominator: U256) -> Result<U256, SettlementError> {
    if denominator.is_zero() {
        return Err(SettlementError::ArithmeticOverflow);
    }
    let denominator = widen_u256(denominator);
    let product = widen_u256(left) * widen_u256(right);
    let quotient = product / denominator;
    let rounded = if product % denominator == U512::ZERO {
        quotient
    } else {
        quotient
            .checked_add(U512::ONE)
            .ok_or(SettlementError::ArithmeticOverflow)?
    };
    narrow_u512(rounded)
}

fn widen_u256(value: U256) -> U512 {
    let limbs = value.into_limbs();
    U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
}

fn narrow_u512(value: U512) -> Result<U256, SettlementError> {
    let limbs = value.into_limbs();
    if limbs[4..].iter().any(|limb| *limb != 0) {
        return Err(SettlementError::ArithmeticOverflow);
    }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

/// One USD in the 8-decimal fixed-point representation used across settlement
/// pricing (`10^USD_PRICE_DECIMALS`).
pub const USD_PRICE_SCALE: u64 = 100_000_000;

/// Gnosis mainnet: xDAI is the native gas asset and is defined to be
/// USD-pegged.
pub const GNOSIS_CHAIN_ID: u64 = 100;

pub const fn is_gnosis_chain(chain_id: u64) -> bool {
    chain_id == GNOSIS_CHAIN_ID
}

/// Arc mainnet and its testnet: the native gas asset IS USDC.
pub const ARC_CHAIN_ID: u64 = 5_042;
pub const ARC_TESTNET_CHAIN_ID: u64 = 5_042_002;

pub const fn is_arc_chain(chain_id: u64) -> bool {
    chain_id == ARC_CHAIN_ID || chain_id == ARC_TESTNET_CHAIN_ID
}

/// The native/USD price source for a chain. xDAI is defined to be USD-pegged
/// and Arc's native coin IS USDC, which also keeps their stablecoin settlement
/// and relayer funding independent of Binance availability; every other chain
/// consults the market.
///
/// On Arc the peg is load-bearing beyond convenience: the client's fee floor is
/// "$0.01 worth of the native coin" and it reads the price out of THIS quote.
/// An unpriced native asset degrades that floor to a blind 0.001-coin fallback
/// — a tenth of a cent on a dollar coin — so a market outage must not be able
/// to move what a person is charged on a chain whose coin is the dollar.
pub fn pegged_native_usd_price(chain_id: u64) -> Option<U256> {
    (is_gnosis_chain(chain_id) || is_arc_chain(chain_id)).then_some(U256::from(USD_PRICE_SCALE))
}

/// Converts Binance's decimal `SYMBOLUSDT` quote into an 8-decimal USD fixed-point value.
/// Extra precision rounds upward so a stablecoin reimbursement never undercharges the relay.
pub fn parse_market_usd_price(value: &str) -> Option<U256> {
    let value = value.trim();
    let mut parts = value.split('.');
    let whole = parts.next()?;
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    let scale = U256::from(USD_PRICE_SCALE);
    let whole = U256::from_str(whole).ok()?.checked_mul(scale)?;
    let kept = &fraction[..fraction.len().min(USD_PRICE_DECIMALS as usize)];
    let fraction_value = if kept.is_empty() {
        U256::ZERO
    } else {
        U256::from_str(kept)
            .ok()?
            .checked_mul(U256::from(10u8).pow(U256::from(USD_PRICE_DECIMALS - kept.len() as u32)))?
    };
    let mut price = whole.checked_add(fraction_value)?;
    if value
        .split_once('.')
        .is_some_and(|(_, fraction)| fraction.len() > USD_PRICE_DECIMALS as usize)
        && value.split_once('.').is_some_and(|(_, fraction)| {
            fraction[USD_PRICE_DECIMALS as usize..]
                .bytes()
                .any(|byte| byte != b'0')
        })
    {
        price = price.checked_add(U256::ONE)?;
    }
    (!price.is_zero()).then_some(price)
}

/// The Tempo in-band requirement: markup applied with ceiling rounding, then
/// the same $0.01 minimum used by the generic stablecoin settlement path.
/// `None` on overflow.
pub fn marked_tempo_cost(cost: U256, markup_bps: u64) -> Option<U256> {
    let numerator = cost.checked_mul(U256::from(markup_bps))?;
    let denominator = U256::from(10_000u64);
    let marked = (numerator / denominator)
        .checked_add(U256::from(u8::from(!(numerator % denominator).is_zero())))?;
    Some(marked.max(U256::from(10u128.pow(crate::tempo::PATH_USD_DECIMALS - 2))))
}

/// Whether any operation in the batch pays its reimbursement in an allowlisted
/// stablecoin — the only case that needs a native/USD market price. Judged
/// from the signed calldata alone.
pub fn has_stablecoin_payment(
    recipient: Address,
    chain_assets: &ChainAssetConfig,
    call_datas: &[&[u8]],
) -> bool {
    let allowlist = chain_assets
        .stablecoins
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    call_datas.iter().any(|call_data| {
        parse_reimbursement(call_data, recipient, &allowlist)
            .is_ok_and(|reimbursement| !reimbursement.stablecoins.is_empty())
    })
}

/// Fee inputs to the settlement verdict, all supplied by the shell.
#[derive(Clone, Copy, Debug)]
pub struct FeeContext {
    /// The quoted cap: `2 × base fee + tip`.
    pub quoted_fee_per_gas: u128,
    pub base_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    /// Basis points of the base fee below which a cap is not worth signing.
    pub inclusion_floor_bps: u64,
}

/// The settlement verdict for one bundle at the current market.
#[derive(Clone, Debug)]
pub enum SettlementDecision {
    /// Keep the quoted fee: everything is accepted, or a rejection no price
    /// can cure is present, or no viable repricing exists. The gate downstream
    /// speaks per operation.
    KeepQuote {
        evaluation: BatchSettlementEvaluation,
    },
    /// A shortfall exists but the affordable fee cannot fund an includable
    /// outer transaction (below the floor, or not lower than the quote). Keep
    /// the quote; the shell logs the diagnosis.
    FloorUnfundable {
        evaluation: BatchSettlementEvaluation,
        affordable: u128,
        floor: u128,
    },
    /// Every payer clears the requirement at this lower cap: sign at
    /// `fee_per_gas` and use the repriced evaluation.
    Reprice {
        fee_per_gas: u128,
        evaluation: BatchSettlementEvaluation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettlementDecisionError {
    CostOverflow,
    Evaluation(SettlementError),
}

impl Display for SettlementDecisionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            // Byte-frozen: the shell folds this into its executor error text.
            Self::CostOverflow => formatter.write_str("bundle native cost overflow"),
            Self::Evaluation(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for SettlementDecisionError {}

/// Settles the bundle at a fee the payers can actually fund.
///
/// The quoted cap buys inclusion headroom, not cost — the chain only ever
/// charges `base fee + tip` — so a reimbursement short at the quoted cap is
/// usually still a good payment at a lower one. Rather than refuse a signature
/// the user already gave, the signed reimbursement is treated as the budget
/// and the outer transaction repriced down into it, provided the result still
/// clears the inclusion floor. The markup survives untouched: the requirement
/// is linear in the cap. Repricing only ever lowers the cap, so an operation
/// accepted at the quoted fee stays accepted; anything still short is left for
/// the hold/reject gate downstream.
pub fn decide_settlement(
    recipient: Address,
    chain_assets: &ChainAssetConfig,
    call_datas: &[&[u8]],
    allocations: &[U256],
    native_usd_price: Option<U256>,
    fees: &FeeContext,
) -> Result<SettlementDecision, SettlementDecisionError> {
    let costs_at = |fee: u128| -> Result<Vec<U256>, SettlementDecisionError> {
        allocations
            .iter()
            .map(|gas| {
                crate::cost::native_cost(*gas, fee).ok_or(SettlementDecisionError::CostOverflow)
            })
            .collect()
    };
    let evaluate_at =
        |costs: &[U256]| -> Result<BatchSettlementEvaluation, SettlementDecisionError> {
            let inputs = call_datas
                .iter()
                .zip(costs)
                .map(|(call_data, cost)| SettlementInput {
                    call_data,
                    gas_native_cost: *cost,
                })
                .collect::<Vec<_>>();
            evaluate_batch(recipient, chain_assets, &inputs, native_usd_price)
                .map_err(SettlementDecisionError::Evaluation)
        };

    let settlement = evaluate_at(&costs_at(fees.quoted_fee_per_gas)?)?;
    // Nothing to renegotiate, or a rejection no price can cure (malformed
    // calldata, an unsupported asset): leave the quote alone.
    if settlement.all_accepted()
        || settlement
            .operations
            .iter()
            .any(|evaluation| !evaluation.accepted() && !evaluation.is_shortfall())
    {
        return Ok(SettlementDecision::KeepQuote {
            evaluation: settlement,
        });
    }

    let Some(affordable) = affordable_fee_per_gas(fees.quoted_fee_per_gas, &settlement.operations)
    else {
        return Ok(SettlementDecision::KeepQuote {
            evaluation: settlement,
        });
    };
    let Some(floor) = inclusion_floor_fee_per_gas(
        fees.base_fee_per_gas,
        fees.max_priority_fee_per_gas,
        fees.inclusion_floor_bps,
    ) else {
        return Ok(SettlementDecision::KeepQuote {
            evaluation: settlement,
        });
    };
    if affordable < floor || affordable >= fees.quoted_fee_per_gas {
        return Ok(SettlementDecision::FloorUnfundable {
            evaluation: settlement,
            affordable,
            floor,
        });
    }

    let repriced = evaluate_at(&costs_at(affordable)?)?;
    if !repriced.all_accepted() {
        return Ok(SettlementDecision::KeepQuote {
            evaluation: settlement,
        });
    }
    Ok(SettlementDecision::Reprice {
        fee_per_gas: affordable,
        evaluation: repriced,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use alloy::primitives::{Address, B256, U256, address, keccak256};

    use super::{
        ChainAssetConfig, Reimbursement, ReimbursementParseError, SettlementEvaluation,
        SettlementInput, SettlementLog, SettlementRejection, StablecoinConfig,
        affordable_fee_per_gas, evaluate_batch, inclusion_floor_fee_per_gas,
        native_to_usd_stable_ceil, parse_reimbursement, settlement_hold_reason,
        settlement_rejection_reason, verify_stable_transfer_logs,
    };

    #[test]
    fn explains_an_insufficient_in_band_reimbursement() {
        assert_eq!(
            settlement_rejection_reason(
                U256::from(105_959_625_000_000_000u64),
                U256::from(111_625_968_750_000_000u64),
                true,
            ),
            "in-band reimbursement is below the required amount: paid=105959625000000000, required=111625968750000000, shortfall=5666343750000000"
        );
    }

    #[test]
    fn distinguishes_rejection_causes() {
        assert_eq!(
            settlement_rejection_reason(U256::from(2u8), U256::from(1u8), false),
            "in-band reimbursement transfer logs do not prove payment to the settlement recipient"
        );
        assert_eq!(
            settlement_rejection_reason(U256::from(2u8), U256::from(1u8), true),
            "in-band reimbursement was rejected"
        );
    }

    #[test]
    fn hold_reason_reports_progress_against_the_budget() {
        assert_eq!(
            settlement_hold_reason(U256::from(90u8), U256::from(100u8), 3, 12),
            "waiting for network fees to fit the signed in-band reimbursement: \
             paid=90, required=100, shortfall=10, attempt=3/12"
        );
    }

    const RECIPIENT: Address = address!("1111111111111111111111111111111111111111");
    const STABLECOIN: Address = address!("2222222222222222222222222222222222222222");
    const SENDER: Address = address!("6666666666666666666666666666666666666666");

    fn evaluation(
        paid: u128,
        required: u128,
        rejection: Option<SettlementRejection>,
    ) -> SettlementEvaluation {
        SettlementEvaluation {
            reimbursement: Reimbursement::default(),
            gas_native_cost: U256::from(required),
            payment_asset: None,
            paid_amount: U256::from(paid),
            required_amount: U256::from(required),
            rejection,
        }
    }

    #[test]
    fn a_fully_paid_operation_reprices_to_nothing_lower() {
        let paid = evaluation(1_000, 1_000, None);
        assert_eq!(paid.paid_ratio_bps(), 10_000);
        assert_eq!(affordable_fee_per_gas(100, &[paid]), Some(100));
    }

    #[test]
    fn the_weakest_payer_sets_the_bundle_fee() {
        // 99.13% paid — the exact shortfall from issue #137's rejected payroll.
        let short = evaluation(
            4_603_572_816_058_075_872,
            4_643_859_330_530_512_244,
            Some(SettlementRejection::InsufficientPayment),
        );
        assert_eq!(short.paid_ratio_bps(), 9_913);
        let generous = evaluation(3_000, 1_000, None);
        // 100 gwei quoted → the bundle reprices to what the weakest payer funded, not the average.
        assert_eq!(
            affordable_fee_per_gas(100_000_000_000, &[generous, short]),
            Some(99_130_000_000),
        );
    }

    #[test]
    fn repricing_never_raises_the_quoted_fee() {
        let overpaid = evaluation(10_000, 1_000, None);
        assert_eq!(affordable_fee_per_gas(500, &[overpaid]), Some(500));
    }

    #[test]
    fn a_zero_requirement_reads_as_fully_paid() {
        assert_eq!(evaluation(0, 0, None).paid_ratio_bps(), 10_000);
    }

    #[test]
    fn a_payment_far_below_the_requirement_reprices_toward_zero() {
        let dust = evaluation(1, 1_000_000, Some(SettlementRejection::InsufficientPayment));
        assert_eq!(dust.paid_ratio_bps(), 0);
        assert_eq!(affordable_fee_per_gas(100, &[dust]), None);
    }

    #[test]
    fn only_an_insufficient_payment_counts_as_a_shortfall() {
        assert!(evaluation(1, 2, Some(SettlementRejection::InsufficientPayment)).is_shortfall());
        for terminal in [
            SettlementRejection::MalformedCallData,
            SettlementRejection::ArithmeticOverflow,
            SettlementRejection::UnsupportedPaymentCombination,
        ] {
            assert!(!evaluation(1, 2, Some(terminal)).is_shortfall());
        }
        assert!(!evaluation(2, 2, None).is_shortfall());
    }

    #[test]
    fn the_inclusion_floor_scales_the_base_fee_and_adds_the_tip() {
        // 1.5x of a 100 gwei base fee, plus a 30 gwei tip.
        assert_eq!(
            inclusion_floor_fee_per_gas(100_000_000_000, 30_000_000_000, 15_000),
            Some(180_000_000_000),
        );
    }

    #[test]
    fn the_issue_137_shortfall_still_clears_the_inclusion_floor() {
        // The rejected payroll: 5,478,008 gas at a 2x base-fee cap, 0.87% underpaid.
        let base_fee = 93_000_000_000u128;
        let tip = 94_000_000_000u128;
        let quoted = base_fee * 2 + tip;
        let short = evaluation(
            9_913,
            10_000,
            Some(SettlementRejection::InsufficientPayment),
        );
        let affordable = affordable_fee_per_gas(quoted, &[short]).unwrap();
        let floor = inclusion_floor_fee_per_gas(base_fee, tip, 15_000).unwrap();
        assert!(affordable >= floor, "affordable={affordable} floor={floor}");
        // And the repriced cap still covers what the chain actually charges.
        assert!(affordable >= base_fee + tip);
    }

    #[test]
    fn parses_values_above_u128_without_saturation() {
        let amount = U256::from(u128::MAX) + U256::ONE;
        let call_data = safe_multisend(&[Entry::native(RECIPIENT, amount)]);
        let parsed = parse_reimbursement(&call_data, RECIPIENT, &BTreeSet::new()).unwrap();

        assert_eq!(parsed.native, amount);
    }

    #[test]
    fn rejects_u256_sum_overflow_instead_of_wrapping_or_saturating() {
        let call_data = safe_multisend(&[
            Entry::native(RECIPIENT, U256::MAX),
            Entry::native(RECIPIENT, U256::ONE),
        ]);

        assert_eq!(
            parse_reimbursement(&call_data, RECIPIENT, &BTreeSet::new()),
            Err(ReimbursementParseError::ArithmeticOverflow)
        );
    }

    #[test]
    fn does_not_credit_malicious_transfer_shaped_calldata() {
        let mut entries = vec![Entry::erc20(STABLECOIN, RECIPIENT, U256::from(50_000u64))];
        entries[0].operation = 1;
        let call_data = safe_multisend(&entries);
        let allowlist = BTreeSet::from([STABLECOIN]);
        let parsed = parse_reimbursement(&call_data, RECIPIENT, &allowlist).unwrap();
        assert!(parsed.stablecoins.is_empty());

        let mut wrong_outer_operation = safe_multisend(&[Entry::native(RECIPIENT, U256::ONE)]);
        // executeUserOp's operation word starts at selector + 96.
        wrong_outer_operation[4 + 96 + 31] = 0;
        assert_eq!(
            parse_reimbursement(&wrong_outer_operation, RECIPIENT, &allowlist),
            Err(ReimbursementParseError::MalformedCallData)
        );

        let unknown_token = address!("5555555555555555555555555555555555555555");
        let call_data = safe_multisend(&[Entry::erc20(
            unknown_token,
            RECIPIENT,
            U256::from(50_000u64),
        )]);
        assert!(
            parse_reimbursement(&call_data, RECIPIENT, &allowlist)
                .unwrap()
                .stablecoins
                .is_empty()
        );
    }

    #[test]
    fn evaluates_each_operation_without_cross_subsidy() {
        let config = native_config(5);
        let rich = safe_multisend(&[Entry::native(RECIPIENT, U256::from(399u64))]);
        let poor = safe_multisend(&[Entry::native(RECIPIENT, U256::ONE)]);
        let evaluation = evaluate_batch(
            RECIPIENT,
            &config,
            &[
                SettlementInput {
                    call_data: &rich,
                    gas_native_cost: U256::from(100u64),
                },
                SettlementInput {
                    call_data: &poor,
                    gas_native_cost: U256::from(100u64),
                },
            ],
            None,
        )
        .unwrap();

        assert!(evaluation.operations[0].accepted());
        assert_eq!(
            evaluation.operations[1].rejection,
            Some(SettlementRejection::InsufficientPayment)
        );
        assert!(!evaluation.all_accepted());
    }

    #[test]
    fn uses_configured_markup_with_ceiling_rounding() {
        let mut config = native_config(5);
        config.settlement_markup_bps = 15_001;
        let four = safe_multisend(&[Entry::native(RECIPIENT, U256::from(4u8))]);
        let five = safe_multisend(&[Entry::native(RECIPIENT, U256::from(5u8))]);
        let evaluation = evaluate_batch(
            RECIPIENT,
            &config,
            &[
                SettlementInput {
                    call_data: &four,
                    gas_native_cost: U256::from(3u8),
                },
                SettlementInput {
                    call_data: &five,
                    gas_native_cost: U256::from(3u8),
                },
            ],
            None,
        )
        .unwrap();

        assert_eq!(evaluation.operations[0].required_amount, U256::from(5u8));
        assert!(!evaluation.operations[0].accepted());
        assert!(evaluation.operations[1].accepted());
    }

    #[test]
    fn confirms_stablecoin_payment_from_single_op_transfer_logs() {
        let call_data = safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(100u8))]);
        let reimbursement =
            parse_reimbursement(&call_data, RECIPIENT, &BTreeSet::from([STABLECOIN])).unwrap();
        let logs = vec![
            transfer_log(STABLECOIN, SENDER, RECIPIENT, U256::from(40u8)),
            transfer_log(STABLECOIN, SENDER, RECIPIENT, U256::from(60u8)),
        ];
        assert!(verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &logs
        ));

        let wrong_sender = address!("7777777777777777777777777777777777777777");
        assert!(!verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[transfer_log(
                STABLECOIN,
                wrong_sender,
                RECIPIENT,
                U256::from(100u8),
            )]
        ));
        assert!(!verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[
                transfer_log(STABLECOIN, SENDER, RECIPIENT, U256::MAX),
                transfer_log(STABLECOIN, SENDER, RECIPIENT, U256::ONE),
            ]
        ));
    }

    #[test]
    fn enforces_native_and_stablecoin_floors_with_bundler_favourable_rounding() {
        let native_config = native_config(18);
        let below_native = safe_multisend(&[Entry::native(
            RECIPIENT,
            U256::from(10_000_000_000_000u64 - 1),
        )]);
        let at_native =
            safe_multisend(&[Entry::native(RECIPIENT, U256::from(10_000_000_000_000u64))]);
        let native_result = evaluate_batch(
            RECIPIENT,
            &native_config,
            &[
                SettlementInput {
                    call_data: &below_native,
                    gas_native_cost: U256::ZERO,
                },
                SettlementInput {
                    call_data: &at_native,
                    gas_native_cost: U256::ZERO,
                },
            ],
            None,
        )
        .unwrap();
        assert!(!native_result.operations[0].accepted());
        assert!(native_result.operations[1].accepted());

        let stable_config = config_with_stable(18, 6);
        let below_floor =
            safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(9_999u64))]);
        let at_floor =
            safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(10_000u64))]);
        let stable_result = evaluate_batch(
            RECIPIENT,
            &stable_config,
            &[
                SettlementInput {
                    call_data: &below_floor,
                    gas_native_cost: U256::ZERO,
                },
                SettlementInput {
                    call_data: &at_floor,
                    gas_native_cost: U256::ZERO,
                },
            ],
            Some(U256::from(200_000_000_000u64)),
        )
        .unwrap();
        assert_eq!(
            stable_result.operations[0].required_amount,
            U256::from(10_000u64)
        );
        assert!(!stable_result.operations[0].accepted());
        assert!(stable_result.operations[1].accepted());
    }

    #[test]
    fn converts_native_cost_to_stable_units_with_ceiling_rounding() {
        // 0.001 ETH at $2,000/ETH costs $2, so a 6-decimal $1 stable requires 2_000_000 units.
        assert_eq!(
            native_to_usd_stable_ceil(
                U256::from(1_000_000_000_000_000u64),
                18,
                U256::from(200_000_000_000u64),
                6,
            )
            .unwrap(),
            U256::from(2_000_000u64)
        );

        // The exact rational result is one billionth of a base unit and must round up.
        assert_eq!(
            native_to_usd_stable_ceil(U256::ONE, 0, U256::ONE, 0).unwrap(),
            U256::ONE
        );
    }

    #[test]
    fn rejects_mixed_payment_assets() {
        let config = config_with_stable(18, 6);
        let mixed = safe_multisend(&[
            Entry::native(RECIPIENT, U256::from(10_000_000_000_000u64)),
            Entry::erc20(STABLECOIN, RECIPIENT, U256::from(10_000u64)),
        ]);
        let result = evaluate_batch(
            RECIPIENT,
            &config,
            &[SettlementInput {
                call_data: &mixed,
                gas_native_cost: U256::ZERO,
            }],
            None,
        )
        .unwrap();
        assert_eq!(
            result.operations[0].rejection,
            Some(SettlementRejection::UnsupportedPaymentCombination)
        );
    }

    fn native_config(native_decimals: u32) -> ChainAssetConfig {
        ChainAssetConfig {
            native_decimals,
            settlement_markup_bps: 14_000,
            stablecoins: BTreeMap::new(),
        }
    }

    fn config_with_stable(native_decimals: u32, stable_decimals: u32) -> ChainAssetConfig {
        ChainAssetConfig {
            native_decimals,
            settlement_markup_bps: 14_000,
            stablecoins: BTreeMap::from([(
                STABLECOIN,
                StablecoinConfig {
                    symbol: "USDC".into(),
                    decimals: stable_decimals,
                },
            )]),
        }
    }

    fn transfer_log(token: Address, from: Address, to: Address, amount: U256) -> SettlementLog {
        SettlementLog {
            address: token,
            topics: vec![
                keccak256(b"Transfer(address,address,uint256)"),
                B256::from(super::address_word(from)),
                B256::from(super::address_word(to)),
            ],
            data: amount.to_be_bytes::<32>().to_vec().into(),
        }
    }

    struct Entry {
        operation: u8,
        to: Address,
        value: U256,
        data: Vec<u8>,
    }

    impl Entry {
        fn native(to: Address, value: U256) -> Self {
            Self {
                operation: 0,
                to,
                value,
                data: Vec::new(),
            }
        }

        fn erc20(token: Address, recipient: Address, amount: U256) -> Self {
            let mut data = Vec::with_capacity(68);
            data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
            data.extend_from_slice(&super::address_word(recipient));
            data.extend_from_slice(&amount.to_be_bytes::<32>());
            Self {
                operation: 0,
                to: token,
                value: U256::ZERO,
                data,
            }
        }
    }

    fn safe_multisend(entries: &[Entry]) -> Vec<u8> {
        let mut transactions = Vec::new();
        for entry in entries {
            transactions.push(entry.operation);
            transactions.extend_from_slice(entry.to.as_slice());
            transactions.extend_from_slice(&entry.value.to_be_bytes::<32>());
            transactions.extend_from_slice(&U256::from(entry.data.len()).to_be_bytes::<32>());
            transactions.extend_from_slice(&entry.data);
        }

        let mut inner = Vec::new();
        inner.extend_from_slice(&[0x8d, 0x80, 0xff, 0x0a]);
        inner.extend_from_slice(&U256::from(32u8).to_be_bytes::<32>());
        inner.extend_from_slice(&U256::from(transactions.len()).to_be_bytes::<32>());
        inner.extend_from_slice(&transactions);
        pad_to_word(&mut inner);

        let mut outer = Vec::new();
        outer.extend_from_slice(&[0x7b, 0xb3, 0x74, 0x28]);
        outer.extend_from_slice(&super::address_word(super::TRUSTED_MULTISEND));
        outer.extend_from_slice(&[0u8; 32]);
        outer.extend_from_slice(&U256::from(128u64).to_be_bytes::<32>());
        outer.extend_from_slice(&U256::ONE.to_be_bytes::<32>());
        outer.extend_from_slice(&U256::from(inner.len()).to_be_bytes::<32>());
        outer.extend_from_slice(&inner);
        pad_to_word(&mut outer);
        outer
    }

    fn pad_to_word(bytes: &mut Vec<u8>) {
        let padding = (32 - bytes.len() % 32) % 32;
        bytes.resize(bytes.len() + padding, 0);
    }

    // ----- decide_settlement verdict table -----

    use super::{
        FeeContext, SettlementDecision, SettlementDecisionError, decide_settlement,
        has_stablecoin_payment,
    };

    fn fees(quoted: u128, base: u128, tip: u128, floor_bps: u64) -> FeeContext {
        FeeContext {
            quoted_fee_per_gas: quoted,
            base_fee_per_gas: base,
            max_priority_fee_per_gas: tip,
            inclusion_floor_bps: floor_bps,
        }
    }

    #[test]
    fn fully_funded_batch_keeps_the_quote() {
        // gas 100 × fee 1 = cost 100; markup 1.4 → required 140; paid 140.
        let paid = safe_multisend(&[Entry::native(RECIPIENT, U256::from(140u64))]);
        let decision = decide_settlement(
            RECIPIENT,
            &native_config(5),
            &[paid.as_slice()],
            &[U256::from(100u64)],
            None,
            &fees(1, 1, 0, 15_000),
        )
        .unwrap();
        match decision {
            SettlementDecision::KeepQuote { evaluation } => assert!(evaluation.all_accepted()),
            other => panic!("expected KeepQuote, got {other:?}"),
        }
    }

    #[test]
    fn shortfall_above_the_floor_reprices_to_the_signed_budget() {
        // gas 1 × quoted 1000 = cost 1000; required 1400; paid 700 → ratio
        // 5000 bps → affordable 500. Floor = 200 × 1.5 + 0 = 300 ≤ 500 < 1000.
        // Repriced: cost 500, required 700, paid 700 → all accepted.
        let paid = safe_multisend(&[Entry::native(RECIPIENT, U256::from(700u64))]);
        let decision = decide_settlement(
            RECIPIENT,
            &native_config(5),
            &[paid.as_slice()],
            &[U256::ONE],
            None,
            &fees(1_000, 200, 0, 15_000),
        )
        .unwrap();
        match decision {
            SettlementDecision::Reprice {
                fee_per_gas,
                evaluation,
            } => {
                assert_eq!(fee_per_gas, 500);
                assert!(evaluation.all_accepted());
            }
            other => panic!("expected Reprice, got {other:?}"),
        }
    }

    #[test]
    fn affordable_fee_below_the_inclusion_floor_keeps_the_quote_with_a_diagnosis() {
        // Same batch, but base fee 400 → floor 600 > affordable 500.
        let paid = safe_multisend(&[Entry::native(RECIPIENT, U256::from(700u64))]);
        let decision = decide_settlement(
            RECIPIENT,
            &native_config(5),
            &[paid.as_slice()],
            &[U256::ONE],
            None,
            &fees(1_000, 400, 0, 15_000),
        )
        .unwrap();
        match decision {
            SettlementDecision::FloorUnfundable {
                evaluation,
                affordable,
                floor,
            } => {
                assert_eq!(affordable, 500);
                assert_eq!(floor, 600);
                assert!(!evaluation.all_accepted());
            }
            other => panic!("expected FloorUnfundable, got {other:?}"),
        }
    }

    #[test]
    fn an_uncurable_rejection_disables_repricing_for_the_whole_batch() {
        // Malformed calldata is not a shortfall; no price can cure it, so the
        // quote stays even though the second op is short.
        let short = safe_multisend(&[Entry::native(RECIPIENT, U256::from(700u64))]);
        let decision = decide_settlement(
            RECIPIENT,
            &native_config(5),
            &[&[] as &[u8], short.as_slice()],
            &[U256::ONE, U256::ONE],
            None,
            &fees(1_000, 200, 0, 15_000),
        )
        .unwrap();
        match decision {
            SettlementDecision::KeepQuote { evaluation } => {
                assert_eq!(
                    evaluation.operations[0].rejection,
                    Some(SettlementRejection::MalformedCallData)
                );
                assert!(evaluation.operations[1].is_shortfall());
            }
            other => panic!("expected KeepQuote, got {other:?}"),
        }
    }

    #[test]
    fn cost_overflow_is_reported_with_the_frozen_error_text() {
        let paid = safe_multisend(&[Entry::native(RECIPIENT, U256::from(1u64))]);
        let error = decide_settlement(
            RECIPIENT,
            &native_config(5),
            &[paid.as_slice()],
            &[U256::MAX],
            None,
            &fees(2, 1, 0, 15_000),
        )
        .unwrap_err();
        assert_eq!(error, SettlementDecisionError::CostOverflow);
        assert_eq!(error.to_string(), "bundle native cost overflow");
    }

    #[test]
    fn parses_binance_native_usd_prices_with_bundler_favourable_rounding() {
        use super::parse_market_usd_price;
        assert_eq!(
            parse_market_usd_price("3024.12"),
            Some(U256::from(302_412_000_000u64))
        );
        assert_eq!(
            parse_market_usd_price("1.0000000001"),
            Some(U256::from(100_000_001u64))
        );
        for invalid in ["", "0", "-1", "1e3", "1.2.3"] {
            assert_eq!(parse_market_usd_price(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn prices_tempo_path_usd_with_ceiling_and_the_default_one_point_four_x_gate() {
        use super::marked_tempo_cost;
        use crate::tempo;
        // 100,000 gas at Tempo's 20e9 attodollar base fee is exactly 0.002 pathUSD.
        assert_eq!(
            tempo::tempo_cost_in_path_usd(
                U256::from(100_000u64),
                U256::from(tempo::TEMPO_BASE_FEE_ATTO),
            )
            .unwrap(),
            U256::from(2_000u64)
        );
        // The normal in-band 1.4x markup still applies, then the common $0.01 floor protects
        // micro-transactions from consuming a relayer float for a dust reimbursement.
        assert_eq!(
            marked_tempo_cost(U256::from(2_000u64), 14_000).unwrap(),
            U256::from(10_000u64)
        );
        assert_eq!(
            marked_tempo_cost(U256::from(20_000u64), 14_000).unwrap(),
            U256::from(28_000u64)
        );
    }

    #[test]
    fn stablecoin_payment_detection_reads_the_signed_calldata_only() {
        let native_only = safe_multisend(&[Entry::native(RECIPIENT, U256::from(140u64))]);
        let stable = safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(200u64))]);
        let config = config_with_stable(18, 6);
        assert!(!has_stablecoin_payment(
            RECIPIENT,
            &config,
            &[native_only.as_slice()]
        ));
        assert!(has_stablecoin_payment(
            RECIPIENT,
            &config,
            &[native_only.as_slice(), stable.as_slice()]
        ));
    }

    // ----- proof-of-payment guards (fund-safety: an unproven or misdirected
    // payment must never be credited) -----

    #[test]
    fn verify_stable_transfer_logs_enforces_every_field_of_the_transfer_event() {
        let call_data = safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(100u8))]);
        let reimbursement =
            parse_reimbursement(&call_data, RECIPIENT, &BTreeSet::from([STABLECOIN])).unwrap();

        // The exact, correct log proves payment.
        let good = transfer_log(STABLECOIN, SENDER, RECIPIENT, U256::from(100u8));
        assert!(verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[good.clone()]
        ));

        // Each single-field corruption must FAIL — otherwise a Transfer that
        // did not actually pay the settlement recipient would be credited.
        let wrong_token = address!("9999999999999999999999999999999999999999");
        let outsider = address!("7777777777777777777777777777777777777777");

        // (1) wrong emitting token contract
        let mut log = good.clone();
        log.address = wrong_token;
        assert!(!verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[log]
        ));

        // (2) wrong recipient topic (paid to a third party)
        let paid_elsewhere = transfer_log(STABLECOIN, SENDER, outsider, U256::from(100u8));
        assert!(!verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[paid_elsewhere]
        ));

        // (3) wrong event signature (topic0)
        let mut log = good.clone();
        log.topics[0] = keccak256(b"Approval(address,address,uint256)");
        assert!(!verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[log]
        ));

        // (4) wrong topic count (an anonymous or malformed event)
        let mut log = good.clone();
        log.topics.pop();
        assert!(!verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[log]
        ));

        // (5) wrong data length (not a single uint256 amount)
        let mut log = good.clone();
        log.data = vec![0u8; 31].into();
        assert!(!verify_stable_transfer_logs(
            &reimbursement,
            SENDER,
            RECIPIENT,
            &[log]
        ));
    }

    #[test]
    fn parse_reimbursement_ignores_transfers_to_a_different_recipient() {
        let outsider = address!("7777777777777777777777777777777777777777");
        // A correctly-shaped native transfer and ERC-20 transfer, both paying
        // someone OTHER than the settlement recipient, must credit nothing.
        let call_data = safe_multisend(&[
            Entry::native(outsider, U256::from(1_000u64)),
            Entry::erc20(STABLECOIN, outsider, U256::from(1_000u64)),
        ]);
        let reimbursement =
            parse_reimbursement(&call_data, RECIPIENT, &BTreeSet::from([STABLECOIN])).unwrap();
        assert!(reimbursement.native.is_zero());
        assert!(reimbursement.stablecoins.is_empty());
    }

    #[test]
    fn an_empty_reimbursement_is_rejected_as_insufficient_payment() {
        // Valid calldata that parses cleanly but pays the recipient nothing
        // (the transfer goes to a third party) must reject via the (false, 0)
        // zero-payment branch of evaluate_one.
        let outsider = address!("7777777777777777777777777777777777777777");
        let call_data = safe_multisend(&[Entry::native(outsider, U256::from(5_000u64))]);
        let result = evaluate_batch(
            RECIPIENT,
            &native_config(18),
            &[SettlementInput {
                call_data: &call_data,
                gas_native_cost: U256::from(1_000u64),
            }],
            None,
        )
        .unwrap();
        let evaluation = &result.operations[0];
        assert!(evaluation.paid_amount.is_zero());
        assert!(!evaluation.accepted());
        assert_eq!(
            evaluation.rejection,
            Some(SettlementRejection::InsufficientPayment)
        );
    }

    // ----- stablecoin decision path (the whole verdict table was native-only) -----

    #[test]
    fn decide_settlement_keeps_a_fully_funded_stablecoin_quote() {
        use super::{SettlementDecision, decide_settlement};
        // gas 1 × quoted 100 = cost 100; markup 1.4 → marked 140.
        // At $2,000/ETH (2e11 in 8-dec) a 6-decimal stable requires
        // ceil(140 · 2e11 · 1e6 / 1e26) = 1 unit, lifted to the 10_000 floor.
        let paid = safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(10_000u64))]);
        let decision = decide_settlement(
            RECIPIENT,
            &config_with_stable(18, 6),
            &[paid.as_slice()],
            &[U256::from(1u64)],
            Some(U256::from(200_000_000_000u64)),
            &fees(100, 1, 0, 15_000),
        )
        .unwrap();
        match decision {
            SettlementDecision::KeepQuote { evaluation } => {
                assert!(evaluation.all_accepted());
                assert_eq!(evaluation.operations[0].payment_asset, Some(STABLECOIN));
            }
            other => panic!("expected KeepQuote, got {other:?}"),
        }
    }

    #[test]
    fn a_stablecoin_below_the_floor_cannot_be_repriced_into_acceptance() {
        use super::{SettlementDecision, decide_settlement};
        // The payment (3_000) sits below the 10_000 stablecoin floor, so the
        // required amount is pinned at the floor at EVERY fee — no reprice can
        // drop it to what was paid. The safety net (decide_settlement:859) must
        // keep the quote, never reprice-and-accept an underpaid operation.
        let paid = safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(3_000u64))]);
        let decision = decide_settlement(
            RECIPIENT,
            &config_with_stable(18, 6),
            &[paid.as_slice()],
            &[U256::from(1u64)],
            Some(U256::from(100_000_000u64)),
            &fees(100, 1, 0, 15_000),
        )
        .unwrap();
        match decision {
            SettlementDecision::KeepQuote { evaluation } => {
                assert!(!evaluation.all_accepted());
                assert!(evaluation.operations[0].is_shortfall());
            }
            other => panic!("expected KeepQuote (safety net), got {other:?}"),
        }
    }

    // ----- conversion / price guards (Tier 2) -----

    #[test]
    fn native_to_usd_stable_ceil_rejects_a_zero_price() {
        use super::SettlementError;
        assert_eq!(
            native_to_usd_stable_ceil(U256::from(1_000u64), 18, U256::ZERO, 6),
            Err(SettlementError::ArithmeticOverflow)
        );
    }

    #[test]
    fn a_stablecoin_operation_without_a_native_price_reports_the_missing_price() {
        use super::SettlementError;
        // A stablecoin-paying op needs the native/USD market price to size the
        // requirement; evaluating it with `None` must error, not under-charge.
        let paid = safe_multisend(&[Entry::erc20(STABLECOIN, RECIPIENT, U256::from(10_000u64))]);
        let error = evaluate_batch(
            RECIPIENT,
            &config_with_stable(18, 6),
            &[SettlementInput {
                call_data: &paid,
                gas_native_cost: U256::from(1_000u64),
            }],
            None,
        )
        .unwrap_err();
        assert_eq!(error, SettlementError::MissingNativeUsdPrice);
    }

    #[test]
    fn marked_tempo_cost_rounds_up_on_a_non_even_markup() {
        use super::marked_tempo_cost;
        // 7_143 · 14_000 / 10_000 = 10_000.2 — the remainder must round UP to
        // 10_001 (and stay above the $0.01 / 10_000-unit floor), never
        // truncate down to 10_000 and under-charge.
        assert_eq!(
            marked_tempo_cost(U256::from(7_143u64), 14_000).unwrap(),
            U256::from(10_001u64)
        );
    }
}
