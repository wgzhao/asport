use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    fs::{self, File},
    io::BufReader,
    net::{IpAddr, SocketAddr},
    path::Path,
    str::FromStr,
};

use rustls::{pki_types::CertificateDer, RootCertStore};
use rustls_pemfile::Item;
use serde::{de::Error as DeError, Deserialize, Deserializer};
use tokio::net;

use asport::ForwardMode;

use crate::error::Error;

pub fn load_certs<P: AsRef<Path>>(paths: Vec<P>, disable_native: bool) -> Result<RootCertStore, Error> {
    let mut certs = RootCertStore::empty();

    for path in &paths {
        let mut file = BufReader::new(File::open(path)?);

        while let Ok(Some(item)) = rustls_pemfile::read_one(&mut file) {
            if let Item::X509Certificate(cert) = item {
                certs.add(cert)?;
            }
        }
    }

    if certs.is_empty() {
        for path in &paths {
            certs.add(CertificateDer::from(fs::read(path)?))?;
        }
    }

    if !disable_native {
        for cert in rustls_native_certs::load_native_certs().map_err(Error::LoadNativeCerts)? {
            let _ = certs.add(cert);
        }
    }

    Ok(certs)
}

pub fn union_proxy_protocol_addresses(source: Option<SocketAddr>, destination: SocketAddr)
                                      -> Option<(SocketAddr, SocketAddr)> {
    match (source, destination) {
        // If destination is an IPv6 address and source is an IPv4 address, convert source to an IPv6-mapped-IPv4 address
        // Avoid to be UNKNOWN or AF_UNSPEC
        // See also: https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt
        (Some(SocketAddr::V4(source_v4)), destination @ SocketAddr::V6(_)) => {
            let source = SocketAddr::new(IpAddr::from(source_v4.ip().to_ipv6_mapped()), source_v4.port());
            Some((source, destination))
        }
        // If destination is an IPv4 address and source is an IPv6 address, try to convert source to an IPv4-mapped-IPv6 address
        (Some(source @ SocketAddr::V6(source_v6)), destination @ SocketAddr::V4(_)) => {
            match source_v6.ip().to_ipv4_mapped() {
                Some(ipv4) => {
                    let source = SocketAddr::new(IpAddr::from(ipv4), source_v6.port());
                    Some((source, destination))
                }
                // Finally, it will be convert to UNKNOWN (v1) or AF_UNSPEC (v2).
                None => Some((source, destination)),
            }
        }
        (Some(source), destination) => Some((source, destination)),
        _ => None,
    }
}

pub enum CongestionControl {
    Cubic,
    NewReno,
    Bbr,
}

impl FromStr for CongestionControl {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("cubic") {
            Ok(Self::Cubic)
        } else if s.eq_ignore_ascii_case("new_reno") || s.eq_ignore_ascii_case("newreno") {
            Ok(Self::NewReno)
        } else if s.eq_ignore_ascii_case("bbr") {
            Ok(Self::Bbr)
        } else {
            Err("invalid congestion control")
        }
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Network {
    Tcp,
    Udp,
    Both,
}

impl Network {
    pub(crate) fn is_tcp(&self) -> bool {
        matches!(self, Self::Tcp)
    }

    pub(crate) fn is_udp(&self) -> bool {
        matches!(self, Self::Udp)
    }

    pub(crate) fn is_both(&self) -> bool {
        matches!(self, Self::Both)
    }

    pub(crate) fn tcp(&self) -> bool {
        self.is_both() || self.is_tcp()
    }

    pub(crate) fn udp(&self) -> bool {
        self.is_both() || self.is_udp()
    }
}

impl Display for Network {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Udp => write!(f, "udp"),
            Self::Both => write!(f, "both"),
        }
    }
}

impl FromStr for Network {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("tcp") {
            Ok(Self::Tcp)
        } else if s.eq_ignore_ascii_case("udp") {
            Ok(Self::Udp)
        } else if vec!["both", "tcpudp", "tcp_udp", "tcp-udp", "all"].iter()
            .any(|&x| s.eq_ignore_ascii_case(x)) {
            Ok(Self::Both)
        } else {
            Err("invalid network")
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum UdpForwardMode {
    Native,
    Quic,
}

impl FromStr for UdpForwardMode {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("native") {
            Ok(Self::Native)
        } else if s.eq_ignore_ascii_case("quic") {
            Ok(Self::Quic)
        } else {
            Err("invalid UDP relay mode")
        }
    }
}

impl Display for UdpForwardMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Native => write!(f, "native"),
            Self::Quic => write!(f, "quic"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ProxyProtocol {
    None,
    V1,
    V2,
}

impl FromStr for ProxyProtocol {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("v1") {
            Ok(Self::V1)
        } else if s.eq_ignore_ascii_case("v2") {
            Ok(Self::V2)
        } else if vec!["none", "disable", "disabled", "off"].iter()
            .any(|&x| s.eq_ignore_ascii_case(x)) {
            Ok(Self::None)
        } else {
            Err("invalid proxy protocol version")
        }
    }
}

impl Display for ProxyProtocol {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::None => write!(f, "none"),
            Self::V1 => write!(f, "v1"),
            Self::V2 => write!(f, "v2"),
        }
    }
}


