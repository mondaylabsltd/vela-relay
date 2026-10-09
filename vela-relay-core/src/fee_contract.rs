//! The wallet ↔ relay in-band fee contract, checked end to end through the
//! relay's own functions (`docs/fees.md` §3). Test-only: every rule lives in
//! [`crate::estimate`], [`crate::cost`], [`crate::gas_math`] and
//! [`crate::settlement`]; this module asks whether a client that follows the
//! published contract is accepted, at which speed, and whether the relay can
//! lose on it.
//!
//! The market is Ethereum mainnet at block 26,149,237 (2026-10-08), where the
//! overcharge was measured: next block's base fee 2,784,444,443 wei, the
//! node's tip 0, tier tips 0.147 / 1.0 / 1.795 gwei from the reward
//! percentiles (`gas_math::tests::the_ethereum_tiers_at_the_block_the_overcharge_was_measured`).
//! The operations are the investigation's classes with their measured or
//! replayed gas.

use alloy::primitives::{Address, U256, address};

use crate::{
    cost::{BillingTerms, buffered_gas},
    gas_math::{OuterFee, SubmissionTier, TierTips, in_band_fee_per_gas, tier_outer_fee},
    settlement::{
        ChainAssetConfig, DEFAULT_SETTLEMENT_INCLUSION_FLOOR_BPS, FeeContext, SettlementDecision,
        decide_settlement, decide_submission_fees,
    },
};

const TREASURY: Address = address!("3e59292e18417f814112f731e7163534c6d2fe3c");
const BASE: u128 = 2_784_444_443;
const TIPS: TierTips = TierTips {
    slow: 147_320_634,
    standard: 1_000_000_000,
    fast: 1_795_116_512,
};
const TIERS: [SubmissionTier; 3] = [
    SubmissionTier::Slow,
    SubmissionTier::Standard,
    SubmissionTier::Fast,
];

/// An operation class: the `settlementGas` the estimate promised, and the gas
/// its bundle used (what the executor measures and bills `buffered_gas` of).
struct Operation {
    name: &'static str,
    settlement_gas: u128,
    gas_used: u128,
    /// What vela-wallet before `settlementGas` priced: its padded limits.
    padded_limits: u128,
}

/// Mined operations carry the estimate replayed at their parent block
/// (`estimate::tests`); the swap, the undeployed send and the deployed backup
/// carry the investigation's measured gas with `settlementGas` set to exactly
/// what the executor bills — no estimate slack at all, the worst case.
fn operations() -> [Operation; 6] {
    let exact = |gas_used: u128| buffered(gas_used);
    [
        Operation {
            name: "ETH send (tx 0x7132ee31)",
            settlement_gas: 208_070,
            gas_used: 146_824,
            padded_limits: 583_954,
        },
        Operation {
            name: "USDT send (tx 0xe42fb6b9)",
            settlement_gas: 249_371,
            gas_used: 183_941,
            padded_limits: 622_689,
        },
        Operation {
            name: "Uniswap swap, swap-sized",
            settlement_gas: exact(291_357),
            gas_used: 291_357,
            padded_limits: 913_722,
        },
        Operation {
            name: "first operation, undeployed Safe",
            settlement_gas: exact(504_609),
            gas_used: 504_609,
            padded_limits: 2_219_986,
        },
        Operation {
            name: "registry backup, deployed Safe",
            settlement_gas: exact(4_675_105),
            gas_used: 4_675_105,
            padded_limits: 5_945_819,
        },
        Operation {
            name: "registry backup, first operation (tx 0x86795d08)",
            settlement_gas: 5_831_838,
            gas_used: 5_034_866,
            padded_limits: 7_659_195,
        },
    ]
}

fn buffered(gas_used: u128) -> u128 {
    let terms = BillingTerms::default();
    buffered_gas(
        U256::from(gas_used),
        terms.gas_buffer_bps,
        terms.fixed_gas_buffer,
    )
    .unwrap()
    .to::<u128>()
}

fn assets() -> ChainAssetConfig {
    ChainAssetConfig {
        native_decimals: 18,
        settlement_markup_bps: BillingTerms::default().settlement_markup_bps,
        stablecoins: Default::default(),
    }
}

/// `executeUserOp(MultiSend, …, delegatecall)` paying `amount` wei to the
/// treasury — the shape `settlement::parse_reimbursement` credits.
fn native_payment(amount: u128) -> Vec<u8> {
    let word = |value: u128| {
        let mut word = vec![0u8; 16];
        word.extend(value.to_be_bytes());
        word
    };
    let mut packed = vec![0u8];
    packed.extend(TREASURY.as_slice());
    packed.extend(word(amount));
    packed.extend(word(0));
    let mut multisend = vec![0x8d, 0x80, 0xff, 0x0a];
    multisend.extend(word(32));
    multisend.extend(word(packed.len() as u128));
    multisend.extend(packed);
    let padding = (32 - multisend.len() % 32) % 32;
    multisend.resize(multisend.len() + padding, 0);
    let mut call_data = vec![0x7b, 0xb3, 0x74, 0x28];
    let mut trusted = vec![0u8; 12];
    trusted.extend(address!("38869bf66a61cf6bdb996a6ae40d5853fd43b526").as_slice());
    call_data.extend(trusted);
    call_data.extend(word(0));
    call_data.extend(word(128));
    call_data.extend(word(1));
    call_data.extend(word(multisend.len() as u128));
    call_data.extend(multisend);
    call_data
}

