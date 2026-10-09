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
required = max( markup × settlement_gas × cap ,  floor )
```

- **`markup`** — default **11000 bps = 1.1×** (`VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS`,
  hard lower bound 1.0×). Because the chain never charges more than the cap per
  gas, this is the relay's **guaranteed** margin over the gas it is billed for;
  in any calm market the cap sits well above `base + tip` and the margin is far
  larger (§2c measures it). It was 1.4× while the gas billed was the outer
  limit (§1a); the backtest in §2c chose 1.1×.
- **`settlement_gas`** — the gas the operation is billed for (§1a): on Ethereum
  and the other chains listed there, the gas its bundle measurably used plus
  the buffer; elsewhere, its share of the outer gas limit.
- **`cap`** — the `maxFeePerGas` the outer transaction is signed with. When the
  client names no speed it is `2 × base_fee + tip`
  (`gas_math::quoted_outer_fee`), the raw market tip, exactly as always. A
  client that names a speed gets that tier's cap and tip (§2a), clamped to what
  its payment funds. The cap is **inclusion headroom, not cost** — the chain
  only ever charges `base_fee + effective tip`.
- **`floor`** — a dust guard: `0.00001` native coin, or `0.01` of a stablecoin
  (≈ **1 cent**). This is NOT the price; it only bites when `markup × gas`
  rounds below it (near-zero-gas ops). Both the quote layer and the settlement
  layer compute it through the same `minimum_amount(decimals, fraction)` with
  the same constants, so they can never disagree (pinned by
  `the_dust_floor_is_the_requirement_when_the_gas_costs_less`).

## 1a. The gas an operation pays for — used, not reserved

Two gas figures exist for every bundle, and they used to be one:

- **The outer gas LIMIT** the relay signs: `cost::allocate_bundle_gas` over
  `max(simulated gas, eth_estimateGas of the bundle, the ops' own gas)` plus
  the buffer (`VELA_RELAY_EXECUTOR_GAS_BUFFER_BPS` 15%, `…_FIXED_GAS_BUFFER`
  30,000). `eth_estimateGas` of `handleOps` answers the gas the EntryPoint
  RESERVES for every declared limit, not the gas the bundle burns, so this is
  the figure that keeps a bundle from running out of gas. It is unchanged.
- **The settlement gas** each operation is billed for
  (`cost::settlement_gas_allocations`): on a chain that charges `gasUsed ×
  price` and whose bundle simulation ran in full (`eth_simulateV1` or
  `debug_traceCall`), the **measured** gas plus the same buffer, the buffer
  never taking it past the operations' own declared limits:

  ```
  billed_gas(used) = min( used + ⌈15% × used⌉ + 30,000 ,  max( declared limits , used ) )      (cost::billed_gas)
  declared limits  = Σ verificationGasLimit + callGasLimit + preVerificationGas (+ paymaster limits)
  ```

  It is split across a bundle's operations in proportion to the gas the
  EntryPoint accounts to each (`UserOperationEvent.actualGasUsed`), summing to
  the total exactly; a bundle of one operation bills the whole.

The 2026-10-02 Ethereum send that exposed the gap (tx `0x7132ee31…`): the
bundle used **146,824** gas, its `eth_estimateGas` was 322,126, and the relay
signed — and billed — a **400,445** limit. It now still signs 400,445 and bills
**198,848**. Pinned by
`ethereum_measures_the_payment_against_the_gas_used_and_signs_the_estimated_limit`.

Which chains settle on measured gas is a list, never an assumption
(`cost::settlement_gas_rule`):

| rule | chains | billed gas |
|---|---|---|
| `Measured` | Ethereum, Sepolia, Holesky, Hoodi; Gnosis, Chiado; Polygon, Amoy; BNB Smart Chain and its testnet | `buffered_gas(measured)` |
| `MeasuredAtLeastHalfTheLimit` | Avalanche C-Chain, Fuji | the same, but never under half the signed outer limit — Avalanche charges `max(gasUsed, gasLimit / 2)` since 2026-09-22, and its only simulation (`debug_traceCall`) is believed only up to `max(estimate, declared limits)` (`cost::credible_simulated_gas`) |
| `OuterLimit` | every other chain: Arbitrum (its L1 data cost rides inside gas units its simulation need not show), the OP stack (its L1 fee is charged beside gas), anything unlisted | the limit allocation, as before |

A simulation that measured nothing — the Pimlico `eth_call` stand-in, whose
bundle figure is an `eth_estimateGas` — bills the limit allocation on every
chain.

**`settlementGas` — the same figure, promised before signing.**
`eth_estimateUserOperationGas` returns an optional `settlementGas` (hex) on a
`Measured` chain: the executor's own rule, `buffered_gas`, over the gas a
one-operation `handleOps` carrying this operation is predicted to use
(`estimate::settlement_gas`):

```
used = 21,000 + calldata gas of handleOps([op], 0xff…ff)     (the returned limits, the request's signature)
     + preOpGas                                              (simulateValidation: validation, and any deployment)
     + execution                                             (the call's measured gas less its own 21,000 + calldata)
     + 10,000                                                (ENTRY_POINT_OVERHEAD_GAS)
settlementGas = billed_gas(used)     over the limits returned beside it — the executor's own rule
```

The execution of a **deployed** account is `eth_estimateGas` of its
`callData` from the EntryPoint. An **undeployed** account has no code to
estimate against — that call measured a plain transfer (a first send was
estimated at the 50,000 call-gas floor and used 52,804; a first backup at
~30,000 and used 4,315,038) — so its execution is measured with
`eth_simulateV1` of `[factory call from the SenderCreator, callData from the
EntryPoint]` in one block, which also sets its `callGasLimit` (1.5 × the
measurement, as for a deployed one). Where no endpoint performs that, the old
`eth_estimateGas` answers and `settlementGas` is omitted. It is omitted on
every chain that is not `Measured`, too: there the executor bills the limit
and nothing smaller can be promised.

**One cap at both ends.** The estimate and the executor cap the buffer by the
same rule over the same limits (`cost::billed_gas`). They used to differ — the
estimate capped at the limits it returned, the executor at the outer gas
allocation, which, built on the same measurement plus the same buffer, never
binds — so an operation whose buffer outgrew its limits was promised less
than it was billed (pinned by
`the_estimate_promises_the_gas_the_executor_bills_when_the_buffer_outgrows_the_limits`).

The 10,000 overhead is measured, not derived. The relay's two estimate calls,
replayed at the parent block of four mined Vela operations (archive state),
left this much of the receipt unexplained, and the predicted `settlementGas`
against what the executor bills for the same bundle:

| operation | receipt gas | unexplained | `settlementGas` | executor bills | ratio |
|---|---|---|---|---|---|
| ETH send `0x7132ee31…` | 146,824 | 1,249 | 208,070 | 198,848 | 1.046 |
| USDT send `0xe42fb6b9…` | 183,941 | 2,368 | 249,371 | 241,533 | 1.032 |
| USDT send `0x22cbfbd6…` | 169,192 | 3,511 | 228,988 | 224,571 | 1.020 |
| first op + backup `0x86795d08…` | 5,034,866 | −930 | 5,831,838 | 5,820,096 | 1.002 |

(`settlementGas` here with vela-wallet's dummy signature, which costs 3,060
gas of calldata against a real one's 3,792.) Signed with the limits the same
estimate returns, every one of the four executes in full at its parent block
(`simulateHandleOp`, `UserOperationEvent.success`), and with a third of the
call limit none does. Pinned by
`the_settlement_gas_of_a_mined_ethereum_send_covers_what_the_executor_billed` and
`an_undeployed_safe_is_measured_with_its_code_in_place`.

**The limits are still what the operation must carry.** `verificationGasLimit`
is 1.5 × the measured `preOpGas` (at least 100,000) — the measured value since
the `ValidationResult` decode was fixed: 61,906 for a deployed Safe's send on
Ethereum (so 100,000), 412,195 for a counterfactual one's first (so 618,293;
it was answered 100,000 before). `callGasLimit` is 1.5 × the measured
execution (at least 50,000).

Every rounding step in the chain (`mul_div_ceil` markup, `native_to_usd_stable_ceil`
USD conversion, Binance price parse, Tempo cost) rounds **toward the relay**, and
every multiply/scale is `checked_` and fails closed on overflow. The relay can
never round in the payer's favor or wrap silently.

## 2. Repricing — the safety valve that makes a fixed client payment work

A client signs its payment at quote time; the base fee at *inclusion* time may be
higher. The relay does not simply reject a payment that falls short of the nominal
`required` — it first tries to **reprice the outer transaction down** to a fee the
payment CAN cover, because the cap was headroom, not cost
(`settlement::decide_settlement`):

1. Evaluate at the quoted fee. If every op is fully paid → **KeepQuote** (submit
   as quoted).
2. If an op is short (but the payment parsed and went to the right recipient — a
   *shortfall*, not a malformed/misdirected payment), compute the **affordable
   fee** the weakest payer funds: `affordable = quoted_fee × (paid / required)`.
3. If `affordable` is at least the **inclusion floor**
   (`inclusion_floor_bps × base_fee + the tip the transaction is signed with`,
   default **1.125×base + tip**, `VELA_RELAY_EXECUTOR_SETTLEMENT_INCLUSION_FLOOR_BPS`)
   and below the quoted fee → **Reprice** to `affordable` and submit. Repricing
   preserves the full markup (reimbursement still covers `markup × gas ×
   new_fee`, and the chain can never charge more than `new_fee`). Because the
   floor carries the **signed** tip, a reprice can never drop the cap below
   `base_fee + tip`.
4. If `affordable` is below the inclusion floor → **FloorUnfundable**: the
   operation is held in the delayed inbox while the market may come back
   (`VELA_RELAY_EXECUTOR_SETTLEMENT_HOLD_MAX_ATTEMPTS`, 12 attempts ≈ 35 min),
   then rejected. Never a loss — the relay never signs an outer transaction it
   would lose money on.

The floor was 1.5× (three blocks of the largest EIP-1559 rise). The backtest in
§2c chose 1.125×: the largest rise EIP-1559 allows into the next block, so a
floored cap is always valid for the very next block. A signed transaction
cannot be bumped (the outbox broadcasts exact bytes), so a bundle priced out
of later blocks would wedge its lane — but over 10.4 days a higher floor left
no fewer bundles priced out (they are the ones whose slow tip waits; ~0.03% of
slow sends at either floor) and held or rejected more slow sends.

Because the floor uses `max(cost, dust_floor)` on the stablecoin path, a payment
below the *dust* floor can never be repriced into acceptance (the requirement is
pinned at the floor at every fee) — a case pinned by
`a_stablecoin_below_the_floor_cannot_be_repriced_into_acceptance`.

## 2a. Submission speed — the client may name a tier

Repricing (§2) only ever moves the cap **down**. A client may name a submission
speed on `eth_sendUserOperation`, as an **optional third parameter**:

```jsonc
["eth_sendUserOperation", [ <userOperation>, <entryPoint>, "standard" ]]
```

The value is a **tier NAME** (`"slow" | "standard" | "fast"`), never a wei
amount. The relay resolves the name against the base fee and the tips it reads
at submit time, so a quote that went stale between signing and inclusion can
never set the price. An unknown name is refused with `-32602 invalid params`
before any handler runs; omitting the parameter is the relay's own pace, byte
for byte (`2 × base_fee + market tip`, the market tip signed).

### A tier is TWO levers

| tier | cap — `base_fee_bps` | tip — the reward percentile it signs | what each buys |
|---|---|---|---|
| `slow` | **1.5×** base_fee | **25th** percentile, floored | the cheapest tip that is still mined within a few blocks |
| `standard` | **1.5×** base_fee | **50th** percentile (the median) | next-block inclusion in the backtest |
| `fast` | **1.75×** base_fee | **70th** percentile | outbids seven tenths of a block's gas |

```
window    = eth_feeHistory over the chain's last minute of blocks, at least 20   (tip_window_blocks)
least     = 0.001 gwei if the window paid any tip, else 0
node      = eth_maxPriorityFeePerGas, unless the window paid tips and it is above their median p50
rewarded  = max( the window's median p25 , least )

busy window  (mean gasUsedRatio ≥ 30%):   tip[slow] = max(rewarded, node),  tip[standard] = max(median p50, slow),
                                          tip[fast] = max(median p70, standard)
quiet window (mean gasUsedRatio < 30%):   tip[slow] = max(node, least) (rewarded without a node),
                                          tip[standard] = 1.25 × slow,  tip[fast] = 2 × slow
tips.floor = rewarded (the node's tip where the window paid none); below a 40% mean, the lower of
             that and the quiet tip[slow]                                                     (TierTips)

absent  ⇒ cap = 2 × base_fee + market_tip,  signed tip = market_tip           (unchanged)
present ⇒ funded = the cap the reimbursements fund, measured at the tier's own cap;   floor = inclusion_floor × base_fee
          funded ≥ floor + tip[tier]     →  cap = min( max(base_fee_bps[tier] × base_fee, floor) + tip[tier] , funded ),
                                            tip = tip[tier]
          funded ≥ floor + tips.floor    →  cap = funded,  tip = funded − floor    (the tip shaved, never below tips.floor)
          otherwise                      →  cap = floor + tips.floor,  tip = tips.floor  (held, §2)
```

**The cap buys spike resilience. Only the tip buys priority.** They are
different goods and a tier has to move both, because an EIP-1559 block builder
orders transactions by the **effective tip**,
`min(maxPriorityFeePerGas, maxFeePerGas − baseFee)`, which any cap above
`base_fee + tip` leaves unchanged. Until 2026-09-21 the relay scaled only the
cap, and a mined Polygon `fast` receipt (base 250.710, max 775.525, max
priority 30.35 gwei) paid the builder exactly what `slow` would have. Every tier
now signs its own tip.

**The tips are read from what blocks actually paid.** `eth_feeHistory(n,
"latest", [25, 50, 70])` reports, per block, the effective tip at the 25th, 50th
and 70th percentile of its gas, and how full the block was. A tier's tip is the
median of its column over the window, so one odd block moves nothing. `slow` is
never below 0.001 gwei once the window paid any tip at all, because a block's
low percentiles are often a single wei on Ethereum. A chain whose blocks pay no
tip (Arbitrum) keeps zero. Without a readable reward column (a legacy chain, a
failed call) every tier falls back to the market tip scaled `1.00 / 1.25 /
2.00` — the rule before rewards were read — on the quote and the executor
alike (`TierTips::resolve`).

- **The window is a minute of blocks, never fewer than 20**
  (`gas_math::tip_window_blocks`, from `pace::block_interval_ms`): Ethereum
  and Gnosis read 20, Polygon, OP Mainnet and Base 30, Avalanche and Unichain
  60, BNB Smart Chain 134, Arbitrum 240 (at most 256; every endpoint probed
  serves that many). Twenty BSC blocks are nine seconds, less than a quote's
  own age: the executor's window held none of the quote's blocks, and 38% of
  BSC `standard` and `fast` sends had their tip shaved below the one quoted.
- **The node's tip is a floor only where the blocks do not contradict it.**
  `eth_maxPriorityFeePerGas` carries a chain's enforced minimum (bor on
  Polygon), which a tip must clear or be refused outright — and an enforced
  minimum is never above what the median block paid, since every included
  transaction cleared it. Nodes disagree, and the quote and the executor ask
  different ones: on BNB Smart Chain the directory's endpoints answered 0.05,
  0.1, 1 and 3 gwei over blocks whose median paid 0.05 (2026-10-09), and an
  executor reading 1 gwei signed `slow` at 1 gwei — holding, then rejecting,
  every send quoted at 0.05. An answer above the window's median p50 is that
  node's opinion and is left out. A window that paid no tip at all (Stable,
  XRPL EVM) contradicts nothing, and the node is believed.
