use std::{collections::HashSet, str::FromStr};

use alloy::primitives::{Address, B256, U256, keccak256};
use serde_json::Value;

use crate::task::UserOperationEvent;

pub fn receipt_succeeded(receipt: &Value) -> Option<bool> {
    let status = receipt.get("status")?.as_str()?;
    let status = parse_u256(status)?;
    if status > U256::from(1) {
        return None;
    }
    Some(status == U256::from(1))
}

/// Parses only EntryPoint logs that belong to the persisted bundle. A malicious contract can emit
/// a byte-identical event signature, so checking both emitter and membership is mandatory.
pub fn user_operation_events(
    receipt: &Value,
    entry_point: Address,
    membership: &[String],
) -> Vec<UserOperationEvent> {
    let membership = membership
        .iter()
        .filter_map(|hash| B256::from_str(hash).ok())
        .collect::<HashSet<_>>();
    let signature =
        keccak256(b"UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)");

    receipt
        .get("logs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|log| {
            let address = Address::from_str(log.get("address")?.as_str()?).ok()?;
            if address != entry_point {
                return None;
            }
            let topics = log.get("topics")?.as_array()?;
            if B256::from_str(topics.first()?.as_str()?).ok()? != signature {
                return None;
            }
            let hash = B256::from_str(topics.get(1)?.as_str()?).ok()?;
            if !membership.contains(&hash) {
                return None;
            }
            let data = parse_bytes(log.get("data")?.as_str()?)?;
            let success = parse_word(&data, 1)?;
            if success > U256::from(1) {
                return None;
            }
            Some(UserOperationEvent {
                user_operation_hash: hash.to_string(),
                success: success == U256::from(1),
                actual_gas_cost: quantity(parse_word(&data, 2)?),
                actual_gas_used: quantity(parse_word(&data, 3)?),
            })
        })
        .collect()
}

/// A mined bundle's gas against the gas its operations were billed for — the
/// one number that says whether used-gas billing (`docs/fees.md` §1a) held.
///
/// The relay is paid at least `markup × billed gas × cap` and charged `gas
/// used × effective gas price`, never more than the cap. Where it bills the
/// outer limit the billed gas is at least the gas any execution can use; where
/// it bills measured gas it is the simulation plus 15% and 30,000, which a
/// bundle whose gas depends on when it runs can exceed. Both shells log this
/// for every receipt — a warning when the bundle used more than it was billed
/// for — so the residual risk is watched, not assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleBilling {
    pub billed_gas: U256,
    pub gas_used: U256,
    /// `gas_used / billed_gas`, in basis points, saturating.
    pub used_over_billed_bps: u64,
    /// `billed_gas × cap`: the requirement before the markup.
    pub billed_at_cap: U256,
    /// `gas_used × effectiveGasPrice`: what the chain charged, when the
    /// receipt reports the price.
    pub charged: Option<U256>,
    /// The bundle burned more gas than its operations were billed for.
    pub under_billed: bool,
}

/// [`BundleBilling`] of a receipt for a bundle whose intent recorded what it
/// billed; `None` for an intent that did not, or a receipt without `gasUsed`.
pub fn bundle_billing(
    intent: &crate::task::PreparedBundleIntent,
    receipt: &Value,
) -> Option<BundleBilling> {
    let billed_gas = parse_u256(intent.billed_gas.as_deref()?)?;
    let billed_fee_per_gas = parse_u256(intent.billed_fee_per_gas.as_deref()?)?;
    let gas_used = parse_u256(receipt.get("gasUsed")?.as_str()?)?;
    let charged = receipt
        .get("effectiveGasPrice")
        .and_then(Value::as_str)
        .and_then(parse_u256)
        .and_then(|price| gas_used.checked_mul(price));
    let used_over_billed_bps = if billed_gas.is_zero() {
        u64::MAX
    } else {
        gas_used
            .saturating_mul(U256::from(10_000u64))
            .checked_div(billed_gas)
            .map_or(u64::MAX, |ratio| u64::try_from(ratio).unwrap_or(u64::MAX))
    };
    Some(BundleBilling {
        billed_gas,
        gas_used,
        used_over_billed_bps,
        billed_at_cap: billed_gas.saturating_mul(billed_fee_per_gas),
        charged,
        under_billed: gas_used > billed_gas,
    })
}

