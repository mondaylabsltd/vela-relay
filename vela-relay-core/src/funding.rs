//! Relayer float and treasury top-up policy.
//!
//! How much a relayer should hold, how large one top-up may be, and whether
//! the treasury can afford it are all decided here; the shell fetches
//! balances/nonces and signs/broadcasts the transfer.

use std::fmt::{Display, Formatter};

use alloy::primitives::U256;

use crate::settlement::USD_PRICE_SCALE;

/// The least gas a plain native treasury → relayer transfer is signed with:
/// the intrinsic cost of a value transfer on Ethereum and on OP-stack chains,
/// which charge their L1 data separately.
///
/// It is a floor, not the limit. Arbitrum folds the L1 data cost into the gas
/// limit, so a 21 000 transfer there is refused — "intrinsic gas too low
/// (supplied gas 21000)"; the same transfer estimated 21 397 on 2026-10-03.
/// Every top-up the relay signed for Arbitrum was refused, the relayer was
/// never funded, and every operation on the chain waited in "funding" until
/// the queue gave up on it. See [`native_top_up_gas_limit`].
pub const TOP_UP_GAS_LIMIT: u64 = 21_000;
/// Headroom on the chain's own estimate of the top-up — the same 1.2× the
/// Tempo top-up carries. Arbitrum's L1 component moves with the L1 price
/// between the estimate and inclusion.
pub const TOP_UP_GAS_BUFFER_BPS: u64 = 12_000;
/// Preferred per-transfer relayer top-up cap in whole USD, when a market
/// price is available; otherwise the shell falls back to its static wei cap.
pub const NATIVE_TOP_UP_USD_CAP: u64 = 20;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FundingPlanError {
    TargetOverflow,
    AmountUnderflow,
    DeficitUnderflow,
    DeficitExceedsCap { deficit: U256, cap: U256 },
    GasCostOverflow,
    ReserveOverflow,
}

impl Display for FundingPlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        // Byte-frozen: the shell folds these into its executor error text.
        match self {
            Self::TargetOverflow => formatter.write_str("relayer float target overflow"),
            Self::AmountUnderflow => formatter.write_str("relayer funding amount underflow"),
            Self::DeficitUnderflow => formatter.write_str("relayer funding deficit underflow"),
            Self::DeficitExceedsCap { deficit, cap } => write!(
                formatter,
                "current UserOperation prefund exceeds the per-transfer cap: deficit={deficit}, cap={cap}"
            ),
            Self::GasCostOverflow => formatter.write_str("top-up gas cost overflow"),
            Self::ReserveOverflow => formatter.write_str("treasury reserve requirement overflow"),
        }
    }
}

impl std::error::Error for FundingPlanError {}

/// The pre-probe part of a native top-up: how much to request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeTopUpRequest {
    /// The desired float replenishment, capped to one transfer.
    pub amount_capped: U256,
    /// The hard minimum this bundle needs to execute at all.
    pub deficit: U256,
}

/// Target = `max(prefund × multiplier, float_target, float_min)`; the
/// discretionary part above this bundle's deficit is capped per transfer
/// rather than deferring an otherwise fundable operation. Callers only reach
/// this with `relayer_balance < required_prefund`.
pub fn plan_native_top_up(
    relayer_balance: U256,
    required_prefund: U256,
    float_cost_multiplier: u64,
    float_target_wei: u128,
    float_min_wei: u128,
    top_up_cap: U256,
) -> Result<NativeTopUpRequest, FundingPlanError> {
    let target_from_cost = required_prefund
        .checked_mul(U256::from(float_cost_multiplier))
        .ok_or(FundingPlanError::TargetOverflow)?;
    let target = target_from_cost
        .max(U256::from(float_target_wei))
        .max(U256::from(float_min_wei));
    let desired_amount = target
        .checked_sub(relayer_balance)
        .ok_or(FundingPlanError::AmountUnderflow)?;
    let deficit = required_prefund
        .checked_sub(relayer_balance)
        .ok_or(FundingPlanError::DeficitUnderflow)?;
    if deficit > top_up_cap {
        return Err(FundingPlanError::DeficitExceedsCap {
            deficit,
            cap: top_up_cap,
        });
    }
    // A float target may be much larger than one bundle. Cap the discretionary
    // part instead of deferring an otherwise fundable operation.
    Ok(NativeTopUpRequest {
        amount_capped: desired_amount.min(top_up_cap),
        deficit,
    })
}

