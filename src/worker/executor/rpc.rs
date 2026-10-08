use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::{Display, Formatter},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use vela_relay_core::rpc_walk::{self, MethodAnswer, MissingMethods};

use crate::utils::{alchemy, config::ExecutorConfig, rpc as chain_directory};

#[derive(Clone)]
pub(super) struct TrustedRpcClient {
    http: Client,
    explicit_urls: Arc<BTreeMap<u64, Vec<String>>>,
    alchemy_api_key: Option<Arc<str>>,
    directory_urls: Arc<Mutex<HashMap<u64, Vec<String>>>>,
    validated_urls: Arc<Mutex<HashSet<(u64, String)>>>,
    /// The methods a walk proved a chain's endpoints lack (core `rpc_walk`).
    missing_methods: Arc<std::sync::Mutex<MissingMethods>>,
    request_id: Arc<AtomicU64>,
}

/// Wall-clock milliseconds for the core's walk memories.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[derive(Clone, Debug)]
pub(super) struct RpcBatchCall<'a> {
    pub(super) method: &'a str,
    pub(super) params: Value,
}

#[derive(Debug)]
pub(super) enum RpcError {
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
pub(super) enum BroadcastOutcome {
    Accepted(String),
    Ambiguous(String),
    Rejected(String),
}

impl Display for RpcError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
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

impl std::error::Error for RpcError {}

impl TrustedRpcClient {
    pub(super) fn new(config: &ExecutorConfig) -> Result<Self, RpcError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(config.rpc_timeout)
            .build()
            .map_err(|_| RpcError::Unavailable)?;
        Ok(Self {
            http,
            explicit_urls: Arc::new(config.trusted_rpc_urls.clone()),
            alchemy_api_key: config
                .alchemy_api_key
                .as_ref()
                .map(|key| Arc::from(key.expose())),
            directory_urls: Arc::new(Mutex::new(HashMap::new())),
            validated_urls: Arc::new(Mutex::new(HashSet::new())),
            missing_methods: Arc::new(std::sync::Mutex::new(MissingMethods::default())),
            request_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub(super) async fn supports_chain(&self, chain_id: u64) -> bool {
        !self.urls(chain_id).await.is_empty()
    }

    pub(super) async fn call(
        &self,
        chain_id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        self.call_walk(chain_id, method, params).await.0
    }

    /// [`Self::call`], also reporting what each endpoint that answered said
    /// about `method` (core `rpc_walk`).
    pub(super) async fn call_walk(
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
            let Ok(response) = self.request(&url, method, params.clone()).await else {
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
    pub(super) fn lacks_method(&self, chain_id: u64, method: &str) -> bool {
        self.missing_methods
            .lock()
            .expect("missing-method memory mutex")
            .lacks(chain_id, method, now_ms())
    }

    /// Folds a finished walk for `method` into the chain's memory.
    pub(super) fn note_method_walk(&self, chain_id: u64, method: &str, answers: &[MethodAnswer]) {
        self.missing_methods
            .lock()
            .expect("missing-method memory mutex")
            .note_walk(chain_id, method, answers, now_ms());
    }

    pub(super) async fn simulate(
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
            match self.request(&url, method, params.clone()).await {
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
    pub(super) async fn batch(
        &self,
        chain_id: u64,
        calls: &[RpcBatchCall<'_>],
    ) -> Result<Vec<Result<Value, RpcError>>, RpcError> {
        self.batch_walk(chain_id, calls).await.0
    }

    /// [`Self::batch`], also reporting what each endpoint said about each
    /// call it answered (core `rpc_walk`).
    pub(super) async fn batch_walk(
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
        let first_id = self
            .request_id
            .fetch_add(calls.len() as u64, Ordering::Relaxed);
        let mut results = (0..calls.len()).map(|_| None).collect::<Vec<_>>();
        let mut unresolved = (0..calls.len()).collect::<Vec<_>>();
        let mut saw_batch_response = false;

        for url in urls {
            if self.validate_chain(chain_id, &url).await.is_err() {
                continue;
            }
            let payload = unresolved
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
            let response = match self.http.post(&url).json(&payload).send().await {
                Ok(response) => response,
                Err(_) => continue,
            };
            let mut responses = match response.error_for_status() {
                Ok(response) => match response.json::<Vec<UpstreamResponse>>().await {
                    Ok(responses) => responses,
                    Err(_) => continue,
                },
                Err(_) => continue,
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

    pub(super) async fn broadcast_raw_transaction(
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
                BroadcastOutcome::Ambiguous(join_broadcast_diagnostics(ambiguous_diagnostics))
            } else {
                BroadcastOutcome::Rejected(join_broadcast_diagnostics(rejection_diagnostics))
            },
        )
    }

    async fn validate_chain(&self, chain_id: u64, url: &str) -> Result<(), RpcError> {
        let key = (chain_id, url.to_owned());
        if self.validated_urls.lock().await.contains(&key) {
            return Ok(());
        }
        let response = self.request(url, "eth_chainId", json!([])).await?;
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
        self.validated_urls.lock().await.insert(key);
        Ok(())
    }

    async fn request(
        &self,
        url: &str,
        method: &str,
        params: Value,
    ) -> Result<UpstreamResponse, RpcError> {
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        self.http
            .post(url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .map_err(|_| RpcError::Unavailable)?
            .error_for_status()
            .map_err(|_| RpcError::Unavailable)?
            .json::<UpstreamResponse>()
            .await
            .map_err(|_| RpcError::InvalidResponse)
    }

    async fn urls_or_error(&self, chain_id: u64) -> Result<Vec<String>, RpcError> {
        let urls = self.urls(chain_id).await;
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
            && let Some(url) = alchemy::rpc_url(chain_id, api_key)
        {
            append_unique_urls(&mut urls, [url]);
        }

        let directory_urls =
            if let Some(urls) = self.directory_urls.lock().await.get(&chain_id).cloned() {
                urls
            } else {
                let (urls, cacheable) = match chain_directory::directory_rpc_urls(chain_id).await {
                    Ok(urls) => (urls, true),
                    // Do not cache an outage: a subsequent queued batch should be able to retry
                    // the controlled directory after its built-in request retries are exhausted.
                    Err(()) => (Vec::new(), false),
                };
                if cacheable {
                    self.directory_urls
                        .lock()
                        .await
                        .insert(chain_id, urls.clone());
                }
                urls
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

use vela_relay_core::broadcast::{self as core_broadcast, join_broadcast_diagnostics};

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

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap, HashSet},
        sync::{Arc, atomic::AtomicU64},
        time::Duration,
    };

    use axum::{Json, Router, routing::post};
    use reqwest::Client;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::Mutex};
    use vela_relay_core::rpc_walk::{MethodAnswer, MissingMethods};

    use super::{TrustedRpcClient, append_unique_urls};

    const AVALANCHE: u64 = 43_114;

    /// How a fake node answers: every node reports `AVALANCHE` for
    /// `eth_chainId` and `0x1` for any other method, except as named.
    #[derive(Clone, Copy, PartialEq)]
    enum Node {
        /// `-32601` for `eth_simulateV1`, as every Avalanche node answered it
        /// on 2026-10-08.
        LacksSimulateV1,
        /// A `result` for `eth_simulateV1`.
        ServesSimulateV1,
    }

    /// A fake JSON-RPC node on a loopback port; returns its URL and the
    /// methods it was asked, in order.
    async fn fake_node(node: Node) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let answer = move |call: &Value| -> Value {
            let id = call["id"].clone();
            let method = call["method"].as_str().unwrap_or_default().to_owned();
            seen.lock().unwrap().push(method.clone());
            match method.as_str() {
                "eth_chainId" => {
                    json!({ "jsonrpc": "2.0", "id": id, "result": format!("0x{AVALANCHE:x}") })
                }
                "eth_simulateV1" if node == Node::LacksSimulateV1 => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": "the method eth_simulateV1 does not exist/is not available",
                    },
                }),
                _ => json!({ "jsonrpc": "2.0", "id": id, "result": "0x1" }),
            }
        };
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    post(move |Json(body): Json<Value>| {
                        let answer = answer.clone();
                        async move {
                            Json(match body {
                                Value::Array(calls) => {
                                    Value::Array(calls.iter().map(&answer).collect())
                                }
                                call => answer(&call),
                            })
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        (url, asked)
    }

    /// A transport over `urls` alone: no Alchemy, and an empty directory entry
    /// so nothing is fetched from the network.
    fn client(urls: Vec<String>) -> TrustedRpcClient {
        TrustedRpcClient {
            http: Client::builder()
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap(),
            explicit_urls: Arc::new(BTreeMap::from([(AVALANCHE, urls)])),
            alchemy_api_key: None,
            directory_urls: Arc::new(Mutex::new(HashMap::from([(AVALANCHE, Vec::new())]))),
            validated_urls: Arc::new(Mutex::new(HashSet::new())),
            missing_methods: Arc::new(std::sync::Mutex::new(MissingMethods::default())),
            request_id: Arc::new(AtomicU64::new(1)),
        }
    }

    #[tokio::test]
    async fn a_walk_every_node_answers_with_no_such_method_is_remembered() {
        let (first, _) = fake_node(Node::LacksSimulateV1).await;
        let (second, _) = fake_node(Node::LacksSimulateV1).await;
        let rpc = client(vec![first, second]);

        let (result, answers) = rpc.call_walk(AVALANCHE, "eth_simulateV1", json!([])).await;
        assert!(result.is_err());
        assert_eq!(
            answers,
            vec![MethodAnswer::Unsupported, MethodAnswer::Unsupported]
        );
        rpc.note_method_walk(AVALANCHE, "eth_simulateV1", &answers);
        assert!(rpc.lacks_method(AVALANCHE, "eth_simulateV1"));
    }

    #[tokio::test]
    async fn one_node_that_serves_the_method_keeps_it_first() {
        let (first, _) = fake_node(Node::LacksSimulateV1).await;
        let (second, _) = fake_node(Node::ServesSimulateV1).await;
        let rpc = client(vec![first, second]);

        let calls = [super::RpcBatchCall {
            method: "eth_simulateV1",
            params: json!([]),
        }];
        let (result, answers) = rpc.batch_walk(AVALANCHE, &calls).await;
        assert!(matches!(result.as_deref(), Ok([Ok(_)])));
        assert_eq!(
            answers,
            vec![MethodAnswer::Unsupported, MethodAnswer::Served]
        );
        rpc.note_method_walk(AVALANCHE, "eth_simulateV1", &answers);
        assert!(!rpc.lacks_method(AVALANCHE, "eth_simulateV1"));
    }

    #[test]
    fn appends_each_executor_rpc_url_once() {
        let mut urls = vec!["https://first.example".into()];
        append_unique_urls(
            &mut urls,
            [
                "https://first.example".into(),
                "https://second.example".into(),
                "https://second.example".into(),
            ],
        );

        assert_eq!(
            urls,
            vec![
                "https://first.example".to_owned(),
                "https://second.example".to_owned(),
            ]
        );
    }
}
