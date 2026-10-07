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
