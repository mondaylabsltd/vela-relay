//! Serde fetch protocol between the worker handlers and the Durable Objects.
//! The DOs are part of the shell; the payloads reuse the core's frozen store
//! vocabulary so record bytes stay shape-identical to the docker shell's.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vela_relay_core::nonce_slot::{NonceClaim, NonceHolder};
use vela_relay_core::task::{
    PreparedFundingIntent, QueuedUserOperation, RoutedUserOperation, StoredUserOperation,
};

/// One command per RecordDO invocation (POST body).
#[derive(Serialize, Deserialize)]
pub enum RecordCommand {
    /// Create-if-absent — the SETNX of the docker store's `create_queued`;
    /// the DO builds the record via the core's `queued_record`.
    CreateQueued {
        operation: QueuedUserOperation,
    },
    /// Create-if-absent with `admitted = true` — the docker store's
    /// `restore_queued_from_durable_payload`.
    RestoreQueued {
        operation: QueuedUserOperation,
    },
    Get,
    MarkAdmitted,
    /// The docker store's `patch`: a field map, optionally carrying a status
    /// transition judged by `lifecycle::decide_patch`. Timestamps arrive
    /// pre-computed (`now_ms`-bearing patches) — the DO injects no clock into
    /// decisions (Constitution II).
    Patch {
        patch: Value,
    },
    /// One member of `mark_bundle_submitted`: `decide_bundle_submission`
    /// applied to this record (Transition merges status/transactionHash/
    /// admitted exactly as the docker Lua's 't' arm).
    MarkBundleMemberSubmitted {
        bundle_chain_id: u64,
        transaction_hash: String,
    },
    /// The platform dead-lettered this operation's queue message: the core's
    /// `lifecycle::dead_letter_patch` decides whether the record gives up
    /// (only one that never left the relay does). Replies `Patched`.
    GiveUp {
        now_ms: u64,
    },
    /// On a nonce-slot instance (`nonce:{slot}`): the core's
    /// `nonce_slot::claim`, applied atomically. Replies `Nonce`.
    ClaimNonce {
        holder: NonceHolder,
    },
    /// On a nonce-slot instance: `nonce_slot::take_over`, replacing only the
    /// holder the core judged. Replies `Nonce`.
    TakeOverNonce {
        judged_user_operation_hash: String,
        holder: NonceHolder,
    },
    /// On a nonce-slot instance: `nonce_slot::release`. Replies `Released`.
    ReleaseNonce {
        user_operation_hash: String,
    },
}

#[derive(Serialize, Deserialize)]
pub enum RecordReply {
    Created {
        created: bool,
    },
    Record {
        record: Option<Box<StoredUserOperation>>,
    },
    Marked {
        marked: bool,
    },
    Patched {
        patched: bool,
    },
    Indexed {
        indexed: bool,
    },
    Nonce {
        claim: NonceClaim,
    },
    Released {
        released: bool,
    },
}

/// One command per LaneDO invocation.
#[derive(Serialize, Deserialize)]
pub enum LaneCommand {
    /// Drive one lane batch through the core's `ExecutionApp`.
    ExecuteBatch {
        operations: Vec<RoutedUserOperation>,
    },
}

/// Serde mirror of `execution::ItemResolution` for the DO protocol.
#[derive(Serialize, Deserialize)]
pub enum ItemResolutionWire {
    Durable,
    Failed { reason: String },
}

#[derive(Serialize, Deserialize)]
pub enum LaneReply {
    Resolutions {
        resolutions: Vec<ItemResolutionWire>,
    },
}

/// A lease holder's identity: the unique acquisition token plus the TTL it
/// keeps renewing with (docker store `acquire_lease`/`renew_lease` args).
#[derive(Clone, Serialize, Deserialize)]
pub struct LeaseIdentity {
    pub token: String,
    pub ttl_ms: u64,
}

/// One command per TreasuryDO invocation. `renew` piggybacks a lease renewal
/// on every touch from the current holder — the docker shell's background
/// heartbeat task, replaced by renewal-on-touch (declared in
/// contracts/platform-bindings.md).
#[derive(Serialize, Deserialize)]
pub struct TreasuryRequest {
    pub renew: Option<LeaseIdentity>,
    pub command: TreasuryCommand,
}

#[derive(Serialize, Deserialize)]
pub enum TreasuryCommand {
    /// The docker store's `acquire_lease` (`SET NX PX`): succeeds only when
    /// the lease is unheld or expired.
    AcquireLease {
        lease: LeaseIdentity,
    },
    /// The docker store's `renew_lease`: extends only while `token` holds it.
    EnsureLease {
        lease: LeaseIdentity,
    },
    /// The docker store's `release_lease`: deletes only while `token` holds it.
    ReleaseLease {
        token: String,
    },
    LoadFunding,
    /// Put-if-absent — one pending treasury transfer per chain (docker
    /// `save_prepared_funding_intent`).
    SaveFunding {
        intent: PreparedFundingIntent,
    },
    /// Guarded delete — only while the stored intent still carries this
    /// transaction hash (docker `clear_prepared_funding_intent`).
    ClearFunding {
        transaction_hash: String,
    },
    /// One receipt prober per interval per transaction: an expiring throttle
    /// slot, never released (docker: `acquire_lease` with a unique token).
    AcquireReceiptProbe {
        transaction_hash: String,
        ttl_ms: u64,
    },
    /// Telegram alert suppression slot (docker `claim_executor_alert`,
    /// `SET NX PX cooldown`): the chain's coordinator is the natural home for
    /// its cross-lane alert dedup.
    ClaimAlert {
        fingerprint: String,
        token: String,
        ttl_ms: u64,
    },
    /// Token-guarded release so an undelivered alert can retry before the
    /// cooldown expires (docker `release_executor_alert`).
    ReleaseAlert {
        fingerprint: String,
        token: String,
    },
}

#[derive(Serialize, Deserialize)]
pub enum TreasuryReply {
    Acquired {
        acquired: bool,
    },
    Held {
        held: bool,
    },
    Released,
    Funding {
        intent: Option<PreparedFundingIntent>,
    },
    Saved {
        saved: bool,
    },
    Cleared {
        cleared: bool,
    },
}

impl From<vela_relay_core::execution::ItemResolution> for ItemResolutionWire {
    fn from(resolution: vela_relay_core::execution::ItemResolution) -> Self {
        match resolution {
            vela_relay_core::execution::ItemResolution::Durable => Self::Durable,
            vela_relay_core::execution::ItemResolution::Failed { reason } => {
                Self::Failed { reason }
            }
        }
    }
}
