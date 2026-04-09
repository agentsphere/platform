//! Transparent proxy helpers: `SO_ORIGINAL_DST`, protocol detection, CIDR matching.
//!
//! These utilities support the transparent proxy mode where iptables REDIRECT
//! rules intercept traffic and the proxy recovers the original destination via
//! `SO_ORIGINAL_DST` / `IP6T_SO_ORIGINAL_DST`.

use std::io;
use std::net::{IpAddr, SocketAddr};

use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// SO_ORIGINAL_DST
// ---------------------------------------------------------------------------

/// Recover the original destination address from a redirected TCP connection.
///
/// Uses `SO_ORIGINAL_DST` (IPv4) with `IP6T_SO_ORIGINAL_DST` (IPv6) fallback.
/// Retries up to 3 times with 2 ms backoff because the kernel may race with
/// the conntrack entry on very fast accept paths.
///
/// Only available on Linux; returns an error on other platforms.
#[cfg(target_os = "linux")]
pub async fn get_original_dst(stream: &TcpStream) -> io::Result<SocketAddr> {
    let mut last_err = io::Error::new(io::ErrorKind::Other, "no attempts made");

    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        // Try IPv4 first — pass &stream (implements AsFd), not stream.as_fd()
        match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::OriginalDst) {
            Ok(addr) => return Ok(sockaddr_in_to_std(&addr)),
            Err(_) => {}
        }
        // IPv6 fallback
        match nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::Ip6tOriginalDst) {
            Ok(addr6) => return Ok(sockaddr_in6_to_std(&addr6)),
            Err(e) => last_err = io::Error::new(io::ErrorKind::Other, e),
        }
    }

    Err(last_err)
}

/// Stub for non-Linux platforms so the crate compiles everywhere.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
pub async fn get_original_dst(_stream: &TcpStream) -> io::Result<SocketAddr> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_ORIGINAL_DST is only available on Linux",
    ))
}

/// Convert `libc::sockaddr_in` to `std::net::SocketAddr`.
#[cfg(target_os = "linux")]
fn sockaddr_in_to_std(sa: &nix::libc::sockaddr_in) -> SocketAddr {
    let ip = std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
    let port = u16::from_be(sa.sin_port);
    SocketAddr::new(IpAddr::V4(ip), port)
}

/// Convert `libc::sockaddr_in6` to `std::net::SocketAddr`.
#[cfg(target_os = "linux")]
fn sockaddr_in6_to_std(sa: &nix::libc::sockaddr_in6) -> SocketAddr {
    let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
    let port = u16::from_be(sa.sin6_port);
    SocketAddr::new(IpAddr::V6(ip), port)
}

// ---------------------------------------------------------------------------
// Protocol Detection
// ---------------------------------------------------------------------------

/// Detected application-layer protocol after peeking at stream bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedProtocol {
    Http,
    Tcp,
}

/// Peek 1 byte to detect TLS `ClientHello` (content type `0x16`).
pub async fn is_tls_client_hello(stream: &TcpStream) -> bool {
    let mut buf = [0u8; 1];
    match stream.peek(&mut buf).await {
        Ok(1) => buf[0] == 0x16,
        _ => false,
    }
}

/// HTTP method prefixes used for protocol sniffing.
const HTTP_PREFIXES: &[&[u8]] = &[
    b"GET ", b"POST ", b"PUT ", b"DELE", b"PATC", b"HEAD", b"OPTI", b"CONN",
];

/// Peek up to 8 bytes with a 100 ms deadline to detect HTTP vs raw TCP.
pub async fn peek_protocol(stream: &TcpStream) -> DetectedProtocol {
    let mut buf = [0u8; 8];
    let Ok(Ok(peeked)) =
        tokio::time::timeout(std::time::Duration::from_millis(100), stream.peek(&mut buf)).await
    else {
        return DetectedProtocol::Tcp;
    };
    if detect_http_prefix(&buf[..peeked]) {
        DetectedProtocol::Http
    } else {
        DetectedProtocol::Tcp
    }
}

/// Public wrapper: check if the data starts with a known HTTP method.
///
/// Useful when the caller has already read bytes from the stream and
/// wants to classify the protocol without peeking again.
pub fn detect_http_prefix_pub(data: &[u8]) -> bool {
    detect_http_prefix(data)
}

/// Check if the peeked bytes start with a known HTTP method.
fn detect_http_prefix(data: &[u8]) -> bool {
    HTTP_PREFIXES.iter().any(|prefix| {
        data.len() >= prefix.len() && data[..prefix.len()].eq_ignore_ascii_case(prefix)
    })
}

