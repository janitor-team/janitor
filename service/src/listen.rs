//! Listening sockets for the services' web servers.

use tokio::net::{lookup_host, TcpListener, ToSocketAddrs};

/// Bind a listener on every address `addrs` resolve to, so `localhost`
/// covers both `::1` and `127.0.0.1`. With port 0 the others get the
/// port the first one got, so one URL reaches each of them.
pub async fn bind_all<A: ToSocketAddrs>(
    addrs: impl IntoIterator<Item = A>,
) -> std::io::Result<Vec<TcpListener>> {
    let mut resolved = Vec::new();
    for addr in addrs {
        resolved.extend(lookup_host(addr).await?);
    }
    let mut listeners: Vec<TcpListener> = Vec::new();
    let mut unavailable = None;
    let mut ipv6_any = None;
    let mut seen = std::collections::HashSet::new();
    for mut addr in resolved {
        if !seen.insert(addr) {
            continue;
        }
        if let (0, Some(first)) = (addr.port(), listeners.first()) {
            addr.set_port(first.local_addr()?.port());
        }
        match TcpListener::bind(addr).await {
            Ok(listener) => listeners.push(listener),
            // Skip an address this host does not have, such as ::1 with IPv6 off.
            Err(e)
                if e.kind() == std::io::ErrorKind::AddrNotAvailable
                    || e.raw_os_error() == Some(libc::EAFNOSUPPORT) =>
            {
                log::warn!("Not listening on {}: {}", addr, e);
                if addr.ip() == std::net::Ipv6Addr::UNSPECIFIED
                    && e.raw_os_error() == Some(libc::EAFNOSUPPORT)
                {
                    ipv6_any = Some(addr);
                }
                unavailable = Some(std::io::Error::new(e.kind(), format!("{}: {}", addr, e)));
            }
            Err(e) => return Err(std::io::Error::new(e.kind(), format!("{}: {}", addr, e))),
        }
    }
    // With IPv6 switched off in the kernel, :: falls back to 0.0.0.0 if nothing else binds.
    if let (true, Some(addr)) = (listeners.is_empty(), ipv6_any) {
        let fallback = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, addr.port()));
        log::warn!("Listening on {} instead of {}", fallback, addr);
        let listener = TcpListener::bind(fallback)
            .await
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {}", fallback, e)))?;
        listeners.push(listener);
    }
    if listeners.is_empty() {
        return Err(unavailable.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "the listen address resolves to no address",
            )
        }));
    }
    Ok(listeners)
}

/// [`bind_all`] for each of `hosts` (names or addresses) at `port`.
pub async fn bind_hosts(hosts: &[String], port: u16) -> std::io::Result<Vec<TcpListener>> {
    let addrs: Vec<String> = hosts
        .iter()
        .map(|host| format!("{}:{}", host, port))
        .collect();
    bind_all(addrs).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn bound_ips(addr: &str) -> Vec<std::net::IpAddr> {
        let mut ips: Vec<_> = bind_all([addr])
            .await
            .unwrap()
            .iter()
            .map(|listener| listener.local_addr().unwrap().ip())
            .collect();
        ips.sort();
        ips
    }

    #[tokio::test]
    async fn test_bind_all_localhost() {
        let mut expected: Vec<_> = lookup_host("localhost:0")
            .await
            .unwrap()
            .filter(|addr| std::net::TcpListener::bind(addr).is_ok())
            .map(|addr| addr.ip())
            .collect();
        expected.sort();
        assert_eq!(bound_ips("localhost:0").await, expected);
    }

    #[tokio::test]
    async fn test_bind_all_skips_an_address_this_host_does_not_have() {
        if std::net::TcpListener::bind("192.0.2.1:0").is_ok() {
            return;
        }
        let addrs: Vec<std::net::SocketAddr> = vec![
            "127.0.0.1:0".parse().unwrap(),
            "192.0.2.1:0".parse().unwrap(),
        ];
        let ips: Vec<_> = bind_all(&addrs[..])
            .await
            .unwrap()
            .iter()
            .map(|listener| listener.local_addr().unwrap().ip())
            .collect();
        assert_eq!(
            ips,
            vec![std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST)]
        );
        let err = bind_all(&addrs[1..]).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrNotAvailable);
        assert_eq!(
            err.to_string(),
            "192.0.2.1:0: Cannot assign requested address (os error 99)"
        );
    }

    #[tokio::test]
    async fn test_bind_all_port_zero_gives_every_address_the_same_port() {
        let ports: std::collections::HashSet<_> = bind_all(["localhost:0"])
            .await
            .unwrap()
            .iter()
            .map(|listener| listener.local_addr().unwrap().port())
            .collect();
        assert_eq!(ports.len(), 1);
    }

    #[tokio::test]
    async fn test_bind_all_binds_a_repeated_address_once() {
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let ips: Vec<_> = bind_all(&[addr, addr][..])
            .await
            .unwrap()
            .iter()
            .map(|listener| listener.local_addr().unwrap().ip())
            .collect();
        assert_eq!(
            ips,
            vec![std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST)]
        );
    }

    #[tokio::test]
    async fn test_bind_hosts_listens_on_each_host() {
        if std::net::TcpListener::bind("[::1]:0").is_err() {
            return;
        }
        let hosts = ["127.0.0.1".to_string(), "::1".to_string()];
        let addrs: Vec<_> = bind_hosts(&hosts, 0)
            .await
            .unwrap()
            .iter()
            .map(|listener| listener.local_addr().unwrap())
            .collect();
        let port = addrs[0].port();
        assert_eq!(
            addrs,
            vec![
                std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
                std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
            ]
        );
    }

    #[tokio::test]
    async fn test_bind_all_address() {
        assert_eq!(
            bound_ips("127.0.0.1:0").await,
            vec![std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST)]
        );
    }

    #[tokio::test]
    async fn test_bind_all_any_address_without_ipv6() {
        match std::net::TcpListener::bind("[::]:0") {
            Err(e) if e.raw_os_error() == Some(libc::EAFNOSUPPORT) => {}
            _ => return,
        }
        let ipv6_any: std::net::SocketAddr = "[::]:0".parse().unwrap();
        let ipv4_any: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
        let ipv4_loopback: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        for (addrs, ip) in [
            (vec![ipv6_any], ipv4_any.ip()),
            (vec![ipv6_any, ipv4_any], ipv4_any.ip()),
            (vec![ipv4_any, ipv6_any], ipv4_any.ip()),
            (vec![ipv6_any, ipv4_loopback], ipv4_loopback.ip()),
            (vec![ipv4_loopback, ipv6_any], ipv4_loopback.ip()),
        ] {
            let ips: Vec<_> = bind_all(&addrs[..])
                .await
                .unwrap()
                .iter()
                .map(|listener| listener.local_addr().unwrap().ip())
                .collect();
            assert_eq!(ips, vec![ip]);
        }
        let ipv6_loopback: std::net::SocketAddr = "[::1]:0".parse().unwrap();
        let err = bind_all(&[ipv6_loopback][..]).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "[::1]:0: Address family not supported by protocol (os error 97)"
        );
    }
}
