# In-band fees: the settlement rule, end to end

Vela Relay charges no separate fee. Instead every UserOperation declares **zero
EntryPoint fees** (`maxFeePerGas = maxPriorityFeePerGas = 0`) and must embed, in
its own calldata, a trusted Safe MultiSend transfer that reimburses the relay's
settlement recipient for the gas the relay will spend. This is the *in-band*
reimbursement. This document is the authoritative statement of the rule the
relay enforces, and the guidance a client must follow to price a payment that
survives to inclusion.

All of the logic below lives in `vela-relay-core` (I/O-free, replayable) and is
byte-identical across the docker and Cloudflare deployments — `settlement.rs`
(evaluation, repricing, USD conversion), `cost.rs` (gas allocation), `gas_math.rs`
(quote tiers), `quote.rs` (the wallet-facing quote).

## 1. What the relay REQUIRES (the hard rule)

For each operation, evaluated independently (surplus on one op never subsidizes
another in the same bundle):

```
required = max( markup × gas_native_cost ,  floor )
```

- **`markup`** — default **14000 bps = 1.4×** (`VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS`,
  hard lower bound 1.0×). The relay recovers 1.4× the gas it spends.
- **`gas_native_cost`** = the operation's allocated gas × the **submit cap**,
  which by default is `max_fee_per_gas = 2 × base_fee + tip`
  (`gas_math::quoted_outer_fee`). The `2×` is **inclusion headroom, not cost** —
  the chain only ever charges `base_fee + tip`; the extra base-fee multiple lets
  the outer transaction survive a rising base fee without a re-sign. A client
  may raise or lower this multiple by naming a submission speed (§2a); when it
  names none, the cap is exactly the `2×` above.
- **`floor`** — a dust guard: `0.00001` native coin, or `0.01` of a stablecoin
  (≈ **1 cent**). This is NOT the price; it only bites when `1.4 × gas` rounds
  below it (near-zero-gas ops). Both the quote layer and the settlement layer
  compute it through the same `minimum_amount(decimals, fraction)` with the same
  constants, so they can never disagree.

Gas is split across a bundle by `cost::allocate_bundle_gas`: each op pays its own
simulated gas plus an even share of the outer overhead + buffer; the per-op
allocations sum to the bundle total exactly (no wei lost or double-charged), and
every op is guaranteed ≥ its own direct gas (no free-riding).

Every rounding step in the chain (`mul_div_ceil` markup, `native_to_usd_stable_ceil`
USD conversion, Binance price parse, Tempo cost) rounds **toward the relay**, and
every multiply/scale is `checked_` and fails closed on overflow. The relay can
never round in the payer's favor or wrap silently.

## 2. Repricing — the safety valve that makes a fixed client payment work

A client signs its payment at quote time; the base fee at *inclusion* time may be
higher. The relay does not simply reject a payment that falls short of the nominal
`required` — it first tries to **reprice the outer transaction down** to a fee the
payment CAN cover, because the `2×base` quote was headroom, not cost
(`settlement::decide_settlement`):

1. Evaluate at the quoted fee. If every op is fully paid → **KeepQuote** (submit
   as quoted).
2. If an op is short (but the payment parsed and went to the right recipient — a
   *shortfall*, not a malformed/misdirected payment), compute the **affordable
   fee** the weakest payer funds: `affordable = quoted_fee × (paid / required)`.
3. If `affordable` is at least the **inclusion floor**
   (`inclusion_floor_bps × base_fee + tip`, default **1.5×base + tip**) and below
   the quoted fee → **Reprice** to `affordable` and submit. Repricing preserves
   the full markup (reimbursement still covers `markup × gas × new_fee`, and the
   chain can never charge more than `new_fee`).
4. If `affordable` is below the inclusion floor → **FloorUnfundable**: reject.
   This is a clean rejection, never a loss — the relay never signs an outer
   transaction it would lose money on.

Because the floor uses `max(cost, dust_floor)` on the stablecoin path, a payment
below the *dust* floor can never be repriced into acceptance (the requirement is
pinned at the floor at every fee) — a case now pinned by
`a_stablecoin_below_the_floor_cannot_be_repriced_into_acceptance`.

## 2a. Submission speed — the client may name a tier

