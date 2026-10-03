//! Entry points of the Cloudflare shell.

use serde_json::Value;
use worker::{Context, Env, MessageBatch, MessageExt, Request, Response, Result, event};

use crate::proto::{ItemResolutionWire, LaneCommand, LaneReply, RecordCommand, RecordReply};

/// Permissive CORS, matching the docker shell's `CorsLayer::permissive()`.
///
/// Not cosmetic: the wallet is a BROWSER app and every call it makes to this
/// relay is cross-origin. Without these headers the browser refuses the
/// response before any code sees it, so the relay looks unreachable from the
/// web shell while `curl` and the native shells — neither of which enforces
/// CORS — see a perfectly healthy service. Found exactly that way, in a real
/// browser, during the Arc integration (vela-wallet spec 060).
fn with_cors(response: Response) -> Result<Response> {
    let headers = response.headers().clone();
    headers.set("access-control-allow-origin", "*")?;
    headers.set(
        "access-control-allow-methods",
        "GET, POST, OPTIONS, HEAD, PUT, PATCH, DELETE",
    )?;
    headers.set("access-control-allow-headers", "*")?;
    headers.set("access-control-expose-headers", "*")?;
    headers.set("access-control-max-age", "86400")?;
    Ok(response.with_headers(headers))
}

#[event(fetch)]
pub async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    // The preflight never reaches the router: it carries no body and asks only
    // whether the real call is allowed.
    if req.method() == worker::Method::Options {
        return with_cors(Response::empty()?.with_status(204));
    }
    with_cors(crate::http::handle(req, env).await?)
}

/// The queue consumer: groups a delivered batch by (chainId, lane) — pure
/// core routing — forwards each group to its LaneDO, and acks/retries per
/// message from the returned resolutions. Ordering is NOT assumed
/// (research.md R4): redelivery and reorder are absorbed by the core's
/// idempotency and nonce-triage rules.
#[event(queue)]
pub async fn queue(batch: MessageBatch<Value>, env: Env, _ctx: Context) -> Result<()> {
    let config = match crate::config::CfConfig::from_env(&env) {
        Ok(config) => config,
        Err(error) => {
            worker::console_error!("configuration error: {error}");
            batch.retry_all();
            return Ok(());
        }
    };
    if !config.executor_enabled {
        // Enqueue-only posture: leave everything queued, exactly as the
        // docker deployment with the consumer disabled.
        batch.retry_all();
        return Ok(());
    }

    if batch.queue() == DEAD_LETTER_QUEUE {
        give_up_dead_letters(&batch, &env, config.relayer_count).await?;
        return Ok(());
    }

    let messages = batch.messages()?;
    // (chain, lane) → (message indexes, routed operations)
    let mut groups: std::collections::BTreeMap<(u64, u8), (Vec<usize>, Vec<_>)> =
        std::collections::BTreeMap::new();

    for (index, message) in messages.iter().enumerate() {
        match parse_routed(message.body(), config.relayer_count) {
            Ok(routed) => {
                let key = (routed.chain_id, routed.lane);
                let entry = groups.entry(key).or_default();
                entry.0.push(index);
                entry.1.push(routed);
            }
            Err(error) => {
                // Malformed envelopes dead-letter durably before being acked,
                // mirroring the docker consumer's handle_malformed.
                worker::console_error!(
                    "malformed queue UserOperation requires durable dead-lettering: {error}"
                );
                if dead_letter_malformed(&env, message.body(), &error).await {
                    message.ack();
                } else {
                    message.retry();
                }
            }
        }
    }

    for ((chain_id, lane), (indexes, operations)) in groups {
        let resolutions = lane_execute(&env, chain_id, lane, operations).await;
        match resolutions {
            Some(resolutions) if resolutions.len() == indexes.len() => {
                for (position, index) in indexes.into_iter().enumerate() {
                    match &resolutions[position] {
                        ItemResolutionWire::Durable => messages[index].ack(),
                        ItemResolutionWire::Failed { reason } => {
                            worker::console_log!(
                                "lane item will be redelivered: chain_id={chain_id} lane={lane} reason={reason}"
                            );
                            retry_later(
                                &messages[index],
                                reason == vela_relay_core::execution::FUNDING_WAIT_FINISH,
                            );
                        }
                    }
                }
            }
            _ => {
                for index in indexes {
                    retry_later(&messages[index], false);
                }
            }
        }
    }
    Ok(())
}

/// The queue the platform moves a message to once its redeliveries are spent
/// (`dead_letter_queue` in wrangler.jsonc), and where the shell and the lane
/// dead-letter what they cannot process.
const DEAD_LETTER_QUEUE: &str = "vela-relay-dlq";

/// The dead-letter queue's consumer. Until it existed nothing read the queue:
/// an operation whose redeliveries ran out stayed `queued` until its record
/// expired, and the wallet waited on it ("taking longer than usual") for good.
///
/// A message the PLATFORM moved here is the original queue envelope; its
/// record gives up if the core says so (`lifecycle::dead_letter_patch`: only
/// an operation that never left the relay). What the shell or the lane
/// dead-lettered on purpose — a malformed envelope, an envelope that does not
/// match its record — is a different payload with no `schemaVersion`; it was
/// reported when it was sent and its record is not this message's to judge.
/// Every message is acknowledged: the dead-letter queue has nowhere further to
/// go, and a record that could not be reached is retried by the platform.
async fn give_up_dead_letters(
    batch: &MessageBatch<Value>,
    env: &Env,
    relayer_count: usize,
) -> Result<()> {
    let now_ms = worker::Date::now().as_millis();
    for message in batch.messages()? {
        let Ok(routed) = parse_routed(message.body(), relayer_count) else {
            message.ack();
            continue;
        };
        match crate::admission::record_command(
            env,
            routed.chain_id,
            &routed.user_operation_hash,
            &RecordCommand::GiveUp { now_ms },
        )
        .await
        {
            Ok(RecordReply::Patched { patched }) => {
                if patched {
                    worker::console_warn!(
                        "dead-lettered UserOperation rejected after its retries ran out: chain_id={} user_operation_hash={}",
                        routed.chain_id,
                        routed.user_operation_hash
                    );
                }
                message.ack();
            }
            _ => message.retry(),
        }
    }
    Ok(())
}

