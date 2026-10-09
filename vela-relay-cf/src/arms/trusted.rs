//! The docker shell's `TrustedRpcClient` ported onto workerd fetch: the
//! executor-grade JSON-RPC transport (explicit URLs → Alchemy → controlled
//! directory, per-URL `eth_chainId` validation, item-level batch failover,
//! broadcast classification). Interpretation rules come from the core
//! (`broadcast`); this module owns only transport policy (Constitution,
//! Shell-owned concerns).
//!
//! Caches live per isolate (`thread_local`): the validated-URL set, the
//! directory URL list, the endpoints cooling down after a failed chain check
//! or a timeout, and the methods a walk proved the chain's endpoints lack
//! (core `rpc_walk`) — the same lifetimes the docker client gets from its
//! process-wide maps, scoped to one workerd isolate.

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    fmt::{Display, Formatter},
};

use serde::Deserialize;
use serde_json::{Value, json};
use vela_relay_core::{
    broadcast as core_broadcast,
    rpc_host::RpcHostPolicy,
    rpc_walk::{self, EndpointCooldowns, MethodAnswer, MissingMethods},
};
use worker::{Date, Delay, Env, Fetch, Headers, Method, Request, RequestInit};

use super::market;
use crate::config::CfConfig;

pub struct TrustedRpcClient<'env> {
    env: &'env Env,
    explicit_urls: BTreeMap<u64, Vec<String>>,
    alchemy_api_key: Option<String>,
    rpc_host_policy: RpcHostPolicy,
    rpc_timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub struct RpcBatchCall<'a> {
    pub method: &'a str,
    pub params: Value,
}

#[derive(Debug)]
pub enum RpcError {
    NoTrustedRpc(u64),
    WrongChain,
    Unavailable,
    Reverted {
        message: String,
        data: Option<String>,
    },
    InvalidResponse,
}

#[derive(Debug)]
pub enum BroadcastOutcome {
    Accepted(String),
    Ambiguous(String),
    Rejected(String),
}

impl Display for RpcError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        // Byte-frozen: these strings flow into record diagnostics on both
        // shells (docker `RpcError` Display).
        match self {
            Self::NoTrustedRpc(chain_id) => {
                write!(
                    formatter,
                    "no trusted executor RPC is available for chain {chain_id}"
                )
            }
            Self::WrongChain => formatter.write_str("trusted RPC returned the wrong chain ID"),
            Self::Unavailable => formatter.write_str("trusted RPC is temporarily unavailable"),
            Self::Reverted { .. } => formatter.write_str("EVM execution reverted"),
            Self::InvalidResponse => {
                formatter.write_str("trusted RPC returned an invalid response")
            }
        }
    }
}

thread_local! {
    static DIRECTORY_URLS: RefCell<HashMap<u64, Vec<String>>> = RefCell::new(HashMap::new());
    static VALIDATED_URLS: RefCell<HashSet<(u64, String)>> = RefCell::new(HashSet::new());
    static MISSING_METHODS: RefCell<MissingMethods> = RefCell::new(MissingMethods::default());
    static COOLDOWNS: RefCell<EndpointCooldowns> = RefCell::new(EndpointCooldowns::default());
    static REQUEST_ID: Cell<u64> = const { Cell::new(1) };
}

fn next_request_id(count: u64) -> u64 {
    REQUEST_ID.with(|id| {
        let first = id.get();
        id.set(first.wrapping_add(count));
        first
    })
}

impl<'env> TrustedRpcClient<'env> {
    pub fn new(config: &CfConfig, env: &'env Env) -> Self {
        Self {
            env,
            explicit_urls: config.trusted_rpc_urls.clone(),
            alchemy_api_key: config.alchemy_api_key.clone(),
            rpc_host_policy: config.rpc_host_policy,
            rpc_timeout_ms: config.rpc_timeout_ms,
        }
    }

    pub async fn supports_chain(&self, chain_id: u64) -> bool {
        !self.urls(chain_id).await.is_empty()
    }

    pub async fn call(
        &self,
        chain_id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        self.call_walk(chain_id, method, params).await.0
    }