Repricing (§2) only ever moves the cap **down**. A client that wants its
operation in *sooner* may name a submission speed on `eth_sendUserOperation`, as
an **optional third parameter**:

```jsonc
["eth_sendUserOperation", [ <userOperation>, <entryPoint>, "fast" ]]
```

The value is a **tier NAME** (`"slow" | "standard" | "fast"`), never a wei
amount. The relay resolves the name against the base fee it reads at submit
time, so a quote that went stale between signing and inclusion can never set the
price. An unknown name is refused with `-32602 invalid params` before any
handler runs; omitting the parameter is today's wire and today's behaviour,
byte for byte.

```
absent  ⇒ cap = 2 × base_fee + tip                    (exactly §1, unchanged)
present ⇒ cap = max( min( multiplier[tier] × base_fee + tip ,
                          what the reimbursements fund ) ,
                     inclusion_floor )
```

| tier | multiplier on `base_fee` | note |
|---|---|---|
| `slow` | **1.5×** | equals the default inclusion floor exactly |
| `standard` | **2.0×** | identical to the cap with no tier named |
| `fast` | **3.0×** | the default the vela-wallet client sends |

**The tier multiplies the base fee; it is NOT the reported tier price.** The
prices returned by `pimlico_getUserOperationGasPrice` (`slow`/`standard`/`fast`
= 1.0/1.1/1.2 × a ~`1.2 × base` network price) tell the **client what to pay**
(§3). The submit cap tells the **chain how badly the relay wants the block**.
Measured on Ethereum 2026-09-20 with `base = 0.0535` gwei, the reported `fast`
price is **0.0938 gwei while the submit cap is already 0.1070 gwei** — so
submitting at the reported `fast` price would make a fast send *slower* than a
default one, on every EIP-1559 chain. BSC hides this (its `baseFeePerGas` is 0
and the whole price is the tip, so the two coincide); never validate a speed
change on BSC alone.

A higher cap is **not** a higher cost in a calm market: the chain charges
`base_fee + tip` whatever the cap says. It binds only during a spike — which is
exactly when the client wanted the headroom.

**The two clamps, and which wins.**

- *Upper — what the reimbursement funds.* The signed payment must still cover
  `markup × gas × cap` (§1). The cap is therefore held to the **weakest payer in
  the bundle**, so a `fast` neighbour can never price a slower operation out of
  its own transaction, and the relay never signs a cap it would subsidise.
- *Lower — the inclusion floor, and it wins over the upper clamp.* A cap the
  chain will not mine is a rejection dressed as a saving, and the executor has
  no fee-bump path to rescue it. So a request below the floor is **raised** to
  the floor; if the reimbursement cannot fund even that, the operation reaches
  §2's existing `FloorUnfundable` verdict and takes the ordinary shortfall
  path — held in the delayed inbox while the market may still come back, then
  rejected once the hold budget is exhausted. Never signed at a loss.

**Headroom — why no tier needs a subsidy.** The client pays `3 × gas × max(C,R)`
(§3) and the relay requires `1.4 × gas × cap`, so the highest cap a payment
funds is `3 / 1.4 = 2.14 × R`. Against the measured Ethereum market above that
is **3.13 / 3.44 / 3.76 × base_fee** for a client pricing off the `slow` /
`standard` / `fast` quote tier — all comfortably above the 1.5 / 2.0 / 3.0 ×
caps, so in normal conditions no tier is clamped or repriced. Pinned by
`every_tier_fits_inside_the_headroom_a_normal_client_payment_leaves`.

**Where it lives.** `gas_math::{SubmissionTier, tier_outer_fee}` (the names and
the multiplier), `settlement::decide_submission_cap` (the clamps),
`execution::bundle_submission_tier` (one transaction, one price: a bundle takes
the fastest speed any member named, and a member that named none counts as
`standard`). The tier rides the **queue envelope**, not the UserOperation: it is
not part of what the user signed, so it never enters the `userOpHash`, the
admission fingerprint or the durable record. Tempo (§5) prices gas in pathUSD
with no base fee to multiply and ignores the tier.

## 3. What a CLIENT should pay (and why it must exceed the relay minimum)

