use alloy::primitives::U256;

use crate::abi::PackedOperation;

/// Allocates the complete outer transaction gas across its UserOperations without allowing one
/// sender to free-ride on another. Direct EntryPoint gas is charged to that op; shared outer and
/// safety-buffer gas is split deterministically, including the integer remainder.
pub fn allocate_bundle_gas(
    simulated_outer_gas: U256,
    estimated_outer_gas: U256,
    per_operation_gas: &[U256],
    buffer_bps: u64,
    fixed_buffer: u64,
) -> Option<Vec<U256>> {
    if per_operation_gas.is_empty() {
        return Some(Vec::new());
    }
    let direct = per_operation_gas
        .iter()
        .try_fold(U256::ZERO, |sum, gas| sum.checked_add(*gas))?;
    let metered = simulated_outer_gas.max(estimated_outer_gas).max(direct);
    let proportional_buffer = ceil_div(
        metered.checked_mul(U256::from(buffer_bps))?,
        U256::from(10_000),
    )?;
    let total = metered
        .checked_add(proportional_buffer)?
        .checked_add(U256::from(fixed_buffer))?;
    let shared = total.checked_sub(direct)?;
    let count = U256::from(per_operation_gas.len());
    let per_operation_shared = shared / count;
    let remainder = shared % count;

    per_operation_gas
        .iter()
        .enumerate()
        .map(|(index, gas)| {
            gas.checked_add(per_operation_shared)?
                .checked_add(U256::from(u8::from(U256::from(index) < remainder)))
        })
        .collect()
}

/// The most outer gas a `handleOps` bundle can legitimately use: each
/// operation's declared verification, call, paymaster and pre-verification
/// gas, through the EntryPoint's 63/64 forwarding rule, plus the per-operation
/// and per-bundle overhead Tempo's outer limit carries
/// ([`crate::tempo::tempo_handle_ops_gas_limit`]). An operation cannot spend
/// past its own limits, so no real execution of the bundle needs more.
/// `None` on overflow.
pub fn declared_bundle_gas(operations: &[&PackedOperation]) -> Option<U256> {
    let declared = operations.iter().try_fold(U256::ZERO, |total, operation| {
        let limits = operation.packed.accountGasLimits.as_slice();
        let paymaster = operation.packed.paymasterAndData.as_ref();
        // EntryPoint v0.7: paymaster (20) ‖ verification gas (16) ‖ postOp gas (16) ‖ data.
        let (paymaster_verification, paymaster_post_op) = if paymaster.len() >= 52 {
            (
                U256::from_be_slice(&paymaster[20..36]),
                U256::from_be_slice(&paymaster[36..52]),
            )
        } else {
            (U256::ZERO, U256::ZERO)
        };
        total
            .checked_add(U256::from_be_slice(&limits[..16]))?
            .checked_add(U256::from_be_slice(&limits[16..]))?
            .checked_add(paymaster_verification)?
            .checked_add(paymaster_post_op)?
            .checked_add(operation.packed.preVerificationGas)
    })?;
    declared
        .checked_mul(U256::from(64u8))
        .map(|value| value / U256::from(63u8))?
        .checked_add(U256::from(operations.len()).checked_mul(U256::from(PER_OPERATION_OVERHEAD))?)?
        .checked_add(U256::from(BUNDLE_OVERHEAD))
}

/// Per-operation and per-bundle headroom over the declared limits — Tempo's.
const PER_OPERATION_OVERHEAD: u64 = 50_000;
const BUNDLE_OVERHEAD: u64 = 60_000;

