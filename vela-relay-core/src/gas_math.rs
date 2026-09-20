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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GasPricePolicy {
    pub base_fee_multiplier: u128,
    pub slow_multiplier: u128,
    pub standard_multiplier: u128,
    pub fast_multiplier: u128,
    pub priority_fee_divisor: u128,
}

impl Default for GasPricePolicy {
    fn default() -> Self {
        Self {
            base_fee_multiplier: 120,
            slow_multiplier: 100,
            standard_multiplier: 110,
            fast_multiplier: 120,
            priority_fee_divisor: 200,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GasPrice {
    pub max_fee_per_gas: u128,
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistory {
    pub base_fee_per_gas: Vec<String>,
    #[serde(default)]
    pub reward: Vec<Vec<String>>,
}

pub fn price_from_fee_history(
    fee_history: &FeeHistory,
    base_fee_multiplier: u128,
    priority_fee: u128,
) -> Result<GasPrice, GasPriceError> {
    let base_fee = fee_history
        .base_fee_per_gas
        .last()
        .ok_or(GasPriceError::InvalidUpstreamResponse)
        .and_then(|value| parse_quantity(value))?;
    let max_fee_per_gas = scale(base_fee, base_fee_multiplier)?
        .checked_add(priority_fee)
        .ok_or(GasPriceError::ArithmeticOverflow)?;

    Ok(GasPrice {
        max_fee_per_gas: max_fee_per_gas.max(priority_fee),
        max_priority_fee_per_gas: priority_fee,
    })
}

pub fn tiers(
    policy: &GasPricePolicy,
    network_price: GasPrice,
) -> Result<GasPriceTiers, GasPriceError> {
    Ok(GasPriceTiers {
        slow: scale_price(network_price, policy.slow_multiplier)?,
        standard: scale_price(network_price, policy.standard_multiplier)?,
        fast: scale_price(network_price, policy.fast_multiplier)?,
    })
}

pub fn scale_price(price: GasPrice, multiplier: u128) -> Result<GasPrice, GasPriceError> {
    let max_priority_fee_per_gas = scale(price.max_priority_fee_per_gas, multiplier)?;
    let max_fee_per_gas = scale(price.max_fee_per_gas, multiplier)?.max(max_priority_fee_per_gas);

    Ok(GasPrice {
        max_fee_per_gas,
        max_priority_fee_per_gas,
    })
}

/// The executor's quoted outer-fee cap: `2 × base fee + tip`. The multiple
/// buys inclusion headroom, not cost — the chain only ever charges
/// `base fee + tip`. `None` on overflow.
pub fn quoted_outer_fee(base_fee_per_gas: u128, max_priority_fee_per_gas: u128) -> Option<u128> {
    base_fee_per_gas
        .checked_mul(2)?
        .checked_add(max_priority_fee_per_gas)
}

/// The submission speed a client may name on `eth_sendUserOperation`.
///
/// The client names the TIER, never a wei amount. A quote that went stale
/// between signing and inclusion therefore cannot mis-set the price: the relay
/// resolves the name against the base fee it reads at submit time, so the
/// worst a stale quote can do is buy a speed the reimbursement no longer funds
/// — which [`crate::settlement::decide_submission_cap`] then clamps away.
///
/// Ordered `Slow < Standard < Fast` so a bundle can submit at the fastest
/// speed any of its operations asked for.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionTier {
    Slow,
    /// The pace the relay keeps on its own, which is why it is the default:
    /// resolving `Standard` reproduces [`quoted_outer_fee`] exactly.
    #[default]
    Standard,
    Fast,
}

impl SubmissionTier {
    /// Basis points of the base fee this tier caps the outer transaction at.
    ///
    /// These multiply the BASE FEE for the submit cap. They are deliberately
    /// unrelated to the `slow`/`standard`/`fast` prices reported by
    /// `pimlico_getUserOperationGasPrice` (~1.0/1.1/1.2 × a ~1.2 × base
    /// network price): that quote tells the CLIENT what to pay, this
    /// multiplier tells the chain how badly the relay wants the block.
    pub const fn base_fee_bps(self) -> u64 {
        match self {
            Self::Slow => 15_000,
            Self::Standard => 20_000,
            Self::Fast => 30_000,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Standard => "standard",
            Self::Fast => "fast",
        }
    }
}

impl Display for SubmissionTier {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The outer-fee cap a named speed asks for: `multiplier × base fee + tip`.
///
/// **This is not the reported tier price, and confusing the two inverts the
/// feature.** The relay reports `fast` at ~1.2 × base — measured 0.0938 gwei
/// against a 0.0535 gwei Ethereum base fee on 2026-09-20 — while its submit
/// cap is already `2 × base` = 0.1070 gwei. Submitting at the reported `fast`
/// price would therefore make a "fast" send *slower* than today's default on
/// every EIP-1559 chain. (BSC hides the mistake: its `baseFeePerGas` is 0 and
/// the whole price is the tip, so the two numbers coincide there — never
/// validate this on BSC alone.)
///
/// A higher cap is not a higher cost: the chain only ever charges
/// `base fee + tip`, so the extra multiple is bought only during a spike, and
/// only up to what the in-band reimbursement funds.
///
/// The basis-point scale is applied through a widening multiply so the
/// intermediate never decides the answer: `Standard` must resolve to exactly
/// `quoted_outer_fee` for EVERY base fee, or "absent" and "standard" could
/// disagree about today's behaviour at the edges. Division truncates, matching
/// `settlement::inclusion_floor_fee_per_gas`, so `Slow` and the default 1.5×
/// floor land on the same wei rather than a rounding step apart.
///
/// `None` on overflow — the caller then keeps the relay's own pace, so an
/// unpriceable market costs a client its requested speed and nothing else.
pub fn tier_outer_fee(
    tier: SubmissionTier,
    base_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Option<u128> {
    let scaled = U256::from(base_fee_per_gas).checked_mul(U256::from(tier.base_fee_bps()))?
        / U256::from(10_000u64);
    u128::try_from(scaled)
        .ok()?
        .checked_add(max_priority_fee_per_gas)
}

/// The legacy-endpoint tip fallback: `eth_gasPrice − base fee`. `None` when
/// the gas price is below the base fee (a node inconsistency worth refusing).
pub fn tip_from_legacy_gas_price(gas_price: U256, base_fee: U256) -> Option<U256> {
    gas_price.checked_sub(base_fee)
}

/// The fallback tip when neither fee-history rewards nor
/// `eth_maxPriorityFeePerGas` yields a usable value.
pub fn fallback_priority_fee(base_fee: u128, priority_fee_divisor: u128) -> u128 {
    base_fee.div_ceil(priority_fee_divisor).max(1)
}

pub fn median_priority_fee(rewards: &[Vec<String>]) -> Option<u128> {
    let mut values = rewards
        .iter()
        .filter_map(|reward| reward.get(reward.len() / 2))
        .filter_map(|value| parse_quantity(value).ok())
        .collect::<Vec<_>>();

    if values.is_empty() {
        return None;
    }

    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        Some(values[middle - 1].saturating_add(values[middle]) / 2)
    } else {
        Some(values[middle])
    }
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

pub fn legacy_price_from_result(result: Value) -> Result<GasPrice, GasPriceError> {
    let value = result
        .as_str()
        .ok_or(GasPriceError::InvalidUpstreamResponse)
        .and_then(parse_quantity)?;

    Ok(GasPrice {
        max_fee_per_gas: value,
        max_priority_fee_per_gas: value,
    })
}

fn scale(value: u128, multiplier: u128) -> Result<u128, GasPriceError> {
    value
        .checked_mul(multiplier)
        .ok_or(GasPriceError::ArithmeticOverflow)
        .map(|value| value.div_ceil(100))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        FeeHistory, GasPrice, GasPricePolicy, legacy_price_from_result, median_priority_fee,
        parse_quantity, price_from_fee_history, tiers,
    };

    #[test]
    fn calculates_an_eip1559_price_from_fee_history() {
        let fee_history: FeeHistory = serde_json::from_value(json!({
            "baseFeePerGas": ["0x50", "0x64"],
            "reward": [["0x1", "0xa", "0x14"]]
        }))
        .unwrap();

        assert_eq!(
            price_from_fee_history(
                &fee_history,
                GasPricePolicy::default().base_fee_multiplier,
                10
            )
            .unwrap(),
            GasPrice {
                max_fee_per_gas: 130,
                max_priority_fee_per_gas: 10,
            }
        );
    }

    #[test]
    fn calculates_eip1559_tiers_with_independent_fee_caps() {
        let tiers = tiers(
            &GasPricePolicy::default(),
            GasPrice {
                max_fee_per_gas: 130,
                max_priority_fee_per_gas: 10,
            },
        )
        .unwrap();

        assert_eq!(tiers.slow.max_fee_per_gas, 130);
        assert_eq!(tiers.slow.max_priority_fee_per_gas, 10);
        assert_eq!(tiers.standard.max_fee_per_gas, 143);
        assert_eq!(tiers.standard.max_priority_fee_per_gas, 11);
        assert_eq!(tiers.fast.max_fee_per_gas, 156);
        assert_eq!(tiers.fast.max_priority_fee_per_gas, 12);
    }

    #[test]
    fn uses_the_median_priority_fee_from_fee_history_rewards() {
        let rewards: Vec<Vec<String>> = serde_json::from_value(json!([
            ["0x1", "0x4", "0x9"],
            ["0x1", "0x6", "0x9"],
            ["0x1", "0x8", "0x9"]
        ]))
        .unwrap();

        assert_eq!(median_priority_fee(&rewards), Some(6));
    }

    #[test]
    fn quotes_double_base_plus_tip_with_overflow_checks() {
        use super::{quoted_outer_fee, tip_from_legacy_gas_price};
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
    fn the_standard_tier_is_exactly_the_cap_the_relay_already_quoted() {
        use super::{SubmissionTier, quoted_outer_fee, tier_outer_fee};
        // The backwards-compatibility anchor: naming `standard` must resolve
        // to the same wei the executor has always submitted at, so "absent"
        // and "standard" can never disagree about today's behaviour. Swept
        // across a real base fee, a zero base fee (BSC) and a bare tip.
        for (base, tip) in [
            (53_500_000_000u128, 1_000_000_000u128), // Ethereum, 0.0535 gwei
            (0, 3_000_000_000),                      // BSC: no base fee at all
            (1, 0),
            (u128::MAX / 4, 7), // absurd, but the two must still agree
            (u128::MAX, 0),     // and agree on refusing, too
        ] {
            assert_eq!(
                tier_outer_fee(SubmissionTier::Standard, base, tip),
                quoted_outer_fee(base, tip),
                "base={base} tip={tip}"
            );
        }
    }

    #[test]
    fn each_tier_multiplies_the_base_fee_and_adds_the_whole_tip() {
        use super::{SubmissionTier, tier_outer_fee};
        let base = 100_000_000_000u128; // 100 gwei
        let tip = 3_000_000_000u128; //   3 gwei
        assert_eq!(
            tier_outer_fee(SubmissionTier::Slow, base, tip),
            Some(153_000_000_000)
        );
        assert_eq!(
            tier_outer_fee(SubmissionTier::Standard, base, tip),
            Some(203_000_000_000)
        );
        assert_eq!(
            tier_outer_fee(SubmissionTier::Fast, base, tip),
            Some(303_000_000_000)
        );
        // On a chain with no base fee (BSC) every tier is the tip: there is
        // nothing to multiply, and the tip is the whole price.
        for tier in [
            SubmissionTier::Slow,
            SubmissionTier::Standard,
            SubmissionTier::Fast,
        ] {
            assert_eq!(tier_outer_fee(tier, 0, tip), Some(tip));
        }
        assert_eq!(tier_outer_fee(SubmissionTier::Fast, u128::MAX, 0), None);
    }

    #[test]
    fn a_tier_name_round_trips_and_an_unknown_one_is_refused() {
        use super::SubmissionTier;
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
        use super::SubmissionTier;
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
        assert_eq!(
            legacy_price_from_result(json!("0x64")).unwrap(),
            GasPrice {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 100,
            }
        );
        assert!(legacy_price_from_result(json!({ "gasPrice": "0x64" })).is_err());
    }
}
