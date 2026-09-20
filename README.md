# Vela Relay

Vela Relay is an ERC-4337 relay and bundler service. It accepts UserOperations over JSON-RPC,
durably enqueues them in Iggy, records their state in Redis, and executes queued operations in
the background.

The executor is enabled by default. Chain payment-asset metadata comes from Vela's controlled
public directory; native-asset prices come from Binance and listed stablecoins are valued at
1 USD. Gnosis xDAI is fixed at 1 USD and does not use an exchange quote. No per-chain asset or
oracle configuration is required. Tempo is the exception by design:
it has no native gas coin, so Relay uses pathUSD directly and does not query either the chain
directory or Binance for Tempo quotes.

## Architecture

The repository is a three-member workspace with a strict core/shell split
(mirroring the discipline of `p256-index`): one decision core, two deployable
shells.

- **`vela-relay-core`** owns every business decision and is deliberately
  I/O-free — no Redis, no Iggy, no HTTP clients, no runtime, no clocks.
  Modules are business domains: `admission` (the two-phase
  `eth_sendUserOperation` program), `execution` (the per-lane batch program),
  `lifecycle` (the single authoritative status transition table), `settlement`
  (reimbursement parsing, evaluation, and the accept/reprice verdict), `hold`
  (the delayed-inbox ladder and budget), `broadcast`, `funding`, `simulation`
  (verdict parsing and reasons), `receipt`, `wire` (the JSON-RPC envelope
  bytes), `quote`, `estimate`, `alert`, `gas_math`, `abi`, `signing`, `vault`,
  `tempo`, and the shared `task` vocabulary. `cargo test -p vela-relay-core`
  runs the whole decision suite in well under a second with no infrastructure.
- **`vela-relay`** (this crate) is the docker shell: Axum transport, Redis
  storage (Lua reduced to mechanical guarded writes), Iggy queueing, EVM RPC,
  key custody, Binance, and Telegram. HTTP admission and the lane executor
  both drive one crux `Core` per unit of work — the shell executes the
  requested operations and reports what happened back as data; failures fold
  into result variants and the core decides what they mean.
- **`vela-relay-cf`** is the Cloudflare shell (wasm-only; excluded from
  native default builds): the same core decisions wired to Workers
  primitives — Durable Objects supply the guard semantics Redis Lua supplies
  (RecordDO per operation, LaneDO per chain+lane, TreasuryDO per chain),
  Queues replace Iggy, KV holds loss-harmless caches only, and DO alarms
  replace the timer loops. The external JSON-RPC surface is byte-identical to
  the docker shell's (both render through `vela_relay_core::wire`). See
  `docs/cloudflare.md`.

Both shells are governed by Spec Kit artifacts — `specs/001-crux-core-split/`
(the core/shell split) and `specs/002-cf-worker-shell/` (the second shell,
including the Operation→primitive bindings contract and measured tolerances) —
and the project constitution in `.specify/memory/constitution.md`.

## Requirements

- Remote Iggy instance reachable from the Relay process.
- Remote Redis instance reachable from the Relay process.
- `OPERATOR_SECRET` for the relayer pool when the executor is enabled.

## Configure and run

Copy the example configuration and replace every placeholder. Keep the resulting `.env` private.

```sh
cp .env.example .env
cargo run --release
```

The minimal configuration is:

```dotenv
VELA_RELAY_IGGY_URL=iggy+tcp://username:password@iggy.example.com:3000
VELA_RELAY_REDIS_URL=redis://:password@redis.example.com:6379
OPERATOR_SECRET=your-operator-secret
```

`VELA_RELAY_IGGY_URL` is the only Iggy connection setting required. Consumer and provisioner
connections inherit it automatically. For a producer-only instance, set
`VELA_RELAY_EXECUTOR_ENABLED=false`; then an operator secret is not needed.

For execution, Relay resolves RPC endpoints automatically from Vela's controlled chain directory.
If `ALCHEMY_API_KEY` is set, its endpoint is tried first for networks Alchemy supports. You can
optionally prepend an explicit trusted endpoint for a particular chain:

```dotenv
VELA_RELAY_EXECUTOR_RPC_URLS={"42161":"https://your-rpc.example"}
```

For native-gas chains, a low relayer balance triggers a durable treasury top-up. The target is
the greater of the next bundle prefund multiplied by `5` and the configured float target. If
Binance supplies the native USD price, a single top-up is capped at USD 20; without a price the
static `VELA_RELAY_EXECUTOR_TOP_UP_MAX_WEI` cap is used instead (10 native tokens by default).

## Telegram executor alerts

To be notified when a queued UserOperation cannot progress through simulation, funding,
broadcast, or another executor stage, configure both Telegram values:

```dotenv
TELEGRAM_BOT_TOKEN=your-bot-token
TELEGRAM_CHAT_ID=your-chat-id
```

Relay stores alert suppression in Redis. The first occurrence of a failure sends a Telegram
message; identical `chain + stage + normalized error` alerts are suppressed across all workers
and Relay instances for 30 minutes by default. Set
`VELA_RELAY_TELEGRAM_ALERT_COOLDOWN_SECS` to change the cooldown. If Telegram is temporarily
unreachable, Relay releases the suppression slot so a later executor retry can notify again.

