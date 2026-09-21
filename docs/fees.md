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
  the chain only ever charges `base_fee + effective tip`; the extra base-fee
  multiple lets the outer transaction survive a rising base fee without a
  re-sign. A client may raise or lower this multiple, **and the tip the relay
  signs with**, by naming a submission speed (§2a); when it names none, the cap
  is exactly the `2×` above and the tip is the raw market reading.
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
   (`inclusion_floor_bps × base_fee + the tip the transaction is signed with`,
   default **1.5×base + tip**) and below the quoted fee → **Reprice** to
   `affordable` and submit. Repricing preserves the full markup (reimbursement
   still covers `markup × gas × new_fee`, and the chain can never charge more
   than `new_fee`). Because the floor carries the **signed** tip — the tier's,
   once one has resolved (§2a) — a reprice can never drop the cap below
   `base_fee + tip` and shave the priority the client paid for.
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
amount. The relay resolves the name against the base fee *and the tip* it reads
at submit time, so a quote that went stale between signing and inclusion can
never set the price. An unknown name is refused with `-32602 invalid params`
before any handler runs; omitting the parameter is today's wire and today's
behaviour, byte for byte.

### A tier is TWO levers

| tier | `base_fee_bps` — the cap | `tip_bps` — the priority | what each buys |
|---|---|---|---|
| `slow` | **1.5×** base_fee | **1.00×** market tip | equals the default inclusion floor exactly |
| `standard` | **2.0×** base_fee | **1.25×** market tip | the relay's own cap multiple, with a tip premium |
| `fast` | **3.0×** base_fee | **2.00×** market tip | the default the vela-wallet client sends |

```
tip[tier] = tip_bps[tier] × market_tip / 10_000                      (rounded up)

absent  ⇒ cap = 2 × base_fee + market_tip,  signed tip = market_tip
                                                       (exactly §1, unchanged)
present ⇒ cap = max( min( base_fee_bps[tier] × base_fee + tip[tier] ,
                          what the reimbursements fund ) ,
                     inclusion_floor(base_fee, tip[tier]) ,
                     base_fee + tip[tier] )
          signed tip = tip[tier]                               (never clamped)
```

**The cap buys spike resilience. Only the tip buys priority.** They are
different goods and a tier has to move both, because an EIP-1559 block builder
orders transactions by the **effective tip**:

```
effective_tip = min( maxPriorityFeePerGas , maxFeePerGas − baseFee )
```

Any cap above `base_fee + tip` leaves that number completely unchanged. Until
this was fixed the relay scaled only the cap and signed every tier with the raw
market tip, so **paying for `fast` bought no priority at all.** It is not a
suspicion; a mined Polygon `fast` receipt shows it:

```
Base: 250.710118904 Gwei | Max: 775.52505875 Gwei | Max Priority: 30.35 Gwei
Gas Price (effective): 281.060118904 Gwei   (= base + 30.35)
```

`775.52505875 = 3 × 248.39168625 + 30.35`, so the 3× **cap** was applied exactly
right — that machinery always worked. But `maxPriorityFeePerGas` was the bare
market tip, and `min(30.35, 775.525 − 250.710) = 30.35` gwei is precisely
what a `slow` send would have offered the builder. The user paid `fast` and got
`slow`'s place in the block. Under the two-lever tier the same market signs:

| tier | signed tip | cap | effective tip a builder sees | effective gas price |
|---|---|---|---|---|
| `slow` | 30.350 gwei | 406.415 gwei | **30.350 gwei** | 281.060118904 gwei |
| `standard` | 37.9375 gwei | 539.358 gwei | **37.9375 gwei** | 288.647618904 gwei |
| `fast` | 60.700 gwei | 812.830 gwei | **60.700 gwei** | 311.410118904 gwei |

The receipt's own `281.060118904` gwei is, to the wei, what `slow` now pays.
Pinned by `a_fast_send_now_outbids_the_slow_one_on_the_receipt_that_proved_the_defect`.