/// The gas limit a native top-up is signed with: the chain's estimate of the
/// exact transfer with [`TOP_UP_GAS_BUFFER_BPS`] on top, never below
/// [`TOP_UP_GAS_LIMIT`]. Without an estimate it is the floor — and a chain
/// that refuses that is caught by the broadcast, which discards the transfer
/// and signs it again (`execution::broadcast_funding`).
pub fn native_top_up_gas_limit(raw_estimate: Option<u64>) -> u64 {
    raw_estimate.map_or(TOP_UP_GAS_LIMIT, |gas| {
        (gas.saturating_mul(TOP_UP_GAS_BUFFER_BPS) / 10_000).max(TOP_UP_GAS_LIMIT)
    })
}

/// The treasury balance that must survive a top-up: the transfer's own gas at
/// the current fee plus the configured floor.
pub fn native_top_up_reserve(
    gas_limit: u64,
    max_fee_per_gas: u128,
    treasury_floor_wei: u128,
) -> Result<U256, FundingPlanError> {
    U256::from(gas_limit)
        .checked_mul(U256::from(max_fee_per_gas))
        .ok_or(FundingPlanError::GasCostOverflow)?
        .checked_add(U256::from(treasury_floor_wei))
        .ok_or(FundingPlanError::ReserveOverflow)
}

/// Returns a full or partial float top-up only when the treasury can still cover the immediate
/// UserOperation prefund after reserving this transfer's gas and the configured floor.
pub fn treasury_affordable_top_up(
    capped_amount: U256,
    deficit: U256,
    treasury_balance: U256,
    protected_treasury: U256,
) -> Option<U256> {
    let amount = capped_amount.min(treasury_balance.saturating_sub(protected_treasury));
    (amount >= deficit).then_some(amount)
}

/// Converts the whole-USD top-up cap into native units at an 8-decimal USD
/// price, rounding toward the relayer.
pub fn native_amount_for_usd_cap(
    native_decimals: u32,
    native_usd_price: U256,
    usd_cap: u64,
) -> Option<U256> {
    if native_usd_price.is_zero() || native_decimals > 38 {
        return None;
    }
    let native_scale =
        (0..native_decimals).try_fold(U256::ONE, |value, _| value.checked_mul(U256::from(10u8)))?;
    let numerator = U256::from(usd_cap)
        .checked_mul(U256::from(USD_PRICE_SCALE))?
        .checked_mul(native_scale)?;
    let quotient = numerator / native_usd_price;
    let remainder = numerator % native_usd_price;
    quotient.checked_add(U256::from(u8::from(!remainder.is_zero())))
}

