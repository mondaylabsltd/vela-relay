//! In-band gas quote computation: Multicall3 encoding/decoding, quote
//! assembly, exact-decimal USD ordering, and symbol/price validation. Moved
//! from the docker shell's quote handler (spec 002) so both shells produce
//! identical quote bodies from identical chain data. Pure — the shells fetch
//! (Multicall eth_call, Binance price) and hand results in as data.

use num_bigint::BigUint;

use crate::settlement::{
    MIN_NATIVE_FRACTION_DECIMALS, MIN_STABLE_FRACTION_DECIMALS, minimum_amount,
};
use crate::wire::{InBandGasQuote, InBandGasQuoteAsset};

pub const MULTICALL3_ADDRESS: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
const AGGREGATE3_SELECTOR: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
const GET_ETH_BALANCE_SELECTOR: [u8; 4] = [0x4d, 0x23, 0x01, 0xcc];
const ERC20_BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
const ERC20_DECIMALS_SELECTOR: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];

/// A stablecoin candidate for the quote (symbol + contract from the chain
/// directory), already filtered to valid addresses by the shell.
#[derive(Clone, Debug)]
pub struct QuoteStable {
    pub symbol: String,
    pub contract: String,
}

pub struct MulticallCall {
    pub target: [u8; 20],
    pub call_data: Vec<u8>,
}

pub struct MulticallResult {
    pub success: bool,
    pub return_data: Vec<u8>,
}

pub fn multicall_requests(
    safe_address: [u8; 20],
    stablecoins: &[QuoteStable],
) -> Vec<MulticallCall> {
    let mut calls = Vec::with_capacity(1 + stablecoins.len() * 2);
    calls.push(MulticallCall {
        target: address(MULTICALL3_ADDRESS).expect("Multicall3 address is valid"),
        call_data: [
            GET_ETH_BALANCE_SELECTOR.to_vec(),
            address_word(safe_address).to_vec(),
        ]
        .concat(),
    });

    for stablecoin in stablecoins {
        let Some(target) = address(&stablecoin.contract) else {
            continue;
        };
        calls.push(MulticallCall {
            target,
            call_data: ERC20_DECIMALS_SELECTOR.to_vec(),
        });
        calls.push(MulticallCall {
            target,
            call_data: [
                ERC20_BALANCE_OF_SELECTOR.to_vec(),
                address_word(safe_address).to_vec(),
            ]
            .concat(),
        });
    }

    calls
}

/// Assembles the quote list from decoded Multicall results, exactly as the
/// docker handler did. `Err(())` maps to the frozen quote-unavailable error.
#[expect(
    clippy::result_unit_err,
    reason = "The unit error deliberately carries no detail: every failure maps to the single frozen quote-unavailable response, mirroring the docker handler."
)]
pub fn quotes_from_multicall(
    native_decimals: u32,
    native_symbol: &str,
    recipient: &str,
    native_price: Option<String>,
    stablecoins: &[QuoteStable],
    values: Vec<MulticallResult>,
) -> Result<Vec<InBandGasQuote>, ()> {
    let native = values
        .first()
        .filter(|result| result.success)
        .and_then(|result| bytes32_quantity(&result.return_data))
        .ok_or(())?;
    minimum_native_amount(native_decimals).ok_or(())?;
    let native_usd_balance =
        usd_balance_from_values(&native, native_decimals, native_price.as_deref());
    // `$0.01` of the coin at the price this quote reports, which is the price
    // a wallet reads its own minimum from; the safety floor without one.
    let native_minimum = crate::settlement::published_native_minimum(
        native_decimals,
        native_price
            .as_deref()
            .and_then(crate::settlement::parse_market_usd_price),
    )
    .map_err(|_| ())?;
    let mut quotes = vec![InBandGasQuote {
        recipient: recipient.into(),
        asset: InBandGasQuoteAsset::Native,
        fee_token: None,
        decimals: native_decimals,
        symbol: native_symbol.to_owned(),
        balance: native,
        usd_price: native_price.clone(),
        usd_balance: native_usd_balance,
        minimum_amount: format!("0x{native_minimum:x}"),
    }];

    for (index, stablecoin) in stablecoins.iter().enumerate() {
        let decimals_result = values.get(1 + index * 2);
        let balance_result = values.get(2 + index * 2);
        let Some(decimals) = decimals_result
            .filter(|result| result.success)
            .and_then(|result| bytes32_u32(&result.return_data))
        else {
            continue;
        };
        let Some(balance) = balance_result
            .filter(|result| result.success)
            .and_then(|result| bytes32_quantity(&result.return_data))
        else {
            continue;
        };
        let Some(stable_minimum) = minimum_stablecoin_amount(decimals) else {
            continue;
        };
        let usd_balance = usd_balance_from_values(&balance, decimals, Some("1"));

        quotes.push(InBandGasQuote {
            recipient: recipient.into(),
            asset: InBandGasQuoteAsset::Erc20,
            fee_token: Some(stablecoin.contract.clone()),
            decimals,
            symbol: stablecoin.symbol.clone(),
            balance,
            usd_price: Some("1".into()),
            usd_balance,
            minimum_amount: format!("0x{stable_minimum:x}"),
        });
    }

    quotes.sort_by(compare_usd_balance_descending);
    Ok(quotes)
}

