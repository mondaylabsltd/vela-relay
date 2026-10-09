//! One live operation per account nonce, decided at admission.
//!
//! An ERC-4337 account executes exactly one operation per nonce. Before this
//! rule the relay keyed admission on the userOpHash alone, so two different
//! operations at one nonce were both accepted and both answered with a hash;
//! one was bundled, and once it landed the other was rejected as a stale nonce.
//! A wallet told twice "sent" lost one payment without being asked which.
//!
//! Now every admission first claims the operation's **nonce slot**, keyed by
//! `(chainId, entryPoint, sender, nonce)` over the full 256-bit nonce (its
//! 192-bit key and 64-bit sequence), so keyed nonces are separate slots. The
//! slot names one holder. A different operation is refused while that holder
//! is [`HolderState::Live`]. Once the holder is final or abandoned, the
//! different operation takes the slot over, so a retry after a failure goes
//! through. The same operation re-submitted finds its own hash in the slot,
//! so admission stays idempotent.
//!
//! The store executes [`claim`], [`take_over`] and [`release`] atomically.
//! Docker runs one Redis Lua script per call; Cloudflare runs a single-threaded
//! RecordDO instance per slot. The decision about the holder ([`holder_state`])
//! is made here, between those calls, as compare-and-set: a takeover names
//! the holder it judged, and loses to anyone who changed the slot in between.
//! Two different operations racing for one slot therefore never both win.

use serde::{Deserialize, Serialize};

use crate::task::{StoredUserOperation, UserOperationStatus};

/// How long a claim stands without a usable record behind it. Covers an
/// admission between its claim and its queue acknowledgement, which takes one
/// store write and one queue append. A claim this old whose record is missing,
/// or still unadmitted and untouched by the executor, is an admission that
/// crashed, and the next operation at the nonce takes it over.
pub const ADMISSION_GRACE_MS: u64 = 120_000;

/// How long the store keeps a slot. This is garbage collection only: a slot
/// outlives the one-hour operation record it points at, and a stale holder is
/// judged by its record, never by the slot's own age.
pub const SLOT_TTL_MS: u64 = 2 * 3_600 * 1_000;

/// How many times one admission judges a holder and tries to take the slot
/// over before it gives up and refuses. Each retry means another admission
/// changed the slot in between.
pub const TAKEOVER_ATTEMPTS: usize = 3;

/// The opening of the refusal message: the wording, and the code `-32602`,
/// that the TypeScript bundler used for this case until 2026-07-19 (c03494d).
/// Shipped wallets read the `[existingHash:0x…]` marker that follows from
/// the raw error (`vela-core` `user_op::parse_existing_user_op_hash`). The
/// whole message is wire contract, byte for byte.
pub const NONCE_IN_FLIGHT_MESSAGE: &str = "Already have a pending UserOperation from this sender";

/// The machine reason in the refusal's `error.data.reason`.
pub const NONCE_IN_FLIGHT_REASON: &str = "nonce_in_flight";

/// The refusal message, carrying the marker exactly as wallets parse it.
pub fn nonce_in_flight_message(existing_user_operation_hash: &str) -> String {
    format!("{NONCE_IN_FLIGHT_MESSAGE} [existingHash:{existing_user_operation_hash}]")
}

/// The nonce an operation occupies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NonceSlot {
    pub chain_id: u64,
    /// Lowercase `0x` address.
    pub entry_point: String,
    /// Lowercase `0x` address.
    pub sender: String,
    /// The full nonce as `0x` and 64 lowercase hex digits.
    pub nonce: String,
}

impl NonceSlot {
    pub fn new(chain_id: u64, entry_point: [u8; 20], sender: [u8; 20], nonce: [u8; 32]) -> Self {
        Self {
            chain_id,
            entry_point: format!("0x{}", hex::encode(entry_point)),
            sender: format!("0x{}", hex::encode(sender)),
            nonce: format!("0x{}", hex::encode(nonce)),
        }
    }

    /// The store key suffix: Redis `vela:relay:nonce-slot:{key}`, and the
    /// RecordDO instance name `nonce:{key}`.
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.chain_id, self.entry_point, self.sender, self.nonce
        )
    }

    /// The nonce as a JSON-RPC quantity (`0x5`), for the refusal's `data`.
    pub fn nonce_quantity(&self) -> String {
        let digits = self.nonce.trim_start_matches("0x").trim_start_matches('0');
        if digits.is_empty() {
            "0x0".to_owned()
        } else {
            format!("0x{digits}")
        }
    }
}

