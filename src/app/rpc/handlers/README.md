# RPC handlers

## `eth_supportedEntryPoints`

Call `POST /{chainId}` with the following request body:

```json
{
  "jsonrpc": "2.0",
  "method": "eth_supportedEntryPoints",
  "params": [],
  "id": 1
}
```

Response:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": ["0x0000000071727De22E5E9d8BAf0edAc6f37da032"]
}
```

The address list is defined by the `SUPPORTED_ENTRY_POINTS` constant in `supported_entry_points.rs`. The first address is the Bundler's preferred EntryPoint. The current implementation returns the same list for every `chainId`.

## `pimlico_getUserOperationGasPrice`

Call `POST /{chainId}` with the following request body:

```json
{
  "jsonrpc": "2.0",
  "method": "pimlico_getUserOperationGasPrice",
  "params": [],
  "id": 1
}
```

The handler delegates estimation to `GasPriceManager`. On EIP-1559 chains it takes the base fee from `eth_feeHistory` and the tip from `eth_maxPriorityFeePerGas` (then `eth_gasPrice` minus the latest base fee) — the same tip rule the executor signs with, `gas_math::market_tip`. If fee history is unavailable, it falls back to `eth_gasPrice` for legacy-compatible pricing. `docs/fees.md` §2b has the per-tier arithmetic.

The manager returns slow (100%), standard (110%), and fast (120%) tiers. `maxFeePerGas` and `maxPriorityFeePerGas` are scaled independently, while preserving `maxFeePerGas >= maxPriorityFeePerGas`.

Successful gas-price quotes are cached for five seconds. The cache is isolated by `chainId` and caller-provided RPC identity, so callers with different RPC headers never share a quote. Concurrent cache misses for the same key are coalesced into one upstream calculation.

Response:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "slow": {
      "maxFeePerGas": "0x829b42b5",
      "maxPriorityFeePerGas": "0x829b42b5"
    },
    "standard": {
      "maxFeePerGas": "0x88d36a75",
      "maxPriorityFeePerGas": "0x88d36a75"
    },
    "fast": {
      "maxFeePerGas": "0x8f0b9234",
      "maxPriorityFeePerGas": "0x8f0b9234"
    }
  }
}
```

RPC sources are tried in this order:

1. The HTTPS URL supplied by the `x-vela-rpc-url` request header.
2. Alchemy when `ALCHEMY_API_KEY` is set and the numeric EVM `chainId` is present in the Alchemy Chain Resource Directory registry. The registry includes all 80 networks currently listed at `https://www.alchemy.com/rpc`; non-EVM entries are retained for future use but are outside this ERC-4337 JSON-RPC API's scope.
3. HTTPS URLs from the chain directory's `{base}/chains/eip155-{chainId}.json`, in the published order. The base is `VELA_RELAY_CHAIN_DIRECTORY_URL`, `https://ethereum-data.getvela.app` when unset.

Each upstream request has a one-second deadline. The full gas-price calculation, including `eth_feeHistory`, priority-fee fallback, legacy fallback, and every source switch, has a 2.8-second internal budget. This leaves time for the HTTP response to reach the caller within three seconds. If no source succeeds in that budget, the handler returns JSON-RPC error `-32000` with the message `gas price RPC request timed out` instead of waiting longer.

Source-selection logs include `source`, `method`, and `rpc_url`. The URL contains only the scheme, host, and port; its path and query string are redacted so API keys are not written to logs.

Successful responses include the `x-vela-rpc-domain` header. Its value is the domain of the RPC that supplied the primary gas-price data, such as `eth-mainnet.g.alchemy.com`; endpoint paths and API keys are never returned.

When an RPC request fails, that `chainId`, URL, and method combination enters a 30-second cooldown. Calls skip it during the cooldown and continue with the next source. The shared failure cache holds at most 1,024 entries to keep memory bounded when callers supply many distinct URLs.

