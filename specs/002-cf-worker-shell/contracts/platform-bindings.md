# Contract: Platform Bindings (Operation → Cloudflare primitive)

The core's operation vocabularies are FROZEN (they are the crux seam both
shells share). This contract maps every operation to its CF-shell executor and
names the guarantee that answers it. It is the skeleton of the FR-012
equivalence notes: any cell that changes during implementation must update this
file in the same PR.

Legend: RDO = RecordDO, LDO = LaneDO, TDO = TreasuryDO (see data-model.md).

## AdmissionOperation (fetch handler drives `AdmissionApp`)

| Operation | Executor | Guarantee source |
|---|---|---|
| `LoadSettlementAssets` | metadata service fetch + KV cache | same upstream + fail direction as docker (cache only) |
| `FetchTokenDecimals` | chain RPC fetch (failover) | same upstream policy; shell-owned transport |
| `CreateQueued` | RDO create-if-absent | DO serial = SETNX |
| `LoadExisting` | RDO read | read-your-write |
| `Enqueue` | Queues producer send — the envelope travels as its exact JSON text (the v8 structured clone of a serde map arrives as a JS Map, unreadable as an object; the consumer parses the string) | durable at-least-once; `QueueUnavailable` on send failure (crash-window semantics preserved) |
| `MarkAdmitted` | RDO write | DO serial |

## ExecutionOperation (LaneDO drives `ExecutionApp`)

