//! Listening sockets for the services' web servers.

use tokio::net::{lookup_host, TcpListener, ToSocketAddrs};

/// Bind a listener on every address `addr` resolves to, so `localhost`
/// covers both `::1` and `127.0.0.1`. With port 0 the others get the
/// port the first one got, so one URL reaches each of them.
pub async fn bind_all(addr: impl ToSocketAddrs) -> std::io::Result<Vec<TcpListener>> {
    let mut listeners: Vec<TcpListener> = Vec::new();
    let mut unavailable = None;
    let mut seen = std::collections::HashSet::new();
    for mut addr in lookup_host(addr).await? {
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
                unavailable = Some(std::io::Error::new(e.kind(), format!("{}: {}", addr, e)));
            }
            Err(e) => return Err(std::io::Error::new(e.kind(), format!("{}: {}", addr, e))),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn bound_ips(addr: &str) -> Vec<std::net::IpAddr> {
        let mut ips: Vec<_> = bind_all(addr)
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
        let ports: std::collections::HashSet<_> = bind_all("localhost:0")
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
    async fn test_bind_all_address() {
        assert_eq!(
            bound_ips("127.0.0.1:0").await,
            vec![std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST)]
        );
    }
}
