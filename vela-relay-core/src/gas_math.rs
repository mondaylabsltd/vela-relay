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

/// One reported tier of `pimlico_getUserOperationGasPrice`. Four numbers,
/// exactly one meaning each — built by [`tier_price`], which is where the
/// arithmetic and its reasoning live.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GasPrice {
    /// The cap the relay will actually submit this tier at:
    /// `base_fee_bps × base fee + tip[tier]`, identical to [`tier_outer_fee`].
    pub max_fee_per_gas: u128,
    /// The tip the relay will actually **sign** this tier with:
    /// `tip_bps × market tip`. Reported rather than echoing the raw market
    /// tip, because a builder orders by the effective tip and a quote that
    /// showed one tip while the relay signed another would charge the wallet
    /// for priority it never bought. See [`tier_tip`]; the market tip itself
    /// is [`market_tip`], the one resolution the executor signs from too.
    pub max_priority_fee_per_gas: u128,
    /// What the client must reimburse against — `R` in `docs/fees.md` §3.
    /// `0.6 × multiplier × base fee + tip`.
    pub network_fee_per_gas: u128,
    /// `max_fee_per_gas − network_fee_per_gas`: the inclusion headroom the cap
    /// holds above the reimbursement basis. Reported rather than left to be
    /// inferred — vela-core's `accept_bundler_quote` otherwise derives it by
    /// subtraction, so reporting it costs nothing and names the number.
    pub relayer_fee_per_gas: u128,
}

/// The raw market a quote is derived from: a base fee and a tip, kept apart.
///
/// Keeping them apart is the fix. The old "network price" collapsed them into
/// a single `1.2 × base + tip` number and threw the base fee away, so nothing
/// downstream could compute a per-tier price — which is why every tier of
/// `pimlico_getUserOperationGasPrice` reported the same fee to the wallet.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkGasPrice {
    pub base_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
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

/// The part of an `eth_feeHistory` answer the quote reads: the base fees.
///
/// The `reward` column is deliberately NOT read. Its median was the quote's
/// tip until 2026-09-21, and it is not the tip the relay signs with — see
/// [`market_tip`]. Unknown fields are ignored, so the column may still arrive.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistory {
    pub base_fee_per_gas: Vec<String>,
}