The shared source selection, upstream JSON-RPC request, and failover logic lives in `../../../utils/rpc.rs`. Future services and RPC handlers can reuse `utils::rpc::call` with their own method name and parameters.

Set the Alchemy key in the process environment or in the project-root `.env` file before starting the service. Process environment variables take precedence over `.env` values:

```sh
export ALCHEMY_API_KEY="your-key"
cargo run
```

```dotenv
ALCHEMY_API_KEY="your-key"
```

To use a caller-provided RPC before Alchemy and fallback sources, include an HTTPS URL in the request header:

```sh
curl http://127.0.0.1:4567/1 \
  -H 'content-type: application/json' \
  -H 'x-vela-rpc-url: https://your-rpc.example.com' \
  --data '{"jsonrpc":"2.0","method":"pimlico_getUserOperationGasPrice","params":[],"id":1}'
```

## `vela_getInBandGasQuote`

This Vela extension returns the assets that a Safe can use to reimburse an in-band UserOperation.
Call `POST /{chainId}` with one `safeAddress` parameter:

```json
{
  "jsonrpc": "2.0",
  "method": "vela_getInBandGasQuote",
  "params": [{
    "safeAddress": "0x14fB1fB21751E29F7Ec48dC450017552E3D1eA5c"
  }],
  "id": 1
}
```

Each quote identifies the configured settlement recipient, the asset, the user's current balance,
and its USD valuation. `eth_sendUserOperation` still enforces a minimum in-band reimbursement of
`0.00001` native coin or `0.01` USD stablecoin.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": [{
    "recipient": "0xee2cca98ecbff34663591a925968fa4db5a1f0dd",
    "asset": "native",
    "feeToken": null,
    "decimals": 18,
    "symbol": "ETH",
    "balance": "0x0",
    "usdPrice": "3000.12",
    "usdBalance": "0"
  }]
}
```

Chain native-currency and stablecoin metadata is loaded from the Ethereum Data registry and cached
for one hour. A 60-second Binance `{nativeSymbol}USDT` price cache supplies the native USD price.
The native asset is always returned; if that price is unavailable, `usdPrice` and `usdBalance` are
`null` and no stablecoin quote is returned. Stablecoins are restricted to USD-pegged symbols and
use `"1"` as their USD price.

After metadata and price resolution, the handler makes one EVM `eth_call` to the canonical
Multicall3 contract. That call reads the Safe's native balance plus every eligible ERC-20
`decimals()` and `balanceOf(safeAddress)`, reducing the chain-read fan-out to one request.
Successful responses include `x-vela-rpc-domain`, and the standard caller RPC → Alchemy →
Ethereum Data failover order applies.

`usdBalance` is the Safe's balance converted to USD as an exact decimal string. Quotes are ordered
by this value from largest to smallest; assets without a price return `null` and sort last.

## Settlement vault

When `OPERATOR_SECRET` is configured, the settlement recipient is derived with the same HKDF-SHA256 treasury derivation as `vela-bundler`: salt `vela-bundler-dedicated-eoa-v1`, info `treasury`, and a secp256k1 Ethereum address. The derived address is chain-independent and is used as the settlement vault recipient.

`VELA_RELAY_SETTLEMENT_RECIPIENT` remains available for deployments without `OPERATOR_SECRET`. If both variables are set, their addresses must match or startup fails.

## `eth_estimateUserOperationGas`

The relay supports the unpacked EntryPoint v0.7 UserOperation format for the configured EntryPoint. It accepts the optional third `stateOverrides` parameter defined by Pimlico.

```json
{
  "jsonrpc": "2.0",
  "method": "eth_estimateUserOperationGas",
  "params": [
    {
      "sender": "0x1111111111111111111111111111111111111111",
      "nonce": "0x0",
      "factory": null,
      "factoryData": null,
      "callData": "0x",
      "callGasLimit": "0x0",
      "verificationGasLimit": "0x0",
      "preVerificationGas": "0x0",
      "maxFeePerGas": "0x0",
      "maxPriorityFeePerGas": "0x0",
      "paymaster": null,
      "paymasterVerificationGasLimit": null,
      "paymasterPostOpGasLimit": null,
      "paymasterData": null,
      "signature": "0x1234"
    },
    "0x0000000071727De22E5E9d8BAf0edAc6f37da032"
  ],
  "id": 1
}
```

The relay does not forward this bundler-specific method to a normal EVM RPC. It injects the EntryPoint v0.7 simulation code through the standard `eth_call` state-override parameter, then estimates the account execution phase with `eth_estimateGas` using the EntryPoint as `from`.

All supported chains use in-band settlement. `maxFeePerGas` and `maxPriorityFeePerGas` must therefore both be `0x0`; the simulation encodes the same zero values and never substitutes a native EntryPoint fee. The submitted operation is never modified.

Estimation does not require or validate an in-band reimbursement transfer. It estimates the gas
used by the supplied UserOperation and its `callData`; reimbursement admission is performed only
by `eth_sendUserOperation`.

An RPC that rejects state overrides, times out, or is rate limited is cooled down and the next configured source is tried. A genuine EVM revert is returned as a UserOperation simulation error without cooling down that RPC. `FailedOp`, `FailedOpWithRevert`, Solidity panic, and nested gateway revert data are decoded into the JSON-RPC error `data` field. Successful responses include the selected simulation source in `x-vela-rpc-domain`.

The response contains the v0.7 gas fields, including zero-valued paymaster limits when no paymaster is present:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "preVerificationGas": "0x0",
    "verificationGasLimit": "0x0",
    "callGasLimit": "0x0",
    "paymasterVerificationGasLimit": "0x0",
    "paymasterPostOpGasLimit": "0x0"
  }
}
```