When a trusted node does not expose `eth_simulateV1`, Relay first uses the vendored Alto
Pimlico/EntryPoint v0.7 simulation pair through `eth_call`. If that pair is absent, it is deployed
lazily through the canonical CREATE2 deployer using the treasury signer: one durable deployment
transaction is confirmed at a time, then the queued UserOperation is retried. This requires the
canonical deployer at `0x4e59…4956c` to exist on the network; Tempo is excluded because it has no
native-token EIP-1559 deployment path.

## Tempo (pathUSD gas)

Tempo mainnet (`4217`) and Moderato (`42431`) are enabled without asset or oracle configuration.
Their outer transactions use Tempo's native `0x76` envelope and pay fees in pathUSD
(`0x20c0000000000000000000000000000000000000`, six decimals).

`vela_getInBandGasQuote` therefore returns the Safe's pathUSD balance as the single `erc20`
quote with `usdPrice: "1"`; it does not return a synthetic native-coin quote and makes no Binance
request. The UserOperation still uses zero EntryPoint fee fields and must include a trusted Safe
MultiSend transfer of at least `0.01` pathUSD to the settlement vault. `feeToken` is optional and
defaults to pathUSD; a different fee token is rejected until it has an explicit float-management
policy.

The executor derives the relayer's required pathUSD float from the declared UserOperation gas
limits, verifies the final execution and exact pathUSD transfer log with `eth_simulateV1` (or a
trusted `debug_traceCall` fallback), then submits `handleOps` in a signed `0x76` transaction. If
the relayer float is low, the treasury automatically sends a durable pathUSD top-up through a
separate self-paying `0x76` transaction.

Tempo uses the same automatic controlled-directory RPC resolution. No Tempo-specific RPC
configuration is needed. Add an explicit endpoint only when you want it tried ahead of the
directory endpoints:

```dotenv
VELA_RELAY_EXECUTOR_RPC_URLS={"4217":"https://your-tempo-rpc.example"}
```

## Docker

Docker Compose starts only Relay; it deliberately connects to your existing remote Iggy and
Redis services rather than creating either of them.

```sh
cp .env.example .env
docker compose up --build -d
curl --fail http://127.0.0.1:4567/readyz
```

When Iggy or Redis runs on the Docker host, use `host.docker.internal` in their URLs instead of
`127.0.0.1`. For a published release image, set `VELA_RELAY_IMAGE` in `.env`, then run:

```sh
docker compose pull relay
docker compose up -d --no-build
```

See [the Docker deployment guide](docs/docker.md) for configuration details and Docker Hub
publishing setup.

## In-band fees

Relay charges no separate fee: each UserOperation declares zero EntryPoint fees
and embeds a trusted Safe MultiSend transfer reimbursing the settlement recipient
for the gas the relay spends. The relay requires `max(1.4 × gas × (2×base+tip),
floor)` (floor = 0.00001 native or $0.01 stablecoin), recovers 1.4× its gas, and
reprices the outer transaction down toward the inclusion floor rather than
rejecting an honest-but-short payment. A client must pay ABOVE this minimum to
survive the gas-price drift between signing and inclusion. The full rule — the
requirement, the repricing safety valve, the client-side headroom math, and
stablecoin/Tempo specifics — is documented in [docs/fees.md](docs/fees.md).

## HTTP endpoints

| Endpoint | Purpose |
| --- | --- |
| `POST /{chain_id}` | ERC-4337 JSON-RPC endpoint for a chain, for example `POST /42161`. |
| `GET /healthz` | Liveness check; returns `204` while the process is alive. |
| `GET /readyz` | Readiness check; returns `204` after all worker jobs are ready. |
| `GET /health` | Service health information. |
| `GET /version` | Release tag and commit of the running build. |

## Build identity

`GET /` and `GET /version` both report which build is running:

```json
{ "name": "vela-relay", "status": "ok", "version": "v0.9.1", "commit": "0c559650b08d" }
```

Both fields are embedded at compile time by `build_info.rs`, which both shells'
build scripts share, so a deployment cannot report a release it is not running:

- **`version`** — the release tag. `v0.9.1` means the build *is* that release;
  `v0.9.1+3` means three commits past it, and `-dirty` is appended when tracked
  files were modified. It deliberately does not repeat the commit SHA the way
  `git describe` would (`v0.9.1-3-gabc123`) — the `commit` field beside it
  already says that, and said it at a different length. It is *not* the
  `Cargo.toml` version, which has read `0.1.0` across every release tagged so
  far.
- **`commit`** — the 12-character commit SHA (the full `GITHUB_SHA` truncated in
  CI, `git rev-parse` otherwise).

Both read `unknown` when built from a source tree with no `.git` and no CI
environment.

## Releases

Pushing a `v*` tag creates native GitHub Release assets for Linux, macOS, and Windows across
Intel/AMD64 and ARM64 where the platform supports it. The same workflow publishes multi-platform
Linux Docker images (`linux/amd64` and `linux/arm64`) to
`${DOCKERHUB_USERNAME}/vela-relay`.

Each Docker image packages the exact matching Linux executable produced for the GitHub Release;
the workflow does not compile Rust a second time inside Docker.

For Docker Hub publishing, configure the repository Actions settings:

- Variable: `DOCKERHUB_USERNAME`
- Secret: `DOCKERHUB_TOKEN` (a Docker Hub token permitted to push the repository)

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked
cargo test --locked
```

The integration test that requires a running Iggy service is intentionally ignored by default.
