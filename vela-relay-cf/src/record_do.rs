//! RecordDO — one Durable Object per user operation (`{chain_id}:{hash}`),
//! the strongly consistent home of its `StoredUserOperation` (data-model §1).
//!
//! Storage layout: `record` → the frozen camelCase record JSON;
//! `expiresAtMs` → the record TTL deadline (the docker store's 3600 s class).
//! The single alarm currently serves TTL cleanup; US3 packs the receipt-check
//! schedule into the same alarm as earliest-of.
//!
//! Guard semantics: the DO is single-threaded, so create-if-absent and
//! read-modify-write are race-free by construction — the platform supplies
//! what the docker shell builds from Redis SETNX and Lua CAS.
//!
//! The same class also hosts one instance per account nonce slot, named
//! `nonce:{chainId}:{entryPoint}:{sender}:{nonce}` (`nonce_slot`): `nonceHolder`
//! → the slot's holder, under the same `expiresAtMs` alarm. Its commands apply
//! the core's `claim`, `take_over` and `release` with no await between the read
//! and the write, so two admissions racing for a nonce are serialized here.

use worker::{
    Date, DurableObject, Env, Request, Response, Result, State, durable_object, wasm_bindgen,
};

use crate::proto::{RecordCommand, RecordReply};

/// Mirrors the docker store's `USER_OPERATION_TTL_SECS` record class.
const RECORD_TTL_MS: u64 = 3_600 * 1_000;

const RECORD_KEY: &str = "record";
const EXPIRES_KEY: &str = "expiresAtMs";
const NONCE_HOLDER_KEY: &str = "nonceHolder";

#[durable_object]
pub struct RecordDo {
    state: State,
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for RecordDo {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let command: RecordCommand = req.json().await?;
        let reply = match command {
            RecordCommand::CreateQueued { operation } => {
                let existing = self.record().await;
                if existing.is_some() {
                    RecordReply::Created { created: false }
                } else {
                    let record = vela_relay_core::task::queued_record(operation, false);
                    let now = Date::now().as_millis();
                    self.state.storage().put(RECORD_KEY, &record).await?;
                    self.state
                        .storage()
                        .put(EXPIRES_KEY, now + RECORD_TTL_MS)
                        .await?;
                    self.schedule_alarm().await?;
                    RecordReply::Created { created: true }
                }
            }
            RecordCommand::Get => RecordReply::Record {
                record: self.record().await.map(Box::new),
            },
            RecordCommand::MarkAdmitted => match self.record().await {
                None => RecordReply::Marked { marked: false },
                Some(mut record) => {
                    record.admitted = true;
                    self.state.storage().put(RECORD_KEY, &record).await?;
                    RecordReply::Marked { marked: true }
                }
            },
            RecordCommand::RestoreQueued { operation } => {
                if self.record().await.is_some() {
                    RecordReply::Created { created: false }
                } else {
                    let record = vela_relay_core::task::queued_record(operation, true);
                    let now = Date::now().as_millis();
                    self.state.storage().put(RECORD_KEY, &record).await?;
                    self.state
                        .storage()
                        .put(EXPIRES_KEY, now + RECORD_TTL_MS)
                        .await?;
                    self.schedule_alarm().await?;
                    RecordReply::Created { created: true }
                }
            }
            RecordCommand::Patch { patch } => RecordReply::Patched {
                patched: self.apply_patch(&patch).await?,
            },
            RecordCommand::GiveUp { now_ms } => RecordReply::Patched {
                patched: self.give_up(now_ms).await?,
            },
            RecordCommand::MarkBundleMemberSubmitted {
                bundle_chain_id,
                transaction_hash,
            } => RecordReply::Indexed {
                indexed: self
                    .apply_bundle_submission(bundle_chain_id, &transaction_hash)
                    .await?,
            },
            RecordCommand::ClaimNonce { holder } => {
                let current = self.nonce_holder().await?;
                let step = vela_relay_core::nonce_slot::claim(current.as_ref(), &holder);
                self.write_nonce_holder(step.write).await?;
                RecordReply::Nonce { claim: step.reply }
            }
            RecordCommand::TakeOverNonce {
                judged_user_operation_hash,
                holder,
            } => {
                let current = self.nonce_holder().await?;
                let step = vela_relay_core::nonce_slot::take_over(
                    current.as_ref(),
                    &judged_user_operation_hash,
                    &holder,
                );
                self.write_nonce_holder(step.write).await?;
                RecordReply::Nonce { claim: step.reply }
            }
            RecordCommand::ReleaseNonce {
                user_operation_hash,
            } => {
                let current = self.nonce_holder().await?;
                let released =
                    vela_relay_core::nonce_slot::release(current.as_ref(), &user_operation_hash);
                if released {
                    self.state.storage().delete(NONCE_HOLDER_KEY).await?;
                }
                RecordReply::Released { released }
            }
        };
        Response::from_json(&reply)
    }

    async fn alarm(&self) -> Result<Response> {
        let now = Date::now().as_millis();
        let expires_at: Option<u64> = self.state.storage().get(EXPIRES_KEY).await.ok().flatten();
        match expires_at {
            Some(expires_at) if now < expires_at => {
                // Re-arm for the remaining lifetime (alarms are earliest-of;
                // US3 adds the receipt-check schedule here).
                self.state
                    .storage()
                    .set_alarm(std::time::Duration::from_millis(expires_at - now))
                    .await?;
            }
            _ => {
                // TTL elapsed (or state is gone): drop everything, exactly as
                // the Redis TTL would. The empty object then evicts.
                self.state.storage().delete_all().await?;
            }
        }
        Response::empty()
    }
}