/// The simulated outer gas the allocation may believe.
///
/// A simulation's `gasUsed` is not always what the bundle burns. Avalanche
/// C-Chain charges every transaction at least half its gas limit since its
/// 2026-09-22 upgrade (block 95,921,105), and its nodes report a simulated
/// call the same way: `debug_traceCall` — the only simulation Avalanche
/// offers — runs with the node's 50,000,000 default when the call names no
/// gas, so one plain send came back as 25,000,000 gas. Allocated at that, the
/// send needed ~0.3 AVAX of settlement against the ~0.02 its signer paid,
/// was held as `FloorUnfundable`, and was rejected 35 minutes later; no
/// Avalanche send landed after the upgrade (vela-wallet #440).
///
/// The simulation is kept — it is what proves the bundle succeeds — but its
/// figure is believed only up to what the bundle could really use: the
/// chain's own `eth_estimateGas` of the final bundle, or the operations'
/// declared limits, whichever is larger. On a chain that reports real gas the
/// figure is under both and passes through unchanged; where it is over both,
/// it was never execution.
pub fn credible_simulated_gas(simulated: U256, estimated: U256, declared: Option<U256>) -> U256 {
    match declared {
        Some(declared) => simulated.min(estimated.max(declared)),
        None => simulated,
    }
}

/// The executor's default proportional gas buffer: 15% of the metered gas
/// (`VELA_RELAY_EXECUTOR_GAS_BUFFER_BPS`).
pub const DEFAULT_GAS_BUFFER_BPS: u64 = 1_500;
/// The executor's default fixed gas buffer (`VELA_RELAY_EXECUTOR_FIXED_GAS_BUFFER`).
pub const DEFAULT_FIXED_GAS_BUFFER: u64 = 30_000;

/// The terms the executor bills an operation on, which every number the relay
/// publishes for a client to pay against must state exactly as the executor
/// applies them: `settlementGas` from `eth_estimateUserOperationGas` carries
/// the gas buffer, and the per-tier `inBandFeePerGas` the markup. Both shells
/// build it from the same configuration the executor reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BillingTerms {
    /// `VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS`.
    pub settlement_markup_bps: u64,
    /// `VELA_RELAY_EXECUTOR_GAS_BUFFER_BPS`.
    pub gas_buffer_bps: u64,
    /// `VELA_RELAY_EXECUTOR_FIXED_GAS_BUFFER`.
    pub fixed_gas_buffer: u64,
}

impl Default for BillingTerms {
    fn default() -> Self {
        Self {
            settlement_markup_bps: crate::settlement::DEFAULT_SETTLEMENT_MARKUP_BPS,
            gas_buffer_bps: DEFAULT_GAS_BUFFER_BPS,
            fixed_gas_buffer: DEFAULT_FIXED_GAS_BUFFER,
        }
    }
}

/// What an operation's in-band reimbursement is measured against on a chain:
/// the gas its bundle really burns, or the outer gas limit (`docs/fees.md` §1).
///
/// The outer transaction's gas LIMIT is always the estimate-based allocation
/// ([`allocate_bundle_gas`]): it carries the EntryPoint's reservation for
/// every declared limit (eth_estimateGas of `handleOps` answers ~322,000 for a
/// Safe send that burns 146,824 on Ethereum), so a bundle never runs out of
/// gas. Billing that limit made a person pay for gas the chain never charges.
/// Where the chain charges `gasUsed × price` and a full simulation reports that
/// `gasUsed`, the relay bills the measured gas plus its buffer instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementGasRule {
    /// The chain charges the gas the transaction uses, and nothing else; a full
    /// simulation (`eth_simulateV1`, `debug_traceCall`) measures it.
    Measured,
    /// Avalanche C-Chain since its 2026-09-22 upgrade (block 95,921,105)
    /// charges `max(gasUsed, gasLimit / 2)`: the measured gas, but never less
    /// than half the outer limit the relay signs.
    MeasuredAtLeastHalfTheLimit,
    /// Every other chain bills the outer limit, as before. A rollup's L1 data
    /// cost either rides inside its gas units (Arbitrum's `eth_estimateGas`
    /// carries it, a simulation need not) or is charged beside them (the OP
    /// stack's L1 fee); neither is in a simulation's `gasUsed`. An unlisted
    /// chain is treated the same way, so a chain is never billed on a figure
    /// nobody has checked against its fee rules.
    OuterLimit,
}

