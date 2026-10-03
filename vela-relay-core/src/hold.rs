//! The delayed-inbox retry ladder and the settlement hold budget.
//!
//! One policy, one place: the backoff schedule (base 5 s doubling to a 5 min
//! cap) and the hold-budget cutoff both live here. The shell's Redis scripts
//! receive the precomputed schedule as arguments and perform a mechanical
//! table lookup — the attempt counter and the due-time clock anchor stay
//! server-side so writers and the claim reader share one clock, but no delay
//! value is ever computed outside this module.

use alloy::primitives::U256;

use crate::settlement::settlement_hold_reason;

/// First-retry delay of the delayed inbox.
pub const DELAYED_RETRY_BASE_MS: u64 = 5_000;
/// Ceiling of the doubling schedule.
pub const DELAYED_RETRY_MAX_MS: u64 = 5 * 60 * 1_000;

/// The deferral delay before retry `attempt` (1-based): base doubling per
/// attempt, capped. Attempts beyond the cap keep the cap.
pub fn retry_delay_ms(attempt: u32) -> u64 {
    let mut delay = DELAYED_RETRY_BASE_MS;
    let mut remaining = attempt;
    while remaining > 1 && delay < DELAYED_RETRY_MAX_MS {
        delay = (delay * 2).min(DELAYED_RETRY_MAX_MS);
        remaining -= 1;
    }
    delay
}

/// How long a queue redelivery waits, from how long ago the message was
/// first sent: half of it, inside the ladder's 5 s – 5 min band.
///
/// A queue that cannot report an attempt count (the Cloudflare shell's
/// `Message` has only its send time) still backs off geometrically this way —
/// each wait is half the age, so the gaps grow ×1.5 until the cap. Without it
/// a deferred operation was redelivered about once a second: the queue's 100
/// retries were spent in under two minutes and the operation was
/// dead-lettered, left `queued` forever (Arbitrum, 2026-10-03, while its
/// relayer waited on funding). With it the same 100 retries span about eight
/// hours.
pub fn redelivery_delay_ms(age_ms: u64) -> u64 {
    (age_ms / 2).clamp(DELAYED_RETRY_BASE_MS, DELAYED_RETRY_MAX_MS)
}

/// The full schedule as a lookup table: entry `i` (0-based) is the delay for
/// attempt `i + 1`; every attempt past the end uses the last entry. This is
/// what the shell hands its store scripts.
pub fn retry_delay_schedule_ms() -> Vec<u64> {
    let mut schedule = Vec::new();
    for attempt in 1.. {
        let delay = retry_delay_ms(attempt);
        schedule.push(delay);
        if delay >= DELAYED_RETRY_MAX_MS {
            break;
        }
    }
    schedule
}

/// Outcome of one settlement-hold attempt, judged after the delayed inbox
/// recorded the deferral (`attempt` is the post-increment count).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldDecision {
    /// Keep waiting for the market to come down; the reason string is the
    /// diagnostic the held operation carries.
    Hold { reason: String },
    /// The waiting budget is spent: reject so the wallet can tell the user to
    /// resend at today's prices.
    RejectBudgetExhausted,
}

pub fn decide_hold(attempt: u32, max_attempts: u32, paid: U256, required: U256) -> HoldDecision {
    if attempt > max_attempts {
        return HoldDecision::RejectBudgetExhausted;
    }
    HoldDecision::Hold {
        reason: settlement_hold_reason(paid, required, attempt, max_attempts),
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::{
        DELAYED_RETRY_BASE_MS, DELAYED_RETRY_MAX_MS, HoldDecision, decide_hold, retry_delay_ms,
        retry_delay_schedule_ms,
    };

    #[test]
    fn ladder_doubles_from_five_seconds_to_a_five_minute_cap() {
        let expected = [
            (1, 5_000),
            (2, 10_000),
            (3, 20_000),
            (4, 40_000),
            (5, 80_000),
            (6, 160_000),
            (7, 300_000),
            (8, 300_000),
            (12, 300_000),
            (13, 300_000),
        ];
        for (attempt, delay) in expected {
            assert_eq!(retry_delay_ms(attempt), delay, "attempt {attempt}");
        }
        assert_eq!(DELAYED_RETRY_BASE_MS, 5_000);
        assert_eq!(DELAYED_RETRY_MAX_MS, 300_000);
    }

    #[test]
    fn schedule_table_matches_the_ladder_and_ends_at_the_cap() {
        let schedule = retry_delay_schedule_ms();
        assert_eq!(
            schedule,
            vec![5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 300_000]
        );
        for attempt in 1..=20u32 {
            let index = (attempt as usize).min(schedule.len()) - 1;
            assert_eq!(
                schedule[index],
                retry_delay_ms(attempt),
                "attempt {attempt}"
            );
        }
    }

    #[test]
    fn budget_cutoff_rejects_only_past_the_final_attempt() {
        let paid = U256::from(90u8);
        let required = U256::from(100u8);
        match decide_hold(12, 12, paid, required) {
            HoldDecision::Hold { reason } => assert_eq!(
                reason,
                "waiting for network fees to fit the signed in-band reimbursement: \
                 paid=90, required=100, shortfall=10, attempt=12/12"
            ),
            HoldDecision::RejectBudgetExhausted => panic!("attempt 12 of 12 must still hold"),
        }
        assert_eq!(
            decide_hold(13, 12, paid, required),
            HoldDecision::RejectBudgetExhausted
        );
        assert!(matches!(
            decide_hold(1, 12, paid, required),
            HoldDecision::Hold { .. }
        ));
    }

    #[test]
    fn a_redelivery_waits_half_its_age_inside_the_ladders_band() {
        use super::{DELAYED_RETRY_BASE_MS, DELAYED_RETRY_MAX_MS, redelivery_delay_ms};
        assert_eq!(redelivery_delay_ms(0), DELAYED_RETRY_BASE_MS);
        assert_eq!(redelivery_delay_ms(30_000), 15_000);
        assert_eq!(redelivery_delay_ms(60 * 60 * 1_000), DELAYED_RETRY_MAX_MS);

        // 100 redeliveries — the Cloudflare queue's max_retries — last hours,
        // not the two minutes they lasted at one a second.
        let mut age = 0_u64;
        for _ in 0..100 {
            age += redelivery_delay_ms(age);
        }
        assert!(age > 7 * 60 * 60 * 1_000, "{age} ms");
    }
}