impl RecordDo {
    async fn record(&self) -> Option<vela_relay_core::task::StoredUserOperation> {
        self.state.storage().get(RECORD_KEY).await.ok().flatten()
    }

    /// A slot's holder. A read that fails is an error, never an empty slot:
    /// reading it as empty would let a second operation claim a held nonce.
    async fn nonce_holder(&self) -> Result<Option<vela_relay_core::nonce_slot::NonceHolder>> {
        self.state.storage().get(NONCE_HOLDER_KEY).await
    }

    /// Store a slot's new holder and keep the slot for `nonce_slot::SLOT_TTL_MS`
    /// (garbage collection only: a stale holder is judged by its record).
    async fn write_nonce_holder(
        &self,
        holder: Option<vela_relay_core::nonce_slot::NonceHolder>,
    ) -> Result<()> {
        let Some(holder) = holder else {
            return Ok(());
        };
        let ttl_ms = vela_relay_core::nonce_slot::SLOT_TTL_MS;
        self.state.storage().put(NONCE_HOLDER_KEY, &holder).await?;
        self.state
            .storage()
            .put(EXPIRES_KEY, Date::now().as_millis() + ttl_ms)
            .await?;
        self.state
            .storage()
            .set_alarm(std::time::Duration::from_millis(ttl_ms))
            .await
    }

    async fn schedule_alarm(&self) -> Result<()> {
        self.state
            .storage()
            .set_alarm(std::time::Duration::from_millis(RECORD_TTL_MS))
            .await
    }

    /// The docker store's `patch` semantics: an optional status transition
    /// judged by the core's single table, then a mechanical field merge. The
    /// DO's single thread supplies the guard the Redis Lua CAS supplies.
    async fn apply_patch(&self, patch: &serde_json::Value) -> Result<bool> {
        let Some(record) = self.record().await else {
            return Ok(false);
        };
        let requested = match patch.get("status") {
            None => None,
            Some(requested) => {
                let Ok(requested) = serde_json::from_value::<
                    vela_relay_core::task::UserOperationStatus,
                >(requested.clone()) else {
                    return Ok(false);
                };
                Some(requested)
            }
        };
        match vela_relay_core::lifecycle::decide_patch(record.status, requested) {
            vela_relay_core::lifecycle::PatchDecision::Apply => {}
            vela_relay_core::lifecycle::PatchDecision::RefuseIllegalTransition => {
                return Ok(false);
            }
        }
        let mut record_json = serde_json::to_value(&record)
            .map_err(|error| worker::Error::RustError(error.to_string()))?;
        let (Some(record_object), Some(patch_object)) =
            (record_json.as_object_mut(), patch.as_object())
        else {
            return Ok(false);
        };
        for (key, value) in patch_object {
            record_object.insert(key.clone(), value.clone());
        }
        let merged: vela_relay_core::task::StoredUserOperation =
            serde_json::from_value(record_json)
                .map_err(|error| worker::Error::RustError(error.to_string()))?;
        self.state.storage().put(RECORD_KEY, &merged).await?;
        Ok(true)
    }

    /// A dead-lettered operation gives up — when the core says so — and its
    /// record lives one more TTL from now, so a wallet that comes back within
    /// the hour reads "rejected" rather than nothing at all.
    async fn give_up(&self, now_ms: u64) -> Result<bool> {
        let Some(record) = self.record().await else {
            return Ok(false);
        };
        let Some(patch) = vela_relay_core::lifecycle::dead_letter_patch(&record, now_ms) else {
            return Ok(false);
        };
        if !self.apply_patch(&patch).await? {
            return Ok(false);
        }
        self.state
            .storage()
            .put(EXPIRES_KEY, now_ms + RECORD_TTL_MS)
            .await?;
        self.schedule_alarm().await?;
        Ok(true)
    }

    /// One member of the bundle-submitted index: the core decides, the DO
    /// applies the docker Lua's 't' merge (status/transactionHash/admitted).
    async fn apply_bundle_submission(
        &self,
        bundle_chain_id: u64,
        transaction_hash: &str,
    ) -> Result<bool> {
        let Some(mut record) = self.record().await else {
            return Ok(false);
        };
        match vela_relay_core::lifecycle::decide_bundle_submission(
            record.status,
            record.transaction_hash.as_deref(),
            record.chain_id,
            &record.chain_id_text,
            bundle_chain_id,
            transaction_hash,
        ) {
            vela_relay_core::lifecycle::BundleSubmissionDecision::Transition => {
                record.status = vela_relay_core::task::UserOperationStatus::Submitted;
                record.transaction_hash = Some(transaction_hash.to_owned());
                record.admitted = true;
                self.state.storage().put(RECORD_KEY, &record).await?;
                Ok(true)
            }
            vela_relay_core::lifecycle::BundleSubmissionDecision::IndexOnly => Ok(true),
            vela_relay_core::lifecycle::BundleSubmissionDecision::Skip => Ok(false),
        }
    }
}