**`slow`'s tip is 1.00× — the market tip, and a floor that is never scaled
below.** This is deliberate and load-bearing. The relay has **no per-chain
minimum-tip knowledge**: it takes whatever the node's `eth_maxPriorityFeePerGas`
reports (`gas_math::market_tip`, §2b), and that answer carries the minimum a
chain enforces inside its client — bor on Polygon is the canonical one. A
transaction tipping under that minimum is **rejected outright**, not merely
mined late. A `slow` that shaved the tip would therefore
buy a high rejection rate rather than a saving. `slow` earns its discount from
the lower **cap** (and hence the lower reimbursement basis derived from it),
never from underpaying the builder. Do not "simplify" this to a tip below
10_000 bps without first giving the relay a per-chain minimum-tip source.

**Naming `standard` is no longer byte-identical to naming nothing.** It shares
the `2 × base_fee` cap multiple, but signs `1.25 ×` the market tip. **Naming
nothing still is** byte-identical, and that is the guarantee that matters for
existing clients: `settlement::decide_submission_fees` returns `None` and not
one line of tier arithmetic runs.

**A tier IS this pair, and every price reported for it derives from it.** The
two basis-point tables above are the single definition of a tier, and they live
on one enum (`gas_math::SubmissionTier::{base_fee_bps, tip_bps}`) precisely so
they cannot be edited apart. The same enum produces the row
`pimlico_getUserOperationGasPrice` returns for that speed (§2b), so the tip a
wallet is shown is the tip the relay signs with.

A higher cap is **not** a higher cost in a calm market: the chain charges
`base_fee + effective_tip` whatever the cap says. A higher tip **is** a higher
cost — that is what speed costs — and it is also carried whole into the
reimbursement basis, so `fast` funds the priority it buys.

**The clamps, and which wins.**

- *Upper — what the reimbursement funds.* The signed payment must still cover
  `markup × gas × cap` (§1). The **cap** is therefore held to the **weakest
  payer in the bundle**, so a `fast` neighbour can never price a slower
  operation out of its own transaction, and the relay never signs a cap it
  would subsidise.
- *The tip is never clamped.* Shaving it would hand back exactly the speed the
  client is being charged for, and on a chain with an enforced minimum it would
  turn a slow send into a rejected one. The cap absorbs the shortfall instead.
- *Lower — the inclusion floor, and it wins over the upper clamp.* A cap the
  chain will not mine is a rejection dressed as a saving, and the executor has
  no fee-bump path to rescue it. So a request below the floor is **raised** to
  the floor; if the reimbursement cannot fund even that, the operation reaches
  §2's existing `FloorUnfundable` verdict and takes the ordinary shortfall
  path — held in the delayed inbox while the market may still come back, then
  rejected once the hold budget is exhausted. Never signed at a loss.
- *Lowest — `base_fee + tip[tier]`, the invariant.* The floor is computed from
  the **tier's** tip, so with `inclusion_floor_bps ≥ 10_000` it already implies
  this; it is stated separately anyway, and asserted on the signed pair just
  before `SignBundle`. A cap between `base_fee` and `base_fee + tip` is the
  subtlest form of the original bug: includable, but silently truncating the
  very tip that was paid for. `OuterFee::delivers_full_tip_at` is the
  predicate, and the executor fails the batch rather than sign a pair that
  breaks it.

**Headroom — why no tier needs a subsidy.** The client pays `3 × gas × max(C,R)`
(§3) and the relay requires `1.4 × gas × cap`, so the highest cap a payment
funds is `3 / 1.4 = 2.14 × R`. Because `R` is `0.6 ×` the cap's **base-fee
term** plus the tier's **whole tip** (§2b), the funding property holds *by
construction* rather than by a table that happens to work:

```
3R − 1.4 × cap = 3(0.6·m·base + tip[tier]) − 1.4(m·base + tip[tier])
               = 0.4·m·base  +  1.6·tip[tier]        ≥ 0,  term by term
```

so every tier funds its own cap with at least `3 × 0.6 / 1.4 = 9/7 = 1.286`
to spare — the same +29% floor at `slow`, `standard` and `fast` alike, and the
full `3/1.4 = 2.14` on a chain with no base fee. Measured on Ethereum
2026-09-20 (`base = 0.0535` gwei) the payment funds **2.70 / 3.27 / 4.98 ×
base_fee** against caps of 1.5 / 2.0 / 3.0 × (it was 2.49 / 3.13 / 4.42 when
the tip was unscaled: a bigger tip rides whole into `R`, so the payment grows
with the cap rather than against it). Pinned by
`every_tier_fits_inside_the_headroom_a_normal_client_payment_leaves` and
`every_tier_funds_its_own_cap_with_at_least_the_twenty_nine_percent_band`.