## `eth_sendUserOperation`

The relay currently uses in-band settlement for every chain. It does not accept the normal
ERC-4337 native-prefund route.

```json
{
  "jsonrpc": "2.0",
  "method": "eth_sendUserOperation",
  "params": [
    {
      "sender": "0x1111111111111111111111111111111111111111",
      "nonce": "0x0",
      "callData": "0x...",
      "callGasLimit": "0x5208",
      "verificationGasLimit": "0x10000",
      "preVerificationGas": "0x1000",
      "maxFeePerGas": "0x0",
      "maxPriorityFeePerGas": "0x0",
      "signature": "0x..."
    },
    "0x0000000071727De22E5E9d8BAf0edAc6f37da032"
  ],
  "id": 1
}
```

The handler accepts the unpacked EntryPoint v0.7 format and returns the canonical EntryPoint
`userOpHash` when it queues the operation:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": "0x..."
}
```

Admission is intentionally small and deterministic:

- `maxFeePerGas` and `maxPriorityFeePerGas` must both be exactly `0x0`.
- The Safe calldata must be `executeUserOp` delegating to the canonical Safe MultiSend contract.
- The batch must transfer to the configured settlement recipient either at least `0.00001` native coin or at least `0.01` of one stablecoin listed in that chain's `stables` metadata.
- Stablecoin amounts are converted to smallest units using the token's on-chain `decimals()` result. Transfers in unlisted tokens are ignored.
- The operation must have valid v0.7 structural fields and a non-empty signature. EIP-7702 authorization is not enabled yet.

The reimbursement is decoded from the signed calls, not from a wallet-supplied amount. A
transfer-shaped payload does not count unless it is actually nested under the trusted Safe
MultiSend delegatecall.

After admission, the relay first creates a Redis record with `status: "queued"` and a one-hour
TTL, then appends the following envelope to Iggy. It returns the `userOpHash` only after both
writes are acknowledged; an Iggy failure removes the still-unadmitted Redis record. It does not
execute or submit an outer transaction in the HTTP request path.

```json
{
  "schemaVersion": 1,
  "userOperationHash": "0x...",
  "chainId": 1,
  "entryPoint": "0x0000000071727De22E5E9d8BAf0edAc6f37da032",
  "userOperation": { "...": "original v0.7 request object" }
}
```

### Iggy topology and configuration

Each chain has an isolated, single-partition stream named `chain-{chainId}` and a topic named
`default`. For example, an operation for chain `42161` is appended to
`chain-42161/default`. This gives FIFO ordering within a chain while allowing independent chains
to be consumed in parallel.

Create the desired chain streams and their `default` topic in `init-iggy`. The example below
keeps messages for fourteen days; set retention longer than the maximum expected consumer outage
and incident-recovery window.

```sh
for chain_id in 1 10 42161; do
  iggy stream create "chain-${chain_id}"
  iggy topic create "chain-${chain_id}" default 1 none 14d
