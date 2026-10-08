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

use serde_json::Value;

/// How long an endpoint that failed its chain check or timed out is left out
/// of walks. On Avalanche about a dozen listed endpoints answer 403, 429, 521
/// or 530 to everything, and avalancheapi.terminet.io never answers at all;
/// each was asked again on every walk, the last one for the whole 5 s RPC
/// timeout (2026-10-08).
pub const ENDPOINT_COOLDOWN_MS: u64 = 3 * 60 * 1_000;

/// The endpoints a walk leaves out for now, per chain.
///
/// The operator's own endpoints (`VELA_RELAY_EXECUTOR_RPC_URLS`) are never
/// cooled: they are a deliberate choice, often the one node that serves
/// `debug_traceCall`, and the shells do not report them.
#[derive(Debug, Default)]
pub struct EndpointCooldowns {
    until_ms: HashMap<(u64, String), u64>,
}

impl EndpointCooldowns {
    /// Leave `url` out of the chain's walks for [`ENDPOINT_COOLDOWN_MS`].
    pub fn cool(&mut self, chain_id: u64, url: &str, now_ms: u64) {
        self.until_ms.retain(|_, until| *until > now_ms);
        self.until_ms.insert(
            (chain_id, url.to_owned()),
            now_ms.saturating_add(ENDPOINT_COOLDOWN_MS),
        );
    }

    /// `url` answered its chain check: walk it again.
    pub fn clear(&mut self, chain_id: u64, url: &str) {
        self.until_ms.remove(&(chain_id, url.to_owned()));
    }

    pub fn is_cooling(&self, chain_id: u64, url: &str, now_ms: u64) -> bool {
        self.until_ms
            .get(&(chain_id, url.to_owned()))
            .is_some_and(|until| now_ms < *until)
    }

    /// The endpoints a walk asks, in their order: those not cooling down — or
    /// every one of them when all are, so a cooldown never leaves a chain with
    /// nothing to ask.
    pub fn walkable(&self, chain_id: u64, urls: Vec<String>, now_ms: u64) -> Vec<String> {
        let warm = urls
            .iter()
            .filter(|url| !self.is_cooling(chain_id, url, now_ms))
            .cloned()
            .collect::<Vec<_>>();
        if warm.is_empty() { urls } else { warm }
    }
}

/// The replies in an answer to `calls_sent` JSON-RPC calls: the array a batch
/// gets, or the one object a single call gets. A transport sends a lone call
/// on its own rather than as a batch of one — pocket.network answers a
/// one-call batch with a bare object, which read as no answer and walked on
/// past it (Avalanche `debug_traceCall`, 2026-10-08) — and accepts the bare
/// object from an endpoint that does the same to a batch. `None` is no
/// answer: try the next endpoint.
pub fn batch_replies(body: Value, calls_sent: usize) -> Option<Vec<Value>> {
    match body {
        Value::Array(replies) => Some(replies),
        reply @ Value::Object(_) if calls_sent == 1 => Some(vec![reply]),
        _ => None,
    }
}

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
    use serde_json::json;

    use super::{
        ENDPOINT_COOLDOWN_MS, EndpointCooldowns, MISSING_METHOD_MEMORY_MS, MethodAnswer,
        MissingMethods, batch_replies, method_error_answer, walk_proves_method_missing,
    };

    #[test]
    fn a_cooled_endpoint_is_left_out_of_walks_until_its_cooldown_ends() {
        let urls = || {
            vec![
                "https://terminet.example/".to_owned(),
                "https://publicnode.example/".to_owned(),
                "https://pocket.example/".to_owned(),
            ]
        };
        let mut cooldowns = EndpointCooldowns::default();
        let now = 5_000_000;
        cooldowns.cool(43_114, "https://terminet.example/", now);

        assert_eq!(
            cooldowns.walkable(43_114, urls(), now + 1),
            vec![
                "https://publicnode.example/".to_owned(),
                "https://pocket.example/".to_owned(),
            ]
        );
        // Per chain: the same host for another chain is still asked.
        assert_eq!(cooldowns.walkable(137, urls(), now + 1), urls());
        // Back in the walk once the cooldown is over, in its old place.
        assert_eq!(
            cooldowns.walkable(43_114, urls(), now + ENDPOINT_COOLDOWN_MS),
            urls()
        );
        // ...or as soon as it passes its chain check again.
        cooldowns.clear(43_114, "https://terminet.example/");
        assert_eq!(cooldowns.walkable(43_114, urls(), now + 1), urls());
    }

    #[test]
    fn a_chain_whose_every_endpoint_is_cooling_still_has_them_all_to_ask() {
        let urls = vec![
            "https://first.example/".to_owned(),
            "https://second.example/".to_owned(),
        ];
        let mut cooldowns = EndpointCooldowns::default();
        for url in &urls {
            cooldowns.cool(43_114, url, 0);
        }
        assert_eq!(cooldowns.walkable(43_114, urls.clone(), 1), urls);
    }

    #[test]
    fn a_lone_call_may_be_answered_with_a_bare_object_and_a_batch_may_not() {
        let reply = json!({ "jsonrpc": "2.0", "id": 7, "result": "0x1" });
        assert_eq!(batch_replies(reply.clone(), 1), Some(vec![reply.clone()]));
        assert_eq!(
            batch_replies(json!([reply.clone(), reply.clone()]), 2),
            Some(vec![reply.clone(), reply.clone()])
        );
        // An object for a batch of two is a whole-batch refusal, not replies.
        assert_eq!(batch_replies(reply, 2), None);
        assert_eq!(batch_replies(json!("rate limited"), 1), None);
    }

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