**One clamp did widen.** A client that prices the tier it names is unaffected —
that is the property above. A client that anchors on its **own** chain
measurement `C = base + tip` and then names `fast` (an old wallet that ignores
`networkFeePerGas`, or one that priced `slow`) funds `3C/1.4 = 2.14 × (base +
tip)` against a cap that grew from `3·base + tip` to `3·base + 2·tip`. That cap
is now clamped whenever `tip < 6 × base`, where before it was clamped only
below `tip < 0.75 × base`. Clamping is the safe direction — the relay buys what
the payment funds and stops there, never a loss — but a mis-priced `fast` now
degrades toward `standard` in far more markets, which is exactly the incentive
to price the tier you name. Pinned by
`scaling_the_tip_widens_the_clamp_for_a_client_that_did_not_price_the_tier`.

**Where it lives.** `gas_math::{SubmissionTier, tier_tip, tier_outer_fee}` (the
names and both multipliers; `tier_outer_fee` returns an `OuterFee` — the cap
*and* the tip — so no caller can assign one and forget the other),
`settlement::decide_submission_fees` (the clamps),
`execution::bundle_submission_tier` (one transaction, one price: a bundle takes
the fastest speed any member named, and a member that named none counts as
`standard`). The tier rides the **queue envelope**, not the UserOperation: it is
not part of what the user signed, so it never enters the `userOpHash`, the
admission fingerprint or the durable record. Tempo (§5) prices gas in pathUSD
with no base fee to multiply and ignores the tier.

## 2b. The quote — the four numbers reported per tier

`pimlico_getUserOperationGasPrice` returns one row per tier, and each number in
it has exactly one meaning. With `m` the tier's base-fee multiplier and
`tip[tier] = tip_bps × market_tip` its scaled tip, both from §2a:

| field | meaning | value |
|---|---|---|
| `maxFeePerGas` | the cap the relay **will actually submit at** | `m × base_fee + tip[tier]` |
| `maxPriorityFeePerGas` | the tip the relay **will actually sign with** | `tip[tier]` |
| `networkFeePerGas` (`R`) | what the client must **reimburse against** (§3) | `0.6 × m × base_fee + tip[tier]` |
| `relayerFeePerGas` | `maxFeePerGas − networkFeePerGas`: the inclusion headroom the cap holds above the reimbursement basis | `0.4 × m × base_fee` |

So `R` is **0.9 / 1.2 / 1.8 × base_fee** plus **1.00 / 1.25 / 2.00 × the market
tip** for `slow` / `standard` / `fast`. The tip terms cancel in
`relayerFeePerGas`, which is why the headroom is purely the base-fee part.

**The reported `maxPriorityFeePerGas` is per tier, and it is the number the
relay signs with.** Both come from the same `tier_outer_fee`, applied to the
same market tip, so a wallet can never be shown one tip and charged for
another. That coherence is the quote-side half of the defect in §2a: a
reported tip that did not match the signed one would mislead the tier picker
exactly as a signed tip that did not match the tier misled the builder.

**The market tip is one reading, resolved one way.** `gas_math::market_tip` is
called by the quote (`pimlico_getUserOperationGasPrice`, docker and Worker)
and by the executor (`transaction_context`, docker and Worker, and the
simulation-contract deployer):

1. the node's `eth_maxPriorityFeePerGas`, whatever it returns — **zero
   included**;
2. only when that call gave no quantity: `eth_gasPrice −` the **latest**
   block's base fee.

`eth_maxPriorityFeePerGas` is the node's own answer to what clears, so it
carries a chain's enforced minimum, which is what makes `slow`'s unscaled
`1.00×` safe (§2a). Step 2 subtracts the latest block's base fee because
`eth_gasPrice` is built as `suggested tip + head base fee`: on Polygon on
2026-09-21, `278.534895357 − 250.761673410 = 27.773221947` gwei,
`eth_maxPriorityFeePerGas` to the wei. The quote takes that base fee from the
fee history it already holds (the second-to-last `baseFeePerGas`); subtracting
the last one, the next block's projection, would have read 30.729 gwei. A zero
is an answer, not an absence: Arbitrum reports `0x0`, and the executor has
always signed it.