done
```

For automatic creation on any chain ID, configure a second, separately privileged Iggy identity.
On the first failed write for a chain, the relay creates `chain-{chainId}` and its `default` topic
(one partition, no compression, 14-day expiry), then retries the write once:

```sh
VELA_RELAY_IGGY_PROVISIONER_URL='iggy+tcp://vela-relay-provisioner:<password>@127.0.0.1:5100'
```

Use a distinct production provisioner identity with only stream/topic read-and-manage permissions.
Keep `VELA_RELAY_IGGY_URL` on the low-privilege producer identity. For local development with the
root account, both URLs may temporarily be the same. Because arbitrary public chain IDs can now
create topology, protect the public RPC endpoint with authentication, request rate limits, and an
Iggy stream quota.

The relay requires the following environment variables:

```sh
VELA_RELAY_IGGY_URL='iggy+tcp://vela-relay-producer:<password>@127.0.0.1:5100?reconnection_retries=5&reconnection_interval=1s&reestablish_after=5s&heartbeat_interval=3s&nodelay=true'
VELA_RELAY_IGGY_TOPIC=default                 # optional; this is the default
VELA_RELAY_IGGY_ENQUEUE_TIMEOUT_SECS=5        # optional; this is the default
VELA_RELAY_REDIS_URL='redis://127.0.0.1:6379/0'
VELA_RELAY_REDIS_COMMAND_TIMEOUT_SECS=2       # optional; this is the default
```

`VELA_RELAY_IGGY_URL` and `VELA_RELAY_REDIS_URL` are mandatory. If either service is unavailable,
refuses the write, or does not acknowledge before its timeout, `eth_sendUserOperation` returns
JSON-RPC `-32000` instead of claiming success. The relay never falls back to in-memory state.

Use separate non-root identities in production:

- `VELA_RELAY_IGGY_URL`: producer permissions for sending to existing topics.
- `VELA_RELAY_IGGY_CONSUMER_URL`: list-stream, poll, consumer-group, and offset permissions.
- `VELA_RELAY_IGGY_PROVISIONER_URL`: stream/topic creation permissions.

The consumer URL falls back to the producer URL for local development only. Enable TCP TLS for
any non-loopback connection and keep all three credentials in the runtime secret store. With the
Docker port mapping shown in the deployment example, a relay running on the host connects to TCP
port `5100` (not HTTP port `3010`):

```sh
VELA_RELAY_IGGY_URL='iggy+tcp://<producer>:<password>@127.0.0.1:5100'
VELA_RELAY_IGGY_CONSUMER_URL='iggy+tcp://<consumer>:<password>@127.0.0.1:5100'
```

A relay container on the same Compose network instead uses `iggy:3000`. Port `3010` is the Iggy
HTTP API used by the web UI; it is not the executor transport.

### Iggy configuration

For local and single-identity deployments, only this Iggy setting is required:

```sh
VELA_RELAY_IGGY_URL='iggy+tcp://vela-relay:<password>@127.0.0.1:5100'
```

The relay uses that same connection for producing, consuming, and automatically creating a new
`chain-{id}` stream and its `default` topic. `VELA_RELAY_IGGY_TOPIC` and
`VELA_RELAY_IGGY_ENQUEUE_TIMEOUT_SECS` remain optional overrides with defaults of `default` and
five seconds. Set `VELA_RELAY_IGGY_CONSUMER_URL` and/or `VELA_RELAY_IGGY_PROVISIONER_URL` only
when production credentials are deliberately split by privilege.

### Worker executor

The worker discovers all strictly named `chain-{u64}` streams at startup and every 15 seconds.
Each stream has one shared Iggy consumer group and one dispatcher. The dispatcher routes each
message by the sender's low 32 bits modulo 10 into ten deterministic EOA lanes. Ten independent
Iggy group members cannot consume a one-partition topic concurrently, while ten separate consumer
groups would execute every message ten times; dispatcher-to-lane routing provides ten EOA workers
without either failure mode.

EOAs `0..9` use the same HKDF derivation as `vela-bundler`: salt
`vela-bundler-dedicated-eoa-v1`, info `relayer-#{index}`, and `OPERATOR_SECRET` as input. A lane is
serialized across processes with a Redis lease and a signed-transaction outbox. The exact raw
transaction is persisted before broadcast and replayed after a crash, so an ambiguous RPC response
does not allocate another nonce. Iggy offsets advance only through the contiguous prefix whose
result is durable in Redis.

