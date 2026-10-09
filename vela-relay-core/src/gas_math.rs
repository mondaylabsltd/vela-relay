//! EIP-1559 gas price arithmetic: fee-history interpretation, tier scaling,
//! and quantity parsing. The shell's `GasPriceManager` owns polling, caching,
//! and RPC failover; every price *calculation* lives here.

use std::{
    error::Error,
    fmt::{Display, Formatter},
};

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The one gas-price knob the shell still tunes.
///
/// The tier multipliers used to live here — `base_fee_multiplier: 120` and
/// `slow/standard/fast_multiplier: 100/110/120` — and that was the
/// incoherence this module was built out of: a reported price that varied
/// ±10% cannot fund a submit cap that varies 2×. They are gone. Every number
/// reported for a tier is now derived from the cap that tier submits at
/// ([`SubmissionTier`]), so the price and the speed it buys can never drift
/// apart. See [`tiers`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GasPricePolicy {
    /// Divides the base fee for the quote's last-resort tip, used only when
    /// [`market_tip`] yields none — a market the executor refuses to submit
    /// in. See [`quote_market_tip`].
    pub priority_fee_divisor: u128,
}

impl Default for GasPricePolicy {
    fn default() -> Self {
        Self {
            priority_fee_divisor: 200,
        }
    }
}

/// One reported tier of `pimlico_getUserOperationGasPrice` — built by
/// [`tier_price`], which is where the arithmetic and its reasoning live. The
/// per-tier `inBandFeePerGas` a client pays against is derived from
/// `max_fee_per_gas` when the row goes on the wire
/// ([`crate::wire::gas_price_tier`], [`in_band_fee_per_gas`]).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GasPrice {
    /// The cap the relay will actually submit this tier at:
    /// `base_fee_bps × base fee + tip[tier]`, identical to [`tier_outer_fee`].
    pub max_fee_per_gas: u128,
    /// The tip the relay will actually **sign** this tier with, read from
    /// recent blocks' rewards ([`TierTips`]) — the same reading the executor
    /// signs from, so a wallet is never shown one tip and charged another.
    pub max_priority_fee_per_gas: u128,
    /// `R` — what wallets that price the LIMITS (vela-wallet before
    /// `settlementGas`) reimburse against: `docs/fees.md` §3. Frozen at its
    /// earlier definition, `0.9 / 1.2 / 1.8 × base fee + 1.00 / 1.25 / 2.00 ×`
    /// the market tip ([`tier_network_fee`]): those wallets refuse a quote
    /// whose `R` exceeds three times their own `max(eth_gasPrice, base +
    /// eth_maxPriorityFeePerGas)`, and on Ethereum, where that tip reads ~0, a
    /// reward-percentile tip in `R` would refuse every quote in a calm market.
    /// What they pay on their padded limits still funds the tier (§3).
    pub network_fee_per_gas: u128,
    /// `max_fee_per_gas − network_fee_per_gas`, saturating at zero. Reported
    /// rather than left to be inferred — vela-core's `accept_bundler_quote`
    /// otherwise derives it by subtraction.
    pub relayer_fee_per_gas: u128,
}

/// The raw market a quote is derived from: the next block's base fee, the
/// node's own tip answer ([`market_tip`]), and the tips each tier signs with
/// ([`TierTips`]), kept apart.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkGasPrice {
    pub base_fee_per_gas: u128,
    /// The market tip: `eth_maxPriorityFeePerGas`, else `eth_gasPrice −` base.
    pub max_priority_fee_per_gas: u128,
    pub tier_tips: TierTips,
}

/// How many recent blocks a tier's tip is read over (`eth_feeHistory`'s block
/// count, ending at `latest`). Chosen by the backtest in `docs/fees.md` §2c.
pub const TIP_WINDOW_BLOCKS: u64 = 20;

/// The `eth_feeHistory` reward percentiles requested, in [`SubmissionTier`]
/// order: `slow`, `standard`, `fast` read the 25th, 50th and 70th percentile
/// effective tip of each block. Chosen by the backtest (§2c).
pub const TIP_REWARD_PERCENTILES: [u8; 3] = [25, 50, 70];

/// The least tip `slow` signs on a chain whose recent blocks paid any tip at
/// all: 0.001 gwei. A block's low percentiles are often a single wei (or
/// nothing) on Ethereum; a slow send should still be worth a builder's while.
/// A chain whose fee history shows no tip at all (Arbitrum) keeps zero.
pub const MIN_POSITIVE_TIP: u128 = 1_000_000;

/// The quote→submission allowance in `inBandFeePerGas`, in basis points:
/// 1.0×, chosen by the backtest (`docs/fees.md` §2c). The quote already prices
/// the NEXT block's base fee, and a submission whose market moved since is
/// repriced from the tier's cap (1.5× base or more) down to the inclusion
/// floor (1.125×) with its whole tip, so a quote's drift is paid for by the
/// cap the client already buys; a larger allowance only bought acceptance the
/// backtest did not need. Kept in the formula so the contract states it.
pub const IN_BAND_DRIFT_BPS: u64 = 10_000;

/// The tip each tier signs with — ONE reading, shared by the quote
/// (`pimlico_getUserOperationGasPrice`) and the executor
/// (`settlement::decide_submission_fees`), so what a wallet is quoted is what
/// the relay signs.
///
/// Read from `eth_feeHistory(TIP_WINDOW_BLOCKS, "latest",
/// TIP_REWARD_PERCENTILES)`: each tier's tip is the median, over the window,
/// of each block's own percentile reward (the effective tips its gas paid),
/// so one odd block moves nothing:
///
/// ```text
/// slow     = max( median p25 , MIN_POSITIVE_TIP if the window paid any tip , market tip )
/// standard = max( median p50 , slow )
/// fast     = max( median p70 , standard )
/// ```
///
/// `slow` is never below the node's own `eth_maxPriorityFeePerGas`
/// ([`market_tip`]): that answer carries a chain's enforced minimum (bor on
/// Polygon), and a tip under it is rejected outright rather than mined late.
/// Each faster tier is at least the slower one's, so a faster tier can never
/// bid less.
///
/// Without a usable reward column (a chain whose `eth_feeHistory` omits it, a
/// legacy chain, a failed call) the tiers fall back to the market tip scaled
/// `1.00 / 1.25 / 2.00` ([`scaled_market_tip`]) — the rule before rewards
/// were read — on both paths alike.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TierTips {
    pub slow: u128,
    pub standard: u128,
    pub fast: u128,
}

impl TierTips {
    pub const fn of(&self, tier: SubmissionTier) -> u128 {
        match tier {
            SubmissionTier::Slow => self.slow,
            SubmissionTier::Standard => self.standard,
            SubmissionTier::Fast => self.fast,
        }
    }

    /// The tips from a fee history's reward rows (`[p25, p50, p70]` per
    /// block) and the market tip. A row of another width (some nodes answer
    /// `[]` for an empty block) is left out; `None` when no complete row is
    /// left.
    pub fn from_rewards(rewards: &[Vec<u128>], market_tip: u128) -> Option<Self> {
        let rewards = rewards
            .iter()
            .filter(|row| row.len() == TIP_REWARD_PERCENTILES.len())
            .collect::<Vec<_>>();
        if rewards.is_empty() {
            return None;
        }
        let median = |column: usize| {
            let mut values = rewards.iter().map(|row| row[column]).collect::<Vec<_>>();
            values.sort_unstable();
            let middle = values.len() / 2;
            if values.len() % 2 == 1 {
                values[middle]
            } else {
                // The mean of the two middle values, rounded up.
                let (low, high) = (values[middle - 1], values[middle]);
                low + (high - low).div_ceil(2)
            }
        };
        let paid_any = rewards
            .iter()
            .any(|row| row.iter().any(|reward| *reward > 0));
        let slow = median(0)
            .max(if paid_any { MIN_POSITIVE_TIP } else { 0 })
            .max(market_tip);
        let standard = median(1).max(slow);
        let fast = median(2).max(standard);
        Some(Self {
            slow,
            standard,
            fast,
        })
    }