- **A quiet window signs the node's tip.** Where the window's blocks used less
  than 30% of their gas limit on average, every transaction paying the
  chain's minimum fits in the next block, and the percentiles are what a few
  bots bid: Polygon's 25th percentile read 166–265 gwei against the node's 30,
  Avalanche's 2.5–6.2 gwei on blocks 3% full, Gnosis's 70th 1.5 gwei over an
  8-wei base fee. There each tier signs the node's tip scaled `1.00 / 1.25 /
  2.00` — what every tier signed before rewards were read, mined on Polygon at
  30 gwei — and `fast` still bids twice `slow`. The line is 30%, not one half,
  because Ethereum's base fee targets half-full blocks: its 20-block average is
  below 0.5 in 49% of windows and was never below 0.338 over the 10.4 days of
  §2c, so it always reads busy; BNB Smart Chain's minute reads quiet 92% of
  the time, Polygon's 93%, Avalanche's always.

On Ethereum the node's tip is no guide: on 2026-10-08 `eth_maxPriorityFeePerGas`
answered 0 while blocks paid a median of 1 gwei, so the relay signed every tier
with no tip at all and `fast` bought nothing. At block 26,149,237 the tiers read
0.147 / 1.0 / 1.795 gwei; `standard`'s cap, `1.5 × 2.784 + 1.0 = 5.177` gwei, is
to the wei the `maxFeePerGas` Uniswap quoted for its own swap in that block.
Pinned by `the_ethereum_tiers_at_the_block_the_overcharge_was_measured`.

**The clamps, and which wins.**

- *The cap gives way first.* A payment short of the full tier is repriced down
  toward the inclusion floor, keeping the whole tip — the cap above `base +
  tip` only ever bought resilience.
- *Then the tip, never below what the window proves the chain takes.* A
  payment that cannot fund the floor with its tier's whole tip — a quote
  several blocks old in a rising market, a quote whose node answered a lower
  tip, a quote read in a quiet window and submitted in a busy one, or a wallet
  that priced a cheaper tier than it named — keeps the floor's base-fee
  headroom and gives back priority, down to `tips.floor`: the window's own
  25th-percentile reward, and within reach of the quiet line (a mean below
  40%) the quiet `slow` tip if lower. A slower send, never a stuck or a
  rejected one. (Until the tips were read from rewards the tip was never
  shaved; the backtest in §2c measures how rarely this runs for a fresh quote.)
- *What a payment funds is measured at the tier's own cap.* Where the `$0.01`
  floor is the price, the payment funds far more than the gas costs. Measured
  at the executor's untiered `2 × base + tip` — 17 wei on Gnosis — it read as
  funding that and no more, and a Gnosis `fast` send quoted at a 1.5 gwei tip
  was signed at the slow tier's 0.001 gwei.
- *The weakest payer sets the cap.* A bundle is one transaction at one price; a
  `fast` neighbour can never price a slower operation out of its own bundle.
  A bundle takes the fastest speed any member named, and a member that named
  none counts as `standard`.
- *Never at a loss.* The cap is never above what the reimbursements fund, and a
  payment that cannot fund even the floor at the slowest tip reaches §2's
  `FloorUnfundable` and the ordinary hold.
- *`base + tip` is the invariant.* Every branch keeps the cap at or above the
  base fee plus the tip it signs; the executor asserts
  `OuterFee::delivers_full_tip_at` on the signed pair just before `SignBundle`
  and fails the batch rather than sign one that breaks it.

Pinned by `each_tier_submits_at_its_own_cap_and_its_own_tip`,
`a_payment_short_of_its_tier_at_the_floor_gives_back_priority_not_inclusion`,
`no_clamp_can_resolve_a_cap_that_truncates_its_own_tip` and
`a_client_named_speed_signs_the_reward_percentile_tip_the_quote_showed`.

**Where it lives.** `gas_math::{SubmissionTier, TierTips, tier_outer_fee}` (the
tables and the one tip rule), `settlement::decide_submission_fees` (the clamps),
`execution::bundle_submission_tier` (one transaction, one price). The tier
rides the **queue envelope**, not the UserOperation: it is not part of what the
user signed, so it never enters the `userOpHash`, the admission fingerprint or
the durable record. Tempo (§5) prices gas in pathUSD with no base fee to
multiply and ignores the tier.

## 2b. The quote — what `pimlico_getUserOperationGasPrice` reports per tier

| field | meaning | value |
|---|---|---|
| `maxFeePerGas` | the cap the relay **will submit at** | `base_fee_bps × base_fee + tip[tier]` |
| `maxPriorityFeePerGas` | the tip the relay **will sign with** | `tip[tier]` (§2a) |
| `inBandFeePerGas` | what a client pays **per unit of `settlementGas`** (§3) | `markup × drift × maxFeePerGas`, rounded up — `1.1 × maxFeePerGas` by default |
| `networkFeePerGas` (`R`) | what a wallet pricing the **limits** reimburses against (§3), frozen | `0.9 / 1.2 / 1.8 × base_fee + 1.00 / 1.25 / 2.00 × market tip` |
| `relayerFeePerGas` | `maxFeePerGas − networkFeePerGas`, saturating at 0 | |

`base_fee` is the next block's (the last `baseFeePerGas` of the fee history);
the executor prices the cap on the latest block's when it submits, which a
block later is the same number.

**What is quoted is what is signed.** The quote and the executor read the tier
tips by one rule (`gas_math::TierTips::resolve`) from the same
`eth_feeHistory(tip_window_blocks(chain), "latest", [25, 50, 70])`
(`gas_math::tip_history_params`): the quote through the request's failover
chain, the executor in its transaction-context batch at submit time. Fed the
same answers they produce the same tip and the same cap, to the wei (pinned by
`the_tip_reported_for_a_tier_is_the_tip_the_executor_signs_given_the_same_rpc_answers`,
over Polygon busy and quiet, Arbitrum, Base, Optimism, Ethereum, BSC and a
rising base fee). They are not always fed the same answers — they ask
different nodes, a little apart in time — so the reading is built to agree
anyway: a median over at least a minute of blocks, which a quote and its
submission mostly share; a node's tip only where the blocks bear it out
(pinned by `a_quote_from_one_node_is_accepted_by_an_executor_that_asks_another`);
and where they still differ, the clamps of §2a decide, shaving rather than
holding (`a_quote_read_in_a_quiet_window_is_not_held_when_the_next_one_is_busy`).

**`inBandFeePerGas`** — `settlement markup × IN_BAND_DRIFT × cap`
(`gas_math::in_band_fee_per_gas`), with the operator's configured markup and
`IN_BAND_DRIFT_BPS = 10000` (the backtest found no allowance was needed, §2c).
Paid on `settlementGas`, it funds the executor's whole requirement at the cap
the quote names; a base fee that rose before the submission is absorbed by
repricing that cap down toward the inclusion floor, keeping the tip (§2).
It is published exactly where `eth_estimateUserOperationGas` returns
`settlementGas` — on a chain whose executor bills measured gas (§1a) — and
omitted elsewhere, so no client can multiply it by gas the relay does not bill
on.

**Why `networkFeePerGas` is frozen.** vela-wallet before `settlementGas` pays
`3 × padded limits × max(C, R)` and **refuses** a quote with `R > 3 × C`, where
`C = max(eth_gasPrice, base_fee + eth_maxPriorityFeePerGas)` is its own reading.
On Ethereum that tip reads ~0, so `C` is the bare base fee; a reward-percentile
tip carried into `R` would put `R` above `3 × C` whenever the base fee is low —
most of the time — and those wallets could not send at all. So `R` keeps its
earlier definition over the market tip, those wallets pay exactly what they
did, and what they pay on their padded limits still funds the tier they name
(§3; pinned by `a_wallet_that_prices_the_limits_is_still_accepted_at_its_tier`
and `the_reimbursement_basis_is_frozen_for_wallets_that_price_limits`). The
deployed relay's `R` at block 26,149,237 — 2,505,999,999 / 3,341,333,332 /
5,011,999,998 wei — is reproduced to the wei.

**The market tip is one reading, resolved one way** (`gas_math::market_tip`):
the node's `eth_maxPriorityFeePerGas`, zero included; only when that call gave
no quantity, `eth_gasPrice −` the latest block's base fee. It is the untiered
pace's tip, the floor under `slow` where the blocks bear it out, every tier's
tip (scaled) in a quiet window, and `R`'s tip term. When neither yields a
tip, the executor refuses to submit and only the quote falls back, to
`base_fee / 200` (`gas_math::quote_market_tip`).

**Where it lives.** `gas_math::{tiers, tier_price, TierTips,
in_band_fee_per_gas}`, rendered by both shells through one function,
`wire::gas_price_tiers` (`src/app/rpc/handlers/user_operation_gas_price.rs` and
`vela-relay-cf/src/http.rs`). A generic ERC-4337 bundler omits the last three
fields.

## 2c. Choosing the numbers — the backtest

The cap multiples, the tip percentiles and window, the drift allowance, the
inclusion floor and the markup were chosen by replaying 10.4 days of Ethereum
mainnet: `eth_feeHistory` for blocks 26,076,210–26,150,961 (2026-09-28 to
2026-10-08, 74,752 blocks; base fee median 0.178 gwei, 90th percentile 1.43,
peak 10.65) with reward percentiles 1–99. `docs/fees-backtest.py` fetches the
history and runs the replay.

**The model.** A wallet quotes at block `q` (base fee `B = base[q+1]`, tips from
the window ending at `q`) and pays `F = settlementGas × inBandFeePerGas[tier]`.
The executor submits `a` blocks later — 1, 3 or 5 for a quote 12, 30 or 60 s
old — reading `base[s]` and the window ending at `s`, and applies §2a and §2
exactly as coded (the full tier, a repriced cap, a shaved tip, or a hold
retried on the delayed-inbox ladder: 5, 10, 20, 40, 80, 160 s then every
300 s, 12 attempts, then rejected). A signed bundle is counted included in the
first later block whose base fee its cap covers and whose 25th-percentile
reward its effective tip meets (the "lenient" column uses the 10th). The chain
charges `gas used × (base + effective tip)`; the person's baseline is a
Uniswap-like `gas used × (B + the window's median tip)`. Operation sizes are the
investigation's: an ETH send uses 146,824 gas, an ERC-20 send 163,719, a swap
291,357, a first operation from an undeployed Safe 504,609, each billed
`buffered_gas` of it.

**The targets** (the owner's, 2026-10-09): `standard` and `fast` accepted at the
first pass ≥ 99% for a quote ≤ 12 s old and ≥ 97% at 30 s; `slow` may wait,
but its median delay is a few blocks; `fast` measurably earlier than
`standard`, and `standard` than `slow`; `fast` at most about twice `slow`'s
price; the relay never negative; and, among the parameter sets meeting all of
that, the cheapest.

**The candidates.** 456 sets in a first sweep and 168 in a second around the
front-runners: drift allowance 1.0 / 1.0625 / 1.125; caps
(slow, standard, fast) from 1.25 to 3.0; inclusion floors 1.125 / 1.25 / 1.5;
tip percentiles (10…30, 40…50, 60…90); windows of 10 and 20 blocks. What
failed: `fast` dearer than twice `slow` (every `fast` tip at the 75th
percentile or above, and a `fast` cap of 2.0 or more); slow sends rejected
after the whole hold budget (every set whose inclusion floor was `slow`'s own
cap — a 1.5× floor under a 1.5× cap — and a 1.25× floor without a drift
allowance); `standard` and `fast` acceptance at 30 s for the sets with the
narrowest gap between cap and floor.

**The choice — the cheapest set meeting every target, with `standard` at the
median:** caps 1.5 / 1.5 / 1.75, tips at the 25th / 50th / 70th percentile over
20 blocks, drift allowance 1.0, inclusion floor 1.125, markup 1.1. Over the
10.4 days:

| tier | quote age | accepted at the first pass | of which the whole tier | rejected after the hold budget | blocks to inclusion, mean / p90 / p99 | ditto, lenient | tip's place in the next block |
|---|---|---|---|---|---|---|---|
| `slow` | 12 s | 100.00% | 100.00% | 0.000% | 2.28 / 4 / 12 | 1.08 / 1 / 2 | p25 |
| `slow` | 30 s | 99.95% | 99.95% | 0.012% | 2.36 / 4 / 12 | 1.09 / 1 / 2 | p25 |
| `slow` | 60 s | 99.41% | 99.41% | 0.163% | 2.54 / 4 / 14 | 1.28 / 1 / 2 | p25 |
| `standard` | 12 s | 100.00% | 99.94% | 0.000% | 1.06 / 1 / 2 | 1.02 / 1 / 1 | p50 |
| `standard` | 30 s | 100.00% | 99.67% | 0.000% | 1.07 / 1 / 2 | 1.03 / 1 / 1 | p50 |
| `standard` | 60 s | 99.96% | 98.82% | 0.011% | 1.13 / 1 / 2 | 1.06 / 1 / 1 | p50 |
| `fast` | 12 s | 100.00% | 99.80% | 0.000% | 1.01 / 1 / 1 | 1.00 / 1 / 1 | p70 |
| `fast` | 30 s | 100.00% | 99.12% | 0.000% | 1.01 / 1 / 1 | 1.00 / 1 / 1 | p70 |
| `fast` | 60 s | 100.00% | 98.16% | 0.000% | 1.02 / 1 / 1 | 1.00 / 1 / 1 | p70 |

("Accepted" counts the full tier, a repriced cap and a shaved tip; "the whole
tier" excludes the shaved tip. Blocks are counted from the first submission
attempt, holds included.)

| tier | price ÷ `slow`'s, median | ETH send: paid ÷ baseline, median (p90) | swap: ditto | relay profit ÷ chain charge, ETH send: min / 1st pct / median | undeployed first op: ditto |
|---|---|---|---|---|---|
| `slow` | 1.00 | 1.56 (1.97) | 1.45 (1.82) | +49% / +80% / +120% | +33% / +61% / +96% |
| `standard` | 1.27 | 1.99 (2.13) | 1.84 (1.97) | +49% / +70% / +98% | +33% / +52% / +77% |
| `fast` | 1.87 | 2.93 (3.62) | 2.71 (3.35) | +49% / +64% / +100% | +33% / +47% / +79% |

The relay's profit is never negative by construction — it is paid at least
`markup × billed gas × the signed cap` and charged at most `gas used × that
cap` — and over the replay it was never under +33% of the chain's charge.
With a gas estimate 3% short of what the executor bills, `standard` and `fast`
are still accepted whole ≥ 99.67% at 12 s and ≥ 98.66% at 30 s. A drift
allowance of 1.0625 with a 1.25× floor — 6% dearer — accepts `standard` whole
99.96% / 99.68% at 12 / 30 s and rejects 0.046% of slow sends at 30 s instead
of 0.012%: the gap between a tier's cap and the floor already pays for a
quote's drift, so the allowance bought nothing the targets asked for.

**The markup.** It decides price and guaranteed margin only — acceptance is the
same at every markup, because the published price scales with it. Over the
replay (`standard`, ETH send, 12 s): 1.0× pays 1.81× the baseline with a
worst-case profit of +35% (+21% on an undeployed first op); 1.05× 1.90× and
+42%; **1.1× 1.99× and +49% (+33%)**; 1.2× 2.17×; 1.4× 2.53×. 1.0× also meets
every target; 1.1× is chosen so that the margin the relay is *guaranteed* — the
one left when a spike consumes the whole cap and the operation burns its whole
gas buffer — still pays for what no operation is billed for (relayer top-ups, a
bundle that reverts on-chain) rather than breaking even. An operator who
prefers the cheapest set sets `VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS=10000`;
every published price follows, because both shells price `inBandFeePerGas`
with the executor's own configured markup.

The parameters were fitted to Ethereum, the chain where gas is a person's real
cost. Elsewhere the same tables apply; on the cheap chains the `$0.01` floor is
the price anyway (§1), and the reward-percentile tips carry each chain's own
market.

## 3. What a CLIENT should pay

The relay publishes the price so the client does not re-derive it:

```
F = max( settlementGas × inBandFeePerGas[tier] ,  settlementGas × own_check(tier) ,  dust floor )
```

- **`settlementGas`** comes from `eth_estimateUserOperationGas` (§1a), the gas
  the executor will bill; **`inBandFeePerGas[tier]`** from
  `pimlico_getUserOperationGasPrice` (§2b) for the tier the client will NAME on
  `eth_sendUserOperation`.
- **`own_check(tier)`** is the same published formula applied to the client's
  own chain measurement, so a relay reading a lagging market cannot under-price
  it: `markup × drift × (base_fee_bps[tier] × base_fee + tip[tier])`, with the
  markup the relay applies (1.1× by default), the drift allowance (1.0), the
  tables of §2a, and the client's own `base_fee` and tier tips — read, for an
  exact match, as the relay reads them (§2a: `eth_feeHistory` over the
  chain's tip window, the median of each column — or the node's tip scaled in
  a quiet window — `slow` at least 0.001 gwei once the window paid any tip and
  at least the node's tip where the blocks bear it out, each faster tier at
  least the slower one).
- **The dust floor**: at least `$0.01` of the native coin (and never below the
  relay's `0.00001`-coin floor), or `$0.01` of a stablecoin.
- **A fresh quote.** The acceptance table in §2c is for quotes 12, 30 and 60 s
  old. A client that re-quotes once a block while the confirm surface is up,
  and refreshes a quote older than a block before signing, sits in the 12 s
  row.
- **A sanity bound.** A relay figure far above the client's own reading is
  refused rather than paid (vela-wallet: `R > 3 × C`).

**On a chain without `settlementGas`** (§1a: the executor bills the outer
limit there), and with a relay older than this contract, the client prices the
returned gas limits as before: `3 × (verificationGasLimit + callGasLimit +
preVerificationGas + overhead) × max(C, R)`. That pays far more than the
requirement and is always accepted.

**A wallet that prices the limits on a relay that bills used gas** — vela-wallet
before `settlementGas` — overpays, and is accepted at the whole tier it names:
at block 26,149,237 its `3 × padded limits × max(C, R)` funds every tier of
every operation class at its full cap and tip (pinned by
`a_wallet_that_prices_the_limits_is_still_accepted_at_its_tier`). Its `R`
stays inside its own `R > 3 × C` refusal because `R` is frozen (§2b).

### What it costs, worked out

The investigation's block, 26,149,237 (base 2.78 gwei, ETH $2,423.92), for the
2026-10-02 ETH send (`settlementGas` 208,070, gas used 146,824):

| tier | `inBandFeePerGas` | paid | the chain charges | the deployed wallet + relay quoted |
|---|---|---|---|---|
| `slow` | 4.756 gwei | $2.40 | ~$1.04 | $11.80 |
| `standard` | 5.694 gwei | $2.87 | ~$1.35 | $14.19 |
| `fast` | 7.335 gwei | $3.70 | ~$1.63 | $21.28 |

Pinned by `what_an_ethereum_send_costs_at_each_tier`. For every operation class
and tier, `a_wallet_paying_the_published_price_on_settlement_gas_is_accepted_at_its_tier`
checks that this payment funds the requirement at the quote's block, is still
accepted at the whole tier tip after one block of the largest base-fee rise,
and leaves the relay paid above the most the chain can charge.

**The margins it guarantees.** Against the requirement at the cap the quote
named, a client following this section pays it exactly (times its estimate's
slack over the executor's billing: 1.002–1.046 on the replayed operations).
The base fee may then rise before the submission by:

| tier | the whole tip, the cap repriced | the tip shaved toward `slow`'s | above that |
|---|---|---|---|
| `slow` | +33% (1.5 / 1.125) | — (its tip is the floor's) | held, then rejected |
| `standard` | +33%, plus what its tip over `slow`'s buys | down to `slow`'s tip | held, then rejected |
| `fast` | +56% (1.75 / 1.125), plus the same | down to `slow`'s tip | held, then rejected |

(at a tip small beside the base fee; a real tip widens every band, since the
cap a payment funds carries it whole). Every larger move fails safe: a held,
then rejected send, never an under-charge or a loss to the relay. One block of
the largest rise is 12.5%, so a quote a block old is accepted at its whole
tier; the backtest in §2c measures the rest.

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
(attodollar-denominated), applies the same settlement markup (1.1× by default)
over its outer limit with the same `$0.01` floor (`marked_tempo_cost`), and signs the outer transaction with Tempo's
`0x76` envelope paying fees in pathUSD. The client mirrors this with a separate
Tempo model (2× margin plus an explicit gas/split cushion annotated "must match
vela-relay", added after a real sub-floor deploy rejection). A submission tier
(§2a) has nothing to act on here — there is no base fee to multiply and no
priority tip to scale, since `TempoSignRequest` carries no priority-fee field
at all — so Tempo ignores it. (This is genuinely different from BSC, where the
base fee is zero but the tip is the whole price and therefore still buys
speed.)

## 6. Summary

- The relay requires `max(markup × settlement_gas × cap, floor)`: a 1.1× markup
  (`VELA_RELAY_EXECUTOR_SETTLEMENT_MARKUP_BPS`), the cap the outer transaction
  is signed with, and the `0.00001`-coin / `$0.01` dust floor. It rounds every
  step in its own favor with fail-closed overflow.
- **It bills the gas a bundle uses, not the gas it reserves** (§1a): on
  Ethereum and the other listed chains, the measured gas plus 15% and 30,000;
  the outer LIMIT stays estimate-based so no bundle runs out of gas. Avalanche
  is billed at least half the signed limit, which is what it charges; every
  other chain is billed the limit, as before.
  `eth_estimateUserOperationGas` returns the same figure as `settlementGas`
  before the client signs, and its `verificationGasLimit` is now the measured
  validation gas.
- Repricing turns the cap into a live safety valve: a short-but-honest payment
  is repriced down to a fundable cap, down to the 1.125×base inclusion floor,
  rather than held.
- A client may name a speed. **A tier is two levers**: a cap (1.5 / 1.5 / 1.75 ×
  base) and a tip read from what recent blocks paid — the median over a
  minute of blocks (at least 20) of each block's 25th / 50th / 70th
  percentile reward, or, where the blocks are under 30% full, the node's own
  tip scaled 1.00 / 1.25 / 2.00; the node's tip a floor only where the blocks
  bear it out. The quote and the executor read the tips by one rule, so what
  is quoted is what is signed. A payment that cannot fund its tier gives back
  cap headroom first, then priority down to what the window proves the chain
  takes, then is held — never signed at a loss. Naming nothing is the relay's
  own pace, unchanged.
- `pimlico_getUserOperationGasPrice` reports each tier's cap and tip, its
  `inBandFeePerGas` (`markup × cap`) where `settlementGas` is returned, and a
  frozen `networkFeePerGas` for wallets that still price the limits.
- **A client pays `settlementGas × inBandFeePerGas[tier]`** (or its own
  reading of the same formula, if higher, and never under the dust floor). At
  the block the overcharge was measured that is $2.40 / $2.87 / $3.70 for an
  ETH send the chain charges $1.04–1.63, against $11.80 / $14.19 / $21.28
  before. Over 10.4 days of Ethereum, a quote ≤ 12 s old was accepted at its
  whole tier ≥ 99.8% of the time on every tier, `fast` was mined first and
  `slow` last, and the relay never earned less than +33% over the chain's
  charge (§2c). A wallet that still prices its padded limits overpays and is
  accepted at its whole tier.
- Stablecoin reimbursements are verified against the real on-chain Transfer
  event; a misdirected or wrong-token transfer is never credited.
