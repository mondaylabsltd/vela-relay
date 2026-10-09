//! Why an operation did not go through, as a code a wallet can put into words.
//!
//! The status record has always carried `last_executor_stage` and
//! `last_executor_error`, but the error is a sentence for operators and the
//! stage is too coarse: `in_band_settlement` covers a payment the market
//! outran, a payment under the minimum, and a payment that could not be read.
//! A wallet that keys its wording on the stage told every refusal as "network
//! fees stayed above the amount you approved".
//!
//! Every terminal rejection now records one [`RejectionReason`], and
//! `pimlico_getUserOperationStatus` returns it as `rejection_reason` for a
//! `rejected` or `failed` operation. A record written before the code existed
//! gets one derived from its stage ([`RejectionReason::of_record`]). The
//! codes and the plain words they stand for are listed in
//! `src/app/rpc/handlers/README.md`.

use serde::{Deserialize, Serialize};

use crate::task::{StoredUserOperation, UserOperationStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    /// Refused at submission: another operation of the account is still in
    /// flight at this nonce (`nonce_slot`). Only ever the `data.reason` of the
    /// synchronous refusal, never a record's.
    NonceInFlight,
    /// The network's fees stayed above what the signed fee covers, through
    /// the whole hold.
    FeeBelowMarket,
    /// The signed fee is below the relay's minimum (`$0.01`, `docs/fees.md` §1).
    FeeBelowMinimum,
    /// The fee payment is missing, unreadable, in an unsupported combination,
    /// or not proven by the transfer logs.
    FeePaymentInvalid,
    /// Another operation of the account already used this nonce on-chain.
    NonceUsed,
    /// The operation fails when simulated: it would revert.
    SimulationFailed,
    /// The queued payload is malformed.
    InvalidOperation,
    /// Tempo: a fee token other than pathUSD.
    UnsupportedFeeToken,
    /// The relay stopped retrying without sending it (dead letter).
    RelayGaveUp,
    /// Mined, but its execution failed.
    RevertedOnchain,
    /// The whole bundle transaction reverted.
    BundleFailed,
    /// A reason this relay version does not know.
    Unknown,
}

impl RejectionReason {
    pub const ALL: [Self; 12] = [
        Self::NonceInFlight,
        Self::FeeBelowMarket,
        Self::FeeBelowMinimum,
        Self::FeePaymentInvalid,
        Self::NonceUsed,
        Self::SimulationFailed,
        Self::InvalidOperation,
        Self::UnsupportedFeeToken,
        Self::RelayGaveUp,
        Self::RevertedOnchain,
        Self::BundleFailed,
        Self::Unknown,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NonceInFlight => "nonce_in_flight",
            Self::FeeBelowMarket => "fee_below_market",
            Self::FeeBelowMinimum => "fee_below_minimum",
            Self::FeePaymentInvalid => "fee_payment_invalid",
            Self::NonceUsed => "nonce_used",
            Self::SimulationFailed => "simulation_failed",
            Self::InvalidOperation => "invalid_operation",
            Self::UnsupportedFeeToken => "unsupported_fee_token",
            Self::RelayGaveUp => "relay_gave_up",
            Self::RevertedOnchain => "reverted_onchain",
            Self::BundleFailed => "bundle_failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value)
            .unwrap_or(Self::Unknown)
    }

    /// The reason a record reports, `None` while it is not rejected or failed.
    ///
    /// The stored code wins. A record without one (written before the code
    /// existed, or rejected on-chain by the receipt reconciler) is judged by
    /// what it carries: a receipt means it was mined and failed; otherwise
    /// the executor stage that rejected it.
    pub fn of_record(record: &StoredUserOperation) -> Option<Self> {
        match record.status {
            UserOperationStatus::Failed => Some(Self::BundleFailed),
            UserOperationStatus::Rejected => Some(
                record
                    .rejection_reason
                    .as_deref()
                    .map(Self::parse)
                    .unwrap_or_else(|| {
                        if record.receipt.is_some() || record.event.is_some() {
                            Self::RevertedOnchain
                        } else {
                            Self::of_stage(record.last_executor_stage.as_deref())
                        }
                    }),
            ),
            _ => None,
        }
    }