    /// The fallback: the market tip scaled `1.00 / 1.25 / 2.00`. `None` on
    /// overflow.
    pub fn scaled(market_tip: u128) -> Option<Self> {
        Some(Self {
            slow: scaled_market_tip(SubmissionTier::Slow, market_tip)?,
            standard: scaled_market_tip(SubmissionTier::Standard, market_tip)?,
            fast: scaled_market_tip(SubmissionTier::Fast, market_tip)?,
        })
    }

    /// Rewards when there are usable ones, else the scaled market tip.
    pub fn resolve(rewards: Option<&[Vec<u128>]>, market_tip: u128) -> Option<Self> {
        rewards
            .and_then(|rewards| Self::from_rewards(rewards, market_tip))
            .or_else(|| Self::scaled(market_tip))
    }
}

/// The `eth_feeHistory` params the tier tips are read from — by the quote and
/// by the executor's transaction context alike.
pub fn tip_history_params() -> Value {
    serde_json::json!([
        format!("0x{TIP_WINDOW_BLOCKS:x}"),
        "latest",
        TIP_REWARD_PERCENTILES
    ])
}

/// The reward rows of an `eth_feeHistory` answer to [`tip_history_params`],
/// `None` when it carries none the relay can read whole.
pub fn tip_rewards(fee_history: &Value) -> Option<Vec<Vec<u128>>> {
    serde_json::from_value::<FeeHistory>(fee_history.clone())
        .ok()?
        .rewards()
}

/// `inBandFeePerGas`: the wei per unit of `settlementGas` a client pays for a
/// tier — `markup × drift × cap`, rounded up (`docs/fees.md` §3; the drift
/// allowance is 1.0, so this is `markup × cap`):
///
/// ```text
/// inBandFeePerGas[tier] = settlement_markup × IN_BAND_DRIFT × (base_fee_bps[tier] × base + tip[tier])
/// ```
///
/// Paid on `settlementGas`, it funds the executor's whole requirement
/// (`markup × settlement gas × cap`) at the cap this quote names; a base fee
/// that rose before the submission is absorbed by repricing that cap down to
/// what was paid (§2). `None` on overflow.
pub fn in_band_fee_per_gas(max_fee_per_gas: u128, settlement_markup_bps: u64) -> Option<u128> {
    let product = U256::from(max_fee_per_gas)
        .checked_mul(U256::from(settlement_markup_bps))?
        .checked_mul(U256::from(IN_BAND_DRIFT_BPS))?;
    let denominator = U256::from(100_000_000u64);
    let rounded = (product / denominator)
        .checked_add(U256::from(u8::from(!(product % denominator).is_zero())))?;
    u128::try_from(rounded).ok()
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GasPriceTiers {
    pub slow: GasPrice,
    pub standard: GasPrice,
    pub fast: GasPrice,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GasPriceError {
    NoPriceAvailable,
    InvalidUpstreamResponse,
    ArithmeticOverflow,
    ResponseDeadlineExceeded,
}

impl Display for GasPriceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPriceAvailable => formatter.write_str("no gas price is available"),
            Self::InvalidUpstreamResponse => {
                formatter.write_str("upstream RPC returned an invalid gas price response")
            }
            Self::ArithmeticOverflow => formatter.write_str("gas price calculation overflowed"),
            Self::ResponseDeadlineExceeded => {
                formatter.write_str("the gas price response deadline was exceeded")
            }
        }
    }
}

impl Error for GasPriceError {}

/// The part of an `eth_feeHistory` answer the relay reads: the base fees,
/// and the reward rows the tier tips come from ([`TierTips`]).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistory {
    pub base_fee_per_gas: Vec<String>,
    #[serde(default)]
    pub reward: Option<Vec<Vec<String>>>,
}

impl FeeHistory {
    /// The reward rows as numbers. `None` when the answer carries none, or
    /// any entry is not a quantity — a column the relay cannot read whole is
    /// not read at all.
    pub fn rewards(&self) -> Option<Vec<Vec<u128>>> {
        let rows = self.reward.as_ref()?;
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|value| parse_quantity(value).ok())
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>()
            .filter(|rows| !rows.is_empty())
    }

    /// The LAST `baseFeePerGas`: the projected base fee of the block after
    /// the newest one. The quote prices every tier's cap and reimbursement
    /// basis with this.
    pub fn next_block_base_fee(&self) -> Result<u128, GasPriceError> {
        self.base_fee_per_gas
            .last()
            .ok_or(GasPriceError::InvalidUpstreamResponse)
            .and_then(|value| parse_quantity(value))
    }

    /// The newest block's OWN base fee — the second-to-last `baseFeePerGas`,
    /// the number `eth_getBlockByNumber("latest")` reports and the executor
    /// reads. `eth_gasPrice` is built as `suggested tip + this`, so it is the
    /// base fee [`market_tip`] subtracts; the next block's projection would be
    /// off by up to 12.5% of the base fee in either direction.
    ///
    /// A history of one entry has no second-to-last and yields that entry.
    pub fn latest_block_base_fee(&self) -> Result<u128, GasPriceError> {
        let entries = &self.base_fee_per_gas;
        entries
            .len()
            .checked_sub(2)
            .and_then(|index| entries.get(index))
            .or_else(|| entries.last())
            .ok_or(GasPriceError::InvalidUpstreamResponse)
            .and_then(|value| parse_quantity(value))
    }
}

/// The next block's base fee `eth_feeHistory` reported, paired with the
/// market tip the caller resolved ([`quote_market_tip`]) and the tier tips
/// read from the same answer's rewards ([`TierTips::resolve`]). The base fee
/// is carried raw so each tier can scale it by its own cap multiplier.
pub fn price_from_fee_history(
    fee_history: &FeeHistory,
    priority_fee: u128,
) -> Result<NetworkGasPrice, GasPriceError> {
    Ok(NetworkGasPrice {
        base_fee_per_gas: fee_history.next_block_base_fee()?,
        max_priority_fee_per_gas: priority_fee,
        tier_tips: TierTips::resolve(fee_history.rewards().as_deref(), priority_fee)
            .ok_or(GasPriceError::ArithmeticOverflow)?,
    })
}

/// The three tiers of `pimlico_getUserOperationGasPrice`, each one derived
/// from the cap the relay would submit it at. One definition, shared with
/// [`SubmissionTier`]/[`tier_outer_fee`]: the tier a client NAMES on
/// `eth_sendUserOperation` and the tier a client is PRICED for are the same
/// thing.
pub fn tiers(network_price: NetworkGasPrice) -> Result<GasPriceTiers, GasPriceError> {
    let price = |tier| {
        tier_price(
            tier,
            network_price.base_fee_per_gas,
            &network_price.tier_tips,
            network_price.max_priority_fee_per_gas,
        )
        .ok_or(GasPriceError::ArithmeticOverflow)
    };

    Ok(GasPriceTiers {
        slow: price(SubmissionTier::Slow)?,
        standard: price(SubmissionTier::Standard)?,
        fast: price(SubmissionTier::Fast)?,
    })
}

/// The executor's quoted outer-fee cap when the client named **no** tier:
/// `2 × base fee + the raw market tip`. The multiple buys inclusion headroom,
/// not cost — the chain only ever charges `base fee + tip`.
///
/// This is the relay's own pace, untouched by submission tiers: an operation
/// that names no speed is signed at this cap with the market tip, exactly as
/// it always has been. `None` on overflow.
pub fn quoted_outer_fee(base_fee_per_gas: u128, max_priority_fee_per_gas: u128) -> Option<u128> {
    base_fee_per_gas
        .checked_mul(2)?
        .checked_add(max_priority_fee_per_gas)
}