/// The [`SettlementGasRule`] of a chain. Listed by measurement, never assumed:
/// Ethereum and its testnets, Gnosis, Polygon PoS and BNB Smart Chain charge
/// `gasUsed × (base fee + tip)` with no separate data fee; Avalanche charges at
/// least half the limit.
pub const fn settlement_gas_rule(chain_id: u64) -> SettlementGasRule {
    match chain_id {
        // Ethereum, Sepolia, Holesky, Hoodi; Gnosis, Chiado; Polygon, Amoy;
        // BNB Smart Chain and its testnet.
        1 | 11_155_111 | 17_000 | 560_048 | 100 | 10_200 | 137 | 80_002 | 56 | 97 => {
            SettlementGasRule::Measured
        }
        43_114 | 43_113 => SettlementGasRule::MeasuredAtLeastHalfTheLimit,
        _ => SettlementGasRule::OuterLimit,
    }
}

/// The gas `n` units of measured gas are billed as: `n`, plus `buffer_bps` of
/// it rounded up, plus `fixed_buffer` — the executor's configured buffer
/// (`VELA_RELAY_EXECUTOR_GAS_BUFFER_BPS`, `…_FIXED_GAS_BUFFER`, 15% and
/// 30,000 by default). The same function prices the `settlementGas`
/// `eth_estimateUserOperationGas` returns, so the gas a wallet pays for and
/// the gas the executor bills are one rule. `None` on overflow.
pub fn buffered_gas(gas: U256, buffer_bps: u64, fixed_buffer: u64) -> Option<U256> {
    gas.checked_add(ceil_div(
        gas.checked_mul(U256::from(buffer_bps))?,
        U256::from(10_000),
    )?)?
    .checked_add(U256::from(fixed_buffer))
}

/// The gas each operation's reimbursement is evaluated against — its share of
/// the bundle's settlement gas (`docs/fees.md` §1).
///
/// - [`SettlementGasRule::OuterLimit`], or a simulation that did not run the
///   bundle in full (`measured_outer_gas` is `None`: the Pimlico `eth_call`
///   and the bundle's `eth_estimateGas` stand-in measure no gas), bills the
///   outer-limit allocation exactly as before.
/// - [`SettlementGasRule::Measured`] bills [`buffered_gas`] of the measured
///   outer gas, never more than the limit allocation in total.
/// - [`SettlementGasRule::MeasuredAtLeastHalfTheLimit`] also never less than
///   half the outer limit, rounded up — what Avalanche charges.
///
/// The total is split across operations in proportion to the gas the
/// EntryPoint accounts to each (`UserOperationEvent.actualGasUsed`), evenly
/// when no operation reports any, with the integer remainder going one wei to
/// each of the first operations, so the shares sum to the total exactly. For a
/// bundle of one operation the share IS the total. `None` on overflow or when
/// the inputs disagree in length.
pub fn settlement_gas_allocations(
    rule: SettlementGasRule,
    measured_outer_gas: Option<U256>,
    limit_allocations: &[U256],
    per_operation_gas: &[U256],
    buffer_bps: u64,
    fixed_buffer: u64,
) -> Option<Vec<U256>> {
    let measured = match (rule, measured_outer_gas) {
        (SettlementGasRule::OuterLimit, _) | (_, None) => {
            return Some(limit_allocations.to_vec());
        }
        (_, Some(measured)) => measured,
    };
    if limit_allocations.len() != per_operation_gas.len() {
        return None;
    }
    if limit_allocations.is_empty() {
        return Some(Vec::new());
    }
    let limit = limit_allocations
        .iter()
        .try_fold(U256::ZERO, |sum, gas| sum.checked_add(*gas))?;
    let mut total = buffered_gas(measured, buffer_bps, fixed_buffer)?.min(limit);
    if rule == SettlementGasRule::MeasuredAtLeastHalfTheLimit {
        total = total.max(ceil_div(limit, U256::from(2u8))?);
    }

    let weights = per_operation_gas
        .iter()
        .try_fold(U256::ZERO, |sum, gas| sum.checked_add(*gas))?;
    let count = U256::from(per_operation_gas.len());
    let shares = per_operation_gas
        .iter()
        .map(|gas| {
            if weights.is_zero() {
                Some(total / count)
            } else {
                Some(total.checked_mul(*gas)? / weights)
            }
        })
        .collect::<Option<Vec<_>>>()?;
    let assigned = shares
        .iter()
        .try_fold(U256::ZERO, |sum, gas| sum.checked_add(*gas))?;
    let mut remainder = total.checked_sub(assigned)?;
    Some(
        shares
            .into_iter()
            .map(|share| {
                let extra = U256::from(u8::from(!remainder.is_zero()));
                remainder -= extra;
                share + extra
            })
            .collect(),
    )
}

