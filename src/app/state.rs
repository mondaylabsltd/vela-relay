use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use vela_relay_core::cost::BillingTerms;

use crate::gas_price::GasPriceManager;

use super::{queue::UserOperationQueue, user_operation_store::UserOperationStatusStore};

#[derive(Clone)]
pub struct AppState {
    gas_price: GasPriceManager,
    billing_terms: BillingTerms,
    readiness: Readiness,
    settlement_recipient: Option<String>,
    user_operation_queue: Option<UserOperationQueue>,
    user_operation_status_store: Option<UserOperationStatusStore>,
}

#[derive(Clone)]
pub struct Readiness {
    expected_jobs: Arc<[&'static str]>,
    ready_jobs: Arc<Mutex<HashSet<&'static str>>>,
}

impl AppState {
    pub fn with_settlement_recipient(
        expected_jobs: &[&'static str],
        settlement_recipient: Option<String>,
    ) -> Self {
        Self {
            gas_price: GasPriceManager::default(),
            billing_terms: BillingTerms::default(),
            readiness: Readiness {
                expected_jobs: Arc::from(expected_jobs),
                ready_jobs: Arc::new(Mutex::new(HashSet::new())),
            },
            settlement_recipient,
            user_operation_queue: None,
            user_operation_status_store: None,
        }
    }

    /// The executor's billing terms, published by the RPC exactly as the
    /// executor applies them (`settlementGas`, `inBandFeePerGas`).
    pub fn with_billing_terms(mut self, billing_terms: BillingTerms) -> Self {
        self.billing_terms = billing_terms;
        self
    }

    pub fn with_user_operation_queue(mut self, user_operation_queue: UserOperationQueue) -> Self {
        self.user_operation_queue = Some(user_operation_queue);
        self
    }

    pub fn with_user_operation_status_store(
        mut self,
        user_operation_status_store: UserOperationStatusStore,
    ) -> Self {
        self.user_operation_status_store = Some(user_operation_status_store);
        self
    }

    pub fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    pub fn billing_terms(&self) -> BillingTerms {
        self.billing_terms
    }

    pub fn gas_price(&self) -> GasPriceManager {
        self.gas_price.clone()
    }

    pub fn settlement_recipient(&self) -> Option<&str> {
        self.settlement_recipient.as_deref()
    }

    pub fn user_operation_queue(&self) -> Option<UserOperationQueue> {
        self.user_operation_queue.clone()
    }

    pub fn user_operation_status_store(&self) -> Option<UserOperationStatusStore> {
        self.user_operation_status_store.clone()
    }
}

impl Readiness {
    pub fn mark_job_ready(&self, job: &'static str) {
        self.ready_jobs().insert(job);
    }

    pub fn mark_job_unready(&self, job: &'static str) {
        self.ready_jobs().remove(job);
    }

    pub fn clear(&self) {
        self.ready_jobs().clear();
    }

    pub fn is_ready(&self) -> bool {
        let ready_jobs = self.ready_jobs();
        self.expected_jobs
            .iter()
            .all(|job| ready_jobs.contains(job))
    }

    fn ready_jobs(&self) -> std::sync::MutexGuard<'_, HashSet<&'static str>> {
        self.ready_jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}