Discovery accepts any `chain-{id}`. By default, the worker reads `nativeCurrency` and `stables`
from the controlled `eip155-{chainId}.json` directory entry, reads listed ERC-20 decimals from the
chain, and accepts each listed stablecoin as 1 USD. Native gas is converted using Binance's
`<native-symbol>USDT` ticker, cached for 60 seconds. This path has no DEX or Chainlink calls;
`VELA_RELAY_EXECUTOR_CHAIN_ASSETS` is not supported.

The executor starts by default when `OPERATOR_SECRET` is supplied; set
`VELA_RELAY_EXECUTOR_ENABLED=false` for a producer-only relay.

Before signing, every UserOperation is simulated alone; the final assembled `handleOps` is then
simulated again. Reimbursement is parsed only from the canonical Safe
`executeUserOp -> MultiSend(delegatecall) -> CALL` path. Stablecoin payment additionally requires
the final bundle simulation to contain matching allowlisted `Transfer(sender, treasury, amount)`
logs. Costs use the final bundle gas, EIP-1559 fee ceiling, deterministic per-op allocation, safety
buffers, a default 1.4x settlement gate, and per-asset minimums. One payer cannot subsidize another.

Treasury top-ups are serialized once per chain. Redis atomically reserves the daily cap and stores
the signed funding transaction, removing the reserve/outbox crash window. A funding receipt is
required before the relayer submits `handleOps`.

### UserOperation lookups and status

`pimlico_getUserOperationStatus` reads the one-hour Redis record and returns one of
`not_found`, `queued`, `not_submitted`, `submitted`, `rejected`, `included`, or `failed`, with a
`transactionHash` when one is known. A queued retry or locally rejected operation also returns
`lastExecutorStage`, `lastExecutorError`, and `lastExecutorAttemptAtMs`; these show whether it
is blocked in simulation, funding, broadcast, or rejected for a reason such as insufficient
in-band reimbursement.
`eth_getUserOperationByHash` returns the original stored operation while its record remains
available. `eth_getUserOperationReceipt` returns `null` until the operation is included and its
receipt is known.

HTTP status and receipt methods always read Redis and never fan user polling out to EVM RPCs. The
worker's centralized reconciler batches `eth_getTransactionReceipt` by trusted chain RPC. A
reverted `handleOps` transaction marks the whole persisted bundle `failed`; a successful outer
transaction marks successful `UserOperationEvent` entries `included` and on-chain unsuccessful or
missing entries `rejected`. This keeps chain RPC traffic bounded by pending bundle count rather
than wallet polling volume.
