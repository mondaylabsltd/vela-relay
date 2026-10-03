//! The single authoritative UserOperation lifecycle state machine.
//!
//! Every durable status write flows through the decisions here. The shell's
//! Redis scripts perform only mechanical guarded writes: they verify that the
//! stored state still equals the state a decision was computed against and
//! apply the merge; whether a transition is legal is decided in this module,
//! nowhere else.
//!
//! Behavior is lifted 1:1 from the pre-split Lua tables
//! (`PATCH_RECORD_SCRIPT`, `MARK_BUNDLE_SUBMITTED_SCRIPT`); see
//! `specs/001-crux-core-split/data-model.md` §2.

use crate::task::{StoredUserOperation, UserOperationStatus};

/// The transition table. Same-status writes are always legal field merges;
/// terminal states (and the API-only `NotFound`) have no outgoing transitions.
pub fn transition_is_allowed(current: UserOperationStatus, next: UserOperationStatus) -> bool {
    use UserOperationStatus::{Failed, Included, NotSubmitted, Queued, Rejected, Submitted};

    current == next
        || matches!(
            (current, next),
            (Queued, NotSubmitted | Submitted | Rejected | Failed)
                | (NotSubmitted, Submitted | Rejected | Failed)
                | (Submitted, Included | Rejected | Failed)
        )
}

/// Outcome of judging one status patch against the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchDecision {
    /// The patch may be merged into the record (guarded on `current`).
    Apply,
    /// The requested transition is illegal; the record must stay untouched.
    RefuseIllegalTransition,
}

/// Judge a record patch. `requested` is the patch's `status` field, if any; a
/// patch without a status change (or restating the current status) is always a
/// legal field merge.
pub fn decide_patch(
    current: UserOperationStatus,
    requested: Option<UserOperationStatus>,
) -> PatchDecision {
    match requested {
        Some(next) if !transition_is_allowed(current, next) => {
            PatchDecision::RefuseIllegalTransition
        }
        _ => PatchDecision::Apply,
    }
}

/// Outcome of judging one bundle member when a handleOps transaction is
/// submitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleSubmissionDecision {
    /// `queued`/`not_submitted` on the right chain: becomes `submitted` with
    /// the bundle's transaction hash and `admitted = true`, and joins the
    /// bundle index.
    Transition,
    /// Already `submitted` with the same transaction hash: joins the bundle
    /// index again without mutation (idempotent producer retry).
    IndexOnly,
    /// Wrong chain, terminal, or submitted under a different transaction:
    /// left untouched and unindexed.
    Skip,
}

/// Lua's `%.14g` `tostring` renders integers with at most 14 significant
/// digits exactly; beyond that the pre-split script deliberately failed
/// closed instead of aliasing chains. Preserved so legacy records (no
/// `chainIdText`) keep refusing outside that range.
const LEGACY_CHAIN_ID_EXACT_LIMIT: u64 = 100_000_000_000_000;

/// Judge one stored record against a submitted bundle. `chain_id_text` is the
/// record's decimal-text chain id (empty for legacy records, which fall back
/// to the numeric field within Lua's canonical-render range, fail-closed
/// beyond it).
pub fn decide_bundle_submission(
    status: UserOperationStatus,
    record_transaction_hash: Option<&str>,
    record_chain_id: u64,
    record_chain_id_text: &str,
    bundle_chain_id: u64,
    bundle_transaction_hash: &str,
) -> BundleSubmissionDecision {
    let same_chain = if record_chain_id_text.is_empty() {
        record_chain_id == bundle_chain_id && record_chain_id < LEGACY_CHAIN_ID_EXACT_LIMIT
    } else {
        record_chain_id_text == bundle_chain_id.to_string()
    };
    if !same_chain {
        return BundleSubmissionDecision::Skip;
    }
    match status {
        UserOperationStatus::Queued | UserOperationStatus::NotSubmitted => {
            BundleSubmissionDecision::Transition
        }
        UserOperationStatus::Submitted
            if record_transaction_hash == Some(bundle_transaction_hash) =>
        {
            BundleSubmissionDecision::IndexOnly
        }
        _ => BundleSubmissionDecision::Skip,
    }
}

/// The executor stage a dead-lettered operation is left at.
pub const DEAD_LETTER_STAGE: &str = "dead_letter";