/// Who holds a slot, and since when: the Redis hash fields
/// `userOperationHash` and `claimedAtMs` in docker, this struct's camelCase
/// JSON in a RecordDO slot instance on Cloudflare.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NonceHolder {
    pub user_operation_hash: String,
    /// The shell's clock when the claim was made.
    pub claimed_at_ms: u64,
}

/// What a claim or a takeover found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NonceClaim {
    /// The slot now names the claimant. `fresh` is false when it already
    /// did: the same operation submitted again.
    Claimed { fresh: bool },
    /// Another operation holds the slot.
    Held { holder: NonceHolder },
}

/// A store step: what to write (None = leave the slot as it is) and what to
/// answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotStep {
    pub write: Option<NonceHolder>,
    pub reply: NonceClaim,
}

/// Claim an empty slot, or recognise one the claimant already holds.
pub fn claim(current: Option<&NonceHolder>, claimant: &NonceHolder) -> SlotStep {
    match current {
        None => SlotStep {
            write: Some(claimant.clone()),
            reply: NonceClaim::Claimed { fresh: true },
        },
        Some(holder) if same_hash(holder, claimant) => SlotStep {
            write: None,
            reply: NonceClaim::Claimed { fresh: false },
        },
        Some(holder) => SlotStep {
            write: None,
            reply: NonceClaim::Held {
                holder: holder.clone(),
            },
        },
    }
}

/// Replace the holder the caller judged final, and only that holder. A slot
/// that changed since (someone else took it over) is answered as held by its
/// new holder; one that emptied (it expired) is claimed.
pub fn take_over(
    current: Option<&NonceHolder>,
    judged_user_operation_hash: &str,
    claimant: &NonceHolder,
) -> SlotStep {
    match current {
        Some(holder) if same_hash(holder, claimant) => SlotStep {
            write: None,
            reply: NonceClaim::Claimed { fresh: false },
        },
        Some(holder)
            if !holder
                .user_operation_hash
                .eq_ignore_ascii_case(judged_user_operation_hash) =>
        {
            SlotStep {
                write: None,
                reply: NonceClaim::Held {
                    holder: holder.clone(),
                },
            }
        }
        _ => SlotStep {
            write: Some(claimant.clone()),
            reply: NonceClaim::Claimed { fresh: true },
        },
    }
}

/// Whether to clear the slot: only while it still names this operation. An
/// admission that claimed a slot and then failed before its record existed
/// gives the slot back, so the next operation at the nonce is not refused for
/// one that never got in.
pub fn release(current: Option<&NonceHolder>, user_operation_hash: &str) -> bool {
    current.is_some_and(|holder| {
        holder
            .user_operation_hash
            .eq_ignore_ascii_case(user_operation_hash)
    })
}

fn same_hash(holder: &NonceHolder, claimant: &NonceHolder) -> bool {
    holder
        .user_operation_hash
        .eq_ignore_ascii_case(&claimant.user_operation_hash)
}

/// What the holder of a slot is, from its operation record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HolderState {
    /// Waiting, funding, held for fees, or in a bundle awaiting inclusion, or
    /// its admission is still running. A different operation is refused.
    Live,
    /// Included, rejected or failed. The nonce is free to try again.
    Final,
    /// No usable record ADMISSION_GRACE_MS after the claim: its record
    /// expired, or its admission crashed before the queue confirmed it.
    Abandoned,
}

