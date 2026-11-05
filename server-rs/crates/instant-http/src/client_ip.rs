use std::net::IpAddr;

use axum::http::HeaderMap;
use ipnet::IpNet;

/// Client address resolved from the socket peer and trusted proxy headers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedClientIp(pub Option<IpAddr>);

/// Resolves the client address from a forwarded chain only when the socket
/// peer belongs to an explicitly trusted proxy network.
#[must_use]
pub fn resolve_client_ip(
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    trusted_proxies: &[IpNet],
) -> Option<IpAddr> {
    let peer = peer?;
    if !trusted_proxies
        .iter()
        .any(|network| network.contains(&peer))
    {
        return Some(peer);
    }

    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').collect::<Vec<_>>())
        .unwrap_or_default();
    let mut client = peer;
    for candidate in forwarded.iter().rev() {
        if !trusted_proxies
            .iter()
            .any(|network| network.contains(&client))
        {
            break;
        }
        let Ok(candidate) = candidate.trim().parse::<IpAddr>() else {
            return Some(peer);
        };
        client = candidate;
    }
    if forwarded.is_empty()
        && let Some(real_ip) = headers
            .get("x-real-ip")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
    {
        client = real_ip;
    }
    Some(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(forwarded: Option<&str>, real_ip: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(forwarded) = forwarded {
            headers.insert("x-forwarded-for", forwarded.parse().unwrap());
        }
        if let Some(real_ip) = real_ip {
            headers.insert("x-real-ip", real_ip.parse().unwrap());
        }
        headers
    }

    fn peer(address: &str) -> Option<IpAddr> {
        Some(address.parse().unwrap())
    }

    fn network(address: &str) -> IpNet {
        address.parse().unwrap()
    }

    #[test]
    fn client_ip_ignores_forwarded_headers_from_untrusted_peers() {
        let trusted = [network("10.0.0.0/8")];

        assert_eq!(
            resolve_client_ip(
                peer("203.0.113.10"),
                &headers(Some("198.51.100.7"), None),
                &trusted
            )
            .map(|ip| ip.to_string()),
            Some("203.0.113.10".to_owned())
        );
    }

    #[test]
    fn client_ip_walks_a_forwarded_chain_from_trusted_proxy_hops() {
        let trusted = [network("10.0.0.0/8")];

        assert_eq!(
            resolve_client_ip(
                peer("10.0.0.4"),
                &headers(Some("198.51.100.7, 10.2.0.8, 10.1.0.6"), None),
                &trusted
            )
            .map(|ip| ip.to_string()),
            Some("198.51.100.7".to_owned())
        );
    }

    #[test]
    fn client_ip_stops_before_an_untrusted_intermediate_hop() {
        let trusted = [network("10.0.0.0/8")];

        assert_eq!(
            resolve_client_ip(
                peer("10.0.0.4"),
                &headers(Some("192.0.2.2, 198.51.100.9, 10.1.0.6"), None),
                &trusted
            )
            .map(|ip| ip.to_string()),
            Some("198.51.100.9".to_owned())
        );
    }

    #[test]
    fn client_ip_uses_real_ip_only_from_a_trusted_peer_without_forwarded_headers() {
        let trusted = [network("10.0.0.0/8")];

        assert_eq!(
            resolve_client_ip(
                peer("10.0.0.4"),
                &headers(None, Some("198.51.100.7")),
                &trusted
            )
            .map(|ip| ip.to_string()),
            Some("198.51.100.7".to_owned())
        );
        assert_eq!(
            resolve_client_ip(
                peer("203.0.113.10"),
                &headers(None, Some("198.51.100.7")),
                &trusted
            )
            .map(|ip| ip.to_string()),
            Some("203.0.113.10".to_owned())
        );
        assert_eq!(
            resolve_client_ip(
                peer("10.0.0.4"),
                &headers(Some("198.51.100.7"), Some("192.0.2.2")),
                &trusted
            )
            .map(|ip| ip.to_string()),
            Some("198.51.100.7".to_owned())
        );
    }

    #[test]
    fn client_ip_resolves_ipv6_forwarded_chains() {
        let trusted = [network("fd00::/8")];

        assert_eq!(
            resolve_client_ip(
                peer("fd00::4"),
                &headers(Some("2001:db8::7, fd00::8"), None),
                &trusted
            )
            .map(|ip| ip.to_string()),
            Some("2001:db8::7".to_owned())
        );
    }
}