pub fn native_cost(gas: U256, max_fee_per_gas: u128) -> Option<U256> {
    gas.checked_mul(U256::from(max_fee_per_gas))
}

fn ceil_div(value: U256, divisor: U256) -> Option<U256> {
    if divisor.is_zero() {
        return None;
    }
    let quotient = value / divisor;
    let remainder = value % divisor;
    quotient.checked_add(U256::from(u8::from(!remainder.is_zero())))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::{allocate_bundle_gas, credible_simulated_gas, declared_bundle_gas};
    use crate::abi::PackedOperation;
    use crate::task::{UserOperation, UserOperationV0_7};

    /// The limits of the Avalanche send in vela-wallet #440's era: 300,000
    /// verification, 281,713 call, 112,977 pre-verification.
    fn send(paymaster: Option<(&str, &str)>) -> PackedOperation {
        let operation = UserOperationV0_7 {
            sender: "0x2c1c947000000000000000000000000000007c23".into(),
            nonce: "0x2".into(),
            factory: None,
            factory_data: None,
            call_data: "0x".into(),
            call_gas_limit: "0x44c71".into(),
            verification_gas_limit: "0x493e0".into(),
            pre_verification_gas: "0x1b951".into(),
            max_fee_per_gas: "0x0".into(),
            max_priority_fee_per_gas: "0x0".into(),
            paymaster: paymaster.map(|_| "0x1111111111111111111111111111111111111111".into()),
            paymaster_verification_gas_limit: paymaster.map(|(v, _)| v.into()),
            paymaster_post_op_gas_limit: paymaster.map(|(_, p)| p.into()),
            paymaster_data: paymaster.map(|_| "0x".into()),
            signature: "0x".into(),
            eip7702_auth: None,
            fee_token: None,
        };
        PackedOperation::try_from(&UserOperation::V0_7(Box::new(operation))).unwrap()
    }

    #[test]
    fn allocation_is_exact_and_assigns_remainder_deterministically() {
        let allocation = allocate_bundle_gas(
            U256::from(100),
            U256::from(120),
            &[U256::from(40), U256::from(30), U256::from(20)],
            1_000,
            2,
        )
        .unwrap();

        // total = max(100,120,90) + 10% + 2 = 134
        assert_eq!(allocation, [U256::from(55), U256::from(45), U256::from(34)]);
        assert_eq!(allocation.into_iter().sum::<U256>(), U256::from(134));
    }

    #[test]
    fn never_allocates_less_than_an_events_direct_gas() {
        let allocation = allocate_bundle_gas(
            U256::from(1),
            U256::from(1),
            &[U256::from(100), U256::from(200)],
            1_500,
            30,
        )
        .unwrap();

        assert!(allocation[0] >= U256::from(100));
        assert!(allocation[1] >= U256::from(200));
    }

    #[test]
    fn the_declared_bound_is_tempos_rule_over_every_limit() {
        let op = send(None);
        // (300,000 + 281,713 + 112,977) × 64/63 + 50,000 + 60,000
        assert_eq!(declared_bundle_gas(&[&op]), Some(U256::from(815_716u64)));
        // Two: twice the limits, two operations' headroom, one bundle's.
        assert_eq!(
            declared_bundle_gas(&[&op, &op]),
            Some(U256::from(1_571_433u64))
        );
        // A paymaster's verification and postOp limits count too.
        let paid = send(Some(("0x186a0", "0xc350")));
        assert_eq!(declared_bundle_gas(&[&paid]), Some(U256::from(968_097u64)));
        // Tempo's outer limit is the same rule without paymasters.
        assert_eq!(
            crate::tempo::tempo_handle_ops_gas_limit(&[&op]).map(U256::from),
            Ok(U256::from(815_716u64))
        );
    }

    #[test]
    fn avalanches_half_the_default_cap_is_not_believed() {
        let op = send(None);
        let declared = declared_bundle_gas(&[&op]);
        // vela-wallet #440: the trace of one send at the node's 50M default
        // gas reported 25,000,000 (0x17d7840).
        let traced = U256::from(25_000_000u64);
        let estimated = U256::from(262_000u64);
        assert_eq!(
            credible_simulated_gas(traced, estimated, declared),
            U256::from(815_716u64)
        );
        // An estimate above the declared bound (an L1 data component, say)
        // is the ceiling instead.
        assert_eq!(
            credible_simulated_gas(traced, U256::from(2_400_000u64), declared),
            U256::from(2_400_000u64)
        );
        // Settlement at the 7.5 gwei inclusion floor (1.5 × Avalanche's 5
        // gwei base), 1.4× markup, prod's 15% + 30,000 buffer: 0.302 AVAX
        // at the traced figure, 0.0102 at the believed one.
        let required = |simulated: U256| {
            let allocated = allocate_bundle_gas(
                simulated,
                estimated,
                &[U256::from(250_000u64)],
                1_500,
                30_000,
            )
            .unwrap()[0];
            allocated * U256::from(7_500_000_000u64) * U256::from(14u8) / U256::from(10u8)
        };
        assert_eq!(required(traced), U256::from(302_190_000_000_000_000u128));
        assert_eq!(
            required(credible_simulated_gas(traced, estimated, declared)),
            U256::from(10_164_777_000_000_000u128)
        );
    }

    /// The 2026-10-02 mainnet send (tx 0x7132ee31…, Safe 0x88cC…6894): its
    /// bundle used 146,824 gas, its eth_estimateGas was 322,126 (the
    /// EntryPoint's reservation for the declared limits), and the relay signed
    /// — and billed — the 400,445 limit.
    #[test]
    fn ethereum_bills_the_gas_a_send_used_and_keeps_the_limit_it_signs() {
        use super::{SettlementGasRule, settlement_gas_allocations, settlement_gas_rule};
        let used = U256::from(146_824u64);
        let estimated = U256::from(322_126u64);
        // The EntryPoint's own accounting for the op (UserOperationEvent).
        let accounted = [U256::from(231_186u64)];
        let limit = allocate_bundle_gas(used, estimated, &accounted, 1_500, 30_000).unwrap();
        assert_eq!(limit, [U256::from(400_445u64)]);
        assert_eq!(settlement_gas_rule(1), SettlementGasRule::Measured);
        let billed = settlement_gas_allocations(
            settlement_gas_rule(1),
            Some(used),
            &limit,
            &accounted,
            1_500,
            30_000,
        )
        .unwrap();
        // 146,824 + 22,024 (15%, rounded up) + 30,000.
        assert_eq!(billed, [U256::from(198_848u64)]);
        // A simulation that measured nothing bills the limit, as before.
        assert_eq!(
            settlement_gas_allocations(
                SettlementGasRule::Measured,
                None,
                &limit,
                &accounted,
                1_500,
                30_000
            ),
            Some(limit.clone())
        );
        // So does a chain that is not listed — Arbitrum, Base, anything new.
        for chain_id in [42_161, 8_453, 10, 123_456_789] {
            assert_eq!(
                settlement_gas_allocations(
                    settlement_gas_rule(chain_id),
                    Some(used),
                    &limit,
                    &accounted,
                    1_500,
                    30_000
                ),
                Some(limit.clone())
            );
        }
    }

    #[test]
    fn avalanche_bills_at_least_half_the_limit_it_signs() {
        use super::{SettlementGasRule, settlement_gas_allocations, settlement_gas_rule};
        assert_eq!(
            settlement_gas_rule(43_114),
            SettlementGasRule::MeasuredAtLeastHalfTheLimit
        );
        assert_eq!(
            settlement_gas_rule(43_113),
            SettlementGasRule::MeasuredAtLeastHalfTheLimit
        );
        let limit = [U256::from(800_001u64)];
        let accounted = [U256::from(200_000u64)];
        // 100,000 measured → 145,000 buffered, below half the 800,001 limit.
        assert_eq!(
            settlement_gas_allocations(
                SettlementGasRule::MeasuredAtLeastHalfTheLimit,
                Some(U256::from(100_000u64)),
                &limit,
                &accounted,
                1_500,
                30_000,
            ),
            Some(vec![U256::from(400_001u64)])
        );
        // Above half, the measured figure stands.
        assert_eq!(
            settlement_gas_allocations(
                SettlementGasRule::MeasuredAtLeastHalfTheLimit,
                Some(U256::from(500_000u64)),
                &limit,
                &accounted,
                1_500,
                30_000,
            ),
            Some(vec![U256::from(605_000u64)])
        );
    }

    #[test]
    fn a_bundle_splits_its_settlement_gas_by_what_the_entry_point_accounts_to_each() {
        use super::{SettlementGasRule, settlement_gas_allocations};
        let limit = [U256::from(400_000u64), U256::from(700_000u64)];
        let accounted = [U256::from(231_186u64), U256::from(470_000u64)];
        let billed = settlement_gas_allocations(
            SettlementGasRule::Measured,
            Some(U256::from(414_000u64)),
            &limit,
            &accounted,
            1_500,
            30_000,
        )
        .unwrap();
        // 414,000 + 62,100 + 30,000 = 506,100, split 231,186 : 470,000.
        assert_eq!(billed.iter().copied().sum::<U256>(), U256::from(506_100u64));
        assert_eq!(billed, [U256::from(166_865u64), U256::from(339_235u64)]);
        // Nothing accounted: an even split, the remainder to the first.
        let even = settlement_gas_allocations(
            SettlementGasRule::Measured,
            Some(U256::from(100_001u64)),
            &[U256::from(500_000u64); 3],
            &[U256::ZERO; 3],
            0,
            0,
        )
        .unwrap();
        assert_eq!(
            even,
            [
                U256::from(33_334u64),
                U256::from(33_334u64),
                U256::from(33_333u64)
            ]
        );
        // Never more than the limit allocation in total.
        assert_eq!(
            settlement_gas_allocations(
                SettlementGasRule::Measured,
                Some(U256::from(1_000_000u64)),
                &[U256::from(300_000u64)],
                &[U256::from(1u64)],
                1_500,
                30_000,
            ),
            Some(vec![U256::from(300_000u64)])
        );
    }

    #[test]
    fn a_real_figure_passes_through() {
        let declared = declared_bundle_gas(&[&send(None)]);
        // Under both bounds: the trace measured execution, and is kept.
        assert_eq!(
            credible_simulated_gas(U256::from(301_000u64), U256::from(262_000u64), declared),
            U256::from(301_000u64)
        );
        // Without a bound (overflow) nothing is second-guessed.
        assert_eq!(
            credible_simulated_gas(U256::from(25_000_000u64), U256::from(1u64), None),
            U256::from(25_000_000u64)
        );
    }
}