    /// The reason a stage name implies, for records that carry no code.
    /// `in_band_settlement` meant a market shortfall in the overwhelming
    /// majority of the records written before codes existed.
    pub fn of_stage(stage: Option<&str>) -> Self {
        match stage {
            Some("nonce") => Self::NonceUsed,
            Some("simulation") => Self::SimulationFailed,
            Some(crate::lifecycle::DEAD_LETTER_STAGE) => Self::RelayGaveUp,
            Some("queue") => Self::InvalidOperation,
            Some("tempo_fee_token") => Self::UnsupportedFeeToken,
            Some("in_band_settlement") => Self::FeeBelowMarket,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RejectionReason;
    use crate::task::{
        QueuedUserOperation, UserOperation, UserOperationEvent, UserOperationStatus,
        UserOperationV0_7, queued_record,
    };

    fn record(status: UserOperationStatus) -> crate::task::StoredUserOperation {
        let mut record = queued_record(
            QueuedUserOperation {
                user_operation_hash: "0xab".into(),
                chain_id: 1,
                entry_point: "0x0000000071727De22E5E9d8BAf0edAc6f37da032".into(),
                user_operation: UserOperation::V0_7(Box::new(UserOperationV0_7 {
                    sender: "0x1111111111111111111111111111111111111111".into(),
                    nonce: "0x0".into(),
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
            true,
        );
        record.status = status;
        record
    }

    #[test]
    fn every_code_round_trips_through_its_wire_name() {
        for reason in RejectionReason::ALL {
            assert_eq!(RejectionReason::parse(reason.as_str()), reason);
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::Value::String(reason.as_str().to_owned())
            );
        }
        assert_eq!(
            RejectionReason::parse("a_code_from_a_newer_relay"),
            RejectionReason::Unknown
        );
    }

    #[test]
    fn only_a_rejected_or_failed_record_has_a_reason() {
        for status in [
            UserOperationStatus::Queued,
            UserOperationStatus::NotSubmitted,
            UserOperationStatus::Submitted,
            UserOperationStatus::Included,
        ] {
            assert_eq!(RejectionReason::of_record(&record(status)), None);
        }
        assert_eq!(
            RejectionReason::of_record(&record(UserOperationStatus::Failed)),
            Some(RejectionReason::BundleFailed)
        );
    }

    #[test]
    fn a_stored_code_wins_and_an_old_record_is_judged_by_its_stage() {
        let mut stored = record(UserOperationStatus::Rejected);
        stored.last_executor_stage = Some("in_band_settlement".into());
        stored.rejection_reason = Some("fee_below_minimum".into());
        assert_eq!(
            RejectionReason::of_record(&stored),
            Some(RejectionReason::FeeBelowMinimum)
        );

        for (stage, reason) in [
            ("nonce", RejectionReason::NonceUsed),
            ("simulation", RejectionReason::SimulationFailed),
            ("dead_letter", RejectionReason::RelayGaveUp),
            ("queue", RejectionReason::InvalidOperation),
            ("tempo_fee_token", RejectionReason::UnsupportedFeeToken),
            ("in_band_settlement", RejectionReason::FeeBelowMarket),
            ("something_new", RejectionReason::Unknown),
        ] {
            let mut old = record(UserOperationStatus::Rejected);
            old.last_executor_stage = Some(stage.into());
            assert_eq!(RejectionReason::of_record(&old), Some(reason), "{stage}");
        }
    }

    #[test]
    fn a_rejection_with_a_receipt_failed_on_chain() {
        let mut mined = record(UserOperationStatus::Rejected);
        mined.receipt = Some(serde_json::json!({ "status": "0x1" }));
        mined.event = Some(UserOperationEvent {
            user_operation_hash: "0xab".into(),
            success: false,
            actual_gas_cost: "0x1".into(),
            actual_gas_used: "0x1".into(),
        });
        assert_eq!(
            RejectionReason::of_record(&mined),
            Some(RejectionReason::RevertedOnchain)
        );
    }
}
