# Contract: Deployment Parity (FROZEN)

The external contract of the new deployment IS
[`001/contracts/external-api.md`](../../001-crux-core-split/contracts/external-api.md)
— the eight JSON-RPC methods at `POST /{chain_id}`, the operational GET
endpoints, the status vocabulary, the stored record JSON shape, and every
reason string. This file adds only the parity obligations and the declared
platform deltas.

## Parity obligations

1. **Deterministic battery**: the replay harness
   (`001/replay-harness/`) run against the CF deployment and the docker
   deployment MUST produce byte-identical response bodies for every
   deterministic surface (SC-001). The battery is the executable form of this
   contract and gates every merge that touches the HTTP surface.
2. **Single rendering**: both shells MUST obtain envelope parsing and response
   rendering from `vela_relay_core::wire` (additive module, pinned by native
   tests). A shell-local divergence in bytes is a build defect, not a tuning
   knob.
3. **Stored shape**: `StoredUserOperation` JSON in RecordDO storage is
   byte-compatible with the Redis value (same serde types — structural).

## Declared platform deltas (not behavior changes)

- **Transport headers**: HTTP/2 at the edge lowercases header names; the
  `x-vela-rpc-domain` header value and `content-type` are preserved. Bodies
  are the parity surface.
- **`/` and `/version`**: both carry `version` (release tag) and `commit`
  (short SHA), produced by the `build_info.rs` both shells' build scripts
  share. The VALUES are environment-specific — two deployments built from the
  same commit agree, a CI tag build and a local one need not — so the fields
  are shape-parity, not byte-parity. (Before the CF shell had a build script
  at all, its `/version` reported a hardcoded `"build": "dev"` on every
  deployment, live ones included.)
- **`/readyz` semantics**: the docker deployment's four job names gate
  readiness in-process. The CF deployment has no resident process; `/readyz`
  reports binding availability (queue, DO namespaces, KV) — shape-compatible,
  semantics documented, and NOT part of the byte-parity battery.
- **Ack granularity**: per-message queue acknowledgment replaces the
  contiguous-durable-prefix offset (see data-model §2) — durability still
  gates acknowledgment; only redundant redelivery of already-durable items
  disappears.