    /// [`Self::call`], also reporting what each endpoint that answered said
    /// about `method` (core `rpc_walk`).
    pub async fn call_walk(
        &self,
        chain_id: u64,
        method: &str,
        params: Value,
    ) -> (Result<Value, RpcError>, Vec<MethodAnswer>) {
        let mut answers = Vec::new();
        let urls = match self.urls_or_error(chain_id).await {
            Ok(urls) => urls,
            Err(error) => return (Err(error), answers),
        };
        for url in urls {
            if self.validate_chain(chain_id, &url).await.is_err() {
                continue;
            }
            let Ok(response) = self.request(chain_id, &url, method, params.clone()).await else {
                continue;
            };
            answers.extend(response.method_answer());
            if let (Some(result), None) = response.into_result_and_error() {
                return (Ok(result), answers);
            }
        }
        (Err(RpcError::Unavailable), answers)
    }

    /// Whether a recent walk proved this chain's endpoints lack `method`.
    pub fn lacks_method(&self, chain_id: u64, method: &str) -> bool {
        let now = Date::now().as_millis();
        MISSING_METHODS.with(|missing| missing.borrow().lacks(chain_id, method, now))
    }

    /// Folds a finished walk for `method` into the chain's memory.
    pub fn note_method_walk(&self, chain_id: u64, method: &str, answers: &[MethodAnswer]) {
        let now = Date::now().as_millis();
        MISSING_METHODS.with(|missing| {
            missing
                .borrow_mut()
                .note_walk(chain_id, method, answers, now);
        });
    }

    pub async fn simulate(
        &self,
        chain_id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let urls = self.urls_or_error(chain_id).await?;
        for url in urls {
            if self.validate_chain(chain_id, &url).await.is_err() {
                continue;
            }
            match self.request(chain_id, &url, method, params.clone()).await {
                Ok(response) => match response.into_result_and_error() {
                    (Some(result), None) => return Ok(result),
                    (None, Some(error)) if error.is_execution_revert() => {
                        return Err(error.into_revert());
                    }
                    _ => continue,
                },
                Err(_) => continue,
            }
        }
        Err(RpcError::Unavailable)
    }