/// Redeliver after the core's backoff for the message's age
/// (`hold::redelivery_delay_ms`). A bare `retry()` came back about once a
/// second: an operation waiting on its relayer's funding spent the queue's
/// 100 retries in under two minutes and was dead-lettered, left `queued`
/// forever (Arbitrum, 2026-10-03). The docker consumer has no retry limit to
/// spend; this queue does.
fn retry_later(message: &worker::Message<Value>, waiting_on_funding: bool) {
    let sent_ms = message.timestamp().as_millis();
    let age_ms = worker::Date::now().as_millis().saturating_sub(sent_ms);
    let delay_s =
        vela_relay_core::hold::redelivery_delay_ms(age_ms, waiting_on_funding).div_ceil(1_000);
    message.retry_with_options(
        &worker::QueueRetryOptionsBuilder::new()
            .with_delay_seconds(u32::try_from(delay_s).unwrap_or(u32::MAX))
            .build(),
    );
}

/// The docker consumer's `parse_routed_operation`, with the queue's
/// chain-agnostic envelope: chainId comes from the payload, lane from the
/// core routing fn; the Iggy positional fields have no meaning here.
fn parse_routed(
    payload: &Value,
    pool_width: usize,
) -> std::result::Result<vela_relay_core::task::RoutedUserOperation, String> {
    // Some delivery paths hand the JSON envelope through as a string; unwrap
    // one level before validating, then treat it identically.
    let unwrapped;
    let payload = match payload.as_str() {
        Some(raw) => {
            unwrapped = serde_json::from_str::<Value>(raw)
                .map_err(|error| format!("invalid queue envelope: {error}"))?;
            &unwrapped
        }
        None => payload,
    };
    let schema_version = payload
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .ok_or("invalid queue envelope: schemaVersion is missing")?;
    if schema_version != 1 {
        return Err(format!("unsupported queue schema version {schema_version}"));
    }
    let user_operation_hash = payload
        .get("userOperationHash")
        .and_then(Value::as_str)
        .ok_or("invalid queue envelope: userOperationHash is missing")?
        .to_owned();
    let chain_id = payload
        .get("chainId")
        .and_then(Value::as_u64)
        .ok_or("invalid queue envelope: chainId is missing")?;
    let entry_point = payload
        .get("entryPoint")
        .and_then(Value::as_str)
        .ok_or("invalid queue envelope: entryPoint is missing")?
        .to_owned();
    let user_operation = payload
        .get("userOperation")
        .cloned()
        .ok_or("invalid queue envelope: userOperation is missing")?;
    let sender = user_operation
        .get("sender")
        .and_then(Value::as_str)
        .ok_or("UserOperation sender is missing")?
        .to_owned();
    let lane = vela_relay_core::vault::relayer_index_for_sender(&sender, pool_width) as u8;
    // Absent on every envelope written before submission tiers existed; a name
    // this relay does not know fails the envelope rather than quietly
    // submitting at a speed nobody chose.
    let submission_tier = match payload.get("submissionTier") {
        Some(value) => Some(
            serde_json::from_value::<vela_relay_core::gas_math::SubmissionTier>(value.clone())
                .map_err(|error| format!("invalid queue envelope: submissionTier {error}"))?,
        ),
        None => None,
    };

    Ok(vela_relay_core::task::RoutedUserOperation {
        schema_version: schema_version as u32,
        user_operation_hash,
        chain_id,
        entry_point,
        user_operation,
        sender,
        lane,
        stream: "vela-relay-ops".into(),
        partition_id: 0,
        offset: 0,
        submission_tier,
    })
}

async fn lane_execute(
    env: &Env,
    chain_id: u64,
    lane: u8,
    operations: Vec<vela_relay_core::task::RoutedUserOperation>,
) -> Option<Vec<ItemResolutionWire>> {
    let namespace = env.durable_object("LANES").ok()?;
    let id = namespace.id_from_name(&format!("{chain_id}:{lane}")).ok()?;
    let stub = id.get_stub().ok()?;

    let body = serde_json::to_string(&LaneCommand::ExecuteBatch { operations }).ok()?;
    let headers = worker::Headers::new();
    headers.set("content-type", "application/json").ok()?;
    let mut init = worker::RequestInit::new();
    init.with_method(worker::Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body)));
    let request = worker::Request::new_with_init("https://lane-do/", &init).ok()?;
    let mut response = stub.fetch_with_request(request).await.ok()?;
    if response.status_code() != 200 {
        return None;
    }
    match response.json::<LaneReply>().await.ok()? {
        LaneReply::Resolutions { resolutions } => Some(resolutions),
    }
}

async fn dead_letter_malformed(env: &Env, payload: &Value, error: &str) -> bool {
    let Ok(queue) = env.queue("DLQ_QUEUE") else {
        return false;
    };
    let user_operation_hash = payload
        .get("userOperationHash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let dead_letter = serde_json::json!({
        "reason": error,
        "userOperationHash": user_operation_hash,
        "payload": payload,
    });
    queue.send(&dead_letter).await.is_ok()
}
