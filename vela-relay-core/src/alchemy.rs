#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkProtocol {
    Evm,
    NonEvm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlchemyNetwork {
    pub slug: &'static str,
    pub chain_id: Option<u64>,
    pub endpoint_base_url: &'static str,
    pub protocol: NetworkProtocol,
}

impl AlchemyNetwork {
    const fn evm(slug: &'static str, chain_id: u64, endpoint_base_url: &'static str) -> Self {
        Self {
            slug,
            chain_id: Some(chain_id),
            endpoint_base_url,
            protocol: NetworkProtocol::Evm,
        }
    }

    const fn non_evm(slug: &'static str, endpoint_base_url: &'static str) -> Self {
        Self {
            slug,
            chain_id: None,
            endpoint_base_url,
            protocol: NetworkProtocol::NonEvm,
        }
    }

    const fn non_evm_with_chain_id(
        slug: &'static str,
        chain_id: u64,
        endpoint_base_url: &'static str,
    ) -> Self {
        Self {
            slug,
            chain_id: Some(chain_id),
            endpoint_base_url,
            protocol: NetworkProtocol::NonEvm,
        }
    }
}

pub const ALCHEMY_NETWORKS: &[AlchemyNetwork] = &[
    AlchemyNetwork::evm(
        "abstract",
        2741,
        "https://abstract-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("adi", 36900, "https://adi-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("anime", 69000, "https://anime-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "apechain",
        33139,
        "https://apechain-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::non_evm("aptos", "https://aptos-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("arbitrum", 42161, "https://arb-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("arc", 5042, "https://arc-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "arc-testnet",
        5042002,
        "https://arc-testnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("astar", 592, "https://astar-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("avalanche", 43114, "https://avax-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("base", 8453, "https://base-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "berachain",
        80094,
        "https://berachain-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::non_evm(
        "bitcoin-cash",
        "https://bitcoincash-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::non_evm("bitcoin", "https://bitcoin-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("blast", 81457, "https://blast-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("bnb", 56, "https://bnb-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("boba", 288, "https://boba-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("bob", 60808, "https://bob-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("botanix", 3637, "https://botanix-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::non_evm(
        "celestiabridge",
        "https://celestiabridge-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("celo", 42220, "https://celo-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("citrea", 4114, "https://citrea-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("cronos", 25, "https://cronos-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("crossfi", 4158, "https://crossfi-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "degen",
        666666666,
        "https://degen-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::non_evm("dogecoin", "https://dogecoin-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("ethereum", 1, "https://eth-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("flow", 747, "https://flow-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("frax", 252, "https://frax-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("gensyn", 685689, "https://gensyn-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("gnosis", 100, "https://gnosis-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "humanity",
        6985385,
        "https://humanity-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm(
        "hyperliquid",
        999,
        "https://hyperliquid-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::non_evm_with_chain_id(
        "injective",
        1776,
        "https://injective-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("ink", 57073, "https://ink-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("jovay", 5734951, "https://jovay-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("kaia", 8217, "https://kaia-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("katana", 747474, "https://katana-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("lens", 232, "https://lens-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("linea", 59144, "https://linea-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::non_evm("litecoin", "https://litecoin-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("mantle", 5000, "https://mantle-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("matic", 137, "https://polygon-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("megaeth", 4326, "https://megaeth-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("metis", 1088, "https://metis-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("mode", 34443, "https://mode-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("monad", 143, "https://monad-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "moonbeam",
        1284,
        "https://moonbeam-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("mythos", 42018, "https://mythos-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("opbnb", 204, "https://opbnb-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("optimism", 10, "https://opt-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("pharos", 1672, "https://pharos-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("plasma", 9745, "https://plasma-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "polygonzkevm",
        1101,
        "https://polygonzkevm-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("rise", 4153, "https://rise-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "robinhood",
        4663,
        "https://robinhood-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("ronin", 2020, "https://ronin-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "rootstock",
        30,
        "https://rootstock-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("scroll", 534352, "https://scroll-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("sei", 1329, "https://sei-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("settlus", 5371, "https://settlus-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("shape", 360, "https://shape-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::non_evm("solana", "https://solana-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("soneium", 1868, "https://soneium-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("sonic", 146, "https://sonic-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("stable", 988, "https://stable-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::non_evm_with_chain_id(
        "starknet",
        23448594291968336,
        "https://starknet-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::non_evm("stellar", "https://stellar-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("story", 1514, "https://story-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::non_evm("sui", "https://sui-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm(
        "superseed",
        5330,
        "https://superseed-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("tempo", 4217, "https://tempo-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::non_evm_with_chain_id(
        "tron",
        728126428,
        "https://tron-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm(
        "unichain",
        130,
        "https://unichain-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm(
        "worldchain",
        480,
        "https://worldchain-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm(
        "worldmobilechain",
        869,
        "https://worldmobilechain-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("xlayer", 196, "https://xlayer-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::non_evm_with_chain_id(
        "xmtp-ropsten",
        351243127,
        "https://xmtp-ropsten.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm(
        "zetachain",
        7000,
        "https://zetachain-mainnet.g.alchemy.com/v2/",
    ),
    AlchemyNetwork::evm("zksync", 324, "https://zksync-mainnet.g.alchemy.com/v2/"),
    AlchemyNetwork::evm("zora", 7777777, "https://zora-mainnet.g.alchemy.com/v2/"),
];

pub fn rpc_url(chain_id: u64, api_key: &str) -> Option<String> {
    ALCHEMY_NETWORKS
        .iter()
        .find(|network| {
            network.protocol == NetworkProtocol::Evm && network.chain_id == Some(chain_id)
        })
        .map(|network| format!("{}{api_key}", network.endpoint_base_url))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{ALCHEMY_NETWORKS, NetworkProtocol, rpc_url};

    #[test]
    fn includes_every_network_from_the_chain_resource_directory() {
        let slugs = ALCHEMY_NETWORKS
            .iter()
            .map(|network| network.slug)
            .collect::<HashSet<_>>();

        assert_eq!(slugs.len(), ALCHEMY_NETWORKS.len());
        assert_eq!(ALCHEMY_NETWORKS.len(), 81);
        assert!(
            ALCHEMY_NETWORKS
                .iter()
                .any(|network| network.protocol == NetworkProtocol::NonEvm)
        );
        assert!(
            ALCHEMY_NETWORKS
                .iter()
                .filter(|network| network.protocol == NetworkProtocol::Evm)
                .count()
                >= 60
        );
    }

    #[test]
    fn appends_the_api_key_to_evm_directory_endpoints() {
        assert_eq!(
            rpc_url(8453, "test-key"),
            Some("https://base-mainnet.g.alchemy.com/v2/test-key".into())
        );
        assert_eq!(rpc_url(999_999, "test-key"), None);
    }
}