/// The Tempo quote: pathUSD only, priced at one dollar by protocol.
pub fn tempo_quote(recipient: String, balance: String) -> InBandGasQuote {
    let usd_balance = usd_balance_from_values(&balance, crate::tempo::PATH_USD_DECIMALS, Some("1"));
    InBandGasQuote {
        recipient,
        asset: InBandGasQuoteAsset::Erc20,
        fee_token: Some(crate::tempo::PATH_USD.to_string()),
        decimals: crate::tempo::PATH_USD_DECIMALS,
        symbol: crate::tempo::PATH_USD_SYMBOL.into(),
        balance,
        usd_price: Some("1".into()),
        usd_balance,
        // The `$0.01` admission and settlement both require of pathUSD.
        minimum_amount: format!(
            "0x{:x}",
            10u128.pow(crate::tempo::PATH_USD_DECIMALS - MIN_STABLE_FRACTION_DECIMALS)
        ),
    }
}

fn minimum_native_amount(native_decimals: u32) -> Option<u128> {
    minimum_amount(native_decimals, MIN_NATIVE_FRACTION_DECIMALS)
        .ok()
        .and_then(|value| u128::try_from(value).ok())
}

fn minimum_stablecoin_amount(token_decimals: u32) -> Option<u128> {
    minimum_amount(token_decimals, MIN_STABLE_FRACTION_DECIMALS)
        .ok()
        .and_then(|value| u128::try_from(value).ok())
}

/// Orders quotes by the Safe's USD-denominated balance without converting
/// large on-chain values through floating point. Unpriced quotes sort last.
pub fn compare_usd_balance_descending(
    left: &InBandGasQuote,
    right: &InBandGasQuote,
) -> std::cmp::Ordering {
    match (usd_balance_key(left), usd_balance_key(right)) {
        (Some(left_usd), Some(right_usd)) => right_usd
            .cmp(&left_usd)
            .then_with(|| quote_key(left).cmp(&quote_key(right))),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => quote_key(left).cmp(&quote_key(right)),
    }
}

fn quote_key(quote: &InBandGasQuote) -> (&str, &str) {
    (
        quote.symbol.as_str(),
        quote.fee_token.as_deref().unwrap_or_default(),
    )
}

struct UsdBalance {
    numerator: BigUint,
    scale: u32,
}

impl Ord for UsdBalance {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.scale.cmp(&other.scale) {
            std::cmp::Ordering::Equal => self.numerator.cmp(&other.numerator),
            std::cmp::Ordering::Greater => self
                .numerator
                .cmp(&(other.numerator.clone() * power_of_ten(self.scale - other.scale))),
            std::cmp::Ordering::Less => (self.numerator.clone()
                * power_of_ten(other.scale - self.scale))
            .cmp(&other.numerator),
        }
    }
}