**2026-09-21 — the quote's tip was not the signed tip.** Until then the quote
took the median of `eth_feeHistory`'s 50th-percentile reward column, fell back
to `eth_maxPriorityFeePerGas` only when that median was zero, and to
`base_fee / 200` after that; the executor always signed
`eth_maxPriorityFeePerGas` (else `eth_gasPrice − base`). They are different
statistics — what recent
blocks' median transaction tipped, against what the node says clears — and on
Polygon they were three times apart. A live `standard` send:

```
quoted:  maxFeePerGas 600.7 gwei | maxPriorityFeePerGas 107.7 gwei    (slow 86.2, fast 172.4)
signed:  Max 536.5544999 Gwei    | Max Priority 34.71688084 Gwei       (= 1.25 × 27.773504672)
         Gas Price (effective) 282.5 gwei
```

The wallet showed a `standard` tip of 107.7 gwei and priced `R` on it — `R`
carries the tip whole — while the relay paid the builder 34.72. The three
tiers the picker compared were not the tips any send would buy. One batch per
chain from a public endpoint (drpc) the same day shows how far the two
statistics wander:

| chain | old quote tip (reward median, else its fallbacks) | `eth_maxPriorityFeePerGas` — signed then, quoted now |
|---|---|---|
| Polygon | 86.072 gwei | 27.773 gwei |
| Ethereum | 0.2 gwei | 27,224 wei |
| Optimism | 100,000 wei | 1,000,000 wei (the call timed out; `eth_gasPrice − base`) |
| Base | 1,000,000 wei | 1,000,000 wei |
| Arbitrum | 100,380 wei (median and node both 0, so `base / 200`) | 0 |

