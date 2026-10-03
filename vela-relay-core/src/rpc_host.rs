//! Which RPC endpoints the relay will send a request to.
//!
//! The relay fetches URLs it did not choose: the `x-vela-rpc-url` a wallet
//! sends with each request, and the endpoints a chain directory lists. Fetched
//! blindly, the first lets anyone on the internet make the relay call into its
//! own network — the cloud metadata service, the Redis beside it (SSRF). So by
//! default the relay uses only a public `https` endpoint.
//!
//! A relay run beside a private chain — a node on the same machine or the same
//! LAN — needs exactly what that rule refuses. Its operator says so with
//! `VELA_RELAY_ALLOW_PRIVATE_RPC=true`, which allows private hosts and plain
//! `http`. Vela's own deployment never sets it.
//!
//! The rule judges the host as written, not where its name resolves: a public
//! name that resolves to a private address is not caught here.
//!
//! Each shell parses the URL with its own URL type; the decision on the scheme
//! and host is made here, so the two deployments cannot disagree.

use std::net::{IpAddr, Ipv4Addr};

/// The setting's name, shared by both shells' configuration parsers.
pub const ALLOW_PRIVATE_RPC_SETTING: &str = "VELA_RELAY_ALLOW_PRIVATE_RPC";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RpcHostPolicy {
    /// `https` to a public host only. The default, and the only policy Vela's
    /// own deployment runs.
    #[default]
    PublicHttpsOnly,
    /// Also `http`, and private, loopback and link-local hosts — for a relay
    /// that runs beside the node it serves.
    AllowPrivate,
}

impl RpcHostPolicy {
    pub const fn from_setting(allow_private: bool) -> Self {
        if allow_private {
            Self::AllowPrivate
        } else {
            Self::PublicHttpsOnly
        }
    }

    /// `scheme` and `host` as a URL parser reports them; `host` may be a
    /// bracketed IPv6 literal (`[::1]`).
    pub fn allows(self, scheme: &str, host: &str) -> bool {
        if host.is_empty() {
            return false;
        }
        match self {
            Self::PublicHttpsOnly => scheme == "https" && !is_private_host(host),
            Self::AllowPrivate => matches!(scheme, "https" | "http"),
        }
    }
}

/// `localhost` (and its subdomains), and any IP literal that is loopback,
/// unspecified, private (RFC 1918, IPv6 unique-local), link-local (which holds
/// the cloud metadata address), or carrier-grade shared space.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    let literal = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(&host);
    match literal.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => is_private_v4(address),
        Ok(IpAddr::V6(address)) => {
            let first = address.segments()[0];
            address.is_loopback()
                || address.is_unspecified()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
                || address.to_ipv4_mapped().is_some_and(is_private_v4)
        }
        Err(_) => false,
    }
}

fn is_private_v4(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
        || (first == 100 && second & 0xc0 == 64)
}

#[cfg(test)]
mod tests {
    use super::{RpcHostPolicy, is_private_host};

    #[test]
    fn by_default_only_a_public_https_host_is_used() {
        let policy = RpcHostPolicy::default();
        assert!(policy.allows("https", "rpc.example.com"));
        assert!(policy.allows("https", "1.1.1.1"));
        assert!(policy.allows("https", "[2606:4700::1111]"));

        for (scheme, host) in [
            ("http", "rpc.example.com"),
            ("https", "localhost"),
            ("https", "LOCALHOST."),
            ("https", "node.localhost"),
            ("https", "127.0.0.1"),
            ("https", "0.0.0.0"),
            ("https", "10.1.2.3"),
            ("https", "172.16.0.1"),
            ("https", "192.168.1.20"),
            ("https", "169.254.169.254"),
            ("https", "100.64.0.1"),
            ("https", "[::1]"),
            ("https", "[::]"),
            ("https", "[fd00::1]"),
            ("https", "[fe80::1]"),
            ("https", "[::ffff:127.0.0.1]"),
            ("https", ""),
            ("ftp", "rpc.example.com"),
        ] {
            assert!(
                !policy.allows(scheme, host),
                "{scheme}://{host} should be refused"
            );
        }
    }

    /// The bracketed form is what URL parsers report for an IPv6 host; the
    /// docker shell's earlier check parsed it as-is and so never refused
    /// `[::1]`.
    #[test]
    fn a_bracketed_ipv6_loopback_is_private() {
        assert!(is_private_host("[::1]"));
        assert!(is_private_host("::1"));
    }

    #[test]
    fn the_opt_in_allows_private_hosts_and_http_but_nothing_else() {
        let policy = RpcHostPolicy::from_setting(true);
        assert!(policy.allows("http", "127.0.0.1"));
        assert!(policy.allows("http", "anvil"));
        assert!(policy.allows("https", "192.168.1.20"));
        assert!(policy.allows("http", "[::1]"));
        assert!(!policy.allows("ws", "127.0.0.1"));
        assert!(!policy.allows("http", ""));

        assert_eq!(
            RpcHostPolicy::from_setting(false),
            RpcHostPolicy::PublicHttpsOnly
        );
    }

    #[test]
    fn public_addresses_near_the_private_ranges_stay_public() {
        for host in [
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "11.0.0.1",
        ] {
            assert!(!is_private_host(host), "{host} is public");
        }
    }
}