    /// Executes a JSON-RPC batch with item-level failover across trusted endpoints. A successful
    /// item or an explicit EVM revert is final; malformed, omitted, or unsupported-method items
    /// are retried on the next endpoint without repeating already resolved calls.
    pub async fn batch(
        &self,
        chain_id: u64,
        calls: &[RpcBatchCall<'_>],
    ) -> Result<Vec<Result<Value, RpcError>>, RpcError> {
        self.batch_walk(chain_id, calls).await.0
    }

    /// [`Self::batch`], also reporting what each endpoint said about each
    /// call it answered (core `rpc_walk`).
    pub async fn batch_walk(
        &self,
        chain_id: u64,
        calls: &[RpcBatchCall<'_>],
    ) -> (
        Result<Vec<Result<Value, RpcError>>, RpcError>,
        Vec<MethodAnswer>,
    ) {
        let mut answers = Vec::new();
        let result = self.batch_inner(chain_id, calls, &mut answers).await;
        (result, answers)
    }

    async fn batch_inner(
        &self,
        chain_id: u64,
        calls: &[RpcBatchCall<'_>],
        answers: &mut Vec<MethodAnswer>,
    ) -> Result<Vec<Result<Value, RpcError>>, RpcError> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let urls = self.urls_or_error(chain_id).await?;
        let first_id = next_request_id(calls.len() as u64);
        let mut results = (0..calls.len()).map(|_| None).collect::<Vec<_>>();
        let mut unresolved = (0..calls.len()).collect::<Vec<_>>();
        let mut saw_batch_response = false;

        for url in urls {
            if self.validate_chain(chain_id, &url).await.is_err() {
                continue;
            }
            let mut payload = unresolved
                .iter()
                .map(|index| {
                    let call = &calls[*index];
                    json!({
                        "jsonrpc": "2.0",
                        "id": first_id + *index as u64,
                        "method": call.method,
                        "params": call.params,
                    })
                })
                .collect::<Vec<_>>();
            let sent = payload.len();
            // A lone call goes on its own, not as a batch of one (core
            // `rpc_walk::batch_replies`).
            let payload = if sent == 1 {
                payload.remove(0)
            } else {
                Value::Array(payload)
            };
            let Ok(body) = self.post_json(chain_id, &url, &payload).await else {
                continue;
            };
            let Some(mut responses) = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|body| rpc_walk::batch_replies(body, sent))
                .and_then(|replies| {
                    replies
                        .into_iter()
                        .map(serde_json::from_value::<UpstreamResponse>)
                        .collect::<Result<Vec<_>, _>>()
                        .ok()
                })
            else {
                continue;
            };
            saw_batch_response = true;

            let unresolved_set = unresolved.iter().copied().collect::<HashSet<_>>();
            let mut response_by_index = BTreeMap::new();
            let mut duplicate_indices = HashSet::new();
            for response in responses.drain(..) {
                let Some(index) = response
                    .id
                    .checked_sub(first_id)
                    .and_then(|offset| usize::try_from(offset).ok())
                    .filter(|index| unresolved_set.contains(index))
                else {
                    continue;
                };
                if response_by_index.insert(index, response).is_some() {
                    duplicate_indices.insert(index);
                }
            }

            let mut retry = Vec::new();
            for index in unresolved {
                if duplicate_indices.contains(&index) {
                    retry.push(index);
                    continue;
                }
                let response = response_by_index.remove(&index);
                answers.extend(response.as_ref().and_then(UpstreamResponse::method_answer));
                match response.and_then(definitive_batch_result) {
                    Some(result) => results[index] = Some(result),
                    None => retry.push(index),
                }
            }
            unresolved = retry;
            if unresolved.is_empty() {
                break;
            }
        }

        if !saw_batch_response {
            return Err(RpcError::Unavailable);
        }
        for index in unresolved {
            results[index] = Some(Err(RpcError::InvalidResponse));
        }
        Ok(results
            .into_iter()
            .map(|result| result.expect("every batch item is resolved or marked invalid"))
            .collect())
    }

    pub async fn broadcast_raw_transaction(
        &self,
        chain_id: u64,
        raw_transaction: &[u8],
    ) -> Result<BroadcastOutcome, RpcError> {
        let urls = self.urls_or_error(chain_id).await?;
        let raw_transaction = format!("0x{}", hex::encode(raw_transaction));
        let mut ambiguous_diagnostics = Vec::new();
        let mut rejection_diagnostics = Vec::new();

        for url in urls {
            if self.validate_chain(chain_id, &url).await.is_err() {
                continue;
            }
            match self
                .request(
                    chain_id,
                    &url,
                    "eth_sendRawTransaction",
                    json!([raw_transaction.clone()]),
                )
                .await
            {
                Ok(response) => match response.into_result_and_error() {
                    (Some(Value::String(hash)), None) => {
                        return Ok(BroadcastOutcome::Accepted(hash));
                    }
                    (None, Some(error))
                        if error.is_already_known() || error.is_nonce_ambiguous() =>
                    {
                        ambiguous_diagnostics.push(error.diagnostic());
                    }
                    (None, Some(error)) if error.is_definitive_broadcast_rejection() => {
                        rejection_diagnostics.push(error.diagnostic());
                    }
                    (None, Some(error)) => ambiguous_diagnostics.push(error.diagnostic()),
                    _ => ambiguous_diagnostics.push("malformed RPC broadcast response".into()),
                },
                Err(error) => ambiguous_diagnostics.push(error.to_string()),
            }
        }

        Ok(
            if !ambiguous_diagnostics.is_empty() || rejection_diagnostics.is_empty() {
                BroadcastOutcome::Ambiguous(core_broadcast::join_broadcast_diagnostics(
                    ambiguous_diagnostics,
                ))
            } else {
                BroadcastOutcome::Rejected(core_broadcast::join_broadcast_diagnostics(
                    rejection_diagnostics,
                ))
            },
        )
    }

    /// An endpoint is walked only once it reports the chain's id. One that
    /// does not — wrong chain, an error, no answer — is left out of walks for
    /// a while (core `rpc_walk::EndpointCooldowns`).
    async fn validate_chain(&self, chain_id: u64, url: &str) -> Result<(), RpcError> {
        let key = (chain_id, url.to_owned());
        if VALIDATED_URLS.with(|validated| validated.borrow().contains(&key)) {
            return Ok(());
        }
        let checked = self.check_chain(chain_id, url).await;
        match checked {
            Ok(()) => {
                VALIDATED_URLS.with(|validated| validated.borrow_mut().insert(key));
                COOLDOWNS.with(|cooldowns| cooldowns.borrow_mut().clear(chain_id, url));
            }
            Err(_) => self.cool(chain_id, url),
        }
        checked
    }

    async fn check_chain(&self, chain_id: u64, url: &str) -> Result<(), RpcError> {
        let response = self
            .request(chain_id, url, "eth_chainId", json!([]))
            .await?;
        let (result, error) = response.into_result_and_error();
        if error.is_some() {
            return Err(RpcError::InvalidResponse);
        }
        let returned = result
            .and_then(|value| value.as_str().map(str::to_owned))
            .and_then(|value| u64::from_str_radix(value.trim_start_matches("0x"), 16).ok())
            .ok_or(RpcError::InvalidResponse)?;
        if returned != chain_id {
            return Err(RpcError::WrongChain);
        }
        Ok(())
    }

    /// Leaves `url` out of the chain's walks for a while — unless it is one
    /// of the operator's own endpoints, which are never cooled.
    fn cool(&self, chain_id: u64, url: &str) {
        if self
            .explicit_urls
            .get(&chain_id)
            .is_some_and(|explicit| explicit.iter().any(|explicit| explicit == url))
        {
            return;
        }
        let now = Date::now().as_millis();
        COOLDOWNS.with(|cooldowns| cooldowns.borrow_mut().cool(chain_id, url, now));
    }

    async fn request(
        &self,
        chain_id: u64,
        url: &str,
        method: &str,
        params: Value,
    ) -> Result<UpstreamResponse, RpcError> {
        let id = next_request_id(1);
        let body = self
            .post_json(
                chain_id,
                url,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }),
            )
            .await
            .map_err(|()| RpcError::Unavailable)?;
        serde_json::from_str::<UpstreamResponse>(&body).map_err(|_| RpcError::InvalidResponse)
    }

    /// One POST racing the executor deadline — workerd fetch has no native
    /// timeout, so every request is bounded by a `Delay` (docker: reqwest
    /// connect + request timeouts). An endpoint that runs out the deadline is
    /// cooled: it would cost the whole timeout again on the next walk.
    async fn post_json(&self, chain_id: u64, url: &str, payload: &Value) -> Result<String, ()> {
        use futures_util::future::{Either, select};

        let request = async {
            let headers = Headers::new();
            headers
                .set("content-type", "application/json")
                .map_err(|_| ())?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post)
                .with_headers(headers)
                .with_body(Some(worker::wasm_bindgen::JsValue::from_str(
                    &payload.to_string(),
                )));
            let request = Request::new_with_init(url, &init).map_err(|_| ())?;
            let mut response = Fetch::Request(request).send().await.map_err(|_| ())?;
            if !(200..300).contains(&response.status_code()) {
                return Err(());
            }
            response.text().await.map_err(|_| ())
        };
        let deadline = Delay::from(std::time::Duration::from_millis(self.rpc_timeout_ms));
        match select(std::pin::pin!(request), deadline).await {
            Either::Left((result, _)) => result,
            Either::Right(((), _)) => {
                self.cool(chain_id, url);
                Err(())
            }
        }
    }

    /// The endpoints a walk asks: every usable one, less those cooling down.
    async fn urls_or_error(&self, chain_id: u64) -> Result<Vec<String>, RpcError> {
        let urls = self.urls(chain_id).await;
        let now = Date::now().as_millis();
        let urls = COOLDOWNS.with(|cooldowns| cooldowns.borrow().walkable(chain_id, urls, now));
        if urls.is_empty() {
            Err(RpcError::NoTrustedRpc(chain_id))
        } else {
            Ok(urls)
        }
    }

    async fn urls(&self, chain_id: u64) -> Vec<String> {
        let mut urls = self
            .explicit_urls
            .get(&chain_id)
            .cloned()
            .unwrap_or_default();
        if let Some(api_key) = &self.alchemy_api_key
            && let Some(url) = vela_relay_core::alchemy::rpc_url(chain_id, api_key)
        {
            append_unique_urls(&mut urls, [url]);
        }

        let cached = DIRECTORY_URLS.with(|directory| directory.borrow().get(&chain_id).cloned());
        let directory_urls = if let Some(urls) = cached {
            urls
        } else {
            // Do not cache an outage: a subsequent batch should be able to
            // retry the controlled directory (docker parity; the KV metadata
            // cache underneath is success-only too).
            // Filtered by the same rule as every read (docker parity: one
            // `parse_rpc_url` for both).
            match market::fallback_rpc_urls(self.env, chain_id, self.rpc_host_policy).await {
                Ok(urls) => {
                    DIRECTORY_URLS.with(|directory| {
                        directory.borrow_mut().insert(chain_id, urls.clone());
                    });
                    urls
                }
                Err(error) => {
                    worker::console_warn!(
                        "could not fetch controlled directory RPC URLs: chain_id={chain_id} error={error}"
                    );
                    Vec::new()
                }
            }
        };
        append_unique_urls(&mut urls, directory_urls);
        urls
    }
}

