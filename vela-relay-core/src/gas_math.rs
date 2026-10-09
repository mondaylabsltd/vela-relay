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

/// The fewest recent blocks a tier's tip is read over (`eth_feeHistory`'s
/// block count, ending at `latest`): 20, chosen by the backtest in
/// `docs/fees.md` §2c on Ethereum, where it is four minutes.
pub const TIP_WINDOW_BLOCKS: u64 = 20;

/// The least time a tip window covers: a minute ([`tip_window_blocks`]).
///
/// Twenty blocks is nine seconds on BNB Smart Chain — less than a quote's own
/// age — so the executor read its tips from blocks the quote had never seen,
/// and 38% of BSC `standard` and `fast` sends had their tip shaved below the
/// one they were quoted (review, 2026-10-09). Over a minute the window the
/// executor reads still holds most of the quote's blocks, and a median of
/// mostly the same blocks is mostly the same tip.
pub const TIP_WINDOW_MS: u64 = 60_000;

/// The most blocks a tip window spans: Arbitrum's quarter-second blocks make a
/// minute 240. Every endpoint probed on BSC, Polygon, Avalanche, Arbitrum,
/// Base, Monad, Arc, Plume, Tempo and Robinhood Chain answered
/// `eth_feeHistory` over 256 blocks with its reward percentiles (2026-10-09).
pub const TIP_WINDOW_MAX_BLOCKS: u64 = 256;

/// How many blocks a chain's tip window spans: a minute of its blocks
/// ([`crate::pace::block_interval_ms`], 2 s for a chain not listed there),
/// never fewer than [`TIP_WINDOW_BLOCKS`] nor more than
/// [`TIP_WINDOW_MAX_BLOCKS`]. Ethereum and Gnosis keep 20; BNB Smart Chain
/// reads 134, Polygon, OP Mainnet and Base 30, Avalanche and Unichain 60,
/// Arbitrum 240. The quote and the executor ask for the same count
/// ([`tip_history_params`]).
pub fn tip_window_blocks(chain_id: u64) -> u64 {
    let interval = crate::pace::block_interval_ms(chain_id)
        .unwrap_or(crate::pace::DEFAULT_BLOCK_INTERVAL_MS)
        .max(1);
    TIP_WINDOW_MS
        .div_ceil(interval)
        .clamp(TIP_WINDOW_BLOCKS, TIP_WINDOW_MAX_BLOCKS)
}

/// A window whose blocks used less than this share of their gas limit, on
/// average, had room for every transaction paying the chain's minimum tip:
/// 30%, in basis points. There the reward percentiles are what a few bots
/// bid, not the price of a place in the next block — Polygon's 25th
/// percentile read 166–265 gwei against the node's 30, Avalanche's 2.5–6.2
/// gwei on blocks 3% full, Gnosis's 70th 1.5 gwei over an 8-wei base fee
/// (2026-10-09) — so the tiers sign the node's tip instead, scaled `1.00 /
/// 1.25 / 2.00` as before rewards were read ([`TierTips::from_window`]).
///
/// Not one half: Ethereum's base fee targets half-full blocks, so its
/// 20-block average sits below 0.5 in 49% of windows (74,752 mainnet blocks,
/// `docs/fees.md` §2c) — at 0.5 its tiers would drop to the node's zero tip
/// every other minute. Its lowest 20-block average over those 10.4 days was
/// 0.338; BNB Smart Chain's minute is under 0.3 in 92% of windows, Polygon's
/// 93%, Avalanche's always.
pub const UNCONGESTED_GAS_USED_RATIO_BPS: u32 = 3_000;

/// Below this average a window may have been quiet when the quote was read a
/// few blocks earlier: 40%. The tip a payment may be shaved to before it is
/// held ([`TierTips::floor`]) is then the lower of the two readings' `slow`
/// tips, so a quote read in a quiet window is not held by an executor that
/// reads the next one busy (Polygon: 30 gwei quoted, 166 read).
pub const NEAR_UNCONGESTED_GAS_USED_RATIO_BPS: u32 = 4_000;

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
/// floor (1.25×) with its whole tip, so a quote's drift is paid for by the
/// cap the client already buys; a larger allowance only bought acceptance the
/// backtest did not need. Kept in the formula so the contract states it.
pub const IN_BAND_DRIFT_BPS: u64 = 10_000;

