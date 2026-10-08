//! How the executor walks a chain's endpoint list.
//!
//! Every simulation, receipt read and broadcast tries the chain's endpoints
//! one at a time until one answers (`docs/rpc.md`). The walk is cheap where the
//! first endpoint answers, and it is the whole wait where none does: on
//! Avalanche a relay pass asked 29 endpoints for `eth_simulateV1`, which no
//! Avalanche node serves, twice over, and a send took about a minute
//! (vela-wallet #464, 2026-10-08).
//!
//! The rules here decide what a walk may skip and what it has learned. Each
//! shell keeps the state they describe for the life of its process (docker) or
//! isolate (Workers), and supplies the clock.

use std::collections::HashMap;

/// How long a walk that proved a chain's endpoints lack a method is believed.
/// It only reorders the next walks (the method is asked last instead of
/// first), so a stale memory costs a slower walk, never a verdict.
pub const MISSING_METHOD_MEMORY_MS: u64 = 10 * 60 * 1_000;

/// What one endpoint's reply said about the method it was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodAnswer {
    /// A `result`: the endpoint serves the method.
    Served,
    /// A JSON-RPC error saying the endpoint has no such method.
    Unsupported,
    /// Any other JSON-RPC error: the endpoint may well serve the method and
    /// failed this one call (a revert, bad params, a node error).
    Refused,
}

/// The class of a JSON-RPC error object returned for `method`. A reply that
/// never reached JSON-RPC (a timeout, an HTTP error, an unreadable body) is no
/// answer at all and is not classified.
///
/// Every Avalanche endpoint that answered `eth_simulateV1` on 2026-10-08 used
/// code -32601, under five different messages ("the method … does not
/// exist/is not available", "Method not found: …", "method error; invalid
/// method: …", "… is rejected as not supported", "Method not allowed by XCORP
/// Security Gateway Policy"). The message rule catches a provider that says the
/// same under another code.
pub fn method_error_answer(code: Option<i64>, message: &str) -> MethodAnswer {
    if matches!(code, Some(-32_601 | -32_004)) {
        return MethodAnswer::Unsupported;
    }
    let message = message.to_ascii_lowercase();
    let names_the_method = message.contains("method");
    let says_absent = [
        "not found",
        "does not exist",
        "not supported",
        "unsupported",
        "not available",
        "not allowed",
        "invalid method",
    ]
    .iter()
    .any(|phrase| message.contains(phrase));
    if names_the_method && says_absent {
        MethodAnswer::Unsupported
    } else {
        MethodAnswer::Refused
    }
}

/// Whether a finished walk proves that the chain's endpoints lack the method:
/// at least one endpoint said it has no such method, none served it, and none
/// failed it in a way that shows it knows the method. Endpoints that gave no
/// JSON-RPC answer are no evidence either way — on Avalanche a third of the
/// list answers 403, 429 or 521, or hangs.
///
/// The shell calls this only for a walk that reached the end of the list,
/// which is every walk that nobody served: the transport stops early only on
/// a `result` or a revert, and both are in `answers`.
pub fn walk_proves_method_missing(answers: &[MethodAnswer]) -> bool {
    answers.contains(&MethodAnswer::Unsupported)
        && answers
            .iter()
            .all(|answer| *answer == MethodAnswer::Unsupported)
}

/// Per chain, the methods a recent walk proved its endpoints lack.
#[derive(Debug, Default)]
pub struct MissingMethods {
    until_ms: HashMap<(u64, String), u64>,
}

impl MissingMethods {
    /// Folds one finished walk into the memory: remembered when it proves the
    /// method missing, forgotten as soon as any endpoint serves it.
    pub fn note_walk(
        &mut self,
        chain_id: u64,
        method: &str,
        answers: &[MethodAnswer],
        now_ms: u64,
    ) {
        if walk_proves_method_missing(answers) {
            self.until_ms.retain(|_, until| *until > now_ms);
            self.until_ms.insert(
                (chain_id, method.to_owned()),
                now_ms.saturating_add(MISSING_METHOD_MEMORY_MS),
            );
        } else if answers.contains(&MethodAnswer::Served) {
            self.until_ms.remove(&(chain_id, method.to_owned()));
        }
    }

