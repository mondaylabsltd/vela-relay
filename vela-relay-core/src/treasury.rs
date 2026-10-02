//! The relay treasury's public status — the answer to "can this relay actually
//! pay gas on this chain right now?".
//!
//! The wallet asks before it lets anyone sign, because the alternative is what
//! it used to do when the answer was unavailable: accept the operation, show
//! "submitted", and leave the person watching a spinner for a payment that can
//! never land. An empty treasury is a fact the relay knows; the only failure is
//! not saying so.
//!
//! The whole probe is decided here, shared by both shells (docker and
//! Cloudflare): WHICH balance is read ([`balance_read`]), what the answer means
//! ([`treasury_status`]), and the floor a person is warned about. The shells
//! only perform the one RPC read and serialize [`TreasuryStatus`], so the two
//! deployments cannot answer the same question differently.
//!
//! The asset is not always the native coin. Tempo has none: its gas is paid in
//! pathUSD, and `eth_getBalance` there returns a huge placeholder rather than
//! zero. Probing it with `eth_getBalance` once reported a treasury holding no
//! pathUSD at all as healthy, and a person's send then sat in the queue behind
//! a top-up that could never be paid for (2026-10-03, Tempo mainnet).

use serde::Serialize;
use serde_json::{Value, json};

use crate::tempo;

/// 0.0001 native coin — the operator float below which the relay cannot fund a
/// relayer and needs a direct, NON-REFUNDABLE bootstrap deposit. Mirrors
/// `DEFAULT_TREASURY_FLOOR_WEI` in the docker shell's config.
pub const NATIVE_TREASURY_FLOOR: &str = "0x5af3107a4000";

/// What the pathUSD top-up transaction's own gas may cost, in micro-pathUSD
/// (0.05 pathUSD).
///
/// Measured on Tempo mainnet on 2026-10-03 with `eth_estimateGas`: a pathUSD
/// transfer to an address that already holds pathUSD costs 44,721 gas, one to
/// an address that never held any costs 295,571 (Tempo charges ~250k gas for
/// new state), and a never-used treasury's first transaction carries the same
/// ~250k for its account. The worst case — a fresh treasury funding a fresh
/// relayer — stays under 600,000 gas; with the executor's 1.2× top-up buffer
/// (`tempo::buffered_top_up_gas_limit`) at its own outer fee cap over its
/// default base fee (`tempo::tempo_outer_max_fee(TEMPO_BASE_FEE_ATTO)`, ~33×
/// the base fee observed that day) that is 21,600 micro-pathUSD. This covers it
/// more than twice over; the test below holds the arithmetic to it.
pub const PATH_USD_TOP_UP_GAS_HEADROOM: u128 = 50_000;

/// The pathUSD treasury floor on Tempo, in micro-pathUSD: 0.55 pathUSD.
///
/// It is what the executor actually needs before it will move any pathUSD to
/// an empty relayer. A top-up tops the relayer up to
/// `max(prefund, TEMPO_FLOAT_TARGET)` (`funding::plan_tempo_top_up`), and the
/// executor refuses to sign it unless the treasury holds the top-up, the
/// top-up's gas, AND `TEMPO_TREASURY_FLOOR` on top (`execution.rs`,
/// "Tempo treasury pathUSD is below top-up amount, gas, and reserve floor"):
///
/// `TEMPO_TREASURY_FLOOR` (0.2 reserve) + `TEMPO_FLOAT_TARGET` (0.3 top-up of
/// an empty relayer) + [`PATH_USD_TOP_UP_GAS_HEADROOM`] (0.05 gas).
///
/// Reporting only the 0.2 reserve — the analogue of the native floor — would
/// recreate this probe's original bug at a smaller scale: a treasury holding
/// anything between 0.2 and 0.5 pathUSD would read healthy while every top-up
/// it attempted failed. And the wallet asks a person to deposit
/// `floor - balance`, so the floor has to be an amount that actually unblocks
/// the relay.
pub const PATH_USD_TREASURY_FLOOR_MICRO: u128 =
    tempo::TEMPO_TREASURY_FLOOR + tempo::TEMPO_FLOAT_TARGET + PATH_USD_TOP_UP_GAS_HEADROOM;

/// [`PATH_USD_TREASURY_FLOOR_MICRO`] as the RPC `QUANTITY` the probe reports.
pub const PATH_USD_TREASURY_FLOOR: &str = "0x86470";

/// What a chain's treasury pays gas with — and so what the probe must read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum TreasuryAsset {
    /// The chain's native coin, read with `eth_getBalance`.
    #[serde(rename = "native")]
    Native,
    /// Tempo's pathUSD (TIP-20, six decimals), read with `balanceOf`. The wire
    /// name `"pathUSD"` is what every wallet shell matches on to switch to six
    /// decimals and the pathUSD symbol.
    #[serde(rename = "pathUSD")]
    PathUsd,
}

