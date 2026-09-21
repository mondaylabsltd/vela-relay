# Cloudflare deployment (vela-relay-cf)

The second deployment target runs the same `vela-relay-core` decisions on
Cloudflare Workers (Rust/wasm): Durable Objects replace Redis, Queues replace
Iggy, KV holds loss-harmless caches only, DO alarms replace the timer loops.
The external JSON-RPC surface is byte-identical to the docker shell's — both
render through `vela_relay_core::wire`, and the replay battery (spec 002
Gate 2) pins it.

**Workers Paid is required** (the Free plan's 10 ms CPU budget cannot host the
signing/hashing paths). The authoritative gates and as-run measurements live
in `specs/002-cf-worker-shell/quickstart.md`; the Operation→primitive mapping
and declared deltas live in
`specs/002-cf-worker-shell/contracts/platform-bindings.md`.

## Build & local dev

```sh
cd vela-relay-cf
cargo check -p vela-relay-cf --target wasm32-unknown-unknown --locked  # gate
npx wrangler dev        # local: workerd emulates DO + Queues + KV
```

`wrangler.jsonc` carries the bindings; `.dev.vars.example` documents local
vars (copy to `.dev.vars`). Behind an HTTP proxy, worker-build's own binary
downloads (wasm-bindgen, wasm-opt) may fail; point it at locally installed
binaries instead:

```sh
WASM_BINDGEN_BIN=$(which wasm-bindgen) WASM_OPT_BIN=$(which wasm-opt) npx wrangler dev
```

Local-dev gotchas that cost time once: `wrangler dev` must be launched from
`vela-relay-cf/`; local DO/queue/KV state persists in `.wrangler/state` and
must be wiped together with any local test chain (a prepared bundle intent
from a wiped chain correctly refuses to clear — it has no terminal proof);
behind a system proxy, curl to localhost needs `--noproxy '*'`.

## Deploy workflow

1. **Provision** (once per account):
   - a Workers Paid account;
   - the queues: `wrangler queues create vela-relay-ops` and
     `wrangler queues create vela-relay-dlq`.
   The KV namespace needs NO provisioning and NO config edit: the tracked
   `wrangler.jsonc` declares the `CACHE` binding without an id, and
   `wrangler deploy` auto-provisions a namespace on first deploy (and stays
   bound to it afterwards — shown as `env.CACHE (inherited)`). The config
   file deliberately contains nothing account-specific.
2. **Secrets** (never in config files; account-specific values all live
   here):

   ```sh
   wrangler secret put OPERATOR_SECRET      # required when the executor is enabled
   wrangler secret put ALCHEMY_API_KEY      # optional executor RPC tier
   wrangler secret put TELEGRAM_BOT_TOKEN   # optional; pairs with TELEGRAM_CHAT_ID
   wrangler secret put TELEGRAM_CHAT_ID     # optional; both or neither
   ```

   Deploying overwrites remote vars with the config file's `vars` block, so
   never park account-specific values as dashboard vars — the secret store
   survives every deploy.

3. **Vars** (in `wrangler.jsonc` or the dashboard): the executor policy
   values use the docker names, defaults, and bounds
   (`VELA_RELAY_EXECUTOR_*`, `VELA_RELAY_MAX_BUNDLE_OPERATIONS`,
   `VELA_RELAY_TELEGRAM_ALERT_COOLDOWN_SECS`); see
   `vela-relay-cf/src/config.rs` for the full list.
4. `wrangler deploy`. The Durable Object migrations (v1 RecordDo, v2 LaneDo,
   v3 TreasuryDo) ship with the config and apply on first deploy.
5. **Verify**: `GET /health` returns `{"runtime":"workerd",…}`; then run the
   spec 002 Gate 2 replay battery against the deployment.

## Chains and ownership

Chains are dynamic (no per-chain provisioning): admission resolves metadata
from the controlled chain directory, the queue is chain-agnostic, and Durable
Objects materialize on first use. `VELA_RELAY_EXECUTION_CHAINS` stays empty
for directory-driven execution.

**The shared-key rule (FR-010)**: the unit of execution ownership is
(chain, key set). Two deployments may never sign for the same chain with the
same `OPERATOR_SECRET`. When the docker and Cloudflare deployments share an
operator secret, `VELA_RELAY_EXECUTION_CHAINS` becomes mandatory on BOTH and
the two lists must be disjoint; with distinct secrets no restriction applies
(each has its own relayer pool and treasury). Nonce collision between two
signers of one pool is a fund-safety incident, not a performance issue.

**Lane width (R11)**: the pool is 100 relayer EOAs derived from
`OPERATOR_SECRET` (chain-agnostic addresses). `VELA_RELAY_RELAYER_COUNT`
selects the active routing width 1..=100 at provisioning time. Widening is
safe (new lanes get fresh relayers); NARROWING strands any in-flight state
parked on lanes above the new width — drain first (no queued/parked
operations and no prepared intents on the removed lanes).

Executor RPC resolution per chain: explicit `VELA_RELAY_EXECUTOR_RPC_URLS`
(JSON map, http/https) → Alchemy (when `ALCHEMY_API_KEY` is set) → the
chain directory (https, non-local). The directory is
`VELA_RELAY_CHAIN_DIRECTORY_URL` (a `vars` entry; default
`https://ethereum-data.getvela.app`), the same setting as the docker shell;
metadata is cached in KV for an hour. A chain with no resolvable executor
RPC defers its work with the frozen "chain has no trusted executor RPC"
diagnostic — admission still accepts and records.

## Ownership review checklist (Gate 6)

Before pointing production traffic at a second deployment:

- [ ] distinct `OPERATOR_SECRET` per deployment, or disjoint
      `VELA_RELAY_EXECUTION_CHAINS` on both (FR-010);
- [ ] the settlement recipient of each deployment matches its own derived
      vault (`VELA_RELAY_SETTLEMENT_RECIPIENT` unset, or equal to the
      derivation);
- [ ] queue names are deployment-private (two deployments must not consume
      one queue);
- [ ] Gate 2 replay battery byte-clean against the new deployment;
- [ ] alarms observed live (a submitted operation reaches `included`).

## Operational notes

- The wasm binary is ~2.4 MB (~790 KB gzipped) with the full shell linked
  (>12x headroom under the Paid-plan 10 MB gzip limit).
- Every DO boundary speaks JSON text (never JsValue serde): u128 fields
  degrade to floats through JS `JSON.parse`/structured clone. The queue
  envelope travels as its exact JSON text for the same reason.
- workerd's `fetch` has no native timeout — every outbound request in this
  shell races a `Delay` (metadata 10 s, Binance/RPC 2 s, executor RPC
  `VELA_RELAY_EXECUTOR_RPC_TIMEOUT_SECS`, Telegram 5 s).
- Alert suppression slots live in each chain's TreasuryDO (the docker Redis
  `SET NX PX` semantics), so alert dedup is strongly consistent per chain.