fn append_unique_urls(urls: &mut Vec<String>, candidates: impl IntoIterator<Item = String>) {
    for url in candidates {
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamResponse {
    #[serde(default)]
    id: u64,
    #[serde(flatten)]
    fields: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct UpstreamError {
    code: Option<i64>,
    message: Option<String>,
    data: Option<Value>,
}

// Classification and rendering live in the core (`broadcast`); this shell
// only carries the deserialized upstream fields to them.
impl UpstreamError {
    fn diagnostic(&self) -> String {
        core_broadcast::upstream_error_diagnostic(self.code, self.message.as_deref())
    }

    fn is_execution_revert(&self) -> bool {
        core_broadcast::is_executor_revert(self.code, self.message.as_deref().unwrap_or_default())
    }

    fn into_revert(self) -> RpcError {
        RpcError::Reverted {
            data: core_broadcast::revert_data(&self.data),
            message: self.message.unwrap_or_default(),
        }
    }

    fn is_already_known(&self) -> bool {
        core_broadcast::is_broadcast_already_known(self.message.as_deref().unwrap_or_default())
    }

    fn is_nonce_ambiguous(&self) -> bool {
        core_broadcast::is_broadcast_nonce_ambiguous(self.message.as_deref().unwrap_or_default())
    }

    fn is_definitive_broadcast_rejection(&self) -> bool {
        core_broadcast::is_definitive_broadcast_rejection(
            self.message.as_deref().unwrap_or_default(),
        )
    }
}

impl UpstreamResponse {
    /// What this reply says about the method it answers: an error object is
    /// classified (a revert means the node ran the method), a bare `result`
    /// is served, and a reply with neither is no answer.
    fn method_answer(&self) -> Option<MethodAnswer> {
        if let Some(error) = self
            .fields
            .get("error")
            .and_then(|value| serde_json::from_value::<UpstreamError>(value.clone()).ok())
        {
            return Some(if error.is_execution_revert() {
                MethodAnswer::Refused
            } else {
                rpc_walk::method_error_answer(error.code, error.message.as_deref().unwrap_or(""))
            });
        }
        self.fields
            .contains_key("result")
            .then_some(MethodAnswer::Served)
    }

    fn into_result_and_error(mut self) -> (Option<Value>, Option<UpstreamError>) {
        let result = self.fields.remove("result");
        let error = self
            .fields
            .remove("error")
            .and_then(|value| serde_json::from_value(value).ok());
        (result, error)
    }
}

fn definitive_batch_result(response: UpstreamResponse) -> Option<Result<Value, RpcError>> {
    match response.into_result_and_error() {
        (Some(result), None) => Some(Ok(result)),
        (None, Some(error)) if error.is_execution_revert() => Some(Err(error.into_revert())),
        _ => None,
    }
}

// --- batch response decoding (docker engine helpers, byte-identical text) ---

use alloy::primitives::U256;

pub fn response_value<'a>(
    responses: &'a [Result<Value, RpcError>],
    index: usize,
    method: &str,
) -> Result<&'a Value, String> {
    match responses.get(index) {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(format!("{method} failed: {error}")),
        None => Err(format!("{method} is missing from the RPC batch response")),
    }
}

pub fn response_quantity(
    responses: &[Result<Value, RpcError>],
    index: usize,
    method: &str,
) -> Result<U256, String> {
    response_value(responses, index, method)?
        .as_str()
        .and_then(parse_quantity)
        .ok_or_else(|| format!("{method} returned an invalid quantity"))
}

pub fn response_abi_u256(
    responses: &[Result<Value, RpcError>],
    index: usize,
    method: &str,
) -> Result<U256, String> {
    let bytes = response_value(responses, index, method)?
        .as_str()
        .and_then(vela_relay_core::broadcast::parse_hex_bytes)
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| format!("{method} returned invalid ABI data"))?;
    Ok(U256::from_be_slice(&bytes))
}

pub fn response_quantity_optional(
    responses: &[Result<Value, RpcError>],
    index: usize,
) -> Option<U256> {
    responses
        .get(index)
        .and_then(|response| response.as_ref().ok())
        .and_then(Value::as_str)
        .and_then(parse_quantity)
}

/// Canonical JSON-RPC quantity only (docker engine `parse_quantity`): `0x`
/// prefix, no leading zeros, hex digits.
pub fn parse_quantity(value: &str) -> Option<U256> {
    let digits = value.strip_prefix("0x")?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    U256::from_str_radix(digits, 16).ok()
}