fn quantity(value: U256) -> String {
    format!("0x{value:x}")
}

fn parse_word(data: &[u8], index: usize) -> Option<U256> {
    let start = index.checked_mul(32)?;
    let word: [u8; 32] = data.get(start..start + 32)?.try_into().ok()?;
    Some(U256::from_be_bytes(word))
}

fn parse_u256(value: &str) -> Option<U256> {
    U256::from_str_radix(value.strip_prefix("0x")?, 16).ok()
}

fn parse_bytes(value: &str) -> Option<Vec<u8>> {
    let value = value.strip_prefix("0x")?;
    if !value.len().is_multiple_of(2) {
        return None;
    }
    hex::decode(value).ok()
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{U256, address, b256, keccak256};
    use serde_json::json;

    use super::{BundleBilling, bundle_billing, user_operation_events};

    /// The 2026-10-02 send: billed 198,848 gas at its signed cap, mined using
    /// 146,824 at an effective 0.2033 gwei — 74% of what it was billed for.
    #[test]
    fn a_receipt_is_checked_against_the_gas_its_bundle_was_billed_for() {
        let intent = |billed: Option<&str>| crate::task::PreparedBundleIntent {
            chain_id: 1,
            lane: 0,
            entry_point: "0x0000000071727De22E5E9d8BAf0edAc6f37da032".into(),
            raw_transaction: "0x02".into(),
            transaction_hash: "0x7132ee31".into(),
            nonce: 1,
            user_operation_hashes: vec!["0x01".into()],
            billed_gas: billed.map(Into::into),
            billed_fee_per_gas: Some(format!("0x{:x}", 401_021_487u64)),
        };
        let receipt = json!({
            "status": "0x1",
            "gasUsed": format!("0x{:x}", 146_824),
            "effectiveGasPrice": format!("0x{:x}", 203_300_000u64),
        });
        assert_eq!(
            bundle_billing(&intent(Some(&format!("0x{:x}", 198_848))), &receipt),
            Some(BundleBilling {
                billed_gas: U256::from(198_848u64),
                gas_used: U256::from(146_824u64),
                used_over_billed_bps: 7_383,
                billed_at_cap: U256::from(198_848u64 * 401_021_487),
                charged: Some(U256::from(146_824u64 * 203_300_000)),
                under_billed: false,
            })
        );
        // A bundle that burned more than it was billed for is flagged.
        let over = bundle_billing(&intent(Some(&format!("0x{:x}", 140_000))), &receipt).unwrap();
        assert!(over.under_billed);
        assert_eq!(over.used_over_billed_bps, 10_487);
        // An intent written before the billing was recorded says nothing.
        assert_eq!(bundle_billing(&intent(None), &receipt), None);
        // The new fields are optional on the wire both ways.
        let old: crate::task::PreparedBundleIntent = serde_json::from_value(json!({
            "chainId": 1, "lane": 0, "entryPoint": "0x00", "rawTransaction": "0x02",
            "transactionHash": "0x7132ee31", "nonce": 1, "userOperationHashes": ["0x01"],
        }))
        .unwrap();
        assert_eq!(old.billed_gas, None);
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("billedGas")
                .is_none()
        );
    }

    #[test]
    fn filters_event_emitter_and_persisted_membership() {
        let entry_point = address!("1111111111111111111111111111111111111111");
        let included = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let outsider = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let signature =
            keccak256(b"UserOperationEvent(bytes32,address,address,uint256,bool,uint256,uint256)");
        let data = format!("0x{:064x}{:064x}{:064x}{:064x}", 0, 1, 20, 10);
        let receipt = json!({
            "logs": [
                {"address": entry_point, "topics": [signature, included], "data": data},
                {"address": "0x2222222222222222222222222222222222222222", "topics": [signature, included], "data": data},
                {"address": entry_point, "topics": [signature, outsider], "data": data}
            ]
        });

        let events = user_operation_events(&receipt, entry_point, &[included.to_string()]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].user_operation_hash, included.to_string());
    }
}