A client that pays *exactly* the relay's instantaneous requirement is doomed: the
base fee at inclusion is almost always higher than at quote time, so the signed
payment falls short and the op is rejected. **A client must over-pay at quote
time to absorb the quote→inclusion gas drift.** But it must also protect itself —
a client that blindly paid whatever the relay quoted could be over-charged by a
malicious or buggy quote. The vela-wallet client resolves both by keeping the
relay quote in its proper place: a *floor reference and an audited input*, never
the unquestioned anchor. All of this lives twice, held identical by
`fee-policy-parity.test.ts`: `fee_policy.rs` (Rust/WASM, web) and
`safe-transaction.ts` (TS twin, native).

**Two gas numbers, distinct roles:**

- **`C` — the client's own chain measurement.** `deriveChainGasPrice =
  max(eth_gasPrice, base_fee + tip)`. Objective, independent of the relay.
- **`R` — the relay's quoted `networkFeePerGas`** (the relay's quote applies
  `base_fee_multiplier = 120` → ~`1.2×base`).

The pricing pipeline, in order:

```
① reject:   if R > 3 × C   →  GasQuoteTooHigh (never signed)   (MAX_QUOTE_VS_CHAIN_MULTIPLE = 3)
② anchor:   basis = max(C, R)
③ pay:      payment = max( 3 × gas × basis ,  floor )          (INBAND_MARKUP = 3)
```

**① Reject an outrageous quote.** Before anything is signed, `R > 3 × C` is
refused rather than paid. The denominator is the client's *own* measurement `C`,
so the check cannot be fooled by the very quote it is auditing.

**② Anchor on `max(C, R)`, not on `R` alone.** This never lets the 3× drift buffer
ride on an unvetted quote, and it stops a relay *under-report* (`R < C`) from
making the client underpay — the client already measured the true cost `C`. It
still ignores the relay's `requiredAmount` field entirely and self-computes.

**③ Over-pay 3× for drift.** The flat `3×` on `max(C, R)` (vs the relay's 1.4×
requirement) is the quote→inclusion buffer. The signed amount is what the confirm
screen displayed — it is **not** re-priced just before submit (a 30 s quote TTL is
advisory, not enforced), so the whole buffer must live in that 3×. Because
`max(C, R) = R` in the normal case (`R ≈ 1.2×base ≥ C`), the drift math below is
unchanged from anchoring on `R`; when the relay under-reports, the client simply
pays more.

**Floors (client self-imposed).** The client harmonizes the native minimum with
the stablecoin one — both **$0.01 of value**:

- stablecoin payment: at least `$0.01` (unchanged);
- native payment: at least **`$0.01` worth of native** when the coin is priced,
  but never below the relay's `0.00001`-coin admission floor (on a coin dearer
  than ~$1000, `$0.01` buys *less* than 0.00001 of it, and §1's floor must still
  be met);
- native payment with **no USD price**: a flat **`0.001`-coin** blind fallback
  (nothing to value it against).

These are the *client's* minimums; the relay still admits any op meeting its own
`0.00001`-native / `$0.01`-stable floor (§1), so the client paying more only ever
helps. All floors bind only on near-zero-gas ops — the normal 3× gas payment sits
far above them.

### Headroom, worked out

Two bases differ: the client prices against ~`1.2×base` (quote time), the relay
settles against `2×base'` (inclusion time). Netting the client's 3× against the
relay's 1.4×, with the default 1.5×base inclusion floor and tip ≈ 0:

| Base-fee rise, quote → inclusion | Outcome |
|---|---|
| up to **~1.29×** (+29%) | fully paid at the quoted fee → **KeepQuote**, submitted |
| **~1.29× to ~1.71×** (+29% … +71%) | short of nominal, but **repriced** down to a fundable fee → submitted |
| above **~1.71×** (+71%) | `affordable` drops below the inclusion floor → **FloorUnfundable**, cleanly rejected (retry with a fresh quote) |

So the effective tolerance is roughly a **+70% base-fee spike** between signing
and inclusion — comfortable for normal conditions, and any larger spike fails
safe (a rejected send, never an under-charge or a loss to the relay).

The exact numbers move with the tip, the client's gas padding (it pads limits
×1.5), and which quote tier the client prices against; the shape (direct-accept
band → reprice band → clean-reject) is fixed by the rule.

### Integration caveats worth knowing

