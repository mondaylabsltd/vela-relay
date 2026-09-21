# Gas price service

`GasPriceManager` is the application-wide gas price service. It is stored in `AppState`, so RPC handlers, UserOperation estimators, and future executor code use the same policy and chain fee trackers.

## Usage

```rust
let gas_price_manager = state.gas_price();
let quote = gas_price_manager
    .user_operation_gas_prices(chain_id, request.headers().get("x-vela-rpc-url"))
    .await?;

let fast = quote.tiers.fast;
let rpc_domain = quote.rpc_domain;
```

`GasPrice` values are integer wei values. The JSON-RPC handler is responsible for converting them to hexadecimal quantities.

## Estimation policy

1. Request `eth_feeHistory` and `eth_maxPriorityFeePerGas` in parallel. The base fee is the fee history's last `baseFeePerGas`; its reward column is not read.
2. Resolve the market tip with `gas_math::quote_market_tip`, the executor's own rule (`gas_math::market_tip`): `eth_maxPriorityFeePerGas` whatever it returns, zero included; only if it gave no quantity, `eth_gasPrice` minus the latest block's base fee; only if that fails too, a small base-fee-derived value the executor has no equivalent of.
3. If EIP-1559 fee history is unavailable, use `eth_gasPrice` as an all-tip market (base fee 0).
4. Produce the slow, standard, and fast rows with `gas_math::tiers`: each row is the cap and tip that tier is signed with. `docs/fees.md` §2a–2b has the arithmetic.

All upstream calls use `utils::rpc::call`, which applies the shared Alchemy, request-header, and AwesomeTools failover policy.
Each upstream request has a one-second deadline, while the full calculation has a 2.8-second internal response budget. A `GasPriceQuote` includes the domain of the RPC that supplied the primary gas-price data, so the HTTP handler can return it as `x-vela-rpc-domain` without exposing an endpoint path or API key.

Failed RPC requests enter a shared 30-second cooldown keyed by chain ID, RPC URL, and method. This prevents repeated failover attempts from spending time on a recently rate-limited or unavailable endpoint.

## Chain fee trackers

The manager also owns four rolling fee trackers, modelled after Alto's managers. They are intended for chain-specific `preVerificationGas` calculators and are independent from the base gas-price estimator.

- `ArbitrumManager`: L1 and L2 base-fee ranges.
- `CitreaManager`: minimum L1 fee rate.
- `MantleManager`: token ratio, scalar, rollup data gas and overhead, and L1 gas price.
- `OptimismManager`: minimum L1 fee.

Trackers keep a bounded in-memory history and provide conservative minimum or maximum values. They are ready for the corresponding chain-aware pre-verification gas implementation.