/// The submission speed a client may name on `eth_sendUserOperation`.
///
/// The client names the TIER, never a wei amount. A quote that went stale
/// between signing and inclusion therefore cannot mis-set the price: the relay
/// resolves the name against the base fee and tips it reads at submit time, so
/// the worst a stale quote can do is buy a speed the reimbursement no longer
/// funds — which [`crate::settlement::decide_submission_fees`] then clamps
/// away.
///
/// A tier is two levers: the cap [`SubmissionTier::base_fee_bps`] (how far
/// the base fee may rise before the transaction stops being includable) and
/// the tip ([`TierTips`], the reward percentile
/// [`SubmissionTier::tip_percentile`]; what a builder orders by). Both were
/// chosen by the backtest in `docs/fees.md` §2c.
///
/// Ordered `Slow < Standard < Fast` so a bundle can submit at the fastest
/// speed any of its operations asked for.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionTier {
    Slow,
    /// The default a bundle member that named nothing counts as when a
    /// neighbour named a speed. Naming NO tier at all is still the relay's
    /// own pace, byte for byte: `settlement::decide_submission_fees` returns
    /// `None` and not one line of tier arithmetic runs.
    #[default]
    Standard,
    Fast,
}

/// The share of a cap's base-fee term in the reported `networkFeePerGas`
/// (`R`, [`tier_network_fee`]): 0.6, applied to the frozen legacy cap
/// multiples [`SubmissionTier::legacy_base_fee_bps`] (`docs/fees.md` §3).
pub const REIMBURSEMENT_BASIS_BPS: u64 = 6_000;

impl SubmissionTier {
    /// Basis points of the base fee this tier caps the outer transaction at:
    /// `1.5 / 1.5 / 1.75 ×`. The cap buys resilience to a base-fee rise
    /// between signing and inclusion; it is not a cost (the chain charges
    /// `base fee + effective tip`), but a client pays for it, because the
    /// relay's requirement is `markup × gas × cap` (§1). The backtest found
    /// that standard needs no more cap than slow — its speed is its tip — and
    /// fast 1.75 to stay within twice slow's price (§2c).
    pub const fn base_fee_bps(self) -> u64 {
        match self {
            Self::Slow => 15_000,
            Self::Standard => 15_000,
            Self::Fast => 17_500,
        }
    }

    /// The `eth_feeHistory` reward percentile this tier's tip is read at
    /// ([`TIP_REWARD_PERCENTILES`], [`TierTips`]).
    pub const fn tip_percentile(self) -> u8 {
        TIP_REWARD_PERCENTILES[self as usize]
    }

    /// The cap multiples `R` is still derived from — the tier table before
    /// `settlementGas` (`1.5 / 2.0 / 3.0 ×`), frozen for wallets that price
    /// against `networkFeePerGas` ([`GasPrice::network_fee_per_gas`]).
    pub const fn legacy_base_fee_bps(self) -> u64 {
        match self {
            Self::Slow => 15_000,
            Self::Standard => 20_000,
            Self::Fast => 30_000,
        }
    }

    /// Basis points of the market tip in the legacy tier table: `1.00 / 1.25
    /// / 2.00 ×`. `R`'s tip term, and the tier tips' fallback when no reward
    /// column is readable ([`TierTips::scaled`]). `Slow` is the market tip
    /// itself, never less: the node's answer carries a chain's enforced
    /// minimum.
    pub const fn legacy_tip_bps(self) -> u64 {
        match self {
            Self::Slow => 10_000,
            Self::Standard => 12_500,
            Self::Fast => 20_000,
        }
    }

    /// Basis points of the base fee in `R`: `0.6 × legacy_base_fee_bps` →
    /// `0.9 / 1.2 / 1.8 × base fee`.
    pub const fn network_fee_bps(self) -> u64 {
        self.legacy_base_fee_bps() * REIMBURSEMENT_BASIS_BPS / 10_000
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Standard => "standard",
            Self::Fast => "fast",
        }
    }
}

/// The two numbers an EIP-1559 outer transaction is signed with, kept
/// together so a tier can never set one and forget the other.
///
/// That forgetting was the defect: [`tier_outer_fee`] used to return a bare
/// `u128` cap, the executor assigned it to `max_fee_per_gas`, and
/// `max_priority_fee_per_gas` kept the raw market tip on every tier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OuterFee {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

impl OuterFee {
    /// What a block builder actually sees and orders by:
    /// `min(maxPriorityFeePerGas, maxFeePerGas − baseFee)`.
    ///
    /// The whole point of the fix is that this number, not the cap, decides
    /// how fast a transaction is mined.
    pub const fn effective_tip_at(self, base_fee_per_gas: u128) -> u128 {
        let headroom = self.max_fee_per_gas.saturating_sub(base_fee_per_gas);
        if headroom < self.max_priority_fee_per_gas {
            headroom
        } else {
            self.max_priority_fee_per_gas
        }
    }

    /// `maxFeePerGas ≥ baseFee + maxPriorityFeePerGas`: the invariant that
    /// makes the signed tip the tip the builder receives.
    ///
    /// Below `baseFee` the transaction cannot be included at all; between
    /// `baseFee` and `baseFee + tip` it is includable but the tip is silently
    /// truncated — the tier would be paid for and not delivered, which is the
    /// class of bug this whole change exists to remove. Every clamp in
    /// [`crate::settlement::decide_submission_fees`] is bounded below so this
    /// holds.
    pub const fn delivers_full_tip_at(self, base_fee_per_gas: u128) -> bool {
        self.effective_tip_at(base_fee_per_gas) == self.max_priority_fee_per_gas
    }
}