- **The two fee bases are not identical.** The client prices against the relay's
  quoted network fee (~1.2×base); the relay settles against `2×base'`. The 3×
  client markup and the repricing valve cover the gap, but a client that lowered
  its markup toward the relay's 1.4× would lose almost all drift tolerance.
- **The client's floors now sit at or above the relay's.** The client's native
  minimum is `$0.01` of value (or a `0.001`-coin fallback when the coin is
  unpriced), while the relay admits down to `0.00001` native — so at the dust
  floor the client *over-*pays rather than pinning exactly at the relay minimum.
  The old zero-headroom-at-the-floor shortfall (when the floors were equal) is
  gone; a near-zero-gas op is cushioned there too.
- **A quote far above the chain rate is rejected, not paid.** If `R > 3 × C` the
  client fails closed (`GasQuoteTooHigh`) and the user retries with a fresh quote.
  A genuine, sudden >3× mempool spike between the client's measurement and the
  relay's quote is therefore a (rare) rejected send, never an over-charge.
- **No client-side re-price before submit** (confirm-UI flow): the drift budget
  is entirely the 3× buffer. A user sitting on the confirm screen past the 30 s
  TTL spends that budget on think-time.

## 4. Stablecoin payments

A client may reimburse in an allowlisted stablecoin instead of the native coin.
The relay converts its native `required` into stablecoin units via the asset's
USD price (`native_to_usd_stable_ceil`, 8-dp fixed-point price, ceil rounding),
floors it at `$0.01`, and — critically — **verifies the on-chain Transfer event**
from the final bundle simulation actually paid the settlement recipient: the log
must come from the allowlisted token, carry the `Transfer(address,address,uint256)`
signature, and show `sender → recipient` for the claimed amount. A transfer to a
third party, from the wrong token, or with the wrong shape credits nothing
(pinned by `verify_stable_transfer_logs_enforces_every_field_of_the_transfer_event`).

Native transfers have no standard log and are instead covered by the successful
final bundle simulation.

## 5. Tempo (pathUSD gas)

Tempo chains have no native gas coin; the relay prices gas directly in pathUSD
(attodollar-denominated), applies the same 1.4× in-band markup with the same
`$0.01` floor (`marked_tempo_cost`), and signs the outer transaction with Tempo's
`0x76` envelope paying fees in pathUSD. The client mirrors this with a separate
Tempo model (2× margin plus an explicit gas/split cushion annotated "must match
vela-relay", added after a real sub-floor deploy rejection). A submission tier
(§2a) has nothing to act on here — there is no base fee to multiply — so Tempo
ignores it.

## 6. Summary

- The relay requires `max(1.4 × gas × (2×base+tip), floor)`, recovers 1.4× its
  gas, and rounds every step in its own favor with fail-closed overflow.
- Repricing turns the `2×base` headroom into a live safety valve: a short-but-
  honest payment is repriced down to a fundable fee rather than rejected, down to
  the 1.5×base inclusion floor.
- A client may name a submission speed (`slow`/`standard`/`fast`) as an optional
  third `eth_sendUserOperation` parameter. The name selects a **base-fee
  multiplier for the submit cap** (1.5/2.0/3.0×) — not the reported quote-tier
  price, which sits *below* today's cap and would invert the feature. Naming
  nothing is byte-for-byte today's behaviour; the cap is clamped down to what
  the bundle's weakest reimbursement funds and up to the inclusion floor.
- A client must pay above the relay minimum to survive gas drift; vela-wallet
  pays a flat 3× on `max(C, R)` — its own chain measurement `C`, floored by the
  relay quote `R` — giving roughly a +70% base-fee-spike tolerance before a clean,
  loss-free rejection.
- The client protects itself both ways: it rejects a quote `R > 3 × C`
  (`GasQuoteTooHigh`) instead of paying it, and anchoring on `max(C, R)` stops a
  relay under-report from making it underpay.
- The client's own minimums are value-consistent — `$0.01` worth of native
  (`0.001`-coin when unpriced) and `$0.01` stable — sitting at or above the
  relay's `0.00001`-native / `$0.01`-stable admission floor, so paying them only
  ever helps.
- Stablecoin reimbursements are verified against the real on-chain Transfer
  event; a misdirected or wrong-token transfer is never credited.