impl FeeHistory {
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

/// The next block's base fee `eth_feeHistory` reported, paired with the tip
/// the caller resolved ([`quote_market_tip`]). No multiplier is applied here
/// any more: the base fee is carried raw so each tier can scale it by its own
/// cap multiplier.
pub fn price_from_fee_history(
    fee_history: &FeeHistory,
    priority_fee: u128,
) -> Result<NetworkGasPrice, GasPriceError> {
    Ok(NetworkGasPrice {
        base_fee_per_gas: fee_history.next_block_base_fee()?,
        max_priority_fee_per_gas: priority_fee,
    })
}

/// The three tiers of `pimlico_getUserOperationGasPrice`, each one derived
/// from the cap the relay would submit it at. One definition, shared with
/// [`SubmissionTier`]/[`tier_outer_fee`]: the tier a client NAMES on
/// `eth_sendUserOperation` and the tier a client is PRICED for are now the
/// same thing.
pub fn tiers(network_price: NetworkGasPrice) -> Result<GasPriceTiers, GasPriceError> {
    let price = |tier| {
        tier_price(
            tier,
            network_price.base_fee_per_gas,
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
/// resolves the name against the base fee and tip it reads at submit time, so
/// the worst a stale quote can do is buy a speed the reimbursement no longer
/// funds — which [`crate::settlement::decide_submission_fees`] then clamps
/// away.
///
/// A tier owns **two** basis-point tables, [`SubmissionTier::base_fee_bps`]
/// (cap headroom) and [`SubmissionTier::tip_bps`] (priority), plus
/// [`SubmissionTier::network_fee_bps`] derived from the first. They live on
/// one enum precisely so they cannot be edited apart: a change to what a tier
/// costs that forgot what it buys is the defect this module was rebuilt for.
///
/// Ordered `Slow < Standard < Fast` so a bundle can submit at the fastest
/// speed any of its operations asked for.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionTier {
    Slow,
    /// The pace the relay keeps on its own, and so the default a bundle
    /// member that named nothing counts as. Its cap is [`quoted_outer_fee`]'s
    /// `2 × base`; its tip is `1.25 ×` the market tip, so naming `standard`
    /// is no longer byte-identical to naming nothing — **naming nothing is**,
    /// and stays so: `settlement::decide_submission_fees` returns `None` and
    /// not one line of tier arithmetic runs.
    #[default]
    Standard,
    Fast,
}

/// The share of a tier's submit cap the client reimburses against — the
/// reported `networkFeePerGas`, `R` in `docs/fees.md` §3.
///
/// **Where 0.6 comes from, and why the whole design hangs on it.** Before
/// this, the relay reported ONE network price (`base_fee_multiplier = 120` →
/// `1.2 × base`) and submitted at ONE cap (`2 × base`, [`quoted_outer_fee`]).
/// Neither number mattered on its own; their RATIO did:
///
/// ```text
/// 1.2 × base / 2.0 × base = 0.6
/// ```
///
/// because the client pays `INBAND_MARKUP = 3 × gas × R` while the relay
/// requires `settlement markup = 1.4 × gas × cap`. Hold the ratio at 0.6 and
/// every payment funds its own cap with the same margin:
///
/// ```text
/// 3 × (0.6 × cap)     1.8
/// ───────────────  =  ───  =  1.286     → +29% headroom, at every tier
///    1.4 × cap        1.4
/// ```
///
/// So the ratio, applied per tier, is what makes a tier mean something
/// without changing the relationship the fee contract was built on. `fast`
/// funds a `3 × base` cap with the same +29% band that `standard` funds a
/// `2 × base` one with, and `standard`'s base-fee term is `0.6 × 2.0 = 1.2 ×
/// base` — byte for byte the base-fee part of the single price this relay has
/// always reported.
///
/// **The 0.6 is applied to the base-fee term ONLY.** The tier's own tip
/// ([`SubmissionTier::tip_bps`]) is carried whole into `R`, exactly as it is
/// carried whole into the cap, so the two sides share it and
///
/// ```text
/// 3R − 1.4 cap  =  1.6 × tip[tier]  +  0.4 × base_fee_bps × base  ≥  0
/// ```
///
/// for every tier and every market — the funding property holds by
/// construction rather than by a table of numbers that happens to work.
/// 1.286 is the FLOOR of that margin: on a chain whose base fee is zero (BSC)
/// the price is all tip, `R = cap`, and the margin is the full `3/1.4 = 2.143`.
pub const REIMBURSEMENT_BASIS_BPS: u64 = 6_000;

impl SubmissionTier {
    /// Basis points of the base fee this tier caps the outer transaction at.
    /// `1.5 / 2.0 / 3.0 × base fee`. **This lever buys spike resilience, not
    /// priority** — see [`SubmissionTier::tip_bps`] for the one that buys
    /// speed, and [`tier_outer_fee`] for why a tier needs both.
    pub const fn base_fee_bps(self) -> u64 {
        match self {
            Self::Slow => 15_000,
            Self::Standard => 20_000,
            Self::Fast => 30_000,
        }
    }

    /// Basis points of the **market tip** this tier signs the outer
    /// transaction with. `1.0 / 1.25 / 2.0 × market tip`.
    ///
    /// **This is the lever that actually buys speed.** An EIP-1559 block
    /// builder orders by the *effective* tip,
    /// `min(maxPriorityFeePerGas, maxFeePerGas − baseFee)`. Scaling only the
    /// cap leaves that number identical at every tier, so `fast` bought
    /// nothing — proved on a real Polygon `fast` receipt whose 3× cap was
    /// applied correctly (`775.525 = 3 × 248.392 + 30.35` gwei) while its
    /// `maxPriorityFeePerGas` was the bare market `30.35` gwei and it paid the
    /// builder exactly what `slow` would have. Both levers now move together.
    ///
    /// **`Slow` is 10_000 bps — the market tip, never less — and that floor is
    /// load-bearing.** This relay has no per-chain minimum-tip knowledge: it
    /// takes whatever the node's `eth_maxPriorityFeePerGas` reports
    /// ([`market_tip`], shared by the quote and the executor). That answer
    /// respects the minimum a chain enforces in its client — bor on Polygon
    /// is the canonical one — and a transaction tipping under that minimum
    /// is rejected outright rather than merely mined late. A `Slow` that shaved
    /// the tip would therefore buy a high rejection rate, not a saving.
    /// `Slow` earns its discount from the lower CAP (and so from the lower
    /// reimbursement basis [`SubmissionTier::network_fee_bps`] derives from
    /// it), never from underpaying the builder. Do not "simplify" this to a
    /// tip below 10_000 bps without first giving the relay a per-chain
    /// minimum-tip source.
    pub const fn tip_bps(self) -> u64 {
        match self {
            Self::Slow => 10_000,
            Self::Standard => 12_500,
            Self::Fast => 20_000,
        }
    }

    /// Basis points of the base fee a client pricing THIS tier must reimburse
    /// against: `0.6 × base_fee_bps` → `0.9 / 1.2 / 1.8 × base fee`.
    ///
    /// Derived from the cap rather than tabulated beside it, so the two can
    /// never be edited apart. See [`REIMBURSEMENT_BASIS_BPS`] for why 0.6.
    /// The 0.6 applies to the **base-fee term only**: the tier's tip rides
    /// into `R` whole (see [`tier_network_fee`]), which is what keeps the
    /// funding property true at every tier by construction.
    pub const fn network_fee_bps(self) -> u64 {
        self.base_fee_bps() * REIMBURSEMENT_BASIS_BPS / 10_000
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

/// The tip this tier signs with: `tip_bps × market tip`, rounded **up**.
///
/// The tip is what a builder orders by, so this — not the cap — is what a
/// client buys when it names a speed. Rounding up keeps `Standard`'s ×1.25
/// from collapsing onto `Slow` at tiny tips, and one extra wei of tip is
/// carried whole into the reimbursement basis ([`tier_network_fee`]) as well
/// as into the cap, so it can never be a wei the relay eats.
///
/// `Slow` is 10_000 bps, so this returns the market tip **unchanged and
/// unrounded** — the floor described on [`SubmissionTier::tip_bps`].
///
/// `None` on overflow.
pub fn tier_tip(tier: SubmissionTier, market_tip: u128) -> Option<u128> {
    let denominator = U256::from(10_000u64);
    let product = U256::from(market_tip).checked_mul(U256::from(tier.tip_bps()))?;
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
/// maxPriorityFeePerGas = tip_bps[tier] × market tip            1.0 / 1.25 / 2.0 ×
/// maxFeePerGas         = base_fee_bps[tier] × base + that tip  1.5 / 2.0  / 3.0 × base + tip
/// ```
///
/// **A tier is two levers, and it has to be.** The cap alone buys resilience
/// to a base-fee spike; only the tip buys priority, because a builder orders
/// by `min(maxPriorityFeePerGas, maxFeePerGas − baseFee)` and that number is
/// unchanged by any cap above `base + tip`. Returning both from one function,
/// as one struct, is what stops a caller assigning the cap and leaving the
/// market tip in place — which is precisely the bug a real Polygon `fast`
/// receipt caught. It also makes a zero-base-fee chain differentiate at last:
/// with `base = 0` the cap collapses to the tip, so scaling the tip is the
/// *only* thing that can tell BSC's three tiers apart.
///
/// **`maxFeePerGas ≥ base + tip` always holds here**, for every tier: the
/// smallest multiple is `Slow`'s 1.5×, and `floor(1.5 b) ≥ b` for every
/// `b ≥ 0`. [`OuterFee::delivers_full_tip_at`] states it; the clamps in
/// `settlement::decide_submission_fees` are the ones that must preserve it.
///
/// A higher cap is not a higher cost — the chain only ever charges
/// `base fee + effective tip`. A higher tip *is* a higher cost, and that is
/// the point: it is also the reimbursement basis the client pays against
/// ([`tier_network_fee`]), so `fast` funds the priority it buys.
///
/// The basis-point scale is applied through a widening multiply so the
/// intermediate never decides the answer. The base-fee division truncates,
/// matching `settlement::inclusion_floor_fee_per_gas`, so `Slow` and the
/// default 1.5× floor land on the same wei rather than a rounding step apart.
///
/// `None` on overflow — the caller then keeps the relay's own pace, so an
/// unpriceable market costs a client its requested speed and nothing else.
pub fn tier_outer_fee(
    tier: SubmissionTier,
    base_fee_per_gas: u128,
    market_tip: u128,
) -> Option<OuterFee> {
    let tip = tier_tip(tier, market_tip)?;
    let scaled = U256::from(base_fee_per_gas).checked_mul(U256::from(tier.base_fee_bps()))?
        / U256::from(10_000u64);
    Some(OuterFee {
        max_fee_per_gas: u128::try_from(scaled).ok()?.checked_add(tip)?,
        max_priority_fee_per_gas: tip,
    })
}

/// What a client pricing this tier must reimburse against — the reported
/// `networkFeePerGas`, `R`: `0.6 × base_fee_bps × base fee + tip[tier]`.
///
/// **The 0.6 applies to the base-fee term only; the tier's tip is carried
/// WHOLE.** Three reasons, and all are load-bearing:
///
/// - it is what makes the funding property hold **by construction** at every
///   tier. With the same `tip[tier]` on both sides,
///   `3R − 1.4 cap = 1.6 × tip[tier] + (1.8 − 1.4) × base_fee_bps × base ≥ 0`
///   — no tier can buy a speed its own payment has not funded, whatever the
///   market's base/tip mix;
/// - `0.6 × 2.0 = 1.2`, so `Standard`'s base-fee term is the very number the
///   old single `base_fee_multiplier = 120` produced, to the wei. (Its tip
///   term is now `1.25 ×` the market tip, so `standard` is no longer byte-
///   identical to naming no tier at all — naming nothing still signs the raw
///   market tip at a `2 × base` cap, exactly as it always has.)
/// - it keeps a tip-dominated chain sane. On BSC `baseFeePerGas` is 0 and the
///   whole price is the tip: scaling the tip down would quote a reimbursement
///   below the fee the chain actually charges, and every send would fall
///   short. Carried whole, `R = cap = tip[tier]` there — and because
///   `tip[tier]` now varies, BSC's three tiers finally differ.
///
/// The same widening multiply as [`tier_outer_fee`] keeps the intermediate
/// from deciding the answer, but this division rounds **up** where the cap's
/// rounds down — reimbursement rounding goes toward the relay, in the house
/// style of `settlement::mul_div_ceil`, and rounding up is also what makes
/// `Standard`'s base-fee term reproduce `div_ceil(base × 120, 100)` exactly.
///
/// `None` on overflow.
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
        .checked_add(tier_tip(tier, market_tip)?)
}

/// The whole reported row for one tier: the cap, the tip, the reimbursement
/// basis, and the headroom between the first and the third.
///
/// ```text
/// maxPriorityFeePerGas = tip_bps × market tip              1.0 / 1.25 / 2.0 × tip
/// maxFeePerGas       = base_fee_bps × base + that tip      1.5 / 2.0 / 3.0 × base + tip[tier]
/// networkFeePerGas   = 0.6 × base_fee_bps × base + that tip  0.9 / 1.2 / 1.8 × base + tip[tier]
/// relayerFeePerGas   = maxFeePerGas − networkFeePerGas     = 0.4 × base_fee_bps × base
/// ```
///
/// **The reported `maxPriorityFeePerGas` is the tip the relay will sign
/// with** — [`tier_outer_fee`] produces both, here and in
/// `settlement::decide_submission_fees`, from the same `tip_bps` table. A
/// quote that showed the market tip while the relay signed a scaled one (or
/// the reverse) would charge a wallet for priority it did not get, which is
/// the same defect one layer up.
///
/// The identity `maxFeePerGas = networkFeePerGas + relayerFeePerGas` holds by
/// construction, which is exactly the subtraction vela-core's
/// `accept_bundler_quote` performs when a bundler omits `relayerFeePerGas`.
/// The subtraction can never go negative — the tip terms cancel and
/// `ceil(0.6 m b) ≤ floor(m b)` for every base fee — and is saturating anyway
/// so a future edit cannot wrap it.
///
/// `None` on overflow, so an unpriceable market yields no quote rather than a
/// wrong one.
pub fn tier_price(
    tier: SubmissionTier,
    base_fee_per_gas: u128,
    market_tip: u128,
) -> Option<GasPrice> {
    let outer = tier_outer_fee(tier, base_fee_per_gas, market_tip)?;
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
/// value — `tip[tier]` — so the three tiers are `1.0 / 1.25 / 2.0 ×` the
/// reading. There is no base-fee headroom to sell here, but priority is still
/// for sale, because the tip is the whole price. It is the same shape a
/// zero-base-fee EIP-1559 chain (BSC) already takes, and the reason scaling
/// the tip was the missing half of a tier.
pub fn legacy_price_from_result(result: Value) -> Result<NetworkGasPrice, GasPriceError> {
    let value = result
        .as_str()
        .ok_or(GasPriceError::InvalidUpstreamResponse)
        .and_then(parse_quantity)?;

    Ok(NetworkGasPrice {
        base_fee_per_gas: 0,
        max_priority_fee_per_gas: value,
    })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use serde_json::json;

    use super::{
        FeeHistory, GasPricePolicy, NetworkGasPrice, OuterFee, SubmissionTier,
        fallback_priority_fee, legacy_price_from_result, market_tip, parse_quantity,
        price_from_fee_history, quote_market_tip, quoted_outer_fee, tier_network_fee,
        tier_outer_fee, tier_price, tier_tip, tiers,
    };

    const TIERS: [SubmissionTier; 3] = [
        SubmissionTier::Slow,
        SubmissionTier::Standard,
        SubmissionTier::Fast,
    ];

    /// The market every worked example below is measured against: Polygon
    /// (chain 137) on 2026-09-21, one JSON-RPC batch to `polygon.drpc.org`
    /// (block 94190979) — the last `baseFeePerGas` of `eth_feeHistory` and
    /// `eth_maxPriorityFeePerGas`, exactly as the shell reads them
    /// ([`quote_market_tip`]) and the executor signs from ([`market_tip`]).
    const POLYGON_BASE: u128 = 247_805_843_619; // 247.805843619 gwei, next block
    const POLYGON_TIP: u128 = 27_773_221_947; //   27.773221947 gwei

    /// The rest of the same batch. The latest block's own base fee (the
    /// second-to-last `baseFeePerGas`, and `eth_getBlockByNumber("latest")`'s),
    /// `eth_gasPrice`, and the median of `eth_feeHistory`'s 50th-percentile
    /// reward column — the number the quote used as its tip before
    /// 2026-09-21, 3.1× the tip the relay signs.
    const POLYGON_LATEST_BASE: u128 = 250_761_673_410; // 250.761673410 gwei
    const POLYGON_GAS_PRICE: u128 = 278_534_895_357; //  278.534895357 gwei
    const POLYGON_REWARD_MEDIAN: u128 = 86_071_986_761; // 86.071986761 gwei

    /// That batch's `eth_feeHistory("0x5", "latest", [25, 50, 75])` answer,
    /// verbatim — reward column and all, so a test can show it is ignored.
    fn polygon_fee_history_answer() -> serde_json::Value {
        json!({
            "oldestBlock": "0x59d3d7f",
            "reward": [
                ["0x13dc09ac00", "0x13dc09ac00", "0x22566ac185"],
                ["0x13dc09ac00", "0x13dc09ac00", "0x19304bf0bb"],
                ["0xd3f758004", "0x140a4a4a49", "0x1e449a9400"],
                ["0x12d25d220b", "0x1dc78d901b", "0x29529db4b2"],
                ["0x14a2b33740", "0x16017bd628", "0x1e449a9400"]
            ],
            "baseFeePerGas": [
                "0x3a2533c666", "0x3a6909b679", "0x39c7f52c07",
                "0x3a06b2b0b2", "0x3a628f7ac2", "0x39b26118a3"
            ],
            "gasUsedRatio": [0.19374966875, 0.20186779375, 0.19409771875, 0.20936705, 0.1596438875]
        })
    }

    fn polygon_fee_history() -> FeeHistory {
        serde_json::from_value(polygon_fee_history_answer()).unwrap()
    }

    /// A fee history holding just the two base fees the relay reads:
    /// `[latest block's, next block's]`.
    fn fee_history(latest_block_base_fee: u128, next_block_base_fee: u128) -> FeeHistory {
        serde_json::from_value(json!({
            "baseFeePerGas": [
                format!("0x{latest_block_base_fee:x}"),
                format!("0x{next_block_base_fee:x}"),
            ],
        }))
        .unwrap()
    }

    /// What the quote reports as `tier`'s `maxPriorityFeePerGas`, given the
    /// node's answers — the shell's flow in `GasPriceManager::eip1559_price`
    /// and the Worker's `arms::gas_price::eip1559_price`, minus transport.
    fn quoted_tip(
        tier: SubmissionTier,
        fee_history: &FeeHistory,
        max_priority_fee_per_gas: Option<u128>,
        gas_price: Option<u128>,
    ) -> u128 {
        let tip = quote_market_tip(
            fee_history,
            max_priority_fee_per_gas,
            // Fetched only when the node named no tip, as both shells do.
            max_priority_fee_per_gas
                .is_none()
                .then_some(gas_price)
                .flatten(),
            GasPricePolicy::default().priority_fee_divisor,
        )
        .unwrap();
        let row = tiers(price_from_fee_history(fee_history, tip).unwrap()).unwrap();
        match tier {
            SubmissionTier::Slow => row.slow,
            SubmissionTier::Standard => row.standard,
            SubmissionTier::Fast => row.fast,
        }
        .max_priority_fee_per_gas
    }

    /// What the executor signs `tier` with, given the same answers — the
    /// docker engine's and the Worker lane's `transaction_context`, then
    /// `settlement::decide_submission_fees`, whose tip is `tier_outer_fee`'s
    /// and is never clamped. `None` where the executor refuses to submit.
    fn signed_tip(
        tier: SubmissionTier,
        latest_block_base_fee: u128,
        max_priority_fee_per_gas: Option<u128>,
        gas_price: Option<u128>,
    ) -> Option<u128> {
        let tip = market_tip(
            max_priority_fee_per_gas.map(U256::from),
            max_priority_fee_per_gas
                .is_none()
                .then_some(gas_price)
                .flatten()
                .map(U256::from),
            U256::from(latest_block_base_fee),
        )?;
        let tip = u128::try_from(tip).unwrap();
        Some(
            tier_outer_fee(tier, latest_block_base_fee, tip)
                .unwrap()
                .max_priority_fee_per_gas,
        )
    }

    /// The **receipt** that proved the defect: a `fast` send mined on Polygon.
    ///
    /// ```text
    /// Base:  250.710118904 Gwei | Max: 775.52505875 Gwei | Max Priority: 30.35 Gwei
    /// Gas Price (effective): 281.060118904 Gwei  (= base + 30.35)
    /// ```
    ///
    /// `775.52505875 = 3 × 248.39168625 + 30.35`, so the 3× CAP was applied
    /// exactly right — and `maxPriorityFeePerGas` was the bare market tip.
    /// The builder saw `min(30.35, 775.525 − 250.710) = 30.35` gwei, the same
    /// number a `slow` send would have offered. That is the whole defect in
    /// one receipt, and `a_fast_send_now_outbids_the_slow_one_on_the_receipt_that_proved_the_defect`
    /// is where the fix is measured against it.
    const RECEIPT_BASE: u128 = 250_710_118_904; // 250.710118904 gwei, at inclusion
    const RECEIPT_TIP: u128 = 30_350_000_000; //   30.35 gwei, the signed tip
    const RECEIPT_EFFECTIVE_GAS_PRICE: u128 = 281_060_118_904; // 281.060118904 gwei

    #[test]
    fn pairs_the_next_blocks_base_fee_with_the_resolved_tip_without_scaling_either() {
        let fee_history: FeeHistory = serde_json::from_value(json!({
            "baseFeePerGas": ["0x50", "0x64"],
            "reward": [["0x1", "0xa", "0x14"]]
        }))
        .unwrap();

        // The LAST entry is the next block's projected base fee, and it is
        // carried raw: applying a multiplier here is what used to throw the
        // base fee away and leave every tier with the same price. The tip is
        // whatever the caller resolved — never the reward column.
        assert_eq!(
            price_from_fee_history(&fee_history, 7).unwrap(),
            NetworkGasPrice {
                base_fee_per_gas: 100,
                max_priority_fee_per_gas: 7,
            }
        );
    }

    #[test]
    fn the_market_tip_is_the_nodes_own_answer_and_only_then_the_gas_price_derivation() {
        let base = U256::from(POLYGON_LATEST_BASE);
        let gas_price = U256::from(POLYGON_GAS_PRICE);

        // 1. `eth_maxPriorityFeePerGas` wins whenever it answered, and the
        //    gas price is then never consulted — passing it changes nothing.
        assert_eq!(
            market_tip(Some(U256::from(POLYGON_TIP)), None, base),
            Some(U256::from(POLYGON_TIP))
        );
        assert_eq!(
            market_tip(Some(U256::from(POLYGON_TIP)), Some(U256::from(1u64)), base),
            Some(U256::from(POLYGON_TIP))
        );
        // Zero is an answer, not an absence (Arbitrum reports `0x0`); the
        // executor has always signed it, so the quote reports it too.
        assert_eq!(
            market_tip(Some(U256::ZERO), Some(gas_price), base),
            Some(U256::ZERO)
        );

        // 2. Only with no answer: `eth_gasPrice − the latest block's base
        //    fee`. On the live Polygon batch that recovers the node's own tip
        //    to the wei — which is why it is the fallback, and why it must
        //    subtract the LATEST block's base fee.
        assert_eq!(
            market_tip(None, Some(gas_price), base),
            Some(U256::from(POLYGON_TIP))
        );

        // No tip at all: a gas price under the base fee, or none. The
        // executor refuses here; see `quote_market_tip` for the quote.
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

        // `eth_gasPrice = suggested tip + head base fee`, exactly, on the
        // live batch. Subtracting the NEXT block's projection instead would
        // have derived 30.729 gwei — a 10.6% over-report of a tip the
        // executor, which reads the latest block, would never sign.
        assert_eq!(POLYGON_GAS_PRICE - POLYGON_LATEST_BASE, POLYGON_TIP);
        assert_eq!(POLYGON_GAS_PRICE - POLYGON_BASE, 30_729_051_738);

        // A history of one entry has no second-to-last; an empty one has
        // nothing to read at all.
        let single: FeeHistory =
            serde_json::from_value(json!({ "baseFeePerGas": ["0x64"] })).unwrap();
        assert_eq!(single.latest_block_base_fee().unwrap(), 100);
        assert_eq!(single.next_block_base_fee().unwrap(), 100);
        let empty: FeeHistory = serde_json::from_value(json!({ "baseFeePerGas": [] })).unwrap();
        assert!(empty.latest_block_base_fee().is_err());
        assert!(empty.next_block_base_fee().is_err());
        assert!(quote_market_tip(&empty, Some(1), None, 200).is_err());
    }

    #[test]
    fn the_tip_reported_for_a_tier_is_the_tip_the_executor_signs_given_the_same_rpc_answers() {
        // The property the whole quote exists to keep: fed the SAME node
        // answers, the tip `pimlico_getUserOperationGasPrice` reports for a
        // tier and the tip the executor signs that tier with are one wei
        // count. Markets measured 2026-09-21 unless noted.
        type NodeAnswers = (
            &'static str,
            u128,         // the latest block's base fee
            u128,         // the next block's base fee
            Option<u128>, // eth_maxPriorityFeePerGas
            Option<u128>, // eth_gasPrice
        );
        let markets: [NodeAnswers; 8] = [
            (
                "polygon",
                POLYGON_LATEST_BASE,
                POLYGON_BASE,
                Some(POLYGON_TIP),
                Some(POLYGON_GAS_PRICE),
            ),
            // The same batch with `eth_maxPriorityFeePerGas` lost: both
            // paths derive the tip from `eth_gasPrice`, and land on it anyway.
            (
                "polygon, no tip answer",
                POLYGON_LATEST_BASE,
                POLYGON_BASE,
                None,
                Some(POLYGON_GAS_PRICE),
            ),
            // Tips are a zero there. The old quote reported `base / 200`
            // (100_380 wei) — a tip the executor never signed.
            (
                "arbitrum",
                20_076_000,
                20_076_000,
                Some(0),
                Some(20_076_000),
            ),
            (
                "base",
                5_000_000,
                5_000_000,
                Some(1_000_000),
                Some(6_000_000),
            ),
            // `eth_maxPriorityFeePerGas` timed out on the public endpoint;
            // `eth_gasPrice − base` is op-geth's 0.001 gwei minimum.
            ("optimism", 584, 584, None, Some(1_000_584)),
            // The node suggests 27_224 wei; the reward median was 0.2 gwei.
            (
                "ethereum",
                262_374_330,
                249_829_334,
                Some(27_224),
                Some(262_401_554),
            ),
            (
                "bsc shape: no base fee",
                0,
                0,
                Some(50_000_000),
                Some(50_000_000),
            ),
            ("a rising base fee", 100, 112, None, Some(107)),
        ];

        for (chain, latest, next, reported, gas_price) in markets {
            let history = fee_history(latest, next);
            for tier in TIERS {
                assert_eq!(
                    Some(quoted_tip(tier, &history, reported, gas_price)),
                    signed_tip(tier, latest, reported, gas_price),
                    "{chain}/{tier}: the quote reported a tip the executor would not sign"
                );
            }
        }

        // The one documented difference: no tip answer and no usable gas
        // price. The executor refuses to submit — there is no signed tip to
        // agree with — and only the quote falls back, to `base / 200` on the
        // next block's base fee.
        for gas_price in [None, Some(POLYGON_LATEST_BASE - 1)] {
            for tier in TIERS {
                assert_eq!(signed_tip(tier, POLYGON_LATEST_BASE, None, gas_price), None);
            }
            assert_eq!(
                quote_market_tip(
                    &polygon_fee_history(),
                    None,
                    gas_price,
                    GasPricePolicy::default().priority_fee_divisor
                ),
                Ok(fallback_priority_fee(POLYGON_BASE, 200))
            );
        }
    }

    #[test]
    fn the_polygon_quote_reports_the_tip_the_executor_signed_not_the_fee_history_median() {
        // The defect, measured on Polygon on 2026-09-21. The quote reported
        // `standard` at maxFeePerGas 600.7 / maxPriorityFeePerGas 107.7 gwei
        // (slow 86.2, fast 172.4 — 1.00 / 1.25 / 2.00 × an ~86.2 gwei
        // fee-history median), while the mined outer transaction signed:
        //
        //   Max Priority: 34.71688084 Gwei   (1.25 × eth_maxPriorityFeePerGas)
        //   Max:          536.5544999 Gwei   (2 × base + that tip)
        //
        // The wallet was shown, and priced its reimbursement `R` on, a tip
        // 3.1× the one the relay paid the builder.
        let history = polygon_fee_history();

        // The fixture's reward column really is what the old quote read:
        // the median of the 50th-percentile column.
        let mut column = polygon_fee_history_answer()["reward"]
            .as_array()
            .unwrap()
            .iter()
            .map(|percentiles| parse_quantity(percentiles[1].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>();
        column.sort_unstable();
        assert_eq!(column[column.len() / 2], POLYGON_REWARD_MEDIAN);

        // Now the quote reads the node's own tip, the reward column is inert,
        // and `standard` reports 1.25 × 27.773221947 gwei, rounded up.
        let tip = quote_market_tip(&history, Some(POLYGON_TIP), None, 200).unwrap();
        assert_eq!(tip, POLYGON_TIP);
        let row = tiers(price_from_fee_history(&history, tip).unwrap()).unwrap();
        assert_eq!(row.slow.max_priority_fee_per_gas, 27_773_221_947);
        assert_eq!(row.standard.max_priority_fee_per_gas, 34_716_527_434);
        assert_eq!(row.fast.max_priority_fee_per_gas, 55_546_443_894);
        // …not the 107.59 gwei the median would have reported for it.
        assert_eq!(
            tier_tip(SubmissionTier::Standard, POLYGON_REWARD_MEDIAN),
            Some(107_589_983_452)
        );
        assert_ne!(row.standard.max_priority_fee_per_gas, 107_589_983_452);
        // The executor, fed the latest block the same batch reported, signs
        // exactly what was quoted.
        assert_eq!(
            signed_tip(
                SubmissionTier::Standard,
                POLYGON_LATEST_BASE,
                Some(POLYGON_TIP),
                None
            ),
            Some(row.standard.max_priority_fee_per_gas)
        );

        // And the receipt itself. Its 34.71688084 gwei is `tier_tip` of
        // exactly one reading, 27.773504672 gwei — the executor's
        // `eth_maxPriorityFeePerGas` at submit time. A quote fed that reading
        // now reports that `standard` tip to the wei.
        let at_submit = 27_773_504_672u128;
        assert_eq!(
            tier_tip(SubmissionTier::Standard, at_submit),
            Some(34_716_880_840)
        );
        assert_eq!(
            quoted_tip(
                SubmissionTier::Standard,
                &history,
                Some(at_submit),
                Some(POLYGON_GAS_PRICE)
            ),
            34_716_880_840
        );
    }

    #[test]
    fn every_tier_reports_the_cap_and_the_tip_it_will_be_submitted_with() {
        // One market, three genuinely different rows, in BOTH levers.
        // `maxFeePerGas` is the cap `tier_outer_fee` would submit at;
        // `maxPriorityFeePerGas` is the tip it would sign with; and
        // `networkFeePerGas` is 0.6 of the cap's base-fee part plus that same
        // tip carried whole.
        let tiers = tiers(NetworkGasPrice {
            base_fee_per_gas: 100,
            max_priority_fee_per_gas: 40,
        })
        .unwrap();

        assert_eq!(tiers.slow.max_fee_per_gas, 190); // 1.5 × 100 + 40
        assert_eq!(tiers.slow.max_priority_fee_per_gas, 40); // 1.00 × 40
        assert_eq!(tiers.slow.network_fee_per_gas, 130); // 0.9 × 100 + 40
        assert_eq!(tiers.slow.relayer_fee_per_gas, 60); // 0.6 × 100

        assert_eq!(tiers.standard.max_fee_per_gas, 250); // 2.0 × 100 + 50
        assert_eq!(tiers.standard.max_priority_fee_per_gas, 50); // 1.25 × 40
        assert_eq!(tiers.standard.network_fee_per_gas, 170); // 1.2 × 100 + 50
        assert_eq!(tiers.standard.relayer_fee_per_gas, 80); // 0.8 × 100

        assert_eq!(tiers.fast.max_fee_per_gas, 380); // 3.0 × 100 + 80
        assert_eq!(tiers.fast.max_priority_fee_per_gas, 80); // 2.00 × 40
        assert_eq!(tiers.fast.network_fee_per_gas, 260); // 1.8 × 100 + 80
        assert_eq!(tiers.fast.relayer_fee_per_gas, 120); // 1.2 × 100

        // `relayerFeePerGas` is purely the base-fee headroom — the tip terms
        // cancel — which is the shape of "0.6 applies to the base fee only".
        for tier in [tiers.slow, tiers.standard, tiers.fast] {
            assert_eq!(
                tier.relayer_fee_per_gas,
                tier.max_fee_per_gas - tier.network_fee_per_gas
            );
        }
    }

    #[test]
    fn the_reported_tip_is_the_tip_the_relay_will_actually_sign_with() {
        // Coherence between the quote and the submission: whatever
        // `pimlico_getUserOperationGasPrice` reports as a tier's
        // `maxPriorityFeePerGas`, `tier_outer_fee` — the function
        // `settlement::decide_submission_fees` resolves the signed pair from
        // — must produce the same wei. Showing the wallet one tip and signing
        // another is the same class of defect as scaling the cap alone.
        for (base, tip) in [
            (POLYGON_BASE, POLYGON_TIP),
            (RECEIPT_BASE, RECEIPT_TIP),
            (45_517_289, 5_757_642),
            (0, 50_000_000), // BSC
            (1_000_000_000, 0),
            (0, 3), // where the ×1.25 ceiling bites
            (7, 1),
        ] {
            for tier in TIERS {
                let quoted = tier_price(tier, base, tip).unwrap();
                let signed = tier_outer_fee(tier, base, tip).unwrap();
                assert_eq!(
                    quoted.max_priority_fee_per_gas, signed.max_priority_fee_per_gas,
                    "{tier} tip at base={base} tip={tip}"
                );
                assert_eq!(
                    quoted.max_fee_per_gas, signed.max_fee_per_gas,
                    "{tier} cap at base={base} tip={tip}"
                );
                // …and `R` carries that same tip whole, so the client is
                // reimbursing against the priority it is buying.
                assert!(
                    quoted.network_fee_per_gas >= signed.max_priority_fee_per_gas,
                    "{tier} basis at base={base} tip={tip}"
                );
            }
        }
    }

    #[test]
    fn the_standard_base_fee_term_is_byte_for_byte_the_price_the_relay_used_to_report() {
        // The anchor the redesign kept: `0.6 × 2.0 = 1.2`, so `standard`'s
        // BASE-FEE term reproduces the single `base_fee_multiplier = 120`
        // price exactly — `div_ceil(base × 120, 100)` — for every base fee,
        // including the ones where the ceiling bites. That is why the
        // network-fee division rounds up while the cap's rounds down.
        //
        // The TIP term no longer matches: `standard` now signs 1.25 × the
        // market tip and reimburses against it, because a tier that did not
        // move the tip bought no priority (see `tip_bps`). Naming NO tier is
        // what remains byte-for-byte unchanged, and
        // `naming_no_speed_leaves_the_relays_own_pace_untouched` in
        // `settlement` is where that is pinned.
        let old_base_term = |base: u128| (base * 120).div_ceil(100);

        for (base, tip) in [
            (POLYGON_BASE, POLYGON_TIP),
            (45_517_289, 5_757_642),   // Ethereum, 2026-09-21
            (536, 57),                 // Optimism, 2026-09-21
            (0, 50_000_000),           // BSC: no base fee at all
            (1, 0),                    // the ceiling's first bite: 1.2 → 2
            (3, 0),                    // 3.6 → 4
            (5, 0),                    // exact: 6
            (u128::MAX / 32_000, 999), // absurd, and still must agree
        ] {
            let standard_tip = tier_tip(SubmissionTier::Standard, tip).unwrap();
            assert_eq!(
                tier_network_fee(SubmissionTier::Standard, base, tip),
                Some(old_base_term(base) + standard_tip),
                "base={base} tip={tip}"
            );
        }
    }

    #[test]
    fn a_tier_scales_the_tip_as_well_as_the_cap() {
        // The table, stated once: 1.0 / 1.25 / 2.0 on the tip beside
        // 1.5 / 2.0 / 3.0 on the base fee. Both live on `SubmissionTier`, so
        // an edit to one is an edit in the same `match` as the other.
        assert_eq!(SubmissionTier::Slow.tip_bps(), 10_000);
        assert_eq!(SubmissionTier::Standard.tip_bps(), 12_500);
        assert_eq!(SubmissionTier::Fast.tip_bps(), 20_000);
        assert_eq!(SubmissionTier::Slow.base_fee_bps(), 15_000);
        assert_eq!(SubmissionTier::Standard.base_fee_bps(), 20_000);
        assert_eq!(SubmissionTier::Fast.base_fee_bps(), 30_000);

        let tip = 40_000_000_000u128;
        assert_eq!(tier_tip(SubmissionTier::Slow, tip), Some(40_000_000_000));
        assert_eq!(
            tier_tip(SubmissionTier::Standard, tip),
            Some(50_000_000_000)
        );
        assert_eq!(tier_tip(SubmissionTier::Fast, tip), Some(80_000_000_000));

        // `Slow` is the market tip EXACTLY — never scaled, never rounded.
        // The relay has no per-chain minimum-tip knowledge, and a tip under a
        // chain's enforced minimum (bor on Polygon) is rejected outright, so
        // `slow` must save on the cap and not on the builder's fee.
        for tip in [0u128, 1, 3, 7, POLYGON_TIP, RECEIPT_TIP, u128::MAX] {
            assert_eq!(tier_tip(SubmissionTier::Slow, tip), Some(tip), "tip={tip}");
        }

        // The ×1.25 rounds up, so it cannot silently collapse onto `Slow`.
        assert_eq!(tier_tip(SubmissionTier::Standard, 3), Some(4));
        assert_eq!(tier_tip(SubmissionTier::Standard, 4), Some(5));
        // …and a tip large enough to overflow the doubling refuses rather
        // than wraps.
        assert_eq!(tier_tip(SubmissionTier::Fast, u128::MAX), None);
        assert_eq!(tier_outer_fee(SubmissionTier::Fast, 0, u128::MAX), None);
    }

    #[test]
    fn a_fast_send_now_outbids_the_slow_one_on_the_receipt_that_proved_the_defect() {
        // The defect, in the numbers the chain reported. The mined `fast`
        // transaction carried a correct 3× cap and the bare market tip, and
        // the builder was paid `base + 30.35` gwei — which is, to the wei,
        // what `slow` offers under the fixed arithmetic. Paying for `fast`
        // bought nothing.
        let fees = TIERS.map(|tier| {
            (
                tier,
                tier_outer_fee(tier, RECEIPT_BASE, RECEIPT_TIP).unwrap(),
            )
        });

        // The signed tip per tier: 30.35 / 37.9375 / 60.70 gwei.
        assert_eq!(fees[0].1.max_priority_fee_per_gas, 30_350_000_000);
        assert_eq!(fees[1].1.max_priority_fee_per_gas, 37_937_500_000);
        assert_eq!(fees[2].1.max_priority_fee_per_gas, 60_700_000_000);
        // …and the cap that carries it: 406.415 / 539.358 / 812.830 gwei.
        assert_eq!(fees[0].1.max_fee_per_gas, 406_415_178_356);
        assert_eq!(fees[1].1.max_fee_per_gas, 539_357_737_808);
        assert_eq!(fees[2].1.max_fee_per_gas, 812_830_356_712);

        // What a block builder orders by. Before the fix this was
        // 30.35 gwei at every tier; now it is the tier's own tip, because
        // every cap clears `base + tip` with room.
        for (tier, fee) in fees {
            assert_eq!(
                fee.effective_tip_at(RECEIPT_BASE),
                fee.max_priority_fee_per_gas,
                "{tier}: the cap truncated its own tip"
            );
            assert!(fee.delivers_full_tip_at(RECEIPT_BASE), "{tier}");
        }
        assert!(
            fees[0].1.effective_tip_at(RECEIPT_BASE) < fees[1].1.effective_tip_at(RECEIPT_BASE)
                && fees[1].1.effective_tip_at(RECEIPT_BASE)
                    < fees[2].1.effective_tip_at(RECEIPT_BASE),
            "a paid-for speed must outbid the cheaper one"
        );

        // The receipt's own `Gas Price (effective)` — 281.060118904 gwei —
        // is exactly `base + slow`'s tip. The transaction the user paid
        // `fast` for got `slow`'s priority, and `fast` now pays 30.35 gwei
        // more to the builder than that.
        assert_eq!(
            RECEIPT_BASE + fees[0].1.effective_tip_at(RECEIPT_BASE),
            RECEIPT_EFFECTIVE_GAS_PRICE
        );
        assert_eq!(
            RECEIPT_BASE + fees[2].1.effective_tip_at(RECEIPT_BASE),
            311_410_118_904
        );
        assert_eq!(
            RECEIPT_BASE + fees[1].1.effective_tip_at(RECEIPT_BASE),
            288_647_618_904
        );
    }

    #[test]
    fn a_cap_can_never_truncate_the_tip_it_is_paired_with() {
        // The invariant `settlement::decide_submission_fees` must preserve
        // through its clamps, proved here for the unclamped pair across the
        // roundings most likely to cross. Below `base + tip` a builder sees
        // less priority than the client bought; below `base` the transaction
        // is not includable at all.
        for base in (0..=64u128).chain([
            POLYGON_BASE,
            RECEIPT_BASE,
            45_517_289,
            536,
            u128::MAX / 32_000,
        ]) {
            for tip in [0u128, 1, 3, 7, 57, POLYGON_TIP, RECEIPT_TIP] {
                for tier in TIERS {
                    let fee = tier_outer_fee(tier, base, tip).unwrap();
                    assert!(
                        fee.delivers_full_tip_at(base),
                        "{tier}: cap {} cannot carry tip {} at base {base}",
                        fee.max_fee_per_gas,
                        fee.max_priority_fee_per_gas
                    );
                    assert!(fee.max_fee_per_gas >= base + fee.max_priority_fee_per_gas);
                }
            }
        }

        // And the helper reports a truncation when there is one, rather than
        // flattering the caller: a cap one wei short of `base + tip` pays the
        // builder one wei less.
        let truncated = OuterFee {
            max_fee_per_gas: 109,
            max_priority_fee_per_gas: 10,
        };
        assert_eq!(truncated.effective_tip_at(100), 9);
        assert!(!truncated.delivers_full_tip_at(100));
        // Below the base fee there is no tip at all, and no inclusion.
        assert_eq!(truncated.effective_tip_at(200), 0);
    }

    #[test]
    fn a_tier_price_splits_its_cap_into_a_basis_and_the_headroom_above_it() {
        // `maxFeePerGas = networkFeePerGas + relayerFeePerGas`, exactly — the
        // subtraction vela-core's `accept_bundler_quote` performs when a
        // bundler omits `relayerFeePerGas`, so reporting it can never
        // disagree with deriving it. Swept over the base fees where the two
        // roundings are most likely to cross.
        for base in (0..=64u128).chain([POLYGON_BASE, RECEIPT_BASE, 45_517_289, 536]) {
            for tip in [0u128, 1, 7, POLYGON_TIP, RECEIPT_TIP] {
                for tier in TIERS {
                    let price = tier_price(tier, base, tip).unwrap();
                    let outer = tier_outer_fee(tier, base, tip).unwrap();
                    assert_eq!(
                        price.max_fee_per_gas, outer.max_fee_per_gas,
                        "{tier} cap at base={base} tip={tip}"
                    );
                    assert_eq!(
                        price.max_priority_fee_per_gas, outer.max_priority_fee_per_gas,
                        "{tier} tip at base={base} tip={tip}"
                    );
                    assert!(
                        price.network_fee_per_gas <= price.max_fee_per_gas,
                        "{tier}: basis {} above cap {} at base={base} tip={tip}",
                        price.network_fee_per_gas,
                        price.max_fee_per_gas
                    );
                    assert_eq!(
                        price.network_fee_per_gas + price.relayer_fee_per_gas,
                        price.max_fee_per_gas,
                        "{tier} at base={base} tip={tip}"
                    );
                    // The TIER's tip is never scaled by the 0.6: it rides
                    // whole into the basis, which is what makes the funding
                    // property hold by construction.
                    assert!(price.network_fee_per_gas >= price.max_priority_fee_per_gas);
                    // …so the headroom is purely the base-fee term, `0.4 × m × base`.
                    assert_eq!(
                        price.relayer_fee_per_gas,
                        (base * u128::from(tier.base_fee_bps()) / 10_000)
                            - (base * u128::from(tier.network_fee_bps())).div_ceil(10_000),
                        "{tier} headroom at base={base} tip={tip}"
                    );
                }
            }
        }
    }

    /// Every market the funding and refusal proofs are measured over,
    /// deliberately including both degenerate shapes: `base = 0` (the whole
    /// price is the tip) and `tip → 0` (the whole price is the base fee).
    /// Each one is the worst case for a different half of the arithmetic.
    const MARKETS: [(&str, u128, u128); 9] = [
        ("polygon", POLYGON_BASE, POLYGON_TIP),
        ("polygon receipt", RECEIPT_BASE, RECEIPT_TIP),
        ("ethereum", 45_517_289, 5_757_642),
        ("bsc", 0, 50_000_000), // tip-dominated: base = 0
        ("base", 5_000_000, 1_000_000),
        // No priority market: `eth_maxPriorityFeePerGas` is `0x0`. (This
        // row used to read tip 100_000 — `base / 200`, the quote's fallback,
        // a tip the executor never signed.)
        ("arbitrum", 20_076_000, 0),
        ("optimism", 536, 57),
        ("zero tip", 1_000_000_000, 0), // base-dominated: tip = 0
        ("one wei tip", 1_000_000_000, 1),
    ];

    #[test]
    fn every_tier_funds_its_own_cap_with_at_least_the_twenty_nine_percent_band() {
        // The property the 0.6 exists for: the client pays `3 × gas × R`, the
        // relay requires `1.4 × gas × cap`, so `3R ≥ 1.4 × cap` must hold at
        // EVERY tier or the client silently buys a speed it has not funded.
        //
        // Scaling the tip does not disturb it, and that is the point of
        // carrying `tip[tier]` whole on both sides rather than tabulating a
        // separate reimbursement tip:
        //
        //   3R − 1.4 cap = 3(0.6·m·base + t) − 1.4(m·base + t)
        //                = 0.4·m·base + 1.6·t   ≥ 0,  term by term.
        //
        // 1.286 is the floor (tip 0); the whole tip in R pushes it higher, to
        // 3/1.4 = 2.143 when the base fee is 0.
        for (chain, base, tip) in MARKETS {
            for tier in TIERS {
                let price = tier_price(tier, base, tip).unwrap();
                let funds = 3 * price.network_fee_per_gas;
                let needs = 14 * price.max_fee_per_gas / 10;
                assert!(
                    funds >= needs,
                    "{chain}/{tier}: 3 × R = {funds} cannot fund 1.4 × cap = {needs}"
                );
                // …and the margin never drops below the exact floor,
                // `3 × 0.6 / 1.4 = 9/7 = 1.2857`, which a zero tip hits dead
                // on: with no tip to carry whole, `R` is exactly `0.6 × cap`.
                assert!(
                    funds * 7 >= needs * 9,
                    "{chain}/{tier}: margin below 9/7 ({funds} against {needs})"
                );
                // The tip-dominated end of the same property: with no base
                // fee, `R = cap` and the margin is the full 3/1.4.
                if base == 0 {
                    assert_eq!(price.network_fee_per_gas, price.max_fee_per_gas);
                }
            }
        }
    }

    #[test]
    fn the_fast_basis_stays_far_inside_the_clients_three_times_chain_refusal() {
        // vela-core refuses a quote with `networkFeePerGas > 3 × C`
        // (`MAX_QUOTE_VS_CHAIN_MULTIPLE`, `GasQuoteTooHigh`) — a refusal the
        // USER sees. `fast` reports the largest R, so it is the one to prove.
        //
        // Arithmetic, with the tip now scaled ×2 at `fast`:
        //   `C = max(eth_gasPrice, base + tip) ≥ base + tip`, and
        //   R[fast] = ceil(1.8 × base) + 2 tip  ≤  1.8 base + 1 + 2 tip
        //           ≤  3 base + 3 tip  =  3 × C   whenever base + tip ≥ 1,
        // since the slack `1.2 base + tip − 1` is ≥ 0 there. R/C is a weighted
        // average of the two pure cases — 1.8 when tip → 0 and 2.0 when
        // base → 0 — so its SUPREMUM is 2.0, up from 1.8 before the tip was
        // scaled, and still a third below the limit of 3. The guard only
        // bites if the relay reads a market more than 1.5× the one the client
        // read moments earlier.
        for (chain, base, tip) in MARKETS {
            let chain_price = base + tip; // the client's `deriveChainGasPrice`
            let reported = tier_price(SubmissionTier::Fast, base, tip)
                .unwrap()
                .network_fee_per_gas;
            assert!(
                reported <= chain_price * 3,
                "{chain}: R[fast] = {reported} would trip GasQuoteTooHigh against C = {chain_price}"
            );
            // Not merely inside: inside with room. 2.0 × C is the ceiling,
            // reached only where the base fee is zero and the tip is all
            // there is (BSC).
            assert!(
                reported <= chain_price * 2 + 1,
                "{chain}: R[fast] = {reported} exceeds 2.0 × C = {chain_price}"
            );
        }

        // The supremum itself, as a pair of limits rather than a claim: a
        // pure-tip market sits at exactly 2.0 × C, a pure-base one at 1.8 ×.
        let pure_tip = tier_price(SubmissionTier::Fast, 0, 1_000_000)
            .unwrap()
            .network_fee_per_gas;
        assert_eq!(pure_tip, 2_000_000); // 2.0 × C, C = 1_000_000
        let pure_base = tier_price(SubmissionTier::Fast, 1_000_000, 0)
            .unwrap()
            .network_fee_per_gas;
        assert_eq!(pure_base, 1_800_000); // 1.8 × C, C = 1_000_000
    }

    #[test]
    fn tiers_are_ordered_slow_to_fast_in_the_tip_the_cap_and_the_basis() {
        for (chain, base, tip) in MARKETS {
            let tiers = tiers(NetworkGasPrice {
                base_fee_per_gas: base,
                max_priority_fee_per_gas: tip,
            })
            .unwrap();
            assert!(
                tiers.slow.max_fee_per_gas < tiers.standard.max_fee_per_gas
                    && tiers.standard.max_fee_per_gas < tiers.fast.max_fee_per_gas,
                "{chain}: caps out of order"
            );
            assert!(
                tiers.slow.network_fee_per_gas < tiers.standard.network_fee_per_gas
                    && tiers.standard.network_fee_per_gas < tiers.fast.network_fee_per_gas,
                "{chain}: bases out of order"
            );
            // The tip can only tie where there is no tip to scale (`zero
            // tip`) or where it is too small to round apart; everywhere a
            // real market puts one it is strictly increasing, which is what
            // a builder actually ranks by.
            assert!(
                tiers.slow.max_priority_fee_per_gas <= tiers.standard.max_priority_fee_per_gas
                    && tiers.standard.max_priority_fee_per_gas
                        <= tiers.fast.max_priority_fee_per_gas,
                "{chain}: tips out of order"
            );
            if tip >= 4 {
                assert!(
                    tiers.slow.max_priority_fee_per_gas < tiers.standard.max_priority_fee_per_gas
                        && tiers.standard.max_priority_fee_per_gas
                            < tiers.fast.max_priority_fee_per_gas,
                    "{chain}: tips not strictly increasing"
                );
            }
            // And what a builder sees: the effective tip, ordered the same
            // way. This is the number the defect left identical at all three.
            let effective = |price: super::GasPrice| {
                OuterFee {
                    max_fee_per_gas: price.max_fee_per_gas,
                    max_priority_fee_per_gas: price.max_priority_fee_per_gas,
                }
                .effective_tip_at(base)
            };
            assert_eq!(effective(tiers.slow), tiers.slow.max_priority_fee_per_gas);
            assert_eq!(effective(tiers.fast), tiers.fast.max_priority_fee_per_gas);
        }
    }

    #[test]
    fn a_zero_base_fee_chain_finally_differentiates_its_tiers() {
        // BSC: `baseFeePerGas` is 0 and the whole price is the tip, so the
        // cap multiplier has nothing to act on and cap = R = tip[tier]. Under
        // the old cap-only tier these three rows were one number and one
        // speed — the chain that HIDES this class of defect, which is why it
        // is pinned separately rather than standing in for a real market.
        // Scaling the tip is the only thing that can tell them apart here,
        // and now does: 1.0 / 1.25 / 2.0.
        let tip = 50_000_000u128; // 0.05 gwei, BSC's usual median
        let bsc = tiers(NetworkGasPrice {
            base_fee_per_gas: 0,
            max_priority_fee_per_gas: tip,
        })
        .unwrap();

        for (row, expected) in [
            (bsc.slow, 50_000_000u128),
            (bsc.standard, 62_500_000),
            (bsc.fast, 100_000_000),
        ] {
            // With no base fee, all three numbers coincide on the tip — and
            // the headroom is honestly zero, because there is no base-fee
            // spike to hold headroom against.
            assert_eq!(row.max_priority_fee_per_gas, expected);
            assert_eq!(row.max_fee_per_gas, expected);
            assert_eq!(row.network_fee_per_gas, expected);
            assert_eq!(row.relayer_fee_per_gas, 0);
            // The cap equals the tip and the base fee is 0, so the builder
            // receives the whole tip: `min(tip, tip − 0) = tip`.
            let outer = tier_outer_fee(
                match expected {
                    50_000_000 => SubmissionTier::Slow,
                    62_500_000 => SubmissionTier::Standard,
                    _ => SubmissionTier::Fast,
                },
                0,
                tip,
            )
            .unwrap();
            assert_eq!(outer.effective_tip_at(0), expected);
            assert!(outer.delivers_full_tip_at(0));
        }

        // Strictly ordered, at last, in every number that matters.
        assert!(bsc.slow.max_priority_fee_per_gas < bsc.standard.max_priority_fee_per_gas);
        assert!(bsc.standard.max_priority_fee_per_gas < bsc.fast.max_priority_fee_per_gas);
        assert_eq!(bsc.fast.max_priority_fee_per_gas, 2 * tip);

        // And the client's refusal guard still cannot trip: `R = 2 × tip`
        // against `C = base + tip = tip`, so the ratio is exactly 2.0 — the
        // supremum — against a limit of 3.
        assert!(bsc.fast.network_fee_per_gas <= 3 * tip);
        assert_eq!(bsc.fast.network_fee_per_gas, 2 * tip);
    }

    #[test]
    fn the_polygon_market_that_showed_one_price_for_three_tiers_now_shows_three() {
        // The defect, measured: every row of the wallet's tier picker read
        // `0.477459 POL` because `networkFeePerGas` was absent and vela-core
        // fell back to its own chain measurement for all three.
        let tiers = tiers(NetworkGasPrice {
            base_fee_per_gas: POLYGON_BASE,
            max_priority_fee_per_gas: POLYGON_TIP,
        })
        .unwrap();

        assert_eq!(tiers.slow.max_fee_per_gas, 399_481_987_375);
        assert_eq!(tiers.slow.max_priority_fee_per_gas, 27_773_221_947);
        assert_eq!(tiers.slow.network_fee_per_gas, 250_798_481_205);
        assert_eq!(tiers.standard.max_fee_per_gas, 530_328_214_672);
        assert_eq!(tiers.standard.max_priority_fee_per_gas, 34_716_527_434);
        assert_eq!(tiers.standard.network_fee_per_gas, 332_083_539_777);
        assert_eq!(tiers.fast.max_fee_per_gas, 798_963_974_751);
        assert_eq!(tiers.fast.max_priority_fee_per_gas, 55_546_443_894);
        assert_eq!(tiers.fast.network_fee_per_gas, 501_596_962_409);

        // `standard`'s BASE-FEE term is, to the wei, the single price the
        // relay reported for this market before per-tier pricing; only the
        // tip term moved, by the 1.25× a `standard` now signs with.
        assert_eq!(
            tiers.standard.network_fee_per_gas,
            (POLYGON_BASE * 120).div_ceil(100) + POLYGON_TIP + POLYGON_TIP.div_ceil(4)
        );
        // The relay's own pace — what an operation that names NO tier is
        // signed at — is untouched, and now sits between `slow` and
        // `standard` because `standard` buys a bigger tip on top of the same
        // 2 × base cap.
        let untiered = quoted_outer_fee(POLYGON_BASE, POLYGON_TIP).unwrap();
        assert_eq!(untiered, 523_384_909_185);
        assert!(tiers.slow.max_fee_per_gas < untiered);
        assert!(untiered < tiers.standard.max_fee_per_gas);
    }

    #[test]
    fn quotes_double_base_plus_tip_with_overflow_checks() {
        use super::tip_from_legacy_gas_price;
        use alloy::primitives::U256;
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
    fn the_standard_tier_keeps_the_caps_base_fee_term_and_lifts_only_the_tip() {
        // `standard` shares `quoted_outer_fee`'s `2 × base` multiple, so a
        // tiered and an untiered submission differ by exactly the tip scale
        // and nothing else — a bounded, stateable difference rather than two
        // unrelated formulas. Swept across a real base fee, a zero base fee
        // (BSC) and a bare tip.
        for (base, tip) in [
            (53_500_000_000u128, 1_000_000_000u128), // Ethereum, 0.0535 gwei
            (0, 3_000_000_000),                      // BSC: no base fee at all
            (1, 0),
            (u128::MAX / 4, 7), // absurd, but the two must still agree
        ] {
            let tiered = tier_outer_fee(SubmissionTier::Standard, base, tip).unwrap();
            let untiered = quoted_outer_fee(base, tip).unwrap();
            assert_eq!(
                tiered.max_fee_per_gas,
                untiered + (tiered.max_priority_fee_per_gas - tip),
                "base={base} tip={tip}"
            );
            assert_eq!(
                tiered.max_priority_fee_per_gas,
                tip.div_ceil(4) + tip,
                "base={base} tip={tip}"
            );
        }
        // And they agree on refusing an unpriceable market, too.
        assert_eq!(tier_outer_fee(SubmissionTier::Standard, u128::MAX, 0), None);
        assert_eq!(quoted_outer_fee(u128::MAX, 0), None);
    }

    #[test]
    fn each_tier_multiplies_the_base_fee_and_the_tip_together() {
        let base = 100_000_000_000u128; // 100 gwei
        let tip = 3_000_000_000u128; //   3 gwei
        assert_eq!(
            tier_outer_fee(SubmissionTier::Slow, base, tip),
            Some(OuterFee {
                max_fee_per_gas: 153_000_000_000, // 1.5 × 100 + 3.00
                max_priority_fee_per_gas: 3_000_000_000,
            })
        );
        assert_eq!(
            tier_outer_fee(SubmissionTier::Standard, base, tip),
            Some(OuterFee {
                max_fee_per_gas: 203_750_000_000, // 2.0 × 100 + 3.75
                max_priority_fee_per_gas: 3_750_000_000,
            })
        );
        assert_eq!(
            tier_outer_fee(SubmissionTier::Fast, base, tip),
            Some(OuterFee {
                max_fee_per_gas: 306_000_000_000, // 3.0 × 100 + 6.00
                max_priority_fee_per_gas: 6_000_000_000,
            })
        );
        // On a chain with no base fee (BSC) the cap collapses onto the tip —
        // so the tip scale is the ONLY thing separating the tiers there, and
        // it does separate them.
        for (tier, expected) in [
            (SubmissionTier::Slow, 3_000_000_000u128),
            (SubmissionTier::Standard, 3_750_000_000),
            (SubmissionTier::Fast, 6_000_000_000),
        ] {
            assert_eq!(
                tier_outer_fee(tier, 0, tip),
                Some(OuterFee {
                    max_fee_per_gas: expected,
                    max_priority_fee_per_gas: expected,
                })
            );
        }
        assert_eq!(tier_outer_fee(SubmissionTier::Fast, u128::MAX, 0), None);
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
        // The refusal a client gets for a name this relay does not know: a
        // parse failure naming the three it does, never a silent fallback.
        let error = serde_json::from_value::<SubmissionTier>(json!("turbo")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown variant `turbo`, expected one of `slow`, `standard`, `fast`"
        );
        // And a wei amount is not a tier name either — the client may never
        // name an absolute price.
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
        // One number, no base/tip split to be had. Read as pure tip, so each
        // tier's cap, basis and tip all land on the same value — its own
        // scaled tip. The base-fee headroom is honestly zero (there is no
        // base fee), but priority is still for sale.
        let price = legacy_price_from_result(json!("0x64")).unwrap();
        assert_eq!(
            price,
            NetworkGasPrice {
                base_fee_per_gas: 0,
                max_priority_fee_per_gas: 100,
            }
        );
        let tiers = tiers(price).unwrap();
        for (tier, expected) in [
            (tiers.slow, 100u128),
            (tiers.standard, 125),
            (tiers.fast, 200),
        ] {
            assert_eq!(tier.max_fee_per_gas, expected);
            assert_eq!(tier.max_priority_fee_per_gas, expected);
            assert_eq!(tier.network_fee_per_gas, expected);
            assert_eq!(tier.relayer_fee_per_gas, 0);
        }
        assert!(legacy_price_from_result(json!({ "gasPrice": "0x64" })).is_err());
    }
}
