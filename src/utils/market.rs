use reqwest::Client;
use serde::Deserialize;

pub const BINANCE_TICKER_URLS: [&str; 3] = [
    "https://api.binance.com/api/v3/ticker/price?symbol=",
    "https://data-api.binance.vision/api/v3/ticker/price?symbol=",
    "https://api.binance.us/api/v3/ticker/price?symbol=",
];

#[derive(Deserialize)]
struct BinanceTicker {
    price: String,
}

/// Retrieves a native asset's USD price from Binance's public ticker endpoints.
///
/// `api.binance.com` is preferred. The public data endpoint and Binance.US provide independent
/// routing paths for hosts where the primary domain is geo-blocked or unavailable. Prices are
/// intentionally accepted only when they are positive decimal values.
pub async fn binance_usdt_price(client: &Client, symbol: &str) -> Option<String> {
    for endpoint in BINANCE_TICKER_URLS {
        let url = format!("{endpoint}{symbol}USDT");
        let Ok(response) = client.get(url).send().await else {
            continue;
        };
        let Ok(response) = response.error_for_status() else {
            continue;
        };
        let Ok(ticker) = response.json::<BinanceTicker>().await else {
            continue;
        };
        if valid_positive_decimal(&ticker.price) {
            return Some(ticker.price);
        }
    }
    None
}

fn valid_positive_decimal(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && value.bytes().filter(|byte| *byte == b'.').count() <= 1
        && value
            .bytes()
            .any(|byte| byte.is_ascii_digit() && byte != b'0')
}

#[cfg(test)]
mod tests {
    use super::valid_positive_decimal;
    use vela_relay_core::settlement::{
        ARC_CHAIN_ID, ARC_TESTNET_CHAIN_ID, GNOSIS_CHAIN_ID, STABLE_CHAIN_ID, is_gnosis_chain,
        pegged_native_usd_price,
    };

    #[test]
    fn accepts_only_positive_decimal_market_prices() {
        assert!(valid_positive_decimal("570.60"));
        assert!(valid_positive_decimal("0.00001"));
        assert!(!valid_positive_decimal("0"));
        assert!(!valid_positive_decimal("-1"));
        assert!(!valid_positive_decimal("1.2.3"));
    }

    #[test]
    fn identifies_gnosis_mainnet() {
        assert!(is_gnosis_chain(GNOSIS_CHAIN_ID));
        assert!(!is_gnosis_chain(1));
    }

    /// The quote handler no longer asks "is this Gnosis" — it asks whether the
    /// chain's native coin is a dollar by construction, which is now true of
    /// Arc as well (its native coin IS USDC). A market ticker must never be
    /// what decides a value the protocol defines.
    #[test]
    fn prices_every_dollar_native_chain_without_the_market() {
        for chain_id in [GNOSIS_CHAIN_ID, ARC_CHAIN_ID, ARC_TESTNET_CHAIN_ID, STABLE_CHAIN_ID] {
            assert!(
                pegged_native_usd_price(chain_id).is_some(),
                "chain {chain_id} must be pegged, not quoted"
            );
        }
        for chain_id in [1, 10, 56, 137, 8453, 42_161, 4_217] {
            assert!(
                pegged_native_usd_price(chain_id).is_none(),
                "chain {chain_id} must consult the market"
            );
        }
    }
}