/// Tempo variant: the pathUSD float target is a flat constant; the plan is
/// simply the missing amount, `None` when the relayer already holds enough.
pub fn plan_tempo_top_up(
    relayer_balance: U256,
    required_prefund: U256,
) -> Result<Option<U256>, FundingPlanError> {
    let target = required_prefund.max(U256::from(crate::tempo::TEMPO_FLOAT_TARGET));
    let amount = target
        .checked_sub(relayer_balance)
        .ok_or(FundingPlanError::AmountUnderflow)?;
    Ok((!amount.is_zero()).then_some(amount))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::{
        FundingPlanError, NATIVE_TOP_UP_USD_CAP, native_amount_for_usd_cap, plan_native_top_up,
        plan_tempo_top_up, treasury_affordable_top_up,
    };
    use crate::tempo::TEMPO_FLOAT_TARGET;

    #[test]
    fn converts_twenty_usd_top_up_cap_to_native_units_with_ceiling_rounding() {
        // MATIC at $0.20: $20 is exactly 100 MATIC.
        assert_eq!(
            native_amount_for_usd_cap(18, U256::from(20_000_000u64), NATIVE_TOP_UP_USD_CAP),
            Some(U256::from(100_000_000_000_000_000_000u128))
        );
        // ETH at $2,500: $20 is 0.008 ETH.
        assert_eq!(
            native_amount_for_usd_cap(18, U256::from(250_000_000_000u64), NATIVE_TOP_UP_USD_CAP,),
            Some(U256::from(8_000_000_000_000_000u64))
        );
        // A non-integral conversion is rounded toward the relayer.
        assert_eq!(
            native_amount_for_usd_cap(18, U256::from(300_000_000u64), NATIVE_TOP_UP_USD_CAP),
            Some(U256::from(6_666_666_666_666_666_667u128))
        );
        assert_eq!(
            native_amount_for_usd_cap(18, U256::ZERO, NATIVE_TOP_UP_USD_CAP),
            None
        );
        assert_eq!(
            native_amount_for_usd_cap(39, U256::ONE, NATIVE_TOP_UP_USD_CAP),
            None
        );
    }

    #[test]
    fn funds_the_current_bundle_when_the_preferred_float_is_not_affordable() {
        let protected = U256::from(100_231_000_000_000u64);
        // 0.011971738426977855 ETH in the treasury can still pay the requested 0.002 ETH
        // float after retaining the 0.0001 ETH floor and the transfer gas.
        assert_eq!(
            treasury_affordable_top_up(
                U256::from(2_000_000_000_000_000u64),
                U256::from(1_000_000_000_000_000u64),
                U256::from(11_971_738_426_977_855u64),
                protected,
            ),
            Some(U256::from(2_000_000_000_000_000u64))
        );

        // A tight treasury is allowed to provide a reduced float as long as this operation's
        // exact deficit remains covered.
        assert_eq!(
            treasury_affordable_top_up(
                U256::from(2_000u64),
                U256::from(1_000u64),
                U256::from(1_500u64),
                U256::ZERO,
            ),
            Some(U256::from(1_500u64))
        );
        assert_eq!(
            treasury_affordable_top_up(
                U256::from(2_000u64),
                U256::from(1_000u64),
                U256::from(999u64),
                U256::ZERO,
            ),
            None
        );
    }

    #[test]
    fn native_plan_targets_the_float_and_caps_one_transfer() {
        // prefund 100 × 5 = 500 is the binding target; balance 50 → desired
        // 450, deficit 50, capped to 200.
        let plan = plan_native_top_up(
            U256::from(50u64),
            U256::from(100u64),
            5,
            0,
            0,
            U256::from(200u64),
        )
        .unwrap();
        assert_eq!(plan.amount_capped, U256::from(200u64));
        assert_eq!(plan.deficit, U256::from(50u64));

        // The static float targets win when larger than cost × multiplier.
        let plan = plan_native_top_up(
            U256::from(50u64),
            U256::from(100u64),
            5,
            900,
            0,
            U256::from(2_000u64),
        )
        .unwrap();
        assert_eq!(plan.amount_capped, U256::from(850u64));

        // A deficit past the per-transfer cap refuses with the frozen text.
        let error = plan_native_top_up(U256::ZERO, U256::from(300u64), 1, 0, 0, U256::from(200u64))
            .unwrap_err();
        assert!(matches!(error, FundingPlanError::DeficitExceedsCap { .. }));
        assert_eq!(
            error.to_string(),
            "current UserOperation prefund exceeds the per-transfer cap: deficit=300, cap=200"
        );
    }

    #[test]
    fn tempo_plan_tops_up_to_the_flat_float_target() {
        assert_eq!(
            plan_tempo_top_up(U256::ZERO, U256::from(1_000u64)).unwrap(),
            Some(U256::from(TEMPO_FLOAT_TARGET))
        );
        assert_eq!(
            plan_tempo_top_up(U256::from(TEMPO_FLOAT_TARGET), U256::from(1_000u64)).unwrap(),
            None
        );
        assert_eq!(
            plan_tempo_top_up(U256::from(100u64), U256::from(500_000u64)).unwrap(),
            Some(U256::from(499_900u64))
        );
    }

    #[test]
    fn a_top_up_is_signed_with_the_chains_estimate_never_below_a_plain_transfer() {
        use super::{TOP_UP_GAS_LIMIT, native_top_up_gas_limit, native_top_up_reserve};
        // Arbitrum, 2026-10-03: 21 397 estimated, 21 000 refused.
        assert_eq!(native_top_up_gas_limit(Some(21_397)), 25_676);
        // OP-stack chains estimate the plain 21 000: the 1.2x stays on top.
        assert_eq!(native_top_up_gas_limit(Some(21_000)), 25_200);
        // No estimate, or one below a value transfer's cost: the floor.
        assert_eq!(native_top_up_gas_limit(None), TOP_UP_GAS_LIMIT);
        assert_eq!(native_top_up_gas_limit(Some(1)), TOP_UP_GAS_LIMIT);
        assert!(native_top_up_gas_limit(Some(u64::MAX)) > TOP_UP_GAS_LIMIT);
        // The reserve counts the gas the transfer is signed with.
        assert_eq!(
            native_top_up_reserve(25_676, 2, 7).unwrap(),
            alloy::primitives::U256::from(51_359u64)
        );
    }
}