/// The tip each tier signs with — ONE reading, shared by the quote
/// (`pimlico_getUserOperationGasPrice`) and the executor
/// (`settlement::decide_submission_fees`), so what a wallet is quoted is what
/// the relay signs.
///
/// Read from `eth_feeHistory(tip_window_blocks(chain), "latest",
/// TIP_REWARD_PERCENTILES)` ([`tip_history_params`]) — at least 20 blocks and
/// a minute — by [`TierTips::from_window`]: in a busy window, each tier's tip
/// is the median over the window of each block's own percentile reward (the
/// effective tips its gas paid), so one odd block moves nothing; in a quiet
/// one ([`UNCONGESTED_GAS_USED_RATIO_BPS`]), the node's own tip scaled as
/// before rewards were read. Each faster tier is at least the slower one's,
/// so a faster tier can never bid less.
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
    /// The least tip a payment that cannot fund its tier's own may be shaved
    /// to before the executor holds it (`settlement::decide_submission_fees`):
    /// the window's own 25th-percentile reward — what a quarter of its gas
    /// really paid, which no node's opinion moves (the node's tip where the
    /// median block paid none, since empty blocks prove nothing) — and, in a
    /// window that may
    /// have been quiet when the quote was read
    /// ([`NEAR_UNCONGESTED_GAS_USED_RATIO_BPS`]), the quiet reading's `slow`
    /// tip if that is lower. Never above `slow`. A quote read from another
    /// node, or from blocks a little quieter, is shaved to a slower send
    /// rather than held (review F1, 2026-10-09).
    #[serde(default)]
    pub floor: u128,
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
    /// block) and the market tip, with the window's congestion unknown — the
    /// busy reading of [`TierTips::from_window`]. `None` when no complete row
    /// is left.
    pub fn from_rewards(rewards: &[Vec<u128>], market_tip: u128) -> Option<Self> {
        Self::from_window(
            &TipWindow {
                rewards: rewards.to_vec(),
                gas_used_ratio_bps: None,
            },
            market_tip,
        )
    }

    /// The tips from one fee-history window and the node's own tip answer
    /// ([`market_tip`]):
    ///
    /// ```text
    /// least     = MIN_POSITIVE_TIP if the window paid any tip, else 0
    /// node      = the market tip, where it is ≤ the median p50 or that median is zero;
    ///             otherwise no floor at all
    /// rewarded  = max( median p25 , least )                     the window's own slow price
    ///
    /// busy  (mean gasUsedRatio ≥ 30%, or unknown):
    ///   slow = max( rewarded , node ),  standard = max( median p50 , slow ),  fast = max( median p70 , standard )
    /// quiet (mean gasUsedRatio < 30%):
    ///   slow = max( node , least ) — or `rewarded` when the node's answer was discarded —
    ///   standard = max( min( 1.25 × slow , median p50 ) , slow ),  fast = max( min( 2 × slow , median p70 ) , standard )
    /// floor = rewarded — the node's tip where the median block paid none —
    ///         or the lower of that and the quiet slow below a 40% mean
    /// ```
    ///
    /// **The node's tip is a floor only where the blocks do not contradict
    /// it.** It carries a chain's enforced minimum (bor's on Polygon), which a
    /// tip must clear or be refused outright — and an enforced minimum is
    /// never above what the median block paid, since every included
    /// transaction cleared it. Nodes disagree: on BNB Smart Chain the
    /// directory's endpoints answered 0.05, 0.1, 1 and 3 gwei while every
    /// block's median paid 0.05 (2026-10-09). The quote and the executor ask
    /// different nodes, so an executor reading 1 gwei held — then rejected —
    /// sends quoted at 0.05; above the median the answer is that node's
    /// opinion, and it is left out. A window whose median block paid no tip
    /// (Stable, XRPL EVM, Arbitrum: rewards almost all zero, blocks almost
    /// empty) says nothing either way, and the node is believed, as before.
    ///
    /// **A quiet window signs the node's tip.** Where blocks have room for
    /// every transaction paying the minimum, the percentiles price a few bots'
    /// bids, not a place in the next block ([`UNCONGESTED_GAS_USED_RATIO_BPS`]),
    /// so each tier signs the node's tip scaled `1.00 / 1.25 / 2.00` — what
    /// every tier signed before rewards were read, mined on Polygon at 30 gwei
    /// tips — but a faster tier never more than the window's blocks paid at
    /// its own percentile: on BNB Smart Chain, whose blocks pay 0.05 / 0.05 /
    /// 0.057 gwei, twice the node's 0.05 would have made `fast` 40% dearer for
    /// nothing. `fast` bids twice `slow` wherever its blocks paid that much.
    ///
    /// A row of another width (some nodes answer `[]` for an empty block) is
    /// left out; `None` when no complete row is left, or on overflow.
    pub fn from_window(window: &TipWindow, market_tip: u128) -> Option<Self> {
        let rewards = window
            .rewards
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
        let least = if paid_any { MIN_POSITIVE_TIP } else { 0 };
        // The window is evidence of what the chain takes only where its median
        // block paid a tip; a column of mostly zeros may be empty blocks.
        let evidence = median(1) > 0;
        let node = (!evidence || market_tip <= median(1)).then_some(market_tip);
        let rewarded = median(0).max(least);
        let quiet_slow = node.map_or(rewarded, |tip| tip.max(least));
        // The least tip the window proves the chain takes: a quarter of its
        // gas paid `rewarded` or less. Without evidence the node's answer
        // stands.
        let proven = if evidence { rewarded } else { quiet_slow };
        let ratio = window.gas_used_ratio_bps;
        if ratio.is_some_and(|ratio| ratio < UNCONGESTED_GAS_USED_RATIO_BPS) {
            let standard = scaled_market_tip(SubmissionTier::Standard, quiet_slow)?
                .min(median(1))
                .max(quiet_slow);
            let fast = scaled_market_tip(SubmissionTier::Fast, quiet_slow)?
                .min(median(2))
                .max(standard);
            return Some(Self {
                slow: quiet_slow,
                standard,
                fast,
                floor: quiet_slow.min(proven),
            });
        }
        let slow = rewarded.max(node.unwrap_or(0));
        let standard = median(1).max(slow);
        let fast = median(2).max(standard);
        let floor = if ratio.is_some_and(|ratio| ratio < NEAR_UNCONGESTED_GAS_USED_RATIO_BPS) {
            proven.min(quiet_slow)
        } else {
            proven
        };
        Some(Self {
            slow,
            standard,
            fast,
            floor,
        })
    }

    /// The fallback: the market tip scaled `1.00 / 1.25 / 2.00`, the floor
    /// the market tip itself. `None` on overflow.
    pub fn scaled(market_tip: u128) -> Option<Self> {
        Some(Self {
            slow: scaled_market_tip(SubmissionTier::Slow, market_tip)?,
            standard: scaled_market_tip(SubmissionTier::Standard, market_tip)?,
            fast: scaled_market_tip(SubmissionTier::Fast, market_tip)?,
            floor: market_tip,
        })
    }

    /// The window's reading when it has usable rewards, else the scaled market
    /// tip.
    pub fn resolve(window: Option<&TipWindow>, market_tip: u128) -> Option<Self> {
        window
            .and_then(|window| Self::from_window(window, market_tip))
            .or_else(|| Self::scaled(market_tip))
    }
}

