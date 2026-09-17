//! The relay's public view of one Safe on one chain: can it operate, and where
//! is its nonce.
//!
//! The rules here are shared by both shells (docker and Cloudflare) because a
//! wallet that asks two deployments the same question must get the same answer.
//! The shells own the RPC and the JSON; this module owns the grammar and the
//! verdict.

/// The EntryPoint's `getNonce(address,uint192)` selector.
const ENTRY_POINT_NONCE_SELECTOR: &str = "35567e1a";

/// What the relay can say about an account without guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    InsufficientBalance,
    /// A pending nonce ahead of the latest one: an operation is in flight and
    /// this account's next nonce is not knowable yet. Not an error — a state.
    LockedPendingUnknown,
}

impl AccountStatus {
    /// The wire spelling both shells emit.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::InsufficientBalance => "INSUFFICIENT_BALANCE",
            Self::LockedPendingUnknown => "LOCKED_PENDING_UNKNOWN",
        }
    }
}

/// Lowercase a 20-byte hex address, or refuse. `None` is a 400, never a
/// silently-normalized guess.
pub fn normalize_address(value: &str) -> Option<String> {
    let valid = value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit());
    valid.then(|| value.to_ascii_lowercase())
}

/// An RPC `QUANTITY` as a `u64` nonce.
pub fn parse_nonce(quantity: &str) -> Result<u64, ()> {
    let quantity = crate::treasury::parse_quantity(quantity)?;
    u64::from_str_radix(&quantity[2..], 16).map_err(|_| ())
}

/// `getNonce(safe, 0)` calldata. The address must already be normalized.
pub fn entry_point_nonce_calldata(safe_address: &str) -> Option<String> {
    let address = safe_address.strip_prefix("0x")?;
    Some(format!(
        "0x{ENTRY_POINT_NONCE_SELECTOR}{address:0>64}{:0>64}",
        ""
    ))
}

/// The verdict. A pending nonce ahead of latest wins over an empty balance:
/// the in-flight operation is the more specific fact about what happens next.
pub fn account_status(balance: &str, latest_nonce: u64, pending_nonce: u64) -> AccountStatus {
    if pending_nonce > latest_nonce {
        AccountStatus::LockedPendingUnknown
    } else if balance
        .trim_start_matches("0x")
        .bytes()
        .all(|byte| byte == b'0')
    {
        AccountStatus::InsufficientBalance
    } else {
        AccountStatus::Active
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccountStatus, account_status, entry_point_nonce_calldata, normalize_address, parse_nonce,
    };

    #[test]
    fn normalizes_only_real_addresses() {
        assert_eq!(
            normalize_address("0x14FB1FB21751E29F7EC48DC450017552E3D1EA5C"),
            Some("0x14fb1fb21751e29f7ec48dc450017552e3d1ea5c".into())
        );
        assert_eq!(normalize_address("0x1234"), None);
        assert_eq!(normalize_address("14fb1fb21751e29f7ec48dc450017552e3d1ea5c"), None);
    }

    #[test]
    fn builds_the_entry_point_nonce_call() {
        assert_eq!(
            entry_point_nonce_calldata("0x14fb1fb21751e29f7ec48dc450017552e3d1ea5c").unwrap(),
            "0x35567e1a00000000000000000000000014fb1fb21751e29f7ec48dc450017552e3d1ea5c0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn parses_nonces_and_refuses_anything_that_is_not_a_quantity() {
        assert_eq!(parse_nonce("0x2c"), Ok(44));
        assert!(parse_nonce("44").is_err());
        assert!(parse_nonce("0x").is_err());
    }

    #[test]
    fn derives_status_from_balance_and_nonce_state() {
        assert_eq!(account_status("0x1", 44, 44), AccountStatus::Active);
        assert_eq!(
            account_status("0x0", 44, 44),
            AccountStatus::InsufficientBalance
        );
        assert_eq!(
            account_status("0x1", 44, 45),
            AccountStatus::LockedPendingUnknown
        );
        // An in-flight operation is the more specific fact.
        assert_eq!(
            account_status("0x0", 44, 45),
            AccountStatus::LockedPendingUnknown
        );
    }

    #[test]
    fn the_wire_spellings_are_the_ones_the_wallet_reads() {
        assert_eq!(AccountStatus::Active.as_str(), "ACTIVE");
        assert_eq!(
            AccountStatus::InsufficientBalance.as_str(),
            "INSUFFICIENT_BALANCE"
        );
        assert_eq!(
            AccountStatus::LockedPendingUnknown.as_str(),
            "LOCKED_PENDING_UNKNOWN"
        );
    }
}
