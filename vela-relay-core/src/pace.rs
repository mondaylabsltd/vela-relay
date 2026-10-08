//! How fast a chain makes blocks, and the waits the relay measures in blocks.
//!
//! A wait for something to be mined is wrong at both ends when it is a flat
//! number of seconds: three seconds is three Avalanche blocks and a quarter of
//! an Ethereum one. The core decides how long; the shell sleeps, or arms its
//! timer.

/// The typical interval between blocks, for the chains where it is known.
pub fn block_interval_ms(chain_id: u64) -> Option<u64> {
    Some(match chain_id {
        // Ethereum.
        1 => 12_000,
        // OP Mainnet, Base.
        10 | 8_453 => 2_000,
        // BNB Smart Chain, since the Maxwell upgrade (June 2025).
        56 => 750,
        // Gnosis.
        100 => 5_000,
        // Unichain.
        130 => 1_000,
        // Polygon PoS.
        137 => 2_000,
        // Arbitrum One.
        42_161 => 250,
        // Avalanche C-Chain: a mean of 0.99 s over 40 blocks, 2 s at most
        // (2026-10-08).
        43_114 => 1_000,
        _ => return None,
    })
}

/// What a chain not listed in [`block_interval_ms`] is assumed to make.
pub const DEFAULT_BLOCK_INTERVAL_MS: u64 = 2_000;

/// The longest a pass waits for the receipt of a top-up it just sent.
pub const TOP_UP_WAIT_MAX_MS: u64 = 5_000;

/// The shortest such wait, whatever the chain's block time: a receipt takes a
/// moment to reach the public endpoints the relay reads, even on a chain that
/// makes four blocks a second.
pub const TOP_UP_WAIT_MIN_MS: u64 = 2_000;

/// How a pass waits for a receipt: `polls` reads, each after a pause of
/// `pause_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiptWait {
    pub pause_ms: u64,
    pub polls: u32,
}

/// The wait for a lane's own top-up, in the pass that sent it: about two
/// blocks, within [`TOP_UP_WAIT_MIN_MS`]..=[`TOP_UP_WAIT_MAX_MS`], read every
/// half block (every 0.5–1 s).
///
/// Without it a pass that sent a top-up ended there, the operation was
/// redelivered 5–10 s later, and the next pass simulated it all over again.
/// On Avalanche that was 36–43 s between a top-up and its bundle, for a
/// top-up mined in about a second (vela-wallet #464, 2026-10-08).
pub fn top_up_receipt_wait(chain_id: u64) -> ReceiptWait {
    let block = block_interval_ms(chain_id).unwrap_or(DEFAULT_BLOCK_INTERVAL_MS);
    let budget = block
        .saturating_mul(2)
        .clamp(TOP_UP_WAIT_MIN_MS, TOP_UP_WAIT_MAX_MS);
    let pause_ms = (block / 2).clamp(500, 1_000);
    ReceiptWait {
        pause_ms,
        polls: u32::try_from(budget / pause_ms).unwrap_or(u32::MAX),
    }
}

/// The most often a submitted bundle's receipt is asked for, on any chain.
pub const RECEIPT_PACE_FLOOR_MS: u64 = 1_000;

/// How often the lane reconciler asks for a submitted bundle's receipt — and
/// how soon after the broadcast it first asks: a block, but not more often
/// than once a second, and never less often than the operator's
/// `VELA_RELAY_EXECUTOR_RECEIPT_POLL_SECS` (`configured_ms`). A chain not
/// listed in [`block_interval_ms`] keeps the configured interval.
///
/// The record says `included` only once the reconciler has seen the receipt,
/// and the wallet asks the relay for it. A flat 3 s, counted from when the
/// bundle was saved rather than sent, was up to three Avalanche blocks of
/// waiting after the bundle was already mined (vela-wallet #464).
pub fn receipt_pace_ms(chain_id: u64, configured_ms: u64) -> u64 {
    match block_interval_ms(chain_id) {
        Some(block) => block.max(RECEIPT_PACE_FLOOR_MS).min(configured_ms),
        None => configured_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::{ReceiptWait, TOP_UP_WAIT_MAX_MS, receipt_pace_ms, top_up_receipt_wait};

    #[test]
    fn a_receipt_is_asked_for_once_a_block_within_a_second_and_the_configured_interval() {
        // Avalanche: every block.
        assert_eq!(receipt_pace_ms(43_114, 3_000), 1_000);
        // Base, Polygon: every 2 s block.
        assert_eq!(receipt_pace_ms(8_453, 3_000), 2_000);
        assert_eq!(receipt_pace_ms(137, 3_000), 2_000);
        // Arbitrum's quarter-second blocks: once a second at most.
        assert_eq!(receipt_pace_ms(42_161, 3_000), 1_000);
        // Ethereum and Gnosis: never slower than the operator asked.
        assert_eq!(receipt_pace_ms(1, 3_000), 3_000);
        assert_eq!(receipt_pace_ms(100, 3_000), 3_000);
        // A chain nobody listed, and an operator who asked for faster.
        assert_eq!(receipt_pace_ms(999_999, 3_000), 3_000);
        assert_eq!(receipt_pace_ms(8_453, 1_500), 1_500);
    }

    #[test]
    fn a_top_up_is_waited_on_for_about_two_blocks_and_never_more_than_five_seconds() {
        let total = |wait: ReceiptWait| wait.pause_ms * u64::from(wait.polls);
        // Avalanche: two 1 s blocks, read every half second.
        assert_eq!(
            top_up_receipt_wait(43_114),
            ReceiptWait {
                pause_ms: 500,
                polls: 4,
            }
        );
        // Base: two 2 s blocks.
        assert_eq!(
            top_up_receipt_wait(8_453),
            ReceiptWait {
                pause_ms: 1_000,
                polls: 4,
            }
        );
        // Ethereum's two blocks would be 24 s: capped at five.
        assert_eq!(
            top_up_receipt_wait(1),
            ReceiptWait {
                pause_ms: 1_000,
                polls: 5,
            }
        );
        // Arbitrum's two blocks are half a second: the floor gives the
        // receipt time to reach the endpoints.
        assert_eq!(
            top_up_receipt_wait(42_161),
            ReceiptWait {
                pause_ms: 500,
                polls: 4,
            }
        );
        // A chain nobody listed is waited on like a 2 s chain.
        assert_eq!(top_up_receipt_wait(999_999), top_up_receipt_wait(8_453));
        for chain_id in [1, 10, 56, 100, 130, 137, 8_453, 42_161, 43_114, 4_217] {
            assert!(total(top_up_receipt_wait(chain_id)) <= TOP_UP_WAIT_MAX_MS);
        }
    }
}