The fix is the shared function: both paths call `market_tip`, so a reported
tier tip and a signed one come from the same reading by construction. Pinned
by `the_tip_reported_for_a_tier_is_the_tip_the_executor_signs_given_the_same_rpc_answers`
(every tier, over the markets above plus the no-answer and rising-base-fee
shapes) and `the_polygon_quote_reports_the_tip_the_executor_signed_not_the_fee_history_median`
(the verbatim Polygon batch, and the receipt's 34.71688084 gwei to the wei).

**Where the quote and the executor still differ, and why.**

- *The base fee.* The quote prices every tier on the **next** block's base fee
  (the last `baseFeePerGas`); the executor on the **latest** block's, read
  again at submit time. The tip does not depend on it; the cap does, by at
  most one block's 12.5% move, and a quote is always older than its
  submission anyway.
- *No tip at all.* When `eth_maxPriorityFeePerGas` gives no quantity and
  `eth_gasPrice` is unusable (absent, or below the base fee), the executor
  refuses to build the transaction and the operation waits for a later pass.
  Only the quote falls back, to `base_fee / 200` (`gas_math::quote_market_tip`),
  rather than going dark — a price for a market the relay will not sign in at
  that moment.
- *No fee history.* A chain whose `eth_feeHistory` fails is quoted from
  `eth_gasPrice` alone as an all-tip market (base 0); the executor needs an
  EIP-1559 base fee in the latest block and refuses without one. Unchanged.
- *Transport.* The quote reads through the request's failover chain (the
  caller's `x-vela-rpc-url`, then Alchemy, then the public list); the executor
  through its own RPC, whose quantity parser also refuses a non-canonical
  answer (`0x01`) the quote's accepts. The same rule over different nodes can
  still read different numbers; the rule no longer adds a difference of its
  own.

**Where the 0.6 comes from, and what it applies to.** Before per-tier pricing
the relay reported one network price, `1.2 × base`, and submitted at one cap,
`2 × base`. Neither number mattered alone — their **ratio** did, because the
client pays `INBAND_MARKUP = 3 × gas × R` while the relay requires
`1.4 × gas × cap` (§1):

```
1.2 / 2.0 = 0.6      →      3 × (0.6 × cap)     1.8
                            ───────────────  =  ───  =  1.286     (+29%)
                               1.4 × cap        1.4
```

**The 0.6 applies to the base-fee term only; the tier's tip rides into `R`
whole**, exactly as it rides into the cap whole. Sharing the same `tip[tier]`
on both sides is what makes the funding property algebraic rather than
tabulated:

```
3R − 1.4 × cap  =  0.4 × m × base_fee  +  1.6 × tip[tier]   ≥ 0
```

`fast` funds a `3 × base` cap and a `2 × tip` priority with the same +29% floor
that `standard` funds its own with. 1.286 is the **floor** of that margin, not
its value: on a chain with no base fee `R = cap` and the margin is the full
`3 / 1.4 = 2.14`.

**Three consequences worth stating outright.**

- **`standard` keeps the old single price's base-fee term, not its tip term.**
  `0.6 × 2.0 = 1.2`, and the division rounds up exactly as the old
  `base_fee_multiplier = 120` did, so `standard.networkFeePerGas`'s base-fee
  part is the same wei for every base fee. Its tip part is now `1.25 ×` the
  market tip, because a tier that did not move the tip bought no speed. Pinned
  by `the_standard_base_fee_term_is_byte_for_byte_the_price_the_relay_used_to_report`.
  What remains byte-for-byte unchanged end to end is **naming no tier at all**.
- **A zero-base-fee chain finally differentiates.** On BSC `baseFeePerGas` is 0
  and the whole price is the tip, so cap, `R` and the tip all coincide — and
  under a cap-only tier all three rows were one number and one speed. Scaling
  the tip is the *only* thing that can tell them apart there, and now does:
  `1.00 / 1.25 / 2.00 ×` the tip, in every field. This was the second half of
  the same defect, and BSC is the chain that HIDES it. **Never validate a
  pricing or speed change on BSC alone.** Pinned by
  `a_zero_base_fee_chain_finally_differentiates_its_tiers` and
  `a_chain_with_no_base_fee_differentiates_its_tiers_through_the_tip`.
- **The inclusion floor is computed from the tier's tip.** It is a multiple of
  the base fee *plus the tip*, so with `base_fee = 0` it collapses to exactly
  `tip[tier]` — which is also the cap. It binds precisely and does nothing
  silly: it neither vanishes to zero (which would let the relay sign a tipless
  transaction no BSC validator would mine) nor overshoots the cap it lifts.

**The client's `GasQuoteTooHigh` guard still cannot trip on this.** vela-core
refuses a quote with `R > 3 × C` (`MAX_QUOTE_VS_CHAIN_MULTIPLE`), where `C` is
its own measurement, `max(eth_gasPrice, base_fee + tip) ≥ base_fee + tip`. The
largest `R` is `fast`'s, and its tip is now doubled:

```
R[fast] = ceil(1.8 × base) + 2 × tip  ≤  1.8 × base + 1 + 2 × tip
                                      ≤  3 × base + 3 × tip  =  3 × C
```

for any `base + tip ≥ 1`, since the slack `1.2 × base + tip − 1` is then ≥ 0.
`R[fast] / C` is a weighted average of the two pure cases — **1.8** when the
tip vanishes and **2.0** when the base fee does — so its supremum rose from 1.8
to **2.0**, still a third below the limit of 3. On the Polygon receipt market
above (base 250.710, tip 30.35 gwei) the ratio is **1.822**; on BSC it is
exactly 2.0. The guard only bites if the relay reads a market more than `1.5 ×`
the one the client read moments earlier — a spike between two reads, not a
property of this pricing. Pinned by
`the_fast_basis_stays_far_inside_the_clients_three_times_chain_refusal`, over a
market set that deliberately includes both degenerate shapes (`base = 0` and
`tip = 0`).

**Where it lives.** `gas_math::{market_tip, quote_market_tip}` (the tip),
`gas_math::{tier_price, tier_tip, tier_network_fee,
REIMBURSEMENT_BASIS_BPS}` and `gas_math::tiers`, reported through
`wire::GasPriceTier` by both shells (`src/app/rpc/handlers/user_operation_gas_price.rs`
and `vela-relay-cf/src/http.rs`, which share one conversion shape). A generic
ERC-4337 bundler omits `networkFeePerGas`/`relayerFeePerGas`; this relay never
does.

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
- **`R` — the relay's quoted `networkFeePerGas` for the tier the client
  named** (§2b): `0.9 / 1.2 / 1.8 × base_fee` plus `1.00 / 1.25 / 2.00 ×` the
  market tip. It is **per tier in both terms**: a client that asks for `fast`
  is quoted, and must pay against, a larger `R` than one that asks for `slow`,
  and that is precisely how a tier comes to cost something — including on a
  chain with no base fee, where the tip term is the whole of it. A quote with
  `networkFeePerGas` absent makes `accept_bundler_quote` fall back to
  `chain_gas_price`, which collapses every tier onto `C` and leaves the tier
  picker inert — the defect the field exists to close.

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
advisory, not enforced), so the whole buffer must live in that 3×. `max(C, R) = R`
in the normal case for `standard` and `fast` (`R = 1.2 × base + 1.25 × tip` and
`1.8 × base + 2 × tip`, both `≥ C`), so the drift math below is unchanged from
anchoring on `R`; when the relay under-reports, the client simply pays more.
**`slow` is the one tier where the anchor routinely falls back to `C`** — its
`R = 0.9 × base + tip` sits just *below* `base + tip`, and its tip is
unscaled — so a `slow` send costs the client its own chain measurement, not
less. That is the floor the anchor exists to provide: `slow` buys a lower
submit cap, never an under-payment, and never a tip below the market's.

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