/// What the executor signs, and whether it is accepted, for one operation
/// paying `paid` that named `tier`, at a submission whose latest base fee is
/// `base`: `decide_submission_fees` then `decide_settlement`, as
/// `execution::run_batch` composes them. `None` when it is held.
fn submit(paid: u128, settlement_gas: u128, tier: SubmissionTier, base: u128) -> Option<OuterFee> {
    let call_data = native_payment(paid);
    let allocations = [U256::from(settlement_gas)];
    let mut fees = FeeContext {
        quoted_fee_per_gas: crate::gas_math::quoted_outer_fee(base, 0).unwrap(),
        base_fee_per_gas: base,
        max_priority_fee_per_gas: 0,
        inclusion_floor_bps: DEFAULT_SETTLEMENT_INCLUSION_FLOOR_BPS,
        requested_tier: Some(tier),
        tier_tips: TIPS,
    };
    let outer = decide_submission_fees(
        TREASURY,
        &assets(),
        &[call_data.as_slice()],
        &allocations,
        None,
        &fees,
    )
    .unwrap()
    .unwrap();
    fees.quoted_fee_per_gas = outer.max_fee_per_gas;
    fees.max_priority_fee_per_gas = outer.max_priority_fee_per_gas;
    match decide_settlement(
        TREASURY,
        &assets(),
        &[call_data.as_slice()],
        &allocations,
        None,
        &fees,
    )
    .unwrap()
    {
        SettlementDecision::KeepQuote { evaluation } if evaluation.all_accepted() => Some(outer),
        SettlementDecision::Reprice { fee_per_gas, .. } => Some(OuterFee {
            max_fee_per_gas: fee_per_gas,
            ..outer
        }),
        _ => None,
    }
}

/// The relay's requirement at a signed cap: `markup × billed gas × cap`,
/// rounded up as `settlement::evaluate_batch` rounds it.
fn requirement(billed_gas: u128, cap: u128) -> u128 {
    let markup = u128::from(BillingTerms::default().settlement_markup_bps);
    (billed_gas * cap * markup).div_ceil(10_000)
}

/// A wallet that follows the contract pays `settlementGas × inBandFeePerGas`
/// for the tier it names. For every operation class and every tier:
///
/// - at the quote's own block it funds the relay's requirement for the whole
///   tier (the drift allowance is 1.0; the estimate's slack is the spare);
/// - after one block of the largest base-fee rise EIP-1559 allows (12.5%)
///   it is still accepted, at the whole tier tip;
/// - whatever was signed, the chain cannot charge more than the payment
///   covers: the relay never loses.
#[test]
fn a_wallet_paying_the_published_price_on_settlement_gas_is_accepted_at_its_tier() {
    for operation in operations() {
        let billed = buffered(operation.gas_used);
        assert!(
            operation.settlement_gas >= billed,
            "{}: the estimate promised less than the executor bills",
            operation.name
        );
        for tier in TIERS {
            let quoted_cap = tier_outer_fee(tier, BASE, &TIPS).unwrap().max_fee_per_gas;
            let paid = operation.settlement_gas
                * in_band_fee_per_gas(quoted_cap, BillingTerms::default().settlement_markup_bps)
                    .unwrap();

            // At the quote's block: the whole tier.
            let required = requirement(billed, quoted_cap);
            assert!(
                paid * 10_000 >= required * u128::from(crate::gas_math::IN_BAND_DRIFT_BPS),
                "{} {tier}: paid {paid} < the drift allowance × required {required}",
                operation.name
            );
            let signed = submit(paid, billed, tier, BASE)
                .unwrap_or_else(|| panic!("{} {tier}: held at the quote block", operation.name));
            // The cap the quote named, to within the 0.01% the executor's
            // paid/required ratio resolves (`SettlementEvaluation::paid_ratio_bps`).
            assert!(
                signed.max_fee_per_gas * 10_000 >= quoted_cap * 9_998,
                "{} {tier}: signed cap {} < quoted cap {quoted_cap}",
                operation.name,
                signed.max_fee_per_gas
            );
            assert_eq!(
                signed.max_priority_fee_per_gas,
                TIPS.of(tier),
                "{} {tier}",
                operation.name
            );

            // One block of the largest rise later: repriced into the payment
            // if it must be, but accepted, and at the tier's whole tip.
            let risen = BASE * 1_125 / 1_000;
            let signed = submit(paid, billed, tier, risen)
                .unwrap_or_else(|| panic!("{} {tier}: held after a 12.5% rise", operation.name));
            assert_eq!(
                signed.max_priority_fee_per_gas,
                TIPS.of(tier),
                "{} {tier}: the tip was shaved after one block",
                operation.name
            );
            assert!(signed.delivers_full_tip_at(risen));

            // Never at a loss: the payment covers the markup on every gas the
            // bundle can be charged at the cap it was signed with, and the
            // chain charges `min(cap, base + tip)` per gas used.
            assert!(paid >= requirement(billed, signed.max_fee_per_gas));
            let worst_charge = operation.gas_used * signed.max_fee_per_gas;
            assert!(paid > worst_charge, "{} {tier}", operation.name);
        }
    }
}