// ---------------------------------------------------------------------------
// CIDR Matching
// ---------------------------------------------------------------------------

/// Parse a comma-separated list of ports (e.g. `"5432,6379,3306"`).
pub fn parse_ports(s: &str) -> Vec<u16> {
    s.split(',').filter_map(|p| p.trim().parse().ok()).collect()
}

/// Parse a comma-separated list of CIDR strings (e.g. `"10.0.0.0/8,172.16.0.0/12"`).
///
/// Invalid entries are silently skipped.
pub fn parse_cidrs(s: &str) -> Vec<(IpAddr, u8)> {
    s.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (ip_str, prefix_str) = entry.split_once('/')?;
            let ip: IpAddr = ip_str.trim().parse().ok()?;
            let prefix: u8 = prefix_str.trim().parse().ok()?;
            let max_prefix = if ip.is_ipv4() { 32 } else { 128 };
            if prefix > max_prefix {
                return None;
            }
            Some((ip, prefix))
        })
        .collect()
}

/// Check whether `ip` falls within any of the given CIDRs.
pub fn is_internal_ip(ip: IpAddr, internal_cidrs: &[(IpAddr, u8)]) -> bool {
    internal_cidrs
        .iter()
        .any(|(network, prefix_len)| cidr_contains(*network, *prefix_len, ip))
}