    /// Whether a walk within the last [`MISSING_METHOD_MEMORY_MS`] proved the
    /// chain's endpoints lack `method`.
    pub fn lacks(&self, chain_id: u64, method: &str, now_ms: u64) -> bool {
        self.until_ms
            .get(&(chain_id, method.to_owned()))
            .is_some_and(|until| now_ms < *until)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MISSING_METHOD_MEMORY_MS, MethodAnswer, MissingMethods, method_error_answer,
        walk_proves_method_missing,
    };

    #[test]
    fn every_way_an_avalanche_endpoint_said_it_has_no_simulate_v1_is_unsupported() {
        // Measured 2026-10-08 against the directory's endpoints for 43114.
        for (code, message) in [
            (
                -32_601,
                "the method eth_simulateV1 does not exist/is not available",
            ),
            (-32_601, "the method eth_simulateV1 does not exist"),
            (-32_601, "Method not found: eth_simulateV1"),
            (-32_601, "method error; invalid method: eth_simulateV1"),
            (
                -32_601,
                "the method eth_simulateV1 is rejected as not supported",
            ),
            (
                -32_601,
                "Method not allowed by XCORP Security Gateway Policy",
            ),
        ] {
            assert_eq!(
                method_error_answer(Some(code), message),
                MethodAnswer::Unsupported,
                "{message}"
            );
        }
        // The same words under a provider's own code.
        assert_eq!(
            method_error_answer(Some(-32_000), "Method eth_simulateV1 not supported"),
            MethodAnswer::Unsupported
        );
        assert_eq!(
            method_error_answer(Some(-32_004), "eth_simulateV1"),
            MethodAnswer::Unsupported
        );
    }

    #[test]
    fn an_error_that_is_not_about_the_method_shows_the_endpoint_may_serve_it() {
        for (code, message) in [
            (Some(3), "execution reverted"),
            (Some(-32_602), "invalid params"),
            (Some(-32_000), "header not found"),
            (Some(-32_000), "account does not exist"),
            (Some(-32_005), "rate limit exceeded"),
            (None, ""),
        ] {
            assert_eq!(
                method_error_answer(code, message),
                MethodAnswer::Refused,
                "{message}"
            );
        }
    }

    #[test]
    fn only_a_walk_where_every_answer_is_unsupported_proves_the_method_missing() {
        use MethodAnswer::{Refused, Served, Unsupported};
        assert!(walk_proves_method_missing(&[Unsupported]));
        assert!(walk_proves_method_missing(&[Unsupported; 12]));
        // Nobody answered at all: an outage says nothing about the method.
        assert!(!walk_proves_method_missing(&[]));
        assert!(!walk_proves_method_missing(&[Unsupported, Served]));
        assert!(!walk_proves_method_missing(&[Unsupported, Refused]));
    }

    #[test]
    fn a_proven_walk_is_remembered_for_its_time_and_forgotten_when_someone_serves() {
        use MethodAnswer::{Served, Unsupported};
        let mut memory = MissingMethods::default();
        let now = 1_000_000;
        assert!(!memory.lacks(43_114, "eth_simulateV1", now));

        memory.note_walk(43_114, "eth_simulateV1", &[Unsupported, Unsupported], now);
        assert!(memory.lacks(43_114, "eth_simulateV1", now));
        assert!(memory.lacks(43_114, "eth_simulateV1", now + MISSING_METHOD_MEMORY_MS - 1));
        assert!(!memory.lacks(43_114, "eth_simulateV1", now + MISSING_METHOD_MEMORY_MS));
        // Per chain, per method.
        assert!(!memory.lacks(137, "eth_simulateV1", now));
        assert!(!memory.lacks(43_114, "debug_traceCall", now));

        // An inconclusive walk leaves the memory as it was...
        memory.note_walk(43_114, "eth_simulateV1", &[], now + 1);
        assert!(memory.lacks(43_114, "eth_simulateV1", now + 1));
        // ...and one endpoint that serves the method ends it.
        memory.note_walk(43_114, "eth_simulateV1", &[Unsupported, Served], now + 2);
        assert!(!memory.lacks(43_114, "eth_simulateV1", now + 2));
    }
}