pub struct NetworkUdpForwardModeCombine(Network, UdpForwardMode);

impl NetworkUdpForwardModeCombine {
    pub fn new(network: Network, mode: UdpForwardMode) -> Self {
        Self(network, mode)
    }
}

impl From<NetworkUdpForwardModeCombine> for ForwardMode {
    fn from(value: NetworkUdpForwardModeCombine) -> Self {
        let (network, mode) = (value.0, value.1);
        match (network, mode) {
            (Network::Tcp, _) => ForwardMode::Tcp,
            (Network::Udp, UdpForwardMode::Native) => ForwardMode::UdpNative,
            (Network::Udp, UdpForwardMode::Quic) => ForwardMode::UdpQuic,
            (Network::Both, UdpForwardMode::Native) => ForwardMode::TcpUdpNative,
            (Network::Both, UdpForwardMode::Quic) => ForwardMode::TcpUdpQuic,
        }
    }
}

impl From<(Network, UdpForwardMode)> for NetworkUdpForwardModeCombine {
    fn from(value: (Network, UdpForwardMode)) -> Self {
        NetworkUdpForwardModeCombine::new(value.0, value.1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    SocketAddress(SocketAddr),
    DomainAddress(String, u16),
}

impl Address {
    pub fn new(host: String, port: u16) -> Self {
        match host.parse::<IpAddr>() {
            Ok(ip) => Self::SocketAddress(SocketAddr::from((ip, port))),
            Err(_) => Self::DomainAddress(host, port),
        }
    }

    pub async fn resolve(&self) -> Result<impl Iterator<Item=SocketAddr>, Error> {
        match self {
            Self::SocketAddress(addr) => Ok(vec![*addr].into_iter()),
            Self::DomainAddress(host, port) => {
                Ok(net::lookup_host((host.as_str(), *port))
                    .await?
                    .collect::<Vec<_>>()
                    .into_iter())
            }
        }
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        let (host, port) = s
            .rsplit_once(':')
            .ok_or(DeError::custom("invalid server address"))?;

        // remove first and last brackets for IPv6 address
        let host = if host.starts_with('[') && host.ends_with(']') {
            host[1..host.len() - 1].to_string()
        } else {
            host.to_string()
        };

        let port = port.parse().map_err(DeError::custom)?;

        Ok(Address::new(host, port))
    }
}

pub struct ServerAddress {
    addr: Address,
    server_name: String,
}

impl ServerAddress {
    pub fn new(addr: Address, server_name: Option<String>) -> Self {
        let server_name = match (server_name, &addr) {
            (Some(name), _) => name,
            // Use IP address as server name if no server name is provided
            (None, Address::SocketAddress(addr)) => addr.ip().to_string(),
            (None, Address::DomainAddress(domain, _)) => domain.clone(),
        };

        Self { addr, server_name }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub async fn resolve(&self) -> Result<impl Iterator<Item=SocketAddr>, Error> {
        self.addr.resolve().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_address() {
        let s = r#""127.0.0.1:8080""#;
        let addr: Address = serde_json::from_str(s).unwrap();
        assert_eq!(addr, Address::SocketAddress(SocketAddr::from(([127, 0, 0, 1], 8080))));

        let s = r#""[::1]:8080""#;
        let addr: Address = serde_json::from_str(s).unwrap();
        assert_eq!(addr, Address::SocketAddress(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 8080))));

        let s = r#""asport.akinokaede.com:8080""#;
        let addr: Address = serde_json::from_str(s).unwrap();
        assert_eq!(addr, Address::DomainAddress("asport.akinokaede.com".to_string(), 8080));

        // Invalid address
        let s = r#""127.0.0.1""#;
        let addr: Result<Address, _> = serde_json::from_str(s);
        assert!(addr.is_err());

        let s = r#""127.0.0.1:test""#;
        let addr: Result<Address, _> = serde_json::from_str(s);
        assert!(addr.is_err());
    }
}