/// What a queue message the platform dead-lettered — its redeliveries spent —
/// does to its record: an operation that never left the relay becomes
/// `rejected`, so the wallet stops waiting and says it was not sent.
///
/// Before this nothing read the dead-letter queue. The record stayed `queued`
/// until it expired, and the wallet showed "taking longer than usual" for an
/// operation no one would ever send (Arbitrum, 2026-10-03).
///
/// Only `queued` and `not_submitted` give up. A `submitted` operation may
/// still land — its bundle is on the network — and the receipt check settles
/// it; a terminal one is already settled. `None` = leave the record alone.
/// The patch is the store's field map (camelCase), applied through
/// [`decide_patch`] like any other.
pub fn dead_letter_patch(record: &StoredUserOperation, now_ms: u64) -> Option<serde_json::Value> {
    if !matches!(
        record.status,
        UserOperationStatus::Queued | UserOperationStatus::NotSubmitted
    ) {
        return None;
    }
    let reason = match record.last_executor_error.as_deref() {
        Some(last) if !last.is_empty() => format!(
            "the relay stopped retrying this operation without sending it; last error: {last}"
        ),
        _ => "the relay stopped retrying this operation without sending it".to_owned(),
    };
    Some(serde_json::json!({
        "status": "rejected",
        "lastExecutorStage": DEAD_LETTER_STAGE,
        "lastExecutorError": crate::task::truncate_diagnostic(&reason, 512),
        "lastExecutorAttemptAtMs": now_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        BundleSubmissionDecision, PatchDecision, decide_bundle_submission, decide_patch,
        transition_is_allowed,
    };
    use crate::task::UserOperationStatus::{
        self, Failed, Included, NotFound, NotSubmitted, Queued, Rejected, Submitted,
    };

    const EVERY_STATUS: [UserOperationStatus; 7] = [
        NotFound,
        Queued,
        NotSubmitted,
        Submitted,
        Rejected,
        Included,
        Failed,
    ];

    #[test]
    fn status_transition_matrix_is_monotonic() {
        assert!(transition_is_allowed(Queued, NotSubmitted));
        assert!(transition_is_allowed(Queued, Submitted));
        assert!(transition_is_allowed(Queued, Rejected));
        assert!(transition_is_allowed(Queued, Failed));
        assert!(transition_is_allowed(NotSubmitted, Submitted));
        assert!(transition_is_allowed(NotSubmitted, Rejected));
        assert!(transition_is_allowed(NotSubmitted, Failed));
        assert!(transition_is_allowed(Submitted, Included));
        assert!(transition_is_allowed(Submitted, Rejected));
        assert!(transition_is_allowed(Submitted, Failed));

        for terminal in [Rejected, Included, Failed] {
            for next in EVERY_STATUS {
                assert_eq!(
                    transition_is_allowed(terminal, next),
                    terminal == next,
                    "terminal {terminal:?} must not transition to {next:?}"
                );
            }
        }
        assert!(!transition_is_allowed(Submitted, Queued));
        assert!(!transition_is_allowed(NotSubmitted, Queued));
        assert!(!transition_is_allowed(Queued, Included));
        assert!(!transition_is_allowed(NotFound, Queued));
    }

    #[test]
    fn same_status_patches_are_always_field_merges() {
        for status in EVERY_STATUS {
            assert!(transition_is_allowed(status, status));
            assert_eq!(decide_patch(status, Some(status)), PatchDecision::Apply);
            assert_eq!(decide_patch(status, None), PatchDecision::Apply);
        }
    }

    #[test]
    fn illegal_patches_are_refused() {
        for terminal in [Rejected, Included, Failed] {
            for next in EVERY_STATUS {
                if next != terminal {
                    assert_eq!(
                        decide_patch(terminal, Some(next)),
                        PatchDecision::RefuseIllegalTransition
                    );
                }
            }
        }
        assert_eq!(
            decide_patch(Submitted, Some(Queued)),
            PatchDecision::RefuseIllegalTransition
        );
    }

    #[test]
    fn terminal_and_durable_predicates_stay_distinct() {
        for status in EVERY_STATUS {
            assert_eq!(
                status.is_terminal(),
                matches!(status, Rejected | Included | Failed)
            );
            assert_eq!(
                status.is_durable(),
                matches!(status, Submitted | Rejected | Included | Failed)
            );
        }
    }

    #[test]
    fn bundle_submission_transitions_only_pre_submission_members_on_the_same_chain() {
        for status in [Queued, NotSubmitted] {
            assert_eq!(
                decide_bundle_submission(status, None, 42161, "42161", 42161, "0xbundle"),
                BundleSubmissionDecision::Transition
            );
        }
        assert_eq!(
            decide_bundle_submission(
                Submitted,
                Some("0xbundle"),
                42161,
                "42161",
                42161,
                "0xbundle"
            ),
            BundleSubmissionDecision::IndexOnly
        );
        assert_eq!(
            decide_bundle_submission(
                Submitted,
                Some("0xother"),
                42161,
                "42161",
                42161,
                "0xbundle"
            ),
            BundleSubmissionDecision::Skip
        );
        for status in [Rejected, Included, Failed, NotFound] {
            assert_eq!(
                decide_bundle_submission(
                    status,
                    Some("0xbundle"),
                    42161,
                    "42161",
                    42161,
                    "0xbundle"
                ),
                BundleSubmissionDecision::Skip
            );
        }
    }

    #[test]
    fn bundle_submission_chain_comparison_uses_decimal_text_with_fail_closed_legacy_fallback() {
        assert_eq!(
            decide_bundle_submission(Queued, None, 42161, "42161", 1, "0xbundle"),
            BundleSubmissionDecision::Skip,
            "text chain mismatch must skip"
        );
        assert_eq!(
            decide_bundle_submission(Queued, None, 42161, "", 42161, "0xbundle"),
            BundleSubmissionDecision::Transition,
            "legacy numeric fallback matches exact small chain ids"
        );
        assert_eq!(
            decide_bundle_submission(Queued, None, 42161, "", 1, "0xbundle"),
            BundleSubmissionDecision::Skip
        );
        let beyond_canonical = 100_000_000_000_000;
        assert_eq!(
            decide_bundle_submission(
                Queued,
                None,
                beyond_canonical,
                "",
                beyond_canonical,
                "0xbundle"
            ),
            BundleSubmissionDecision::Skip,
            "legacy records beyond Lua's canonical render range fail closed"
        );
        assert_eq!(
            decide_bundle_submission(
                Queued,
                None,
                beyond_canonical,
                "100000000000000",
                beyond_canonical,
                "0xbundle"
            ),
            BundleSubmissionDecision::Transition,
            "text-carrying records are exact at any magnitude"
        );
    }

    #[test]
    fn a_dead_lettered_operation_that_never_left_the_relay_is_rejected() {
        use super::{DEAD_LETTER_STAGE, PatchDecision, dead_letter_patch, decide_patch};
        use crate::task::{StoredUserOperation, UserOperationStatus};

        let record = |status: UserOperationStatus, last: Option<&str>| {
            let mut record: StoredUserOperation = serde_json::from_value(serde_json::json!({
                "status": "queued",
                "transactionHash": null,
                "chainId": 42161,
                "entryPoint": "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
                "userOperation": {
                    "sender": "0x1111111111111111111111111111111111111111",
                    "nonce": "0x0",
                    "factory": null,
                    "factoryData": null,
                    "callData": "0x",
                    "callGasLimit": "0x1",
                    "verificationGasLimit": "0x1",
                    "preVerificationGas": "0x1",
                    "maxFeePerGas": "0x0",
                    "maxPriorityFeePerGas": "0x0",
                    "paymaster": null,
                    "paymasterVerificationGasLimit": null,
                    "paymasterPostOpGasLimit": null,
                    "paymasterData": null,
                    "signature": "0x1234",
                    "eip7702Auth": null
                },
                "admitted": true,
                "blockHash": null,
                "blockNumber": null,
                "receipt": null,
                "event": null,
            }))
            .unwrap();
            record.status = status;
            record.last_executor_error = last.map(str::to_owned);
            record
        };

        // The Arbitrum operation of 2026-10-03, waiting on a top-up forever.
        let stuck = record(
            UserOperationStatus::Queued,
            Some("waiting for relayer funding transaction confirmation"),
        );
        let patch = dead_letter_patch(&stuck, 1_791_031_695_572).unwrap();
        assert_eq!(patch["status"], "rejected");
        assert_eq!(patch["lastExecutorStage"], DEAD_LETTER_STAGE);
        assert_eq!(
            patch["lastExecutorError"],
            "the relay stopped retrying this operation without sending it; last error: waiting for relayer funding transaction confirmation"
        );
        assert_eq!(patch["lastExecutorAttemptAtMs"], 1_791_031_695_572_u64);
        // ...and the store's own table lets it through.
        assert_eq!(
            decide_patch(
                UserOperationStatus::Queued,
                Some(UserOperationStatus::Rejected)
            ),
            PatchDecision::Apply
        );
        assert_eq!(
            dead_letter_patch(&record(UserOperationStatus::NotSubmitted, None), 1).unwrap()["lastExecutorError"],
            "the relay stopped retrying this operation without sending it"
        );

        // A bundle on the network may still land; a settled one stays settled.
        for status in [
            UserOperationStatus::Submitted,
            UserOperationStatus::Included,
            UserOperationStatus::Rejected,
            UserOperationStatus::Failed,
        ] {
            assert_eq!(
                dead_letter_patch(&record(status, None), 1),
                None,
                "{status:?}"
            );
        }
    }
}