/// Judge the holder of a slot. `record` is the holder's operation record,
/// `None` when the store has none.
pub fn holder_state(
    record: Option<&StoredUserOperation>,
    holder: &NonceHolder,
    now_ms: u64,
) -> HolderState {
    let within_grace = now_ms.saturating_sub(holder.claimed_at_ms) < ADMISSION_GRACE_MS;
    let Some(record) = record else {
        // Not written yet (an admission a few milliseconds ahead of this
        // one), or gone.
        return if within_grace {
            HolderState::Live
        } else {
            HolderState::Abandoned
        };
    };
    if record.status.is_terminal() {
        return HolderState::Final;
    }
    // A record the queue confirmed, or that the executor has already touched
    // (it reached the queue even if the acknowledgement was lost), is live
    // for as long as it is not terminal.
    let reached_the_executor = record.status != UserOperationStatus::Queued
        || record.last_executor_attempt_at_ms.is_some();
    if record.admitted || reached_the_executor || within_grace {
        HolderState::Live
    } else {
        HolderState::Abandoned
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ADMISSION_GRACE_MS, HolderState, NonceClaim, NonceHolder, NonceSlot, claim, holder_state,
        nonce_in_flight_message, release, take_over,
    };
    use crate::task::{
        QueuedUserOperation, StoredUserOperation, UserOperation, UserOperationStatus,
        UserOperationV0_7, queued_record,
    };

    const A: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const NOW: u64 = 1_760_000_000_000;

    fn holder(hash: &str, claimed_at_ms: u64) -> NonceHolder {
        NonceHolder {
            user_operation_hash: hash.into(),
            claimed_at_ms,
        }
    }

    fn record(status: UserOperationStatus, admitted: bool) -> StoredUserOperation {
        let mut record = queued_record(
            QueuedUserOperation {
                user_operation_hash: A.into(),
                chain_id: 1,
                entry_point: "0x0000000071727De22E5E9d8BAf0edAc6f37da032".into(),
                user_operation: UserOperation::V0_7(Box::new(UserOperationV0_7 {
                    sender: "0x1111111111111111111111111111111111111111".into(),
                    nonce: "0x5".into(),
                    factory: None,
                    factory_data: None,
                    call_data: "0x".into(),
                    call_gas_limit: "0x1".into(),
                    verification_gas_limit: "0x1".into(),
                    pre_verification_gas: "0x1".into(),
                    max_fee_per_gas: "0x0".into(),
                    max_priority_fee_per_gas: "0x0".into(),
                    paymaster: None,
                    paymaster_verification_gas_limit: None,
                    paymaster_post_op_gas_limit: None,
                    paymaster_data: None,
                    signature: "0x01".into(),
                    eip7702_auth: None,
                    fee_token: None,
                })),
            },
            admitted,
        );
        record.status = status;
        record
    }

    #[test]
    fn the_refusal_carries_the_marker_shipped_wallets_parse() {
        // Byte for byte the old TypeScript bundler's sentence (c03494d), and
        // the marker vela-core's `parse_existing_user_op_hash` reads:
        // "[existingHash:0x" + hex digits + "]".
        assert_eq!(
            nonce_in_flight_message(A),
            format!("Already have a pending UserOperation from this sender [existingHash:{A}]")
        );
        let message = nonce_in_flight_message(A);
        let at = message.find("[existingHash:0x").unwrap() + "[existingHash:0x".len();
        let rest = &message[at..];
        let digits = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
        assert_eq!(digits, 64);
        assert_eq!(rest.as_bytes()[digits], b']');
        // Words a wallet maps to something else must never appear.
        for forbidden in ["AA25", "currently processing", "Retry later"] {
            assert!(!message.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn a_slot_is_keyed_by_chain_entry_point_sender_and_the_whole_nonce() {
        let mut nonce = [0u8; 32];
        nonce[31] = 5;
        let slot = NonceSlot::new(8453, [0x11; 20], [0xAB; 20], nonce);
        assert_eq!(
            slot.key(),
            format!(
                "8453:0x{}:0x{}:0x{}05",
                "11".repeat(20),
                "ab".repeat(20),
                "0".repeat(62)
            )
        );
        assert_eq!(slot.nonce_quantity(), "0x5");
        // A keyed nonce (192-bit key 1, sequence 5) is a different slot.
        let mut keyed = nonce;
        keyed[23] = 1;
        let keyed = NonceSlot::new(8453, [0x11; 20], [0xAB; 20], keyed);
        assert_ne!(keyed.key(), slot.key());
        assert_eq!(keyed.nonce_quantity(), "0x10000000000000005");
        assert_eq!(
            NonceSlot::new(1, [0; 20], [0; 20], [0; 32]).nonce_quantity(),
            "0x0"
        );
    }

    #[test]
    fn a_claim_takes_an_empty_slot_and_recognises_its_own() {
        let a = holder(A, NOW);
        let step = claim(None, &a);
        assert_eq!(step.write, Some(a.clone()));
        assert_eq!(step.reply, NonceClaim::Claimed { fresh: true });

        // The same operation again: nothing written, not fresh.
        let again = claim(Some(&holder(A, NOW - 5_000)), &holder(A, NOW));
        assert_eq!(again.write, None);
        assert_eq!(again.reply, NonceClaim::Claimed { fresh: false });

        // Another operation finds A.
        let other = claim(Some(&a), &holder(B, NOW));
        assert_eq!(other.write, None);
        assert_eq!(other.reply, NonceClaim::Held { holder: a });
    }

    #[test]
    fn two_claims_racing_for_one_slot_never_both_win() {
        // The store applies claims one at a time; whichever goes second sees
        // the first.
        let a = holder(A, NOW);
        let b = holder(B, NOW);
        let first = claim(None, &a);
        let slot = first.write.clone();
        let second = claim(slot.as_ref(), &b);
        assert_eq!(first.reply, NonceClaim::Claimed { fresh: true });
        assert_eq!(second.reply, NonceClaim::Held { holder: a.clone() });
        assert_eq!(second.write, None);

        // And the loser's record lookup finds nothing yet: the winner is
        // still within its admission, so it is live and the loser refused.
        assert_eq!(holder_state(None, &a, NOW + 3), HolderState::Live);
    }

    #[test]
    fn a_takeover_replaces_only_the_holder_it_judged() {
        let a = holder(A, NOW - 10_000);
        let c = holder(C, NOW);
        // A judged final: C takes it.
        let step = take_over(Some(&a), A, &c);
        assert_eq!(step.write, Some(c.clone()));
        assert_eq!(step.reply, NonceClaim::Claimed { fresh: true });

        // B took the slot over first: C's takeover of A loses to B.
        let b = holder(B, NOW - 1);
        let lost = take_over(Some(&b), A, &c);
        assert_eq!(lost.write, None);
        assert_eq!(lost.reply, NonceClaim::Held { holder: b });

        // The slot expired in between: C claims it.
        let expired = take_over(None, A, &c);
        assert_eq!(expired.write, Some(c.clone()));
        assert_eq!(expired.reply, NonceClaim::Claimed { fresh: true });

        // C already holds it: nothing to do.
        let own = take_over(Some(&c), A, &c);
        assert_eq!(own.write, None);
        assert_eq!(own.reply, NonceClaim::Claimed { fresh: false });
    }

    #[test]
    fn a_release_clears_only_the_releasers_own_slot() {
        assert!(release(Some(&holder(A, NOW)), A));
        assert!(!release(Some(&holder(B, NOW)), A));
        assert!(!release(None, A));
    }

    #[test]
    fn a_holder_in_flight_is_live_and_a_settled_one_is_final() {
        let claimed = holder(A, NOW - 30_000);
        for status in [
            UserOperationStatus::Queued,
            UserOperationStatus::NotSubmitted,
            UserOperationStatus::Submitted,
        ] {
            assert_eq!(
                holder_state(Some(&record(status, true)), &claimed, NOW),
                HolderState::Live,
                "{status:?}"
            );
        }
        for status in [
            UserOperationStatus::Included,
            UserOperationStatus::Rejected,
            UserOperationStatus::Failed,
        ] {
            assert_eq!(
                holder_state(Some(&record(status, true)), &claimed, NOW),
                HolderState::Final,
                "{status:?}"
            );
        }
        // A live operation stays live long after its claim: a fee hold lasts
        // about 35 minutes, a bundle can wait longer.
        let old = holder(A, NOW - 50 * 60_000);
        assert_eq!(
            holder_state(Some(&record(UserOperationStatus::Queued, true)), &old, NOW),
            HolderState::Live
        );
    }

    #[test]
    fn an_admission_that_never_finished_stops_holding_the_nonce_after_the_grace() {
        let young = holder(A, NOW - (ADMISSION_GRACE_MS - 1));
        let old = holder(A, NOW - ADMISSION_GRACE_MS);
        // No record at all: written in a moment, or never.
        assert_eq!(holder_state(None, &young, NOW), HolderState::Live);
        assert_eq!(holder_state(None, &old, NOW), HolderState::Abandoned);

        // A record the queue never confirmed, untouched by the executor.
        let unconfirmed = record(UserOperationStatus::Queued, false);
        assert_eq!(
            holder_state(Some(&unconfirmed), &young, NOW),
            HolderState::Live
        );
        assert_eq!(
            holder_state(Some(&unconfirmed), &old, NOW),
            HolderState::Abandoned
        );

        // The executor touched it, so the append did land: live.
        let mut touched = unconfirmed.clone();
        touched.last_executor_attempt_at_ms = Some(NOW - 1_000);
        assert_eq!(holder_state(Some(&touched), &old, NOW), HolderState::Live);
        let mut submitted = unconfirmed;
        submitted.status = UserOperationStatus::Submitted;
        assert_eq!(holder_state(Some(&submitted), &old, NOW), HolderState::Live);
    }

    #[test]
    fn the_holder_is_stored_under_the_field_names_of_the_docker_hash() {
        // The Cloudflare slot stores this JSON; the docker slot is a Redis
        // hash with the same two field names.
        let json = serde_json::to_string(&holder(A, 1_760_000_000_123)).unwrap();
        assert_eq!(
            json,
            format!(r#"{{"userOperationHash":"{A}","claimedAtMs":1760000000123}}"#)
        );
    }
}
