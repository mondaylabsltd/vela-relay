//! How fast a chain makes blocks, and the waits the relay measures in blocks.
//!
//! A wait for something to be mined is wrong at both ends when it is a flat
//! number of seconds: three seconds is three Avalanche blocks and a quarter of
//! an Ethereum one. The core decides how long; the shell sleeps.

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

#[cfg(test)]
mod tests {
    use super::{ReceiptWait, TOP_UP_WAIT_MAX_MS, top_up_receipt_wait};

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