/// Check if `ip` is contained in `network/prefix_len`.
pub fn cidr_contains(network: IpAddr, prefix_len: u8, ip: IpAddr) -> bool {
    match (network, ip) {
        (IpAddr::V4(net), IpAddr::V4(addr)) => {
            let net_bits = u32::from(net);
            let addr_bits = u32::from(addr);
            if prefix_len == 0 {
                return true;
            }
            if prefix_len >= 32 {
                return net_bits == addr_bits;
            }
            let mask = u32::MAX << (32 - prefix_len);
            (net_bits & mask) == (addr_bits & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(addr)) => {
            let net_bits = u128::from(net);
            let addr_bits = u128::from(addr);
            if prefix_len == 0 {
                return true;
            }
            if prefix_len >= 128 {
                return net_bits == addr_bits;
            }
            let mask = u128::MAX << (128 - prefix_len);
            (net_bits & mask) == (addr_bits & mask)
        }
        // IPv4 network vs IPv6 address (or vice versa): never matches
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Outbound Socket Binding
// ---------------------------------------------------------------------------

/// Default bypass source port range (above Linux's default ephemeral 32768-60999).
pub const BYPASS_PORT_MIN: u16 = 61000;
/// Upper bound of bypass source port range.
pub const BYPASS_PORT_MAX: u16 = 65000;

/// Create a `TcpStream` connected to `dest`, bound to a random source port in
/// `port_range` so that iptables `--sport` RETURN rules skip re-intercepting it.
///
/// Retries up to 10 times on `EADDRINUSE` (4001 ports in default range).
pub async fn bind_outbound_socket(
    dest: SocketAddr,
    port_range: (u16, u16),
) -> io::Result<TcpStream> {
    let mut last_err = io::Error::new(io::ErrorKind::AddrInUse, "no attempts made");

    for _ in 0..10 {
        let port = rand::random_range(port_range.0..=port_range.1);
        let bind_addr = if dest.is_ipv4() {
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port)
        } else {
            SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), port)
        };

        let socket = if dest.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };

        match socket.bind(bind_addr) {
            Ok(()) => return socket.connect(dest).await,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                last_err = e;
            }
            Err(e) => return Err(e),
        }
    }

    Err(last_err)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- CIDR parsing ---

    #[test]
    fn parse_cidrs_rfc1918() {
        let cidrs = parse_cidrs("10.0.0.0/8,172.16.0.0/12,192.168.0.0/16");
        assert_eq!(cidrs.len(), 3);
        assert_eq!(cidrs[0], ("10.0.0.0".parse().unwrap(), 8));
        assert_eq!(cidrs[1], ("172.16.0.0".parse().unwrap(), 12));
        assert_eq!(cidrs[2], ("192.168.0.0".parse().unwrap(), 16));
    }

    #[test]
    fn parse_cidrs_empty() {
        assert!(parse_cidrs("").is_empty());
    }

    #[test]
    fn parse_cidrs_invalid_entries_skipped() {
        let cidrs = parse_cidrs("10.0.0.0/8,not-a-cidr,192.168.0.0/16,/8");
        assert_eq!(cidrs.len(), 2);
    }

    #[test]
    fn parse_cidrs_ipv6() {
        let cidrs = parse_cidrs("fd00::/8,::1/128");
        assert_eq!(cidrs.len(), 2);
    }

    #[test]
    fn parse_cidrs_prefix_too_large() {
        let cidrs = parse_cidrs("10.0.0.0/33");
        assert!(cidrs.is_empty());
    }

    #[test]
    fn parse_cidrs_with_whitespace() {
        let cidrs = parse_cidrs(" 10.0.0.0/8 , 172.16.0.0/12 ");
        assert_eq!(cidrs.len(), 2);
    }

    // --- CIDR contains ---

    #[test]
    fn cidr_contains_basic_v4() {
        let net: IpAddr = "10.0.0.0".parse().unwrap();
        assert!(cidr_contains(net, 8, "10.1.2.3".parse().unwrap()));
        assert!(cidr_contains(net, 8, "10.255.255.255".parse().unwrap()));
        assert!(!cidr_contains(net, 8, "11.0.0.1".parse().unwrap()));
    }

    #[test]
    fn cidr_contains_exact_host() {
        let net: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(cidr_contains(net, 32, "10.0.0.1".parse().unwrap()));
        assert!(!cidr_contains(net, 32, "10.0.0.2".parse().unwrap()));
    }

    #[test]
    fn cidr_contains_prefix_zero() {
        let net: IpAddr = "0.0.0.0".parse().unwrap();
        assert!(cidr_contains(net, 0, "1.2.3.4".parse().unwrap()));
        assert!(cidr_contains(net, 0, "255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn cidr_contains_v6() {
        let net: IpAddr = "fd00::".parse().unwrap();
        assert!(cidr_contains(net, 8, "fd12::1".parse().unwrap()));
        assert!(!cidr_contains(net, 8, "fe80::1".parse().unwrap()));
    }

    #[test]
    fn cidr_contains_mixed_family_no_match() {
        let v4_net: IpAddr = "10.0.0.0".parse().unwrap();
        let v6_addr: IpAddr = "::1".parse().unwrap();
        assert!(!cidr_contains(v4_net, 8, v6_addr));
    }

    // --- is_internal_ip ---

    #[test]
    fn is_internal_ip_rfc1918() {
        let cidrs = parse_cidrs("10.0.0.0/8,172.16.0.0/12,192.168.0.0/16");
        assert!(is_internal_ip("10.0.0.1".parse().unwrap(), &cidrs));
        assert!(is_internal_ip("172.20.1.1".parse().unwrap(), &cidrs));
        assert!(is_internal_ip("192.168.1.1".parse().unwrap(), &cidrs));
        assert!(!is_internal_ip("8.8.8.8".parse().unwrap(), &cidrs));
        assert!(!is_internal_ip("1.1.1.1".parse().unwrap(), &cidrs));
    }

    #[test]
    fn is_internal_ip_empty_cidrs() {
        assert!(!is_internal_ip("10.0.0.1".parse().unwrap(), &[]));
    }

    #[test]
    fn is_internal_ip_v6() {
        let cidrs = parse_cidrs("fd00::/8");
        assert!(is_internal_ip("fd12::1".parse().unwrap(), &cidrs));
        assert!(!is_internal_ip("2001:db8::1".parse().unwrap(), &cidrs));
    }

    // --- HTTP prefix detection ---

    #[test]
    fn detect_http_methods() {
        assert!(detect_http_prefix(b"GET / HTTP/1.1"));
        assert!(detect_http_prefix(b"POST /api"));
        assert!(detect_http_prefix(b"PUT /res"));
        assert!(detect_http_prefix(b"DELETE /x"));
        assert!(detect_http_prefix(b"PATCH /x"));
        assert!(detect_http_prefix(b"HEAD /x"));
        assert!(detect_http_prefix(b"OPTIONS"));
        assert!(detect_http_prefix(b"CONNECT "));
    }

    #[test]
    fn detect_non_http() {
        assert!(!detect_http_prefix(b"\x16\x03\x01")); // TLS
        assert!(!detect_http_prefix(b"\x00\x00\x00")); // binary
        assert!(!detect_http_prefix(b"SELE")); // SQL
        assert!(!detect_http_prefix(b"")); // empty
    }

    #[test]
    fn detect_http_short_buffer() {
        // Buffer too short for any prefix
        assert!(!detect_http_prefix(b"GE"));
    }
}