impl PartialOrd for UsdBalance {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for UsdBalance {}

impl PartialEq for UsdBalance {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl UsdBalance {
    fn to_decimal_string(&self) -> String {
        let mut digits = self.numerator.to_str_radix(10);
        if self.scale == 0 {
            return digits;
        }

        let scale = self.scale as usize;
        if digits.len() <= scale {
            digits = format!("{digits:0>width$}", width = scale + 1);
        }
        let decimal_position = digits.len() - scale;
        digits.insert(decimal_position, '.');
        digits
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

pub fn usd_balance_from_values(
    balance: &str,
    decimals: u32,
    usd_price: Option<&str>,
) -> Option<String> {
    let balance = BigUint::parse_bytes(balance.strip_prefix("0x")?.as_bytes(), 16)?;
    let (price, price_scale) = decimal_price(usd_price?)?;
    let scale = decimals.checked_add(price_scale)?;

    Some(UsdBalance {
        numerator: balance * price,
        scale,
    })
    .map(|balance| balance.to_decimal_string())
}

fn usd_balance_key(quote: &InBandGasQuote) -> Option<UsdBalance> {
    let (numerator, scale) = decimal_price(quote.usd_balance.as_deref()?)?;

    Some(UsdBalance { numerator, scale })
}

fn decimal_price(value: &str) -> Option<(BigUint, u32)> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let digits = format!("{whole}{fraction}");
    let price = BigUint::parse_bytes(digits.as_bytes(), 10)?;
    let scale = u32::try_from(fraction.len()).ok()?;

    Some((price, scale))
}

fn power_of_ten(exponent: u32) -> BigUint {
    BigUint::from(10_u8).pow(exponent)
}

pub fn is_usd_stablecoin(symbol: &str) -> bool {
    let symbol = symbol
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();

    symbol.starts_with("USD")
        || symbol.ends_with("USD")
        || matches!(
            symbol.as_str(),
            "DAI" | "FRAX" | "GHO" | "LUSD" | "MIM" | "DOLA" | "CRVUSD" | "USDE" | "USDS"
        )
}

pub fn normalize_usd_price(value: &str) -> Option<String> {
    let value = value.trim();
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && value.bytes().filter(|byte| *byte == b'.').count() <= 1
        && value
            .bytes()
            .any(|byte| byte.is_ascii_digit() && byte != b'0');
    valid.then(|| value.into())
}

pub fn encode_aggregate3(calls: &[MulticallCall]) -> Vec<u8> {
    let encoded_calls = calls.iter().map(encode_aggregate3_call).collect::<Vec<_>>();
    let mut output = AGGREGATE3_SELECTOR.to_vec();
    output.extend(usize_word(32));
    output.extend(usize_word(calls.len()));

    let mut offset = calls.len() * 32;
    for call in &encoded_calls {
        output.extend(usize_word(offset));
        offset += call.len();
    }

    for call in encoded_calls {
        output.extend(call);
    }
    output
}

fn encode_aggregate3_call(call: &MulticallCall) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend(address_word(call.target));
    output.extend(bool_word(true));
    output.extend(usize_word(96));
    output.extend(dynamic_bytes(&call.call_data));
    output
}

#[expect(
    clippy::result_unit_err,
    reason = "The unit error deliberately carries no detail: every decode failure maps to the single frozen quote-unavailable response, mirroring the docker handler."
)]
pub fn decode_aggregate3(value: &str) -> Result<Vec<MulticallResult>, ()> {
    let decoded = || -> Option<Vec<MulticallResult>> {
        let bytes = decode_hex(value).ok()?;
        let array_start = read_usize_word(&bytes, 0)?;
        let length = read_usize_word(&bytes, array_start)?;
        let offsets_start = array_start.checked_add(32)?;
        let mut results = Vec::with_capacity(length);

        for index in 0..length {
            let offset_position = offsets_start.checked_add(index.checked_mul(32)?)?;
            let offset = read_usize_word(&bytes, offset_position)?;
            let tuple_start = offsets_start.checked_add(offset)?;
            let success = read_bool_word(&bytes, tuple_start)?;
            let data_offset = read_usize_word(&bytes, tuple_start.checked_add(32)?)?;
            let data_start = tuple_start.checked_add(data_offset)?;
            let length = read_usize_word(&bytes, data_start)?;
            let return_data_start = data_start.checked_add(32)?;
            let return_data = bytes
                .get(return_data_start..return_data_start.checked_add(length)?)?
                .to_vec();
            results.push(MulticallResult {
                success,
                return_data,
            });
        }

        Some(results)
    };

    decoded().ok_or(())
}

pub fn bytes32_quantity(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 32 {
        return None;
    }
    let digits = hex::encode(bytes).trim_start_matches('0').to_owned();
    Some(if digits.is_empty() {
        "0x0".into()
    } else {
        format!("0x{digits}")
    })
}

fn bytes32_u32(bytes: &[u8]) -> Option<u32> {
    let value = bytes32_quantity(bytes)?;
    u32::from_str_radix(&value[2..], 16).ok()
}

pub fn address(value: &str) -> Option<[u8; 20]> {
    let value = value.strip_prefix("0x")?;
    (value.len() == 40)
        .then(|| hex::decode(value).ok())
        .flatten()
        .and_then(|value| value.try_into().ok())
}

