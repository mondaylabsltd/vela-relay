//! Where the relay reads chain metadata: the published RPC endpoints, the
//! native asset and the accepted stablecoins of each network, served as
//! `{base}/chains/eip155-{chainId}.json` by an `ethereum-data` deployment.
//!
//! Both shells read the base URL from `VELA_RELAY_CHAIN_DIRECTORY_URL` and
//! parse it here, so a relay run by someone else can read a directory it runs
//! itself instead of Vela's. The directory decides which RPC endpoints and
//! stablecoins the relay trusts, so it must be one the operator controls.

pub const DEFAULT_CHAIN_DIRECTORY_URL: &str = "https://ethereum-data.getvela.app";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainDirectory {
    base: String,
}

impl ChainDirectory {
    /// Accepts an `http` or `https` base URL, optionally with a path prefix.
    /// A trailing `/` is dropped; a query string or fragment is refused
    /// because the chain path is appended to the end of the value.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        let rest = value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
            .ok_or_else(|| {
                format!("chain directory URL `{value}` must start with https:// or http://")
            })?;
        let host = rest.split('/').next().unwrap_or_default();
        if host.is_empty() {
            return Err(format!("chain directory URL `{value}` has no host"));
        }
        if value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(format!("chain directory URL `{value}` contains whitespace"));
        }
        if value.contains(['?', '#']) {
            return Err(format!(
                "chain directory URL `{value}` cannot carry a query string or fragment"
            ));
        }
        Ok(Self {
            base: value.trim_end_matches('/').to_owned(),
        })
    }

    /// Absent means Vela's directory, as before the setting existed.
    pub fn from_setting(value: Option<&str>) -> Result<Self, String> {
        value.map_or_else(|| Ok(Self::default()), Self::parse)
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn metadata_url(&self, chain_id: u64) -> String {
        format!("{}/chains/eip155-{chain_id}.json", self.base)
    }
}

/// What one metadata fetch says about a chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing {
    /// The directory holds the chain's metadata.
    Listed,
    /// The directory does not hold the chain. The relay cannot serve it: its
    /// quote and its executor both need the chain's assets from here.
    NotListed,
    /// The directory did not answer; nothing is known about the chain.
    Unavailable,
}

impl Listing {
    /// Judge one fetch from its HTTP status and, for a success, its body.
    ///
    /// Vela's directory is a single-page site: for a chain it does not hold it
    /// answers `200` with its HTML page, not `404` (measured 2026-10-03 for
    /// chain 123456789). So a success whose body is not a JSON object is "not
    /// listed", like a `404` — never a transient failure to retry.
    pub fn of(status: u16, body: &str) -> Self {
        match status {
            200..=299 => {
                if serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(body).is_ok()
                {
                    Self::Listed
                } else {
                    Self::NotListed
                }
            }
            404 | 410 => Self::NotListed,
            _ => Self::Unavailable,
        }
    }
}

impl Default for ChainDirectory {
    fn default() -> Self {
        Self {
            base: DEFAULT_CHAIN_DIRECTORY_URL.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChainDirectory, DEFAULT_CHAIN_DIRECTORY_URL, Listing};

    #[test]
    fn the_directorys_html_page_is_not_a_listing() {
        assert_eq!(
            Listing::of(200, r#"{"chainId":1,"rpc":["https://rpc.example"]}"#),
            Listing::Listed
        );
        assert_eq!(
            Listing::of(200, "<!doctype html><html><body>Vela</body></html>"),
            Listing::NotListed
        );
        assert_eq!(Listing::of(200, ""), Listing::NotListed);
        assert_eq!(Listing::of(200, "[]"), Listing::NotListed);
        assert_eq!(Listing::of(404, "not found"), Listing::NotListed);
        assert_eq!(Listing::of(410, ""), Listing::NotListed);
    }

    #[test]
    fn a_directory_that_did_not_answer_says_nothing_about_the_chain() {
        for status in [0, 301, 403, 429, 500, 502, 503] {
            assert_eq!(
                Listing::of(status, "<html>"),
                Listing::Unavailable,
                "status {status}"
            );
        }
    }

    #[test]
    fn default_is_the_address_the_relay_always_used() {
        assert_eq!(
            ChainDirectory::default().metadata_url(100),
            "https://ethereum-data.getvela.app/chains/eip155-100.json"
        );
        assert_eq!(
            ChainDirectory::from_setting(None).unwrap().base_url(),
            DEFAULT_CHAIN_DIRECTORY_URL
        );
    }

    #[test]
    fn accepts_a_self_hosted_directory_with_or_without_a_path() {
        let directory = ChainDirectory::parse("https://chains.example.org/").unwrap();
        assert_eq!(
            directory.metadata_url(1),
            "https://chains.example.org/chains/eip155-1.json"
        );

        let directory = ChainDirectory::parse(" http://chain-data:8080/data// ").unwrap();
        assert_eq!(
            directory.metadata_url(8453),
            "http://chain-data:8080/data/chains/eip155-8453.json"
        );
    }

    #[test]
    fn refuses_values_the_chain_path_cannot_be_appended_to() {
        for value in [
            "",
            "ethereum-data.example.org",
            "ftp://ethereum-data.example.org",
            "https://",
            "https:///chains",
            "https://example.org/?token=1",
            "https://example.org/#top",
            "https://exa mple.org",
        ] {
            assert!(
                ChainDirectory::parse(value).is_err(),
                "`{value}` should be refused"
            );
        }
    }
}