/// What one `eth_feeHistory` answer to [`tip_history_params`] says about
/// tips: each block's `[p25, p50, p70]` reward, and how full the blocks were.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TipWindow {
    pub rewards: Vec<Vec<u128>>,
    /// The blocks' mean `gasUsedRatio`, in basis points. `None` when the
    /// answer carried none the relay could read: the tips are then read as
    /// for a busy window, the reading before congestion was considered.
    pub gas_used_ratio_bps: Option<u32>,
}

/// The `eth_feeHistory` params the tier tips are read from — by the quote and
/// by the executor's transaction context alike: [`tip_window_blocks`] of the
/// chain's latest blocks, at [`TIP_REWARD_PERCENTILES`].
pub fn tip_history_params(chain_id: u64) -> Value {
    serde_json::json!([
        format!("0x{:x}", tip_window_blocks(chain_id)),
        "latest",
        TIP_REWARD_PERCENTILES
    ])
}

/// The tip window of an `eth_feeHistory` answer to [`tip_history_params`],
/// `None` when it carries no reward rows the relay can read whole.
pub fn tip_window(fee_history: &Value) -> Option<TipWindow> {
    serde_json::from_value::<FeeHistory>(fee_history.clone())
        .ok()?
        .window()
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
/// the reward rows the tier tips come from ([`TierTips`]), and how full the
/// blocks were.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistory {
    pub base_fee_per_gas: Vec<String>,
    #[serde(default)]
    pub reward: Option<Vec<Vec<String>>>,
    /// Kept as raw JSON and read leniently ([`FeeHistory::mean_gas_used_ratio_bps`]):
    /// an answer whose ratios the relay cannot read is still a fee history.
    #[serde(default)]
    pub gas_used_ratio: Option<Vec<Value>>,
}

