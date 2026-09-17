//! The relay treasury's public status — the answer to "can this relay actually
//! pay gas on this chain right now?".
//!
//! The wallet asks before it lets anyone sign, because the alternative is what
//! it used to do when the answer was unavailable: accept the operation, show
//! "submitted", and leave the person watching a spinner for a payment that can
//! never land. An empty treasury is a fact the relay knows; the only failure is
//! not saying so.
//!
//! These are the pure parts, shared by both shells (docker and Cloudflare), so
//! the floor a person is warned about is the same number on both.

/// 0.0001 native coin — the operator float below which the relay cannot fund a
/// relayer and needs a direct, NON-REFUNDABLE bootstrap deposit. Mirrors
/// `DEFAULT_TREASURY_FLOOR_WEI` in the docker shell's config.
pub const NATIVE_TREASURY_FLOOR: &str = "0x5af3107a4000";

/// Validate and lowercase an RPC `QUANTITY`. `Err(())` for anything that is not
/// one — a balance we cannot read is not a balance of zero.
pub fn parse_quantity(value: &str) -> Result<String, ()> {
    let digits = value.strip_prefix("0x").ok_or(())?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("0x{}", digits.to_ascii_lowercase()))
        .ok_or(())
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
    use super::{NATIVE_TREASURY_FLOOR, parse_quantity, quantity_is_below};

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
        assert!(!quantity_is_below("0x10000000000000000", NATIVE_TREASURY_FLOOR));
    }

    /// The case that sent a person to a spinner: a treasury with nothing in it
    /// on a chain the relay otherwise serves.
    #[test]
    fn an_empty_treasury_is_below_the_floor() {
        assert!(quantity_is_below("0x0", NATIVE_TREASURY_FLOOR));
        assert!(quantity_is_below("0x00", NATIVE_TREASURY_FLOOR));
    }
}