impl Display for SubmissionTier {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The market tip scaled by the legacy table: `legacy_tip_bps × market tip`,
/// rounded **up** (so `Standard`'s ×1.25 cannot collapse onto `Slow` at tiny
/// tips). `Slow` is 10_000 bps, so it returns the market tip unchanged.
/// `None` on overflow.
pub fn scaled_market_tip(tier: SubmissionTier, market_tip: u128) -> Option<u128> {
    let denominator = U256::from(10_000u64);
    let product = U256::from(market_tip).checked_mul(U256::from(tier.legacy_tip_bps()))?;
    let scaled = if (product % denominator).is_zero() {
        product / denominator
    } else {
        (product / denominator).checked_add(U256::from(1u64))?
    };
    u128::try_from(scaled).ok()
}

/// The pair a named speed asks the outer transaction to be signed with:
///
/// ```text
/// maxPriorityFeePerGas = tip[tier]                              (TierTips)
/// maxFeePerGas         = base_fee_bps[tier] × base + that tip   1.5 / 1.5 / 1.75 × base + tip
/// ```
///
/// **A tier is two levers, and it has to be.** The cap alone buys resilience
/// to a base-fee spike; only the tip buys priority, because a builder orders
/// by `min(maxPriorityFeePerGas, maxFeePerGas − baseFee)` and that number is
/// unchanged by any cap above `base + tip`. Returning both from one function,
/// as one struct, is what stops a caller assigning the cap and leaving another
/// tip in place — which is precisely the bug a real Polygon `fast` receipt
/// caught.
///
/// **`maxFeePerGas ≥ base + tip` always holds here**: the smallest multiple is
/// 1.5×, and `floor(1.5 b) ≥ b` for every `b ≥ 0`. The base-fee division
/// truncates, matching `settlement::inclusion_floor_fee_per_gas`.
///
/// `None` on overflow — the caller then keeps the relay's own pace, so an
/// unpriceable market costs a client its requested speed and nothing else.
pub fn tier_outer_fee(
    tier: SubmissionTier,
    base_fee_per_gas: u128,
    tips: &TierTips,
) -> Option<OuterFee> {
    let tip = tips.of(tier);
    let scaled = U256::from(base_fee_per_gas).checked_mul(U256::from(tier.base_fee_bps()))?
        / U256::from(10_000u64);
    Some(OuterFee {
        max_fee_per_gas: u128::try_from(scaled).ok()?.checked_add(tip)?,
        max_priority_fee_per_gas: tip,
    })
}

/// `R`, the reported `networkFeePerGas`, frozen at its definition before
/// `settlementGas`: `0.6 × legacy_base_fee_bps × base fee` rounded up, plus
/// the legacy-scaled market tip — `0.9 / 1.2 / 1.8 × base + 1.00 / 1.25 /
/// 2.00 × market tip`. See [`GasPrice::network_fee_per_gas`] for why it
/// cannot follow the new tables. `None` on overflow.
pub fn tier_network_fee(
    tier: SubmissionTier,
    base_fee_per_gas: u128,
    market_tip: u128,
) -> Option<u128> {
    let denominator = U256::from(10_000u64);
    let product = U256::from(base_fee_per_gas).checked_mul(U256::from(tier.network_fee_bps()))?;
    let scaled = if (product % denominator).is_zero() {
        product / denominator
    } else {
        (product / denominator).checked_add(U256::from(1u64))?
    };
    u128::try_from(scaled)
        .ok()?
        .checked_add(scaled_market_tip(tier, market_tip)?)
}

/// The whole reported row for one tier:
///
/// ```text
/// maxPriorityFeePerGas = tip[tier]                            (TierTips — what the relay signs)
/// maxFeePerGas         = base_fee_bps × base + that tip       (the cap it submits at)
/// networkFeePerGas     = R, frozen                            (tier_network_fee)
/// relayerFeePerGas     = maxFeePerGas − R, saturating
/// ```
///
/// The cap and tip are [`tier_outer_fee`]'s, the function
/// `settlement::decide_submission_fees` resolves the signed pair from: the
/// quote and the executor read the same tier tips from the same rule
/// ([`TierTips`]), so what is quoted is what is signed.
///
/// `None` on overflow, so an unpriceable market yields no quote rather than a
/// wrong one.
pub fn tier_price(
    tier: SubmissionTier,
    base_fee_per_gas: u128,
    tips: &TierTips,
    market_tip: u128,
) -> Option<GasPrice> {
    let outer = tier_outer_fee(tier, base_fee_per_gas, tips)?;
    let network_fee_per_gas = tier_network_fee(tier, base_fee_per_gas, market_tip)?;

    Some(GasPrice {
        max_fee_per_gas: outer.max_fee_per_gas,
        max_priority_fee_per_gas: outer.max_priority_fee_per_gas,
        network_fee_per_gas,
        relayer_fee_per_gas: outer.max_fee_per_gas.saturating_sub(network_fee_per_gas),
    })
}

/// The legacy-endpoint tip fallback: `eth_gasPrice − base fee`. `None` when
/// the gas price is below the base fee (a node inconsistency worth refusing).
pub fn tip_from_legacy_gas_price(gas_price: U256, base_fee: U256) -> Option<U256> {
    gas_price.checked_sub(base_fee)
}

/// The market tip every tier scales — resolved ONE way, by the quote
/// (`pimlico_getUserOperationGasPrice`, through [`quote_market_tip`]) and by
/// the executor that signs the outer transaction alike:
///
/// 1. **`eth_maxPriorityFeePerGas`**, whatever quantity it returns — zero
///    included. It is the node's own answer to "what tip clears", so it
///    carries a chain's enforced minimum (bor's on Polygon), which is what
///    lets `Slow` sign it unscaled ([`SubmissionTier::tip_bps`]). A zero is
///    an answer, not an absence: Arbitrum reports `0x0` (measured
///    2026-09-21), and the executor has always signed that zero.
/// 2. Only when that call yielded no quantity at all (it failed, or did not
///    return a hex quantity): **`eth_gasPrice −` the latest block's base
///    fee** ([`tip_from_legacy_gas_price`]). `eth_gasPrice` is built as
///    `suggested tip + head base fee`, so subtracting the head block's base
///    fee recovers exactly the tip step 1 would have given — on Polygon on
///    2026-09-21, `278.534895357 − 250.761673410 = 27.773221947` gwei,
///    `eth_maxPriorityFeePerGas` to the wei.
///
/// `None` when neither yields a tip — no gas price either, or one below the
/// base fee. The executor then refuses to build the transaction; only the
/// quote has a further last resort, and [`quote_market_tip`] documents it.
///
/// A caller fetches `eth_gasPrice` only when `max_priority_fee_per_gas` is
/// `None`, because step 2 is never consulted otherwise; passing `None` for
/// `legacy_gas_price` in that case is exact, not an approximation.
///
/// **Why one function.** Until 2026-09-21 the quote took the median of
/// `eth_feeHistory`'s 50th-percentile reward column while the executor
/// signed `eth_maxPriorityFeePerGas`. On Polygon those read ~86 and ~27.8
/// gwei: the wallet was shown — and priced its reimbursement on — a
/// `standard` tip of 107.7 gwei while the relay signed 34.71688084. Both
/// paths now call this, so they cannot drift apart again.
pub fn market_tip(
    max_priority_fee_per_gas: Option<U256>,
    legacy_gas_price: Option<U256>,
    latest_block_base_fee: U256,
) -> Option<U256> {
    max_priority_fee_per_gas
        .or_else(|| tip_from_legacy_gas_price(legacy_gas_price?, latest_block_base_fee))
}

/// The tip the quote prices every tier from: [`market_tip`], fed the same
/// readings the executor takes, with the latest block's base fee taken from
/// the fee history the quote already holds
/// ([`FeeHistory::latest_block_base_fee`]).
///
/// **One step the executor does not have.** When [`market_tip`] yields
/// nothing, the executor has no tip to sign with and refuses to submit (the
/// batch item fails and the operation waits for a later pass). The quote
/// keeps its long-standing last resort instead of going dark:
/// [`fallback_priority_fee`], `base fee / priority_fee_divisor`, on the next
/// block's base fee. That is a price for a market the relay will not sign in
/// right now, and it is the one place the reported tip and the signed tip can
/// still differ; the executor re-reads the market when it does sign.
///
/// `Err` only when the fee history carries no base fee to read.
pub fn quote_market_tip(
    fee_history: &FeeHistory,
    max_priority_fee_per_gas: Option<u128>,
    legacy_gas_price: Option<u128>,
    priority_fee_divisor: u128,
) -> Result<u128, GasPriceError> {
    let latest_block_base_fee = fee_history.latest_block_base_fee()?;
    match market_tip(
        max_priority_fee_per_gas.map(U256::from),
        legacy_gas_price.map(U256::from),
        U256::from(latest_block_base_fee),
    )
    // Cannot fail: the tip is one of the u128 inputs or a difference below one.
    .and_then(|tip| u128::try_from(tip).ok())
    {
        Some(tip) => Ok(tip),
        None => Ok(fallback_priority_fee(
            fee_history.next_block_base_fee()?,
            priority_fee_divisor,
        )),
    }
}

/// The quote's last-resort tip, used only when [`market_tip`] yields none
/// ([`quote_market_tip`]). The executor has no equivalent.
pub fn fallback_priority_fee(base_fee: u128, priority_fee_divisor: u128) -> u128 {
    base_fee.div_ceil(priority_fee_divisor).max(1)
}

pub fn parse_quantity(value: &str) -> Result<u128, GasPriceError> {
    let value = value
        .strip_prefix("0x")
        .ok_or(GasPriceError::InvalidUpstreamResponse)?;

    if value.is_empty() {
        return Err(GasPriceError::InvalidUpstreamResponse);
    }

    u128::from_str_radix(value, 16).map_err(|_| GasPriceError::InvalidUpstreamResponse)
}

/// A legacy `eth_gasPrice` reading, as a market: no base fee, all tip.
///
/// The endpoint returns one number with no base/tip split, and inventing a
/// split would be a guess the relay then charges for. Treated as pure tip,
/// each tier's cap, reimbursement basis and tip all collapse onto the same
/// value — `tip[tier]`, the reading scaled `1.0 / 1.25 / 2.0` (there is no
/// reward column to read here, [`TierTips::scaled`]). There is no base-fee
/// headroom to sell, but priority is still for sale, because the tip is the
/// whole price.
pub fn legacy_price_from_result(result: Value) -> Result<NetworkGasPrice, GasPriceError> {
    let value = result
        .as_str()
        .ok_or(GasPriceError::InvalidUpstreamResponse)
        .and_then(parse_quantity)?;

    Ok(NetworkGasPrice {
        base_fee_per_gas: 0,
        max_priority_fee_per_gas: value,
        tier_tips: TierTips::scaled(value).ok_or(GasPriceError::ArithmeticOverflow)?,
    })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use serde_json::json;

    use super::{
        FeeHistory, GasPricePolicy, IN_BAND_DRIFT_BPS, MIN_POSITIVE_TIP, NetworkGasPrice, OuterFee,
        SubmissionTier, TIP_REWARD_PERCENTILES, TIP_WINDOW_BLOCKS, TierTips, fallback_priority_fee,
        in_band_fee_per_gas, legacy_price_from_result, market_tip, parse_quantity,
        price_from_fee_history, quote_market_tip, quoted_outer_fee, scaled_market_tip,
        tier_network_fee, tier_outer_fee, tier_price, tiers, tip_history_params, tip_rewards,
    };

    const TIERS: [SubmissionTier; 3] = [
        SubmissionTier::Slow,
        SubmissionTier::Standard,
        SubmissionTier::Fast,
    ];

    /// A Polygon (chain 137) batch of 2026-09-21 to `polygon.drpc.org`
    /// (block 94190979): the next and the latest block's base fee,
    /// `eth_maxPriorityFeePerGas` and `eth_gasPrice`.
    const POLYGON_BASE: u128 = 247_805_843_619; // 247.805843619 gwei, next block
    const POLYGON_TIP: u128 = 27_773_221_947; //   27.773221947 gwei
    const POLYGON_LATEST_BASE: u128 = 250_761_673_410; // 250.761673410 gwei
    const POLYGON_GAS_PRICE: u128 = 278_534_895_357; //  278.534895357 gwei

    /// That batch's base fees, as `eth_feeHistory` reports them.
    fn polygon_fee_history() -> FeeHistory {
        serde_json::from_value(json!({
            "oldestBlock": "0x59d3d7f",
            "baseFeePerGas": [
                "0x3a2533c666", "0x3a6909b679", "0x39c7f52c07",
                "0x3a06b2b0b2", "0x3a628f7ac2", "0x39b26118a3"
            ],
            "gasUsedRatio": [0.19374966875, 0.20186779375, 0.19409771875, 0.20936705, 0.1596438875]
        }))
        .unwrap()
    }

    /// Ethereum mainnet `eth_feeHistory(20, 26149237, [25, 50, 70])`, read
    /// from publicnode on 2026-10-08 — the block the fee investigation
    /// measured at (base 2.779 gwei, `eth_maxPriorityFeePerGas` 0, Uniswap
    /// quoting a 1 gwei tip). The reward rows verbatim, then the newest
    /// block's own base fee and the next block's.
    fn ethereum_fee_history() -> serde_json::Value {
        json!({
            "oldestBlock": "0x18f0062",
            "reward": [
                ["0x117e7460", "0x3b9aca00", "0x59682f00"],
                ["0x11e1a300", "0x3d7f1716", "0x77359400"],
                ["0xee6b280", "0x3b9aca00", "0x77359400"],
                ["0x17d78400", "0x3b9aca00", "0x67d47a81"],
                ["0x3b9aca00", "0x448b9b80", "0x7b0b9330"],
                ["0x2faf081", "0x3b9aca00", "0x536c3498"],
                ["0xb4dbee6", "0x3b9aca00", "0x63c73680"],
                ["0x5f5e100", "0x21f0a0c5", "0x77359400"],
                ["0x89f0cf3", "0x3b9aca00", "0x59682f00"],
                ["0x2faf081", "0x3b9aca00", "0x5d7813c6"],
                ["0xbebc200", "0x347492d8", "0x3dc1ae05"],
                ["0x5f5e100", "0x3b9aca00", "0x5c3215bc"],
                ["0x8f0d180", "0x3b9aca00", "0x839d4b57"],
                ["0x88ceb33", "0x3b9aca00", "0x77359400"],
                ["0x5f5e100", "0x3b9aca00", "0x7d2b7501"],
                ["0x989680", "0x3b9aca00", "0x62832c1e"],
                ["0x8f0d180", "0x59682f00", "0x7b0b9330"],
                ["0x5f5e100", "0x3b9aca00", "0x50775d80"],
                ["0x2f85e119", "0x5e792e40", "0x8b4452d9"],
                ["0x4fd4b36", "0x3b9aca00", "0x6e2a213e"]
            ],
            "baseFeePerGas": ["0xa5a8d017", "0xa5f7401b"],
        })
    }
    const ETHEREUM_LATEST_BASE: u128 = 2_779_303_959;
    const ETHEREUM_NEXT_BASE: u128 = 2_784_444_443;

    /// A fee history of base fees only (`[latest block's, next block's]`),
    /// with the given reward rows.
    fn fee_history(latest: u128, next: u128, rewards: Option<Vec<[u128; 3]>>) -> serde_json::Value {
        let mut value = json!({
            "baseFeePerGas": [format!("0x{latest:x}"), format!("0x{next:x}")],
        });
        if let Some(rewards) = rewards {
            value["reward"] = json!(
                rewards
                    .iter()
                    .map(|row| row
                        .iter()
                        .map(|reward| format!("0x{reward:x}"))
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            );
        }
        value
    }

    /// What the quote reports as `tier`'s `maxPriorityFeePerGas` and cap,
    /// given the node's answers — the docker `GasPriceManager::eip1559_price`
    /// and the Worker's `arms::gas_price::eip1559_price`, minus transport.
    fn quoted(
        tier: SubmissionTier,
        history: &serde_json::Value,
        max_priority_fee_per_gas: Option<u128>,
        gas_price: Option<u128>,
    ) -> (u128, u128) {
        let history: FeeHistory = serde_json::from_value(history.clone()).unwrap();
        let tip = quote_market_tip(
            &history,
            max_priority_fee_per_gas,
            max_priority_fee_per_gas
                .is_none()
                .then_some(gas_price)
                .flatten(),
            GasPricePolicy::default().priority_fee_divisor,
        )
        .unwrap();
        let rows = tiers(price_from_fee_history(&history, tip).unwrap()).unwrap();
        let row = match tier {
            SubmissionTier::Slow => rows.slow,
            SubmissionTier::Standard => rows.standard,
            SubmissionTier::Fast => rows.fast,
        };
        (row.max_priority_fee_per_gas, row.max_fee_per_gas)
    }

    /// What the executor signs `tier` with when the payment funds it, given
    /// the same answers — both shells' `transaction_context`, then
    /// `settlement::decide_submission_fees` over `tier_outer_fee`. The cap is
    /// on the base fee the quote priced (the next block's, which is the
    /// executor's latest one block later). `None` where the executor refuses
    /// to submit.
    fn signed(
        tier: SubmissionTier,
        history: &serde_json::Value,
        base_at_submission: u128,
        latest_block_base_fee: u128,
        max_priority_fee_per_gas: Option<u128>,
        gas_price: Option<u128>,
    ) -> Option<(u128, u128)> {
        let tip = market_tip(
            max_priority_fee_per_gas.map(U256::from),
            max_priority_fee_per_gas
                .is_none()
                .then_some(gas_price)
                .flatten()
                .map(U256::from),
            U256::from(latest_block_base_fee),
        )?;
        let tips = TierTips::resolve(
            tip_rewards(history).as_deref(),
            u128::try_from(tip).unwrap(),
        )?;
        let outer = tier_outer_fee(tier, base_at_submission, &tips).unwrap();
        Some((outer.max_priority_fee_per_gas, outer.max_fee_per_gas))
    }

    #[test]
    fn the_tier_tips_are_the_window_medians_of_the_reward_percentiles() {
        // The request both paths send.
        assert_eq!(TIP_WINDOW_BLOCKS, 20);
        assert_eq!(TIP_REWARD_PERCENTILES, [25, 50, 70]);
        assert_eq!(
            tip_history_params(),
            json!(["0x14", "latest", [25, 50, 70]])
        );
        for (tier, percentile) in TIERS.into_iter().zip([25, 50, 70]) {
            assert_eq!(tier.tip_percentile(), percentile);
        }

        // Odd window: the middle row of each column. One wild block moves
        // nothing.
        let rows = [
            [10, 100, 1_000],
            [20, 200, 2_000],
            [9_999_999_999, 9_999_999_999, 9_999_999_999],
        ]
        .map(|row| row.map(|reward: u128| reward * 1_000_000).to_vec());
        assert_eq!(
            TierTips::from_rewards(&rows, 0),
            Some(TierTips {
                slow: 20_000_000,
                standard: 200_000_000,
                fast: 2_000_000_000,
            })
        );
        // Even window: the mean of the two middle values, rounded up.
        let even = [vec![3, 4, 5], vec![4, 6, 9]];
        let tips = TierTips::from_rewards(&even, 0).unwrap();
        assert_eq!(tips.standard, MIN_POSITIVE_TIP.max(5));
        assert_eq!(
            TierTips::from_rewards(&[vec![1, 2, 3], vec![2, 5, 8]], 0)
                .unwrap()
                .fast,
            MIN_POSITIVE_TIP
        );

        // `slow` never under the node's own answer (a chain's enforced
        // minimum), nor under MIN_POSITIVE_TIP once the window paid any tip;
        // each faster tier never under the slower one.
        let low = [vec![0, 0, 5], vec![0, 0, 5], vec![0, 0, 5]];
        assert_eq!(
            TierTips::from_rewards(&low, 0),
            Some(TierTips {
                slow: MIN_POSITIVE_TIP,
                standard: MIN_POSITIVE_TIP,
                fast: MIN_POSITIVE_TIP,
            })
        );
        let polygon = [vec![30, 31, 40], vec![30, 32, 41], vec![1, 35, 45]].map(|row| {
            row.into_iter()
                .map(|gwei: u128| gwei * 1_000_000_000)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            TierTips::from_rewards(&polygon, POLYGON_TIP).unwrap(),
            TierTips {
                slow: 30_000_000_000,
                standard: 32_000_000_000,
                fast: 41_000_000_000,
            }
        );
        let below_minimum = polygon.clone().map(|row| {
            row.into_iter()
                .map(|reward| reward / 10)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            TierTips::from_rewards(&below_minimum, POLYGON_TIP)
                .unwrap()
                .slow,
            POLYGON_TIP
        );

        // A chain whose blocks paid no tip at all keeps zero (Arbitrum).
        assert_eq!(
            TierTips::from_rewards(&[vec![0, 0, 0], vec![0, 0, 0]], 0),
            Some(TierTips::default())
        );
        // No complete row is no reading: the scaled market tip instead. A
        // row of another width (an empty block's `[]`) is left out.
        assert_eq!(TierTips::from_rewards(&[], 7), None);
        assert_eq!(TierTips::from_rewards(&[vec![1, 2]], 7), None);
        assert_eq!(
            TierTips::from_rewards(
                &[vec![], vec![0, 0, 0], vec![2_000_000, 3_000_000, 4_000_000]],
                0
            ),
            TierTips::from_rewards(&[vec![0, 0, 0], vec![2_000_000, 3_000_000, 4_000_000]], 0)
        );
        assert_eq!(
            TierTips::resolve(None, 40),
            Some(TierTips {
                slow: 40,
                standard: 50,
                fast: 80,
            })
        );
        assert_eq!(TierTips::scaled(u128::MAX), None);
    }

    /// The investigation's block: what each tier quotes, and what a client
    /// pays per unit of settlementGas. Standard's cap is, to the wei, the
    /// `maxFeePerGas` Uniswap quoted for its own swap in the same block
    /// (5,176,666,664: 1.5 × the next base fee + a 1 gwei tip).
    #[test]
    fn the_ethereum_tiers_at_the_block_the_overcharge_was_measured() {
        let history = ethereum_fee_history();
        let rewards = tip_rewards(&history).unwrap();
        assert_eq!(rewards.len(), 20);
        let tips = TierTips::resolve(Some(&rewards), 0).unwrap();
        assert_eq!(
            tips,
            TierTips {
                slow: 147_320_634,       // 0.147 gwei
                standard: 1_000_000_000, // 1 gwei
                fast: 1_795_116_512,     // 1.795 gwei
            }
        );
        let parsed: FeeHistory = serde_json::from_value(history.clone()).unwrap();
        assert_eq!(
            parsed.latest_block_base_fee().unwrap(),
            ETHEREUM_LATEST_BASE
        );
        let rows = tiers(price_from_fee_history(&parsed, 0).unwrap()).unwrap();
        assert_eq!(rows.slow.max_fee_per_gas, 4_323_987_298);
        assert_eq!(rows.standard.max_fee_per_gas, 5_176_666_664);
        assert_eq!(rows.fast.max_fee_per_gas, 6_667_894_287);
        assert_eq!(
            [rows.slow, rows.standard, rows.fast].map(|row| in_band_fee_per_gas(
                row.max_fee_per_gas,
                11_000
            )
            .unwrap()),
            [4_756_386_028, 5_694_333_331, 7_334_683_716]
        );
        // `R` is the frozen formula over the market tip, which the node
        // answered as 0: 0.9 / 1.2 / 1.8 × the next base fee, rounded up.
        // To the wei what the deployed relay reported in that block.
        assert_eq!(rows.slow.network_fee_per_gas, 2_505_999_999);
        assert_eq!(rows.standard.network_fee_per_gas, 3_341_333_332);
        assert_eq!(rows.fast.network_fee_per_gas, 5_011_999_998);
        // Fast costs 1.54× slow here, and a third more than standard.
        assert!(rows.fast.max_fee_per_gas * 100 <= rows.slow.max_fee_per_gas * 155);
    }

    #[test]
    fn the_tip_reported_for_a_tier_is_the_tip_the_executor_signs_given_the_same_rpc_answers() {
        // The property the quote exists to keep: fed the SAME node answers,
        // the tip `pimlico_getUserOperationGasPrice` reports for a tier and
        // the tip the executor signs it with are one wei count — and so is
        // the cap, on the base fee the quote priced.
        type NodeAnswers = (
            &'static str,
            u128,                   // the latest block's base fee
            u128,                   // the next block's base fee
            Option<u128>,           // eth_maxPriorityFeePerGas
            Option<u128>,           // eth_gasPrice
            Option<Vec<[u128; 3]>>, // reward rows
        );
        let markets: [NodeAnswers; 8] = [
            (
                "polygon",
                POLYGON_LATEST_BASE,
                POLYGON_BASE,
                Some(POLYGON_TIP),
                Some(POLYGON_GAS_PRICE),
                Some(vec![
                    [30_000_000_000, 31_000_000_000, 40_000_000_000],
                    [26_000_000_000, 35_000_000_000, 45_000_000_000],
                ]),
            ),
            (
                "polygon, no tip answer",
                POLYGON_LATEST_BASE,
                POLYGON_BASE,
                None,
                Some(POLYGON_GAS_PRICE),
                None,
            ),
            (
                "arbitrum",
                20_076_000,
                20_076_000,
                Some(0),
                Some(20_076_000),
                Some(vec![[0, 0, 0]; 20]),
            ),
            (
                "base",
                5_000_000,
                5_000_000,
                Some(1_000_000),
                Some(6_000_000),
                Some(vec![[1_000_000, 1_500_000, 2_000_000]; 20]),
            ),
            ("optimism", 584, 584, None, Some(1_000_584), None),
            (
                "ethereum",
                262_374_330,
                249_829_334,
                Some(27_224),
                Some(262_401_554),
                Some(vec![
                    [1, 10_000_000, 400_000_000],
                    [3, 100_000_000, 700_000_000],
                    [0, 50_000_000, 900_000_000],
                ]),
            ),
            (
                "bsc shape: no base fee",
                0,
                0,
                Some(50_000_000),
                Some(50_000_000),
                Some(vec![[50_000_000, 60_000_000, 100_000_000]; 20]),
            ),
            ("a rising base fee", 100, 112, None, Some(107), None),
        ];
        for (chain, latest, next, reported, gas_price, rewards) in markets {
            let history = fee_history(latest, next, rewards);
            for tier in TIERS {
                assert_eq!(
                    Some(quoted(tier, &history, reported, gas_price)),
                    signed(tier, &history, next, latest, reported, gas_price),
                    "{chain}/{tier}: the quote reported a tip or cap the executor would not sign"
                );
            }
        }

        // The one documented difference: no tip answer and no usable gas
        // price. The executor refuses to submit; only the quote falls back,
        // to `base / 200` on the next block's base fee.
        for gas_price in [None, Some(POLYGON_LATEST_BASE - 1)] {
            let history = fee_history(POLYGON_LATEST_BASE, POLYGON_BASE, None);
            for tier in TIERS {
                assert_eq!(
                    signed(
                        tier,
                        &history,
                        POLYGON_BASE,
                        POLYGON_LATEST_BASE,
                        None,
                        gas_price
                    ),
                    None
                );
            }
            assert_eq!(
                quote_market_tip(&polygon_fee_history(), None, gas_price, 200),
                Ok(fallback_priority_fee(POLYGON_BASE, 200))
            );
        }
    }

    #[test]
    fn the_market_tip_is_the_nodes_own_answer_and_only_then_the_gas_price_derivation() {
        let base = U256::from(POLYGON_LATEST_BASE);
        let gas_price = U256::from(POLYGON_GAS_PRICE);
        assert_eq!(
            market_tip(Some(U256::from(POLYGON_TIP)), None, base),
            Some(U256::from(POLYGON_TIP))
        );
        assert_eq!(
            market_tip(Some(U256::from(POLYGON_TIP)), Some(U256::from(1u64)), base),
            Some(U256::from(POLYGON_TIP))
        );
        // Zero is an answer, not an absence (Arbitrum reports `0x0`).
        assert_eq!(
            market_tip(Some(U256::ZERO), Some(gas_price), base),
            Some(U256::ZERO)
        );
        // Only with no answer: `eth_gasPrice − the latest block's base fee`,
        // which recovers the node's own tip to the wei.
        assert_eq!(
            market_tip(None, Some(gas_price), base),
            Some(U256::from(POLYGON_TIP))
        );
        assert_eq!(market_tip(None, Some(base - U256::from(1u64)), base), None);
        assert_eq!(market_tip(None, None, base), None);
    }

    #[test]
    fn the_latest_block_base_fee_is_the_one_eth_gas_price_is_built_on() {
        let history = polygon_fee_history();
        assert_eq!(history.next_block_base_fee().unwrap(), POLYGON_BASE);
        assert_eq!(
            history.latest_block_base_fee().unwrap(),
            POLYGON_LATEST_BASE
        );
        assert_eq!(POLYGON_GAS_PRICE - POLYGON_LATEST_BASE, POLYGON_TIP);
        let single: FeeHistory =
            serde_json::from_value(json!({ "baseFeePerGas": ["0x64"] })).unwrap();
        assert_eq!(single.latest_block_base_fee().unwrap(), 100);
        assert_eq!(single.next_block_base_fee().unwrap(), 100);
        let empty: FeeHistory = serde_json::from_value(json!({ "baseFeePerGas": [] })).unwrap();
        assert!(empty.latest_block_base_fee().is_err());
        assert!(empty.next_block_base_fee().is_err());
        assert!(quote_market_tip(&empty, Some(1), None, 200).is_err());
        // A reward column with one unreadable entry is not read at all.
        let broken: FeeHistory = serde_json::from_value(json!({
            "baseFeePerGas": ["0x64", "0x64"],
            "reward": [["0x1", "0x2", "0x3"], ["0x1", "nope", "0x3"]],
        }))
        .unwrap();
        assert_eq!(broken.rewards(), None);
    }

    #[test]
    fn every_tier_reports_the_cap_and_the_tip_it_will_be_submitted_with() {
        // Base 100, tier tips 40 / 50 / 80 read from rewards, market tip 40.
        let rows = tiers(NetworkGasPrice {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 40,
            tier_tips: TierTips {
                slow: 40,
                standard: 50,
                fast: 80,
            },
        })
        .unwrap();
        assert_eq!(rows.slow.max_fee_per_gas, 190); // 1.5 × 100 + 40
        assert_eq!(rows.slow.max_priority_fee_per_gas, 40);
        assert_eq!(rows.standard.max_fee_per_gas, 200); // 1.5 × 100 + 50
        assert_eq!(rows.standard.max_priority_fee_per_gas, 50);
        assert_eq!(rows.fast.max_fee_per_gas, 255); // 1.75 × 100 + 80
        assert_eq!(rows.fast.max_priority_fee_per_gas, 80);
        // `R`, frozen: 0.9 / 1.2 / 1.8 × 100 + 1.00 / 1.25 / 2.00 × 40.
        assert_eq!(rows.slow.network_fee_per_gas, 130);
        assert_eq!(rows.standard.network_fee_per_gas, 170);
        assert_eq!(rows.fast.network_fee_per_gas, 260);
        // The headroom field never goes negative.
        assert_eq!(rows.slow.relayer_fee_per_gas, 60);
        assert_eq!(rows.standard.relayer_fee_per_gas, 30);
        assert_eq!(rows.fast.relayer_fee_per_gas, 0);
        // The table.
        assert_eq!(
            TIERS.map(SubmissionTier::base_fee_bps),
            [15_000, 15_000, 17_500]
        );
    }

    #[test]
    fn the_reimbursement_basis_is_frozen_for_wallets_that_price_limits() {
        // vela-wallet before `settlementGas` pays `3 × padded limits ×
        // max(C, R)` and refuses `R > 3 × C`, with `C = max(eth_gasPrice,
        // base + eth_maxPriorityFeePerGas)`. `R` keeps its definition so
        // those wallets pay, and refuse, exactly as they did:
        // `div_ceil(base × 0.6 × legacy multiple) + legacy-scaled market tip`.
        for (base, tip) in [
            (POLYGON_BASE, POLYGON_TIP),
            (45_517_289, 5_757_642),
            (536, 57),
            (0, 50_000_000),
            (1, 0),
            (3, 0),
            (u128::MAX / 32_000, 999),
        ] {
            for (tier, (numerator, scaled)) in
                TIERS
                    .into_iter()
                    .zip([(90u128, tip), (120, tip + tip.div_ceil(4)), (180, 2 * tip)])
            {
                assert_eq!(
                    tier_network_fee(tier, base, tip),
                    U256::from(base)
                        .checked_mul(U256::from(numerator))
                        .map(|product| product.div_ceil(U256::from(100u8)))
                        .and_then(|term| u128::try_from(term).ok())
                        .and_then(|term| term.checked_add(scaled)),
                    "{tier} base={base} tip={tip}"
                );
            }
        }
        // Whatever reward percentiles say: on Ethereum the node's tip reads
        // ~0, so `R` stays within 1.8 × C even where the fast tier tips 1.8
        // gwei over a 0.1 gwei base fee — a reward tip in `R` would have
        // tripped those wallets' `R > 3 × C` refusal there.
        let calm = NetworkGasPrice {
            base_fee_per_gas: 100_000_000,
            max_priority_fee_per_gas: 0,
            tier_tips: TierTips {
                slow: 10_000_000,
                standard: 100_000_000,
                fast: 1_800_000_000,
            },
        };
        let rows = tiers(calm).unwrap();
        for row in [rows.slow, rows.standard, rows.fast] {
            assert!(row.network_fee_per_gas <= 3 * 100_000_000);
        }
    }

    #[test]
    fn in_band_fee_per_gas_is_the_markup_and_the_drift_allowance_on_the_cap() {
        assert_eq!(IN_BAND_DRIFT_BPS, 10_000);
        // 1.1 × 1.0, rounded up.
        assert_eq!(
            in_band_fee_per_gas(1_000_000_000, 11_000),
            Some(1_100_000_000)
        );
        assert_eq!(in_band_fee_per_gas(1, 11_000), Some(2));
        assert_eq!(in_band_fee_per_gas(0, 11_000), Some(0));
        // The operator's markup is the one priced.
        assert_eq!(
            in_band_fee_per_gas(1_000_000_000, 14_000),
            Some(1_400_000_000)
        );
        assert_eq!(in_band_fee_per_gas(u128::MAX, 11_000), None);
    }

    #[test]
    fn a_fast_send_outbids_a_slow_one_and_its_cap_never_truncates_the_tip() {
        // The defect a Polygon receipt proved (base 250.710, Max 775.525, Max
        // Priority 30.35 gwei: a `fast` send that paid the builder exactly
        // what `slow` would) cannot recur: every tier signs its own tip, and
        // the builder's effective tip at the base fee it was quoted for is
        // that tip, ordered slow ≤ standard ≤ fast.
        for (base, tips) in [
            (
                250_710_118_904u128,
                TierTips::scaled(30_350_000_000).unwrap(),
            ),
            (
                ETHEREUM_NEXT_BASE,
                TierTips {
                    slow: 147_320_634,
                    standard: 1_000_000_000,
                    fast: 1_795_116_512,
                },
            ),
            (0, TierTips::scaled(50_000_000).unwrap()),
        ] {
            let fees = TIERS.map(|tier| tier_outer_fee(tier, base, &tips).unwrap());
            for (tier, fee) in TIERS.into_iter().zip(fees) {
                assert!(fee.delivers_full_tip_at(base), "{tier}");
                assert_eq!(fee.effective_tip_at(base), tips.of(tier));
            }
            assert!(fees[0].effective_tip_at(base) <= fees[1].effective_tip_at(base));
            assert!(fees[1].effective_tip_at(base) <= fees[2].effective_tip_at(base));
            assert!(fees[0].effective_tip_at(base) < fees[2].effective_tip_at(base));
        }
        // Across the roundings most likely to cross.
        for base in (0..=64u128).chain([POLYGON_BASE, 45_517_289, 536, u128::MAX / 32_000]) {
            for tip in [0u128, 1, 3, 7, 57, POLYGON_TIP] {
                let tips = TierTips {
                    slow: tip,
                    standard: tip + 1,
                    fast: tip + 2,
                };
                for tier in TIERS {
                    let fee = tier_outer_fee(tier, base, &tips).unwrap();
                    assert!(fee.max_fee_per_gas >= base + fee.max_priority_fee_per_gas);
                }
            }
        }
        let truncated = OuterFee {
            max_fee_per_gas: 109,
            max_priority_fee_per_gas: 10,
        };
        assert_eq!(truncated.effective_tip_at(100), 9);
        assert!(!truncated.delivers_full_tip_at(100));
        assert_eq!(truncated.effective_tip_at(200), 0);
        assert_eq!(
            tier_outer_fee(SubmissionTier::Fast, u128::MAX, &TierTips::default()),
            None
        );
    }

    #[test]
    fn the_scaled_market_tip_is_the_fallback_and_slow_is_the_market_tip_itself() {
        assert_eq!(
            TIERS.map(SubmissionTier::legacy_tip_bps),
            [10_000, 12_500, 20_000]
        );
        for tip in [0u128, 1, 3, 7, POLYGON_TIP, u128::MAX] {
            assert_eq!(scaled_market_tip(SubmissionTier::Slow, tip), Some(tip));
        }
        assert_eq!(scaled_market_tip(SubmissionTier::Standard, 3), Some(4));
        assert_eq!(scaled_market_tip(SubmissionTier::Fast, u128::MAX), None);
        // A zero-base-fee chain with no reward column still differentiates
        // its tiers, through the tip (BSC).
        let rows = tiers(NetworkGasPrice {
            base_fee_per_gas: 0,
            max_priority_fee_per_gas: 50_000_000,
            tier_tips: TierTips::scaled(50_000_000).unwrap(),
        })
        .unwrap();
        assert_eq!(
            [rows.slow, rows.standard, rows.fast].map(|row| row.max_fee_per_gas),
            [50_000_000, 62_500_000, 100_000_000]
        );
    }

    #[test]
    fn tier_prices_agree_with_the_pair_the_executor_signs() {
        for (base, tips, market) in [
            (
                POLYGON_BASE,
                TierTips::scaled(POLYGON_TIP).unwrap(),
                POLYGON_TIP,
            ),
            (
                ETHEREUM_NEXT_BASE,
                TierTips {
                    slow: 147_320_634,
                    standard: 1_000_000_000,
                    fast: 1_795_116_512,
                },
                0,
            ),
            (0, TierTips::scaled(3).unwrap(), 3),
        ] {
            for tier in TIERS {
                let price = tier_price(tier, base, &tips, market).unwrap();
                let outer = tier_outer_fee(tier, base, &tips).unwrap();
                assert_eq!(price.max_fee_per_gas, outer.max_fee_per_gas);
                assert_eq!(
                    price.max_priority_fee_per_gas,
                    outer.max_priority_fee_per_gas
                );
            }
        }
    }

    #[test]
    fn quotes_double_base_plus_tip_with_overflow_checks() {
        use super::tip_from_legacy_gas_price;
        assert_eq!(quoted_outer_fee(100, 7), Some(207));
        assert_eq!(quoted_outer_fee(u128::MAX, 0), None);
        assert_eq!(
            tip_from_legacy_gas_price(U256::from(150u64), U256::from(100u64)),
            Some(U256::from(50u64))
        );
        assert_eq!(
            tip_from_legacy_gas_price(U256::from(90u64), U256::from(100u64)),
            None
        );
    }

    #[test]
    fn a_tier_name_round_trips_and_an_unknown_one_is_refused() {
        for (name, tier) in [
            ("slow", SubmissionTier::Slow),
            ("standard", SubmissionTier::Standard),
            ("fast", SubmissionTier::Fast),
        ] {
            assert_eq!(
                serde_json::from_value::<SubmissionTier>(json!(name)).unwrap(),
                tier
            );
            assert_eq!(serde_json::to_value(tier).unwrap(), json!(name));
            assert_eq!(tier.as_str(), name);
            assert_eq!(tier.to_string(), name);
        }
        let error = serde_json::from_value::<SubmissionTier>(json!("turbo")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown variant `turbo`, expected one of `slow`, `standard`, `fast`"
        );
        assert!(serde_json::from_value::<SubmissionTier>(json!(1_000)).is_err());
    }

    #[test]
    fn tiers_order_from_slow_to_fast_so_a_bundle_can_take_the_fastest() {
        assert!(SubmissionTier::Slow < SubmissionTier::Standard);
        assert!(SubmissionTier::Standard < SubmissionTier::Fast);
        assert_eq!(
            [
                SubmissionTier::Standard,
                SubmissionTier::Fast,
                SubmissionTier::Slow
            ]
            .into_iter()
            .max(),
            Some(SubmissionTier::Fast)
        );
    }

    #[test]
    fn rejects_invalid_quantities() {
        assert!(parse_quantity("100").is_err());
        assert!(parse_quantity("0x").is_err());
        assert!(parse_quantity("0xnope").is_err());
    }

    #[test]
    fn falls_back_to_legacy_gas_price_response() {
        // One number, no base/tip split and no reward column: read as pure
        // tip, scaled 1.0 / 1.25 / 2.0, so each tier's cap, basis and tip land
        // on the same value.
        let price = legacy_price_from_result(json!("0x64")).unwrap();
        assert_eq!(
            price,
            NetworkGasPrice {
                base_fee_per_gas: 0,
                max_priority_fee_per_gas: 100,
                tier_tips: TierTips {
                    slow: 100,
                    standard: 125,
                    fast: 200,
                },
            }
        );
        let rows = tiers(price).unwrap();
        for (row, expected) in [(rows.slow, 100u128), (rows.standard, 125), (rows.fast, 200)] {
            assert_eq!(row.max_fee_per_gas, expected);
            assert_eq!(row.max_priority_fee_per_gas, expected);
            assert_eq!(row.network_fee_per_gas, expected);
            assert_eq!(row.relayer_fee_per_gas, 0);
        }
        assert!(legacy_price_from_result(json!({ "gasPrice": "0x64" })).is_err());
    }
}