Two bases differ: the client prices against `R` at quote time, the relay settles
against `m × base' + tip[tier]` at inclusion time. Netting the client's 3×
against the relay's 1.4×, with the default 1.5×base inclusion floor and
tip ≈ 0, the tolerance is **per tier** — because a bigger cap bought at quote
time is a bigger cushion to reprice down through:

| tier | anchor | fully paid up to | repriced up to | above that |
|---|---|---|---|---|
| `slow` | `C` (its `R = 0.9×base + tip` sits below `C`) | **~1.43×** (+43%) | — (its cap already **is** the inclusion floor, so there is nothing to reprice down to) | **FloorUnfundable**, cleanly rejected |
| `standard` | `R = 1.2×base + 1.25×tip` | **~1.29×** (+29%) | **~1.71×** (+71%) | **FloorUnfundable**, cleanly rejected |
| `fast` | `R = 1.8×base + 2×tip` | **~1.29×** (+29%) | **~2.57×** (+157%) | **FloorUnfundable**, cleanly rejected |

These base-fee multiples are exactly what they were before the tip was scaled —
the tip terms cancel out of the ratio at `tip ≈ 0`. With a real tip the bands
are **wider** than the table, by `1.14 × tip[tier] / m` on the fully-paid bound
and `0.76 × tip[tier]` on the reprice bound, so `fast`'s doubled tip buys extra
drift tolerance as well as priority. `slow` trades the reprice band away for
the cheapest cap, which is exactly what "slow" should buy. Every larger spike
fails safe (a rejected send, never an under-charge or a loss to the relay).

**Repricing can never shave the tip.** The reprice floor is
`1.5 × base' + tip[tier]` — computed from the **tier's** tip, not the market
one — so a repriced cap still clears `base' + tip[tier]` and the builder is
paid in full. A reprice that clawed back the priority the client paid for would
be the §2a defect arriving by a different door. Pinned by
`a_reprice_can_never_shave_the_tip_the_client_paid_for`.

The exact numbers move with the tip, the client's gas padding (it pads limits
×1.5), and which tier the client priced against; the shape (direct-accept band
→ reprice band → clean-reject) is fixed by the rule.

### Integration caveats worth knowing

- **The two fee bases are not identical, but their ratio is fixed.** The client
  prices against `R = 0.6 × m × base + tip[tier]`; the relay settles against
  `m × base' + tip[tier]`. The 0.6 is chosen so the 3× client markup clears the
  1.4× requirement by 29% at every `m` (§2b), and the shared, unscaled
  `tip[tier]` on both sides is what makes that hold term by term — a client
  that lowered its markup toward the relay's 1.4× would lose almost all drift
  tolerance, at every tier alike.
- **The client must name on `eth_sendUserOperation` the same tier it priced.**
  Pricing off `fast` and submitting with no tier named over-pays for a
  `2 × base` cap at the bare market tip; pricing off `slow` (or off no
  `networkFeePerGas` at all) and naming `fast` under-funds a `3 × base + 2 ×
  tip` cap, which is clamped down toward `standard`. Scaling the tip widened
  that clamp considerably — it now bites whenever `tip < 6 × base`, against
  `tip < 0.75 × base` before (§2a) — so mis-pricing is more visible than it
  was. Clamping is always the safe direction; it is never a loss to either
  side, only a slower send than was asked for.
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
(§2a) has nothing to act on here — there is no base fee to multiply and no
priority tip to scale, since `TempoSignRequest` carries no priority-fee field
at all — so Tempo ignores it. (This is genuinely different from BSC, where the
base fee is zero but the tip is the whole price and therefore still buys
speed.)