pub fn bytes_to_hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn decode_hex(value: &str) -> Result<Vec<u8>, ()> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    hex::decode(value).map_err(|_| ())
}

fn address_word(value: [u8; 20]) -> [u8; 32] {
    let mut word = [0; 32];
    word[12..].copy_from_slice(&value);
    word
}

fn bool_word(value: bool) -> [u8; 32] {
    let mut word = [0; 32];
    word[31] = u8::from(value);
    word
}

fn usize_word(value: usize) -> [u8; 32] {
    let mut word = [0; 32];
    word[24..].copy_from_slice(&(value as u64).to_be_bytes());
    word
}

fn dynamic_bytes(value: &[u8]) -> Vec<u8> {
    let mut output = usize_word(value.len()).to_vec();
    output.extend(value);
    output.resize(output.len() + (32 - value.len() % 32) % 32, 0);
    output
}

fn read_usize_word(bytes: &[u8], offset: usize) -> Option<usize> {
    let word = bytes.get(offset..offset.checked_add(32)?)?;
    if word.get(..24)?.iter().any(|byte| *byte != 0) {
        return None;
    }
    u64::from_be_bytes(word.get(24..)?.try_into().ok()?)
        .try_into()
        .ok()
}

fn read_bool_word(bytes: &[u8], offset: usize) -> Option<bool> {
    let word = bytes.get(offset..offset.checked_add(32)?)?;
    if word.get(..31)?.iter().any(|byte| *byte != 0) {
        return None;
    }
    match word[31] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::wire::{InBandGasQuote, InBandGasQuoteAsset};

    use super::{
        MulticallCall, MulticallResult, QuoteStable, bytes32_quantity,
        compare_usd_balance_descending, decode_aggregate3, encode_aggregate3, is_usd_stablecoin,
        normalize_usd_price, quotes_from_multicall, tempo_quote, usd_balance_from_values,
    };

    #[test]
    fn encodes_and_decodes_multicall3_aggregate3() {
        let encoded = encode_aggregate3(&[MulticallCall {
            target: [1; 20],
            call_data: vec![0x31, 0x3c, 0xe5, 0x67],
        }]);
        assert_eq!(&encoded[..4], &[0x82, 0xad, 0x56, 0xcb]);

        let response = concat!(
            "0x",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000006"
        );
        let decoded = decode_aggregate3(response).unwrap();
        assert!(decoded[0].success);
        assert_eq!(
            bytes32_quantity(&decoded[0].return_data),
            Some("0x6".into())
        );
    }

    #[test]
    fn includes_only_usd_stablecoins_and_valid_market_prices() {
        assert!(is_usd_stablecoin("USDC.e"));
        assert!(is_usd_stablecoin("DAI"));
        assert!(!is_usd_stablecoin("EURC"));
        assert_eq!(normalize_usd_price("3024.12"), Some("3024.12".into()));
        assert_eq!(normalize_usd_price("0"), None);
    }

    #[test]
    fn orders_quotes_by_exact_usd_balance_with_unpriced_assets_last() {
        let mut quotes = [
            quote("ETH", "0xde0b6b3a7640000", 18, Some("2000")),
            quote("USDC", "0x6fc23ac00", 6, Some("1")),
            quote("XDAI", "0x4563918244f40000", 18, None),
        ];

        quotes.sort_by(compare_usd_balance_descending);

        assert_eq!(quotes[0].symbol, "USDC");
        assert_eq!(quotes[0].usd_balance.as_deref(), Some("30000"));
        assert_eq!(quotes[1].symbol, "ETH");
        assert_eq!(quotes[1].usd_balance.as_deref(), Some("2000"));
        assert_eq!(quotes[2].symbol, "XDAI");
        assert_eq!(quotes[2].usd_balance, None);
    }

    #[test]
    fn calculates_minimum_amounts_in_smallest_units() {
        assert_eq!(super::minimum_native_amount(18), Some(1_000_000_000_000));
        assert_eq!(super::minimum_stablecoin_amount(6), Some(10_000));
        assert_eq!(
            super::minimum_stablecoin_amount(18),
            Some(10_000_000_000_000_000)
        );
    }

    fn quote(
        symbol: &str,
        balance: &str,
        decimals: u32,
        usd_price: Option<&str>,
    ) -> InBandGasQuote {
        InBandGasQuote {
            recipient: "0x0000000000000000000000000000000000000001".into(),
            asset: InBandGasQuoteAsset::Native,
            fee_token: None,
            decimals,
            symbol: symbol.into(),
            balance: balance.into(),
            usd_price: usd_price.map(String::from),
            usd_balance: usd_balance_from_values(balance, decimals, usd_price),
            minimum_amount: "0x0".into(),
        }
    }

    fn word(value: u128) -> Vec<u8> {
        let mut word = vec![0u8; 16];
        word.extend(value.to_be_bytes());
        word
    }

    fn result(value: u128) -> MulticallResult {
        MulticallResult {
            success: true,
            return_data: word(value),
        }
    }

    fn minimum_of(quotes: &[InBandGasQuote], symbol: &str) -> u128 {
        let quote = quotes.iter().find(|quote| quote.symbol == symbol).unwrap();
        u128::from_str_radix(quote.minimum_amount.trim_start_matches("0x"), 16).unwrap()
    }

    /// Every row says the least fee the relay takes in it: `$0.01` of the
    /// native coin at the price the row reports, `0.01` of a stablecoin.
    #[test]
    fn every_row_publishes_the_minimum_fee_in_its_own_units() {
        let stables = [QuoteStable {
            symbol: "USDC".into(),
            contract: "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".into(),
        }];
        let values = || vec![result(1), result(6), result(5_000_000)];

        // ETH at $2,494: $0.01 is 0.000004009623… ETH, rounded up to the wei.
        let quotes = quotes_from_multicall(
            18,
            "ETH",
            "0x0000000000000000000000000000000000000001",
            Some("2494".into()),
            &stables,
            values(),
        )
        .unwrap();
        assert_eq!(minimum_of(&quotes, "ETH"), 4_009_623_095_430);
        assert_eq!(minimum_of(&quotes, "USDC"), 10_000);
        // What the old 0.00001-coin floor charged: two and a half cents.
        assert!(minimum_of(&quotes, "ETH") < 10_000_000_000_000 / 2);

        // A dollar coin (Gnosis xDAI, Arc USDC, Stable USDT0): 0.01 of it.
        let pegged = quotes_from_multicall(
            18,
            "xDAI",
            "0x0000000000000000000000000000000000000001",
            Some("1".into()),
            &[],
            vec![result(1)],
        )
        .unwrap();
        assert_eq!(minimum_of(&pegged, "xDAI"), 10_000_000_000_000_000);

        // No price: the safety floor, 0.000001 of the coin.
        let unpriced = quotes_from_multicall(
            18,
            "MNT",
            "0x0000000000000000000000000000000000000001",
            None,
            &[],
            vec![result(1)],
        )
        .unwrap();
        assert_eq!(minimum_of(&unpriced, "MNT"), 1_000_000_000_000);

        // A coin dearer than $10,000 would floor at the safety floor, above
        // `$0.01`; none of the built-in networks has one.
        let dear = quotes_from_multicall(
            18,
            "BTC",
            "0x0000000000000000000000000000000000000001",
            Some("82551".into()),
            &[],
            vec![result(1)],
        )
        .unwrap();
        assert_eq!(minimum_of(&dear, "BTC"), 1_000_000_000_000);

        // Tempo's pathUSD.
        let tempo = tempo_quote(
            "0x0000000000000000000000000000000000000001".into(),
            "0x0".into(),
        );
        assert_eq!(tempo.minimum_amount, "0x2710");

        // The field is on the wire, beside the ones every wallet already reads.
        let json = serde_json::to_value(&quotes[0]).unwrap();
        assert!(json.get("minimumAmount").is_some(), "{json}");
        assert!(json.get("usdPrice").is_some());
    }

    /// The published minimum is accepted by the executor at the same price,
    /// and still after the coin falls 10%, but not 11%.
    #[test]
    fn the_published_minimum_is_accepted_at_settlement_within_the_allowance() {
        use crate::settlement::{
            enforced_native_minimum, parse_market_usd_price, published_native_minimum,
        };
        use alloy::primitives::U256;
        let quoted = parse_market_usd_price("2494").unwrap();
        let paid = published_native_minimum(18, Some(quoted)).unwrap();
        for (settled, accepted) in [("2494", true), ("2244.6", true), ("2219.66", false)] {
            let floor = enforced_native_minimum(18, parse_market_usd_price(settled)).unwrap();
            assert_eq!(paid >= floor, accepted, "settled at {settled}");
        }
        // No price at settlement: the safety floor, which the published
        // minimum always meets.
        assert!(paid >= enforced_native_minimum(18, None).unwrap());
        assert_eq!(
            enforced_native_minimum(18, None).unwrap(),
            U256::from(10u64.pow(12))
        );
    }
}