impl TreasuryAsset {
    /// Tempo pays gas in a TIP-20 stablecoin; every other chain in its native
    /// coin.
    pub const fn for_chain(chain_id: u64) -> Self {
        if tempo::is_tempo_chain(chain_id) {
            Self::PathUsd
        } else {
            Self::Native
        }
    }

    /// The `asset` wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::PathUsd => "pathUSD",
        }
    }

    /// The floor, in this asset's base units, as an RPC `QUANTITY`.
    pub const fn floor(self) -> &'static str {
        match self {
            Self::Native => NATIVE_TREASURY_FLOOR,
            Self::PathUsd => PATH_USD_TREASURY_FLOOR,
        }
    }
}

/// Why the probe has no answer. Every one is a 503: a balance we cannot read is
/// not a balance of zero, and the wallet routes 5xx as transient.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeFailure {
    /// No RPC answered the read.
    RpcUnavailable,
    /// An RPC answered with something that is not a balance.
    InvalidBalance,
    /// The configured treasury cannot be encoded into a `balanceOf` call.
    /// (The native read passes the address through to the node, as it always
    /// has.)
    InvalidAddress,
}

impl ProbeFailure {
    /// The `error` text both shells return with the 503.
    pub const fn message(self) -> &'static str {
        match self {
            Self::RpcUnavailable => "treasury RPC is unavailable",
            Self::InvalidBalance => "treasury RPC returned an invalid balance",
            Self::InvalidAddress => "settlement recipient is not a valid address",
        }
    }
}

/// The one JSON-RPC read the shell performs for the probe.
#[derive(Clone, Debug, PartialEq)]
pub struct BalanceRead {
    pub method: &'static str,
    pub params: Value,
}

/// What to read for `treasury` on `chain_id`: its native balance, or — on
/// Tempo — its pathUSD `balanceOf`. Never `eth_getBalance` on Tempo, whose
/// answer there is a placeholder, not money.
pub fn balance_read(chain_id: u64, treasury: &str) -> Result<BalanceRead, ProbeFailure> {
    match TreasuryAsset::for_chain(chain_id) {
        TreasuryAsset::Native => Ok(BalanceRead {
            method: "eth_getBalance",
            params: json!([treasury, "latest"]),
        }),
        TreasuryAsset::PathUsd => {
            let treasury = crate::quote::address(treasury).ok_or(ProbeFailure::InvalidAddress)?;
            let calldata = tempo::path_usd_balance_calldata(treasury.into());
            Ok(BalanceRead {
                method: "eth_call",
                params: json!([
                    {
                        "to": tempo::PATH_USD.to_string(),
                        "data": crate::quote::bytes_to_hex(&calldata),
                    },
                    "latest",
                ]),
            })
        }
    }
}

/// The probe's answer, exactly as both shells serialize it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreasuryStatus {
    pub chain_id: u64,
    pub address: String,
    pub asset: TreasuryAsset,
    /// In the asset's base units (wei, or micro-pathUSD), as a `QUANTITY`.
    pub balance: String,
    /// Same units as `balance`.
    pub floor: &'static str,
    pub bootstrap_needed: bool,
}

/// The verdict on the RPC result of [`balance_read`] for the same chain.
pub fn treasury_status(
    chain_id: u64,
    treasury: &str,
    result: &Value,
) -> Result<TreasuryStatus, ProbeFailure> {
    let asset = TreasuryAsset::for_chain(chain_id);
    let result = result.as_str().ok_or(ProbeFailure::InvalidBalance)?;
    let balance = match asset {
        TreasuryAsset::Native => parse_quantity(result),
        TreasuryAsset::PathUsd => parse_abi_uint256(result),
    }
    .map_err(|()| ProbeFailure::InvalidBalance)?;
    let floor = asset.floor();

    Ok(TreasuryStatus {
        chain_id,
        address: treasury.to_owned(),
        asset,
        bootstrap_needed: quantity_is_below(&balance, floor),
        balance,
        floor,
    })
}

/// Validate and lowercase an RPC `QUANTITY`. `Err(())` for anything that is not
/// one — a balance we cannot read is not a balance of zero.
pub fn parse_quantity(value: &str) -> Result<String, ()> {
    let digits = value.strip_prefix("0x").ok_or(())?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("0x{}", digits.to_ascii_lowercase()))
        .ok_or(())
}

/// An `eth_call` return of exactly one ABI `uint256` word, as a `QUANTITY`.
/// Anything else — notably the empty `0x` a node returns when no contract
/// answers — is unreadable, never zero.
fn parse_abi_uint256(value: &str) -> Result<String, ()> {
    let digits = value.strip_prefix("0x").ok_or(())?;
    let word = hex::decode(digits).map_err(|_| ())?;
    crate::quote::bytes32_quantity(&word).ok_or(())
}