| Operation | Executor | Guarantee source |
|---|---|---|
| `CheckChainSupported` | trusted-RPC resolution (env override → Alchemy → directory), dynamic per chain; optional `EXECUTION_CHAINS` restriction for shared-key topologies only | docker-parity dynamic chains (R10) |
| `LoadChainAssets` | metadata fetch + KV cache | cache only |
| `LoadRecords` / `ReloadRecord` | RDO reads (one subrequest per record) | read-your-write |
| `DeadLetterRouted` | dead-letter queue send + RDO diagnostic | at-least-once |
| `RestoreQueued` / `MarkAdmitted` / `MarkRejected` / `MarkRejectedWithReason` | RDO writes | DO serial + core decisions |
| `DeferOperation` | LDO delayed-inbox write + alarm (post-increment attempt returned) | DO serial; schedule values core-owned |
| `RecordDeferred` / `NotifyIssue` / `EmitDiagnostic` | RDO diagnostic write / Telegram fetch / structured log | same gating policy (core-decided) |
| `AcquireLaneLease` / `EnsureLaneLease` | constant `true` | LaneDO single-threading (structurally stronger; interrupt path unreachable — declared) |
| `LoadPreparedBundle` / `SavePreparedBundle` / `ClearStaleIntent` | LDO storage (put-if-absent / guarded clear) | DO serial |
| `ResumeBundleIntent` | LDO composite (same sequencing as docker shell) | DO serial + core rules |
| `SimulateIndividually` / `SimulateBundle` / `FetchAccountNonces` | chain RPC fetches (failover), same three-tier order (`eth_simulateV1` → deployed Pimlico `eth_call` → `debug_traceCall`), with `eth_simulateV1` moved last on a chain known or recently proven to lack it (core `simulation::simulate_v1_turn`; the proof is kept per isolate in a `thread_local` `rpc_walk::MissingMethods`, docker: per process); this shell does NOT auto-deploy the Pimlico pair (see Explicitly absent); verdicts convert through the core's `From` impls, so a full execution (`SimulationResult::full_execution`: `eth_simulateV1`, `debug_traceCall`) reaches the core as `OperationSimVerdict::Executed` with its gas and logs, and a lone survivor's is its bundle simulation (no `SimulateBundle`) | shell-owned transport; same interpretation rules (core `simulation`, `rpc_walk`) |
| `FetchTransactionContext` / `FetchTempoContext` / `FetchTreasuryContext` / `FetchTempoTreasuryContext` | chain RPC batch fetches (`FetchTransactionContext`: estimate, block, tip, nonce, balance, and the tier tips' `eth_feeHistory` from core `gas_math::tip_history_params`, whose failure only drops the reward rows) | same call sets as docker |
| (every executor chain read above) | trusted transport walk: an endpoint that fails its `eth_chainId` check or runs out `rpc_timeout_ms` is left out for 3 min (core `rpc_walk::EndpointCooldowns`, `thread_local` per isolate; docker: per process; operator URLs never; all of them when all are cooling); a batch of one is sent as a lone call and a bare-object reply to it is accepted (core `rpc_walk::batch_replies`) | same rules, both shells |
| `FetchMarketPrice` | Binance fetch + KV cache | same fail-open/closed directions (core-decided) |
| `AcquireTreasuryLease` / `EnsureTreasuryLease` / `ReleaseTreasuryLease` | TDO lock (token + deadline; acquire = `SET NX PX`, renew/release guarded by the holder token, expiry judged on read) | DO serial + explicit lock; store error → `Failed` (batch-fatal, as docker). The docker background heartbeat (renew every ttl/3) is replaced by renewal-on-touch: every TDO request from the current holder piggybacks a lease extension (`TreasuryRequest::renew`). The whole TDO boundary speaks JSON TEXT (request body parsed with serde_json, funding intent stored as its JSON string): `PreparedFundingIntent.amount_wei` is a u128, and anything routed through a JS value — `Request::json` (JS `JSON.parse` → JsValue) or DO storage's structured clone — silently degrades it to a float (found by Gate 3) |
| `LoadPreparedFunding` / `SaveFundingIntent` / `ClearFundingIntent` | TDO storage | DO serial |
| `SignTreasuryTransfer` / `SignTreasuryPathUsd` / `SignBundle` / `SignTempoBundle` | core signing fns + secret bindings | keys never enter core; same signing math |
| `AcquireReceiptProbe` | TDO expiring throttle slot (`probe:{txhash}` deadline; never released, exactly the docker per-interval receipt lease; the slot is deleted as housekeeping when its funding intent clears) | DO serial |
| `FetchTransactionReceipt` | chain RPC fetch | same policy |
| `Pause` | `worker::Delay` (docker: `tokio::time::sleep`, fenced like any leased op) | the core paces and bounds every pause (`pace::top_up_receipt_wait`, ≤ 5 s a top-up); the TDO lease (30 s, renewed on touch) outlives it |
| `RecordTreasuryShortfall` / `RecordPartialTopUp` / `RecordFundingSubmitted` / `RecordUnprovenFunding` / `NoteFundingReceipt` | TDO/RDO writes + logs | same texts |
| `CheckBroadcastSeen` / `RememberBroadcast` / `ForgetBroadcast` | LDO cache | cache (30 s), loss harmless |
| `BroadcastRaw` / `ProbeTransactionKnown` / `ProbeStaleNonce` | chain RPC fetches | same classification rules (core `broadcast`) |
| `RecordUnprovenBroadcast` | LDO/RDO diagnostic | same texts |
| `MarkBundleSubmitted` | per-member RDO guarded writes + LDO index | core `decide_bundle_submission` per member; indexed-count guard unchanged |

## Time-driven behaviors

| Behavior | docker mechanism | CF mechanism | Tolerance (declared) | Measured (Gate 4, wrangler dev) |
|---|---|---|---|---|
| Hold-ladder redelivery | Redis zset + claim readers | LDO alarm at min(due); due entries re-drive through the same batch entry; a transient failure climbs the same core ladder in place (claim fencing: a re-park during the batch invalidates the pass's snapshot) | fire within max(30 s, 10%) of due | attempts 1→4 re-driven at +5 s/+10 s/+21 s vs the core's 5/10/20 s ladder — deviation ≤ 1 s |
| Receipt confirmation checks | shell reconciler loop, ticking every `pace::RECEIPT_PACE_FLOOR_MS` (or the configured interval, if shorter); each prepared intent only when its own deadline is due (in-process map by transaction hash); the receipt claim's TTL is the chain's pace | LDO reconcile alarm while an intent exists (as-built: receipts are bundle-scoped exactly as docker; RecordDO alarms remain TTL-only) | the chain's pace, core `pace::receipt_pace_ms`: a block, at most once a second, never slower than `VELA_RELAY_EXECUTOR_RECEIPT_POLL_SECS` (3 s); a chain with no listed block time keeps the configured interval | submitted → `included` in 2–3 s after mining (measured before the per-chain pace) |
| Prepared-bundle reconcile | per-intent deadline: armed on save (+ configured interval: the save-to-broadcast crash window, left to the pass until then) and advanced on submit (+ the chain's pace, from the broadcast) | same LDO alarm (resume → receipt → per-member `receipt_patch` → clear); armed on save (+ configured interval) and advanced on submit (+ the chain's pace) | same values | intent cleared with the receipt write; lane accepted follow-up work immediately |
| Record TTL expiry | Redis TTL | RDO alarm cleanup | ± one alarm granule; never early | unchanged (T00x behavior) |
| Funding retry cadence | in-batch + redelivery | queue retry delay + TDO state | core-owned values; a fresh top-up is first waited on in-batch for about two blocks (`pace::top_up_receipt_wait`) | Gate 3: four full cycles |

Delayed-payload retention: `max(VELA_RELAY_EXECUTOR_ATTEMPT_TTL_SECS, 14 d)`
sliding from the last park/retry write — the docker
`attempt_ttl.max(USER_OPERATION_QUEUE_RETENTION)` PEXPIRE refresh, spelled as
an `updated_ms` check at re-drive time.

## Explicitly absent on this shell

- The lease-interrupt channel (`ExecutionOutcome::Interrupted`) is never
  produced: mutual exclusion is structural. The vocabulary stays shared and
  unchanged; the docker shell continues to use it.
- No operation exists to delete an admitted record (Constitution III — the
  absent-operation guarantee carries over verbatim).
- Automatic Pimlico simulation-contract deployment (docker
  `SimulationContractDeployer`) has no counterpart: deployment needs treasury
  signing plus a receipt wait, which belongs to the docker shell. The CREATE2
  addresses are a pure function of the shared treasury (core
  `simulation::pimlico_contracts_for_treasury`), so a pair deployed by the
  docker shell is found `Ready` here. A `Missing` pair falls through to
  `debug_traceCall`; the `SimulationVerdict::Pending` verdict and the
  deployment-wait diagnostics are never produced on this shell (chains lacking
  all three tiers defer on the core's hold ladder, unchanged).