## 6. Summary

- The relay requires `max(1.4 × gas × (2×base+tip), floor)`, recovers 1.4× its
  gas, and rounds every step in its own favor with fail-closed overflow.
- Repricing turns the `2×base` headroom into a live safety valve: a short-but-
  honest payment is repriced down to a fundable fee rather than rejected, down to
  the 1.5×base inclusion floor.
- A client may name a submission speed (`slow`/`standard`/`fast`) as an optional
  third `eth_sendUserOperation` parameter. **The name selects TWO multipliers,
  not one**: a base-fee multiplier for the submit cap (1.5/2.0/3.0×) and a tip
  multiplier for `maxPriorityFeePerGas` (1.00/1.25/2.00×). Naming nothing is
  byte-for-byte today's behaviour; naming `standard` now differs from it by the
  tip scale alone. The cap is clamped down to what the bundle's weakest
  reimbursement funds and up to the inclusion floor; the tip is never clamped.
- **Only the tip buys speed.** Builders order by
  `min(maxPriorityFeePerGas, maxFeePerGas − baseFee)`, which a bigger cap does
  not move. A mined Polygon `fast` receipt (base 250.710, max 775.525, max
  priority 30.35 gwei) paid the builder exactly what `slow` would have — the
  defect this design closes. `slow`'s tip is floored at the market tip because
  the relay has no per-chain minimum-tip knowledge and an under-tip is rejected
  outright, not merely mined late; `slow` saves on the cap instead.
- **The market tip is the node's `eth_maxPriorityFeePerGas`**, else
  `eth_gasPrice −` the latest base fee — one function, `gas_math::market_tip`,
  for the quote and the executor alike. Until 2026-09-21 the quote used
  `eth_feeHistory`'s median reward instead, and on Polygon quoted a
  `standard` tip of 107.7 gwei the relay then signed at 34.72.
- **A tier is that pair, and its quoted price derives from it.**
  `pimlico_getUserOperationGasPrice` reports, per tier, `maxFeePerGas` = the cap
  (`1.5/2.0/3.0 × base + tip[tier]`), `maxPriorityFeePerGas` = `tip[tier]` —
  the tip the relay will actually sign with — `networkFeePerGas` = `R` =
  `0.6 ×` the cap's base-fee part plus that same tip whole
  (`0.9/1.2/1.8 × base + tip[tier]`), and `relayerFeePerGas` = the difference,
  which is pure base-fee headroom. Sharing `tip[tier]` across cap and basis
  makes `3R − 1.4 × cap = 0.4·m·base + 1.6·tip[tier] ≥ 0` — every tier funds
  itself, by construction, with at least `3 × 0.6 / 1.4 = 1.286`. `standard`
  reproduces the old single price's base-fee term exactly; only its tip term
  moved.
- **A zero-base-fee chain (BSC) finally differentiates.** Cap, `R` and tip all
  collapse onto `tip[tier]` there, so the three tiers are `1.0 / 1.25 / 2.0 ×`
  the market tip — three prices that at last buy three different blocks. Never
  validate a pricing or speed change on BSC alone.
- A client must pay above the relay minimum to survive gas drift; vela-wallet
  pays a flat 3× on `max(C, R)` — its own chain measurement `C`, floored by the
  relay quote `R` for the tier it named — giving a **+43% / +71% / +157%**
  base-fee-spike tolerance at `slow` / `standard` / `fast` before a clean,
  loss-free rejection.
- The client protects itself both ways: it rejects a quote `R > 3 × C`
  (`GasQuoteTooHigh`) instead of paying it, and anchoring on `max(C, R)` stops a
  relay under-report from making it underpay. Doubling `fast`'s tip raised the
  supremum of `R[fast]/C` from 1.8 to **2.0** (reached only where the base fee
  is zero), still a third below the limit of 3, so the refusal cannot trip on
  this pricing.
- The client's own minimums are value-consistent — `$0.01` worth of native
  (`0.001`-coin when unpriced) and `$0.01` stable — sitting at or above the
  relay's `0.00001`-native / `$0.01`-stable admission floor, so paying them only
  ever helps.
- Stablecoin reimbursements are verified against the real on-chain Transfer
  event; a misdirected or wrong-token transfer is never credited.