/// Hex comparison without parsing into a fixed-width integer: a treasury
/// balance can exceed `u128` on a chain with a cheap coin, and a saturating
/// parse would read "very rich" as "at the floor".
pub fn quantity_is_below(value: &str, floor: &str) -> bool {
    let value = value.trim_start_matches("0x").trim_start_matches('0');
    let floor = floor.trim_start_matches("0x").trim_start_matches('0');

    value.len() < floor.len() || (value.len() == floor.len() && value < floor)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use serde_json::{Value, json};

    use super::{
        BalanceRead, NATIVE_TREASURY_FLOOR, PATH_USD_TOP_UP_GAS_HEADROOM, PATH_USD_TREASURY_FLOOR,
        PATH_USD_TREASURY_FLOOR_MICRO, ProbeFailure, TreasuryAsset, balance_read, parse_quantity,
        quantity_is_below, treasury_status,
    };
    use crate::tempo;

    const TREASURY: &str = "0x3e59292e18417f814112f731e7163534c6d2fe3c";
    const TEMPO: u64 = 4_217;
    const MODERATO: u64 = 42_431;
    const X_LAYER: u64 = 196;
    /// What Tempo mainnet's `eth_getBalance` answered for the treasury on
    /// 2026-10-03 while it held no pathUSD at all.
    const TEMPO_NATIVE_PLACEHOLDER: &str =
        "0x9612084f0316e0ebd5182f398e5195a51b5ca47667d4c9b26c9b26c9b26c9b2";

    fn word(value: u128) -> Value {
        json!(format!("0x{value:064x}"))
    }

    #[test]
    fn validates_and_normalizes_rpc_quantities() {
        assert_eq!(parse_quantity("0x000F"), Ok("0x000f".into()));
        assert!(parse_quantity("0x").is_err());
        assert!(parse_quantity("15").is_err());
    }

    #[test]
    fn compares_arbitrary_size_hex_balances_against_the_floor() {
        assert!(quantity_is_below("0x0", NATIVE_TREASURY_FLOOR));
        assert!(quantity_is_below("0x5af3107a3fff", NATIVE_TREASURY_FLOOR));
        assert!(!quantity_is_below("0x5af3107a4000", NATIVE_TREASURY_FLOOR));
        assert!(!quantity_is_below(
            "0x10000000000000000",
            NATIVE_TREASURY_FLOOR
        ));
    }

    /// The case that sent a person to a spinner: a treasury with nothing in it
    /// on a chain the relay otherwise serves.
    #[test]
    fn an_empty_treasury_is_below_the_floor() {
        assert!(quantity_is_below("0x0", NATIVE_TREASURY_FLOOR));
        assert!(quantity_is_below("0x00", NATIVE_TREASURY_FLOOR));
    }

    #[test]
    fn tempo_pays_gas_in_path_usd_and_every_other_chain_in_its_native_coin() {
        assert_eq!(TreasuryAsset::for_chain(TEMPO), TreasuryAsset::PathUsd);
        assert_eq!(TreasuryAsset::for_chain(MODERATO), TreasuryAsset::PathUsd);
        for chain_id in [1, 10, 56, 100, X_LAYER, 8_453, 42_161] {
            assert_eq!(TreasuryAsset::for_chain(chain_id), TreasuryAsset::Native);
        }
        assert_eq!(TreasuryAsset::PathUsd.as_str(), "pathUSD");
        assert_eq!(TreasuryAsset::Native.as_str(), "native");
        assert_eq!(json!(TreasuryAsset::PathUsd), json!("pathUSD"));
        assert_eq!(json!(TreasuryAsset::Native), json!("native"));
    }

    /// The bug, structurally: on Tempo the probe never asks for the native
    /// balance at all, so its placeholder cannot reach the verdict.
    #[test]
    fn tempo_reads_the_treasurys_path_usd_balance_never_eth_get_balance() {
        for chain_id in [TEMPO, MODERATO] {
            assert_eq!(
                balance_read(chain_id, TREASURY),
                Ok(BalanceRead {
                    method: "eth_call",
                    params: json!([
                        {
                            "to": "0x20C0000000000000000000000000000000000000",
                            "data": "0x70a082310000000000000000000000003e59292e18417f814112f731e7163534c6d2fe3c",
                        },
                        "latest",
                    ]),
                })
            );
        }
    }

    #[test]
    fn every_other_chain_keeps_the_native_read() {
        assert_eq!(
            balance_read(X_LAYER, TREASURY),
            Ok(BalanceRead {
                method: "eth_getBalance",
                params: json!([TREASURY, "latest"]),
            })
        );
    }

    /// Production on 2026-10-03, as it is answered now.
    #[test]
    fn an_empty_tempo_treasury_needs_bootstrap_in_path_usd() {
        let status = treasury_status(TEMPO, TREASURY, &word(0)).unwrap();

        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({
                "chainId": 4217,
                "address": TREASURY,
                "asset": "pathUSD",
                "balance": "0x0",
                "floor": "0x86470",
                "bootstrapNeeded": true,
            })
        );
    }

    #[test]
    fn the_native_placeholder_is_never_a_tempo_balance() {
        // Not an ABI word (63 hex digits), so it cannot be read as one either.
        assert_eq!(
            treasury_status(TEMPO, TREASURY, &json!(TEMPO_NATIVE_PLACEHOLDER)),
            Err(ProbeFailure::InvalidBalance)
        );
    }

    #[test]
    fn a_tempo_treasury_is_healthy_from_the_path_usd_floor() {
        let below = treasury_status(TEMPO, TREASURY, &word(PATH_USD_TREASURY_FLOOR_MICRO - 1));
        let at = treasury_status(TEMPO, TREASURY, &word(PATH_USD_TREASURY_FLOOR_MICRO));
        let rich = treasury_status(TEMPO, TREASURY, &word(u128::MAX));

        assert!(below.unwrap().bootstrap_needed);
        let at = at.unwrap();
        assert!(!at.bootstrap_needed);
        assert_eq!(at.balance, "0x86470");
        assert_eq!(at.floor, PATH_USD_TREASURY_FLOOR);
        assert!(!rich.unwrap().bootstrap_needed);
    }

    #[test]
    fn an_unreadable_path_usd_balance_is_a_failure_never_zero() {
        for result in [
            json!("0x"), // no contract answered
            json!("0x00"),
            json!(format!("0x{}", "0".repeat(66))),
            json!(format!("0x{}", "g".repeat(64))),
            json!("0".repeat(64)),
            json!(null),
            json!(0),
        ] {
            assert_eq!(
                treasury_status(TEMPO, TREASURY, &result),
                Err(ProbeFailure::InvalidBalance),
                "{result}"
            );
        }
    }

    #[test]
    fn the_native_answer_is_unchanged() {
        let status = treasury_status(X_LAYER, TREASURY, &json!("0x0")).unwrap();
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({
                "chainId": 196,
                "address": TREASURY,
                "asset": "native",
                "balance": "0x0",
                "floor": "0x5af3107a4000",
                "bootstrapNeeded": true,
            })
        );

        let funded = treasury_status(1, TREASURY, &json!("0x5AF3107A4000")).unwrap();
        assert_eq!(funded.balance, "0x5af3107a4000");
        assert!(!funded.bootstrap_needed);
        assert_eq!(
            treasury_status(1, TREASURY, &json!("0x")),
            Err(ProbeFailure::InvalidBalance)
        );
    }

    #[test]
    fn a_tempo_treasury_that_is_not_an_address_cannot_be_probed() {
        assert_eq!(
            balance_read(TEMPO, "treasury"),
            Err(ProbeFailure::InvalidAddress)
        );
        // The native read passes the value to the node, as it always has.
        assert!(balance_read(X_LAYER, "treasury").is_ok());
    }

    /// The floor is the executor's own requirement for funding an empty
    /// relayer, computed with the executor's functions — so a probe that says
    /// "healthy" means the next top-up can actually be signed.
    #[test]
    fn the_path_usd_floor_covers_one_top_up_of_an_empty_relayer() {
        assert_eq!(
            PATH_USD_TREASURY_FLOOR,
            format!("0x{PATH_USD_TREASURY_FLOOR_MICRO:x}")
        );
        assert_eq!(PATH_USD_TREASURY_FLOOR_MICRO, 550_000);

        // The worst measured case: a fresh treasury funding a fresh relayer.
        const WORST_TOP_UP_GAS: u64 = 600_000;
        let outer_max_fee =
            tempo::tempo_outer_max_fee(U256::from(tempo::TEMPO_BASE_FEE_ATTO)).unwrap();
        let gas_limit = tempo::buffered_top_up_gas_limit(WORST_TOP_UP_GAS).unwrap();
        let gas_cost =
            tempo::tempo_cost_in_path_usd(U256::from(gas_limit), U256::from(outer_max_fee))
                .unwrap();
        assert_eq!(gas_cost, U256::from(21_600u64));
        assert!(gas_cost <= U256::from(PATH_USD_TOP_UP_GAS_HEADROOM));

        // `ensure_tempo_funding`'s requirement for an empty relayer.
        let top_up = crate::funding::plan_tempo_top_up(U256::ZERO, U256::ZERO)
            .unwrap()
            .unwrap();
        let required = top_up + gas_cost + U256::from(tempo::TEMPO_TREASURY_FLOOR);
        assert!(required <= U256::from(PATH_USD_TREASURY_FLOOR_MICRO));
    }
}