impl FeeHistory {
    /// The blocks' mean `gasUsedRatio`, in basis points, rounded to the
    /// nearest. `None` when the answer carries no ratio, or any entry is not a
    /// number between 0 and 1 — a column the relay cannot read whole is not
    /// read at all.
    pub fn mean_gas_used_ratio_bps(&self) -> Option<u32> {
        let ratios = self
            .gas_used_ratio
            .as_ref()?
            .iter()
            .map(|ratio| ratio.as_f64().filter(|ratio| (0.0..=1.0).contains(ratio)))
            .collect::<Option<Vec<_>>>()?;
        if ratios.is_empty() {
            return None;
        }
        let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
        // Within 0..=10_000 by the filter above.
        Some((mean * 10_000.0).round() as u32)
    }

    /// The tip window this answer describes: its reward rows and its blocks'
    /// mean fullness. `None` without a readable reward column.
    pub fn window(&self) -> Option<TipWindow> {
        Some(TipWindow {
            rewards: self.rewards()?,
            gas_used_ratio_bps: self.mean_gas_used_ratio_bps(),
        })
    }

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
        tier_tips: TierTips::resolve(fee_history.window().as_ref(), priority_fee)
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
        SubmissionTier, TIP_REWARD_PERCENTILES, TIP_WINDOW_BLOCKS, TIP_WINDOW_MAX_BLOCKS,
        TIP_WINDOW_MS, TierTips, TipWindow, UNCONGESTED_GAS_USED_RATIO_BPS, fallback_priority_fee,
        in_band_fee_per_gas, legacy_price_from_result, market_tip, parse_quantity,
        price_from_fee_history, quote_market_tip, quoted_outer_fee, scaled_market_tip,
        tier_network_fee, tier_outer_fee, tier_price, tiers, tip_history_params, tip_window,
        tip_window_blocks,
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
        let tips = TierTips::resolve(tip_window(history).as_ref(), u128::try_from(tip).unwrap())?;
        let outer = tier_outer_fee(tier, base_at_submission, &tips).unwrap();
        Some((outer.max_priority_fee_per_gas, outer.max_fee_per_gas))
    }

    #[test]
    fn the_tier_tips_are_the_window_medians_of_the_reward_percentiles() {
        // The request both paths send.
        assert_eq!(TIP_WINDOW_BLOCKS, 20);
        assert_eq!(TIP_REWARD_PERCENTILES, [25, 50, 70]);
        assert_eq!(
            tip_history_params(1),
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
                floor: 20_000_000,
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

        // `slow` never under MIN_POSITIVE_TIP once the window paid any tip;
        // each faster tier never under the slower one.
        let low = [vec![0, 0, 5], vec![0, 0, 5], vec![0, 0, 5]];
        assert_eq!(
            TierTips::from_rewards(&low, 0),
            Some(TierTips {
                slow: MIN_POSITIVE_TIP,
                standard: MIN_POSITIVE_TIP,
                fast: MIN_POSITIVE_TIP,
                floor: MIN_POSITIVE_TIP,
            })
        );
        let gwei =
            |rows: [[u128; 3]; 3]| rows.map(|row| row.map(|gwei| gwei * 1_000_000_000).to_vec());
        let polygon = gwei([[30, 31, 40], [30, 32, 41], [1, 35, 45]]);
        assert_eq!(
            TierTips::from_rewards(&polygon, POLYGON_TIP).unwrap(),
            TierTips {
                slow: 30_000_000_000,
                standard: 32_000_000_000,
                fast: 41_000_000_000,
                floor: 30_000_000_000,
            }
        );
        // `slow` never under the node's own answer where the blocks do not
        // contradict it — at or below the median block's tip it can be a
        // chain's enforced minimum (bor's on Polygon) — while the floor a
        // short payment may be shaved to is what a quarter of the gas paid.
        let under_the_node = gwei([[20, 31, 40], [26, 32, 41], [1, 35, 45]]);
        assert_eq!(
            TierTips::from_rewards(&under_the_node, POLYGON_TIP).unwrap(),
            TierTips {
                slow: POLYGON_TIP,
                standard: 32_000_000_000,
                fast: 41_000_000_000,
                floor: 20_000_000_000,
            }
        );

        // A chain whose blocks paid no tip at all keeps zero (Arbitrum)...
        assert_eq!(
            TierTips::from_rewards(&[vec![0, 0, 0], vec![0, 0, 0]], 0),
            Some(TierTips::default())
        );
        // ...or the node's own tip, which nothing in its blocks contradicts
        // (Stable: every reward zero, the node 0.125 gwei).
        assert_eq!(
            TierTips::from_rewards(&[vec![0, 0, 0], vec![0, 0, 0]], 125_000_000),
            Some(TierTips {
                slow: 125_000_000,
                standard: 125_000_000,
                fast: 125_000_000,
                floor: 125_000_000,
            })
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
                floor: 40,
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
        let window = tip_window(&history).unwrap();
        assert_eq!(window.rewards.len(), 20);
        let tips = TierTips::resolve(Some(&window), 0).unwrap();
        assert_eq!(
            tips,
            TierTips {
                slow: 147_320_634,       // 0.147 gwei
                standard: 1_000_000_000, // 1 gwei
                fast: 1_795_116_512,     // 1.795 gwei
                floor: 147_320_634,
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

        // A quiet window (Polygon, blocks 19% full): both read the node's
        // own tip, scaled, from the same answers.
        let mut quiet = fee_history(
            POLYGON_LATEST_BASE,
            POLYGON_BASE,
            Some(vec![
                [265_700_000_000, 286_600_000_000, 288_700_000_000];
                30
            ]),
        );
        quiet["gasUsedRatio"] = json!(vec![0.19; 30]);
        for tier in TIERS {
            assert_eq!(
                Some(quoted(tier, &quiet, Some(POLYGON_TIP), None)),
                signed(
                    tier,
                    &quiet,
                    POLYGON_BASE,
                    POLYGON_LATEST_BASE,
                    Some(POLYGON_TIP),
                    None
                ),
                "quiet polygon/{tier}"
            );
        }
        assert_eq!(
            quoted(SubmissionTier::Fast, &quiet, Some(POLYGON_TIP), None).0,
            2 * POLYGON_TIP
        );

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

    /// A window of `blocks` identical rows `[p25, p50, p70]`, with the given
    /// mean `gasUsedRatio` in basis points.
    fn window(rows: &[[u128; 3]], gas_used_ratio_bps: Option<u32>) -> TipWindow {
        TipWindow {
            rewards: rows.iter().map(|row| row.to_vec()).collect(),
            gas_used_ratio_bps,
        }
    }

    const GWEI: u128 = 1_000_000_000;

    #[test]
    fn the_tip_window_is_a_minute_of_blocks_and_never_fewer_than_twenty() {
        assert_eq!(TIP_WINDOW_MS, 60_000);
        for (chain_id, blocks) in [
            (1, 20),       // Ethereum: 20 blocks are four minutes
            (100, 20),     // Gnosis: 5 s blocks
            (56, 134),     // BNB Smart Chain: 0.45 s blocks — 20 were 9 s
            (137, 30),     // Polygon
            (8_453, 30),   // Base
            (43_114, 60),  // Avalanche
            (130, 60),     // Unichain
            (42_161, 240), // Arbitrum's quarter-second blocks
            (999_999, 30), // a chain nobody listed: 2 s blocks
        ] {
            assert_eq!(tip_window_blocks(chain_id), blocks, "chain {chain_id}");
            assert!(tip_window_blocks(chain_id) <= TIP_WINDOW_MAX_BLOCKS);
        }
        assert_eq!(
            tip_history_params(56),
            json!(["0x86", "latest", [25, 50, 70]])
        );
    }

    /// Review F7: a quote 20 BNB Smart Chain blocks (9 s) older than the
    /// submission read a window the executor no longer sees at all. A burst
    /// of priority bids in the quote's first 20 blocks moved its median and
    /// not the executor's; over a minute of blocks both medians are the same.
    #[test]
    fn a_quote_and_its_submission_read_mostly_the_same_blocks_on_a_fast_chain() {
        let burst = [70_000_000u128, 90_000_000, 120_000_000];
        let calm = [50_000_000u128, 50_000_001, 57_000_000];
        let history = (0..154)
            .map(|block| if block < 20 { burst } else { calm })
            .collect::<Vec<_>>();
        let tips = |blocks: &[[u128; 3]]| {
            TierTips::from_window(&window(blocks, Some(5_000)), 50_000_000).unwrap()
        };
        // Twenty blocks: the quote saw only the burst, the executor only calm.
        assert_ne!(tips(&history[0..20]), tips(&history[20..40]));
        // A minute (134 blocks): the quote's window and the executor's share
        // 114 blocks, and their medians agree.
        let blocks = usize::try_from(tip_window_blocks(56)).unwrap();
        assert_eq!(tips(&history[0..blocks]), tips(&history[20..20 + blocks]));
    }

    /// Review F1: the quote and the executor ask different nodes for
    /// `eth_maxPriorityFeePerGas`, and on BNB Smart Chain the directory's
    /// endpoints answered 0.05 (most), 0.1 (zan, blockrazor), 1 (48.club,
    /// sentio) and 3 gwei (swiftnodes) while the blocks' median p25 / p50 /
    /// p70 read 0.05 / 0.050000001 / 0.057 gwei (2026-10-09). With the node's
    /// answer as an unconditional floor an executor reading 1 gwei signed
    /// `slow` at 1 gwei — and held every send quoted at 0.05. Above the median
    /// block's tip an answer is that node's opinion; every node now reads the
    /// same tiers. On Ethereum the nodes answered 0 or 10,890 wei over blocks
    /// paying 0.0011 / 0.05 / 0.1 gwei: both below the median, both moot.
    #[test]
    fn a_node_tip_above_what_the_median_block_paid_is_no_floor() {
        let bsc = [[50_000_000, 50_000_001, 57_000_000]; 134];
        for ratio in [None, Some(1_900), Some(5_000)] {
            let reading = |node: u128| TierTips::from_window(&window(&bsc, ratio), node).unwrap();
            let honest = reading(50_000_000);
            for node in [100_000_000, GWEI, 3 * GWEI] {
                assert_eq!(
                    reading(node),
                    honest,
                    "BSC node {node} wei, ratio {ratio:?}"
                );
            }
            assert_eq!(honest.slow, 50_000_000);
            assert_eq!(honest.floor, 50_000_000);
        }
        let ethereum = [[1_100_000, 50_000_000, 100_000_000]; 20];
        let reading = |node: u128| TierTips::from_window(&window(&ethereum, None), node).unwrap();
        assert_eq!(reading(0), reading(10_890));
        assert_eq!(
            reading(0),
            TierTips {
                slow: 1_100_000,
                standard: 50_000_000,
                fast: 100_000_000,
                floor: 1_100_000,
            }
        );
        // A window whose median block paid no tip is no evidence against the
        // node (XRPL EVM, 2026-10-09: blocks almost empty, the node 0.2141
        // gwei): it is believed, and is the floor, as before.
        let xrpl = [[0, 0, 0], [0, 0, 0], [0, 0, 3_000_000]];
        assert_eq!(
            TierTips::from_window(&window(&xrpl, Some(0)), 214_100_000).unwrap(),
            TierTips {
                slow: 214_100_000,
                standard: 214_100_000,
                fast: 214_100_000,
                floor: 214_100_000,
            }
        );
        // An answer at or below the median is still a floor: a chain's
        // enforced minimum is never above what the median block paid.
        let polygon = [[25 * GWEI, 40 * GWEI, 60 * GWEI]; 30];
        assert_eq!(
            TierTips::from_window(&window(&polygon, None), 30 * GWEI)
                .unwrap()
                .slow,
            30 * GWEI
        );
    }

    /// Review F5: where blocks have room for every transaction paying the
    /// chain's minimum, the reward percentiles are a few bots' bids. Measured
    /// 2026-10-09: Polygon's blocks paid 265.7 / 286.6 / 288.7 gwei at the
    /// tier percentiles against the node's 30 on blocks ~19% full; Gnosis's
    /// 70th percentile paid 1.5 gwei over an 8-wei base fee; Avalanche's
    /// 2.497 / 2.639 / 6.241 gwei on blocks 3% full. A quiet window signs the
    /// node's tip scaled 1.00 / 1.25 / 2.00 — bor's 30 gwei minimum kept on
    /// Polygon — and `fast` still bids twice `slow`. Ethereum, whose base
    /// fee targets half-full blocks, never reads as quiet: its lowest 20-block
    /// average over 10.4 days was 0.338.
    #[test]
    fn a_quiet_window_signs_the_nodes_tip_and_a_busy_one_the_percentiles() {
        let polygon = [[265_700_000_000, 286_600_000_000, 288_700_000_000]; 30];
        let quiet = TierTips::from_window(&window(&polygon, Some(1_900)), 30 * GWEI).unwrap();
        assert_eq!(
            quiet,
            TierTips {
                slow: 30 * GWEI,
                standard: 37_500_000_000,
                fast: 60 * GWEI,
                floor: 30 * GWEI,
            }
        );
        let busy = TierTips::from_window(&window(&polygon, Some(4_500)), 30 * GWEI).unwrap();
        assert_eq!(
            busy,
            TierTips {
                slow: 265_700_000_000,
                standard: 286_600_000_000,
                fast: 288_700_000_000,
                floor: 265_700_000_000,
            }
        );
        // Just above the quiet line, a payment quoted in the quiet window a
        // few blocks earlier may be shaved to the quiet `slow` rather than
        // held.
        let near = TierTips::from_window(&window(&polygon, Some(3_500)), 30 * GWEI).unwrap();
        assert_eq!(near.slow, 265_700_000_000);
        assert_eq!(near.floor, 30 * GWEI);

        // Gnosis: the node's 1 wei, lifted to the 0.001 gwei least tip of a
        // window that paid any; `fast` no longer 1.5 gwei, and `standard` no
        // more than `slow` where the median block paid 2 wei.
        let gnosis = [[1, 2, 1_500_000_000]; 20];
        assert_eq!(
            TierTips::from_window(&window(&gnosis, Some(1_800)), 1).unwrap(),
            TierTips {
                slow: MIN_POSITIVE_TIP,
                standard: MIN_POSITIVE_TIP,
                fast: 2 * MIN_POSITIVE_TIP,
                floor: MIN_POSITIVE_TIP,
            }
        );
        // Avalanche: the node's 150 wei, likewise.
        let avalanche = [[2_497_000_000, 2_639_000_000, 6_241_000_000]; 60];
        assert_eq!(
            TierTips::from_window(&window(&avalanche, Some(330)), 150).unwrap(),
            TierTips {
                slow: MIN_POSITIVE_TIP,
                standard: 1_250_000,
                fast: 2 * MIN_POSITIVE_TIP,
                floor: MIN_POSITIVE_TIP,
            }
        );
        // Each faster tier still bids more, and a node answer the blocks
        // contradict is not believed in a quiet window either (BSC's 1 gwei).
        let bsc = [[50_000_000, 50_000_001, 57_000_000]; 134];
        let bsc_quiet = TierTips::from_window(&window(&bsc, Some(1_900)), GWEI).unwrap();
        assert_eq!(bsc_quiet.slow, 50_000_000);
        assert!(bsc_quiet.slow < bsc_quiet.standard && bsc_quiet.standard < bsc_quiet.fast);
        // ...and a faster tier never bids more than its blocks paid at its
        // percentile: the node's 0.05 gwei scaled would be 0.0625 / 0.1 gwei,
        // and BSC's blocks pay 0.05 / 0.057 there.
        assert_eq!(
            TierTips::from_window(&window(&bsc, Some(1_900)), 50_000_000).unwrap(),
            TierTips {
                slow: 50_000_000,
                standard: 50_000_001,
                fast: 57_000_000,
                floor: 50_000_000,
            }
        );
        // Ethereum's quietest minute is busy: the same tips. (Inside the
        // near-quiet band only the floor a short payment may be shaved to
        // drops, to the quiet `slow`.)
        let ethereum = [[1_100_000, 50_000_000, 100_000_000]; 20];
        let quietest = TierTips::from_window(&window(&ethereum, Some(3_380)), 0).unwrap();
        let busy = TierTips::from_window(&window(&ethereum, None), 0).unwrap();
        assert_eq!(
            (quietest.slow, quietest.standard, quietest.fast),
            (busy.slow, busy.standard, busy.fast)
        );
        assert_eq!(quietest.floor, MIN_POSITIVE_TIP);
        assert_eq!(UNCONGESTED_GAS_USED_RATIO_BPS, 3_000);
    }

    #[test]
    fn the_window_reads_its_blocks_fullness_and_ignores_a_column_it_cannot_read() {
        let history = |ratios: serde_json::Value| -> FeeHistory {
            serde_json::from_value(json!({
                "baseFeePerGas": ["0x1", "0x1"],
                "gasUsedRatio": ratios,
                "reward": [["0x1", "0x2", "0x3"]],
            }))
            .unwrap()
        };
        assert_eq!(
            history(json!([0.1, 0.2, 0.30001])).mean_gas_used_ratio_bps(),
            Some(2_000)
        );
        assert_eq!(history(json!([])).mean_gas_used_ratio_bps(), None);
        assert_eq!(history(json!([0.1, "0.2"])).mean_gas_used_ratio_bps(), None);
        assert_eq!(history(json!([0.1, 1.5])).mean_gas_used_ratio_bps(), None);
        // A column of strings is no ratio, not a broken fee history.
        let window = history(json!(["x"])).window().unwrap();
        assert_eq!(window.gas_used_ratio_bps, None);
        assert_eq!(window.rewards, vec![vec![1, 2, 3]]);
        assert_eq!(
            tip_window(&json!({ "baseFeePerGas": ["0x1"], "gasUsedRatio": [0.5] })),
            None
        );
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
                floor: 40,
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
                floor: 10_000_000,
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
                    floor: 147_320_634,
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
                    floor: tip,
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
                    floor: 147_320_634,
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
                    floor: 100,
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