/// The prices themselves, for the record (`docs/fees.md` §3): what a
/// contract-following wallet pays for the 2026-10-02 ETH send in that block,
/// in wei. At ETH $2,423.92 that is $2.40 slow, $2.87 standard, $3.70 fast —
/// for a send the chain charges ~$1.04 / $1.35 / $1.63 (146,824 gas at the base
/// fee plus the tier's tip) — against the $11.80 / $14.19 / $21.28 the deployed
/// wallet and relay quoted for it in the same block.
#[test]
fn what_an_ethereum_send_costs_at_each_tier() {
    let send = &operations()[0];
    let paid = TIERS.map(|tier| {
        let cap = tier_outer_fee(tier, BASE, &TIPS).unwrap().max_fee_per_gas;
        send.settlement_gas * in_band_fee_per_gas(cap, 11_000).unwrap()
    });
    assert_eq!(
        paid,
        [
            989_661_240_845_960,
            1_184_819_936_181_170,
            1_526_127_640_788_120
        ]
    );
    // Fast is at most twice slow, as the owner asked.
    assert!(paid[2] < 2 * paid[0]);
}

/// A wallet that prices the padded LIMITS against `networkFeePerGas` (3 ×
/// limits × max(C, R), vela-wallet before `settlementGas`) overpays, and is
/// accepted at the full tier: an old wallet keeps working on a new relay.
#[test]
fn a_wallet_that_prices_the_limits_is_still_accepted_at_its_tier() {
    // Its chain reading on Ethereum: eth_gasPrice is the base fee and the
    // node suggests no tip, so C = the base fee.
    let chain_price = BASE;
    for operation in operations() {
        let billed = buffered(operation.gas_used);
        for tier in TIERS {
            let network = crate::gas_math::tier_network_fee(tier, BASE, 0).unwrap();
            let paid = 3 * operation.padded_limits * network.max(chain_price);
            let signed = submit(paid, billed, tier, BASE)
                .unwrap_or_else(|| panic!("{} {tier}: an old wallet was held", operation.name));
            assert_eq!(
                signed.max_priority_fee_per_gas,
                TIPS.of(tier),
                "{} {tier}",
                operation.name
            );
            // …and its `R` stays inside its own `R > 3 × C` refusal.
            assert!(network <= 3 * chain_price);
        }
    }
}

/// Avalanche charges `max(gasUsed, gasLimit / 2)`; the gas billed there is
/// never under half the limit the relay signs, so the cap times what the
/// chain charges never exceeds what the payment covers.
#[test]
fn avalanche_bills_at_least_the_gas_the_chain_charges() {
    use crate::cost::{allocate_bundle_gas, settlement_gas_allocations, settlement_gas_rule};
    for (used, estimated) in [
        (100_000u64, 800_000u64),
        (700_000, 800_000),
        (21_000, 25_000_000),
    ] {
        let limit = allocate_bundle_gas(
            U256::from(used),
            U256::from(estimated),
            &[U256::from(used)],
            1_500,
            30_000,
        )
        .unwrap();
        let billed = settlement_gas_allocations(
            settlement_gas_rule(43_114),
            Some(U256::from(used)),
            &limit,
            &[U256::from(used)],
            1_500,
            30_000,
        )
        .unwrap()[0];
        let charged = U256::from(used).max(limit[0].div_ceil(U256::from(2u8)));
        assert!(
            billed >= charged,
            "used {used}: billed {billed} < charged {charged}"
        );
    }
}

/// The dust floor binds identically on both sides: a near-zero-gas
/// operation's requirement is the relay's `0.00001`-coin floor
/// (`settlement::MIN_NATIVE_FRACTION_DECIMALS`), never less.
#[test]
fn the_dust_floor_is_the_requirement_when_the_gas_costs_less() {
    use crate::settlement::{SettlementInput, evaluate_batch, minimum_amount};
    let floor = minimum_amount(18, crate::settlement::MIN_NATIVE_FRACTION_DECIMALS).unwrap();
    assert_eq!(floor, U256::from(10_000_000_000_000u64));
    for (paid, accepted) in [(10_000_000_000_000u128, true), (9_999_999_999_999, false)] {
        let call_data = native_payment(paid);
        let evaluation = evaluate_batch(
            TREASURY,
            &assets(),
            &[SettlementInput {
                call_data: &call_data,
                gas_native_cost: U256::from(1u8),
            }],
            None,
        )
        .unwrap();
        assert_eq!(evaluation.operations[0].required_amount, floor);
        assert_eq!(evaluation.all_accepted(), accepted, "paid {paid}");
    }
}
