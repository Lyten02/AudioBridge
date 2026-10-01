//! Endpoint construction, persisted identity and local address discovery.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::address_lookup::{DnsAddressLookup, PkarrResolver};
use iroh::endpoint::{presets, BindOpts, QuicTransportConfig};
use iroh::{Endpoint, RelayMode, SecretKey};

use super::{is_lan, is_tailscale, NetOptions};
use crate::pairing::MAX_ADDRS;
use crate::proto::{ALPN, DEFAULT_PORT};

const SERVER_KEY_FILE: &str = "server.key";
const CLIENT_KEY_FILE: &str = "client.key";
const PAIRING_SECRET_FILE: &str = "pairing.secret";
const PORT_FILE: &str = "server.port";

/// Reads `N` bytes from `dir/name`, or generates and persists them.
fn load_or_create<const N: usize>(dir: &Path, name: &str) -> Result<[u8; N]> {
    let path = dir.join(name);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(arr) = <[u8; N]>::try_from(bytes.as_slice()) {
            return Ok(arr);
        }
        tracing::warn!("{} is corrupt; regenerating", path.display());
    }
    let mut arr = [0u8; N];
    getrandom::fill(&mut arr).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, arr).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(arr)
}

pub(super) fn server_identity(dir: &Path) -> Result<(SecretKey, [u8; 16])> {
    let key = SecretKey::from_bytes(&load_or_create::<32>(dir, SERVER_KEY_FILE)?);
    let secret = load_or_create::<16>(dir, PAIRING_SECRET_FILE)?;
    Ok((key, secret))
}

pub(super) fn client_identity(dir: &Path) -> Result<SecretKey> {
    Ok(SecretKey::from_bytes(&load_or_create::<32>(dir, CLIENT_KEY_FILE)?))
}

fn transport_config() -> Result<QuicTransportConfig> {
    Ok(QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(2))
        .max_idle_timeout(Some(Duration::from_secs(8).try_into()?))
        // Never queue stale audio: a few frames at most, newest wins.
        .datagram_send_buffer_size(16 * 1024)
        .datagram_receive_buffer_size(Some(64 * 1024))
        .build())
}

fn base_builder(opts: &NetOptions, key: SecretKey) -> Result<iroh::endpoint::Builder> {
    let builder = Endpoint::builder(presets::Minimal)
        .secret_key(key)
        .transport_config(transport_config()?);
    Ok(if opts.relay {
        builder.relay_mode(iroh::endpoint::default_relay_mode())
    } else {
        builder.relay_mode(RelayMode::Disabled)
    })
}

/// Picks a currently free UDP port (IPv4 and, when available, IPv6).
fn free_port() -> Result<u16> {
    for _ in 0..16 {
        let port = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?
            .local_addr()?
            .port();
        if std::net::UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port)).is_ok()
            || std::net::UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).is_err()
        {
            return Ok(port);
        }
    }
    bail!("no free UDP port")
}

/// Binds the PC endpoint on one UDP port for IPv4 and IPv6 (IPv6 optional), so the pairing
/// addresses stay valid across restarts. The port is persisted in `dir`.
pub(super) async fn bind_server(dir: &Path, key: SecretKey, opts: &NetOptions) -> Result<Endpoint> {
    let mk = |port: u16| -> Result<iroh::endpoint::Builder> {
        let mut b = base_builder(opts, key.clone())?.alpns(vec![ALPN.to_vec()]);
        if opts.discovery {
            b = b.address_lookup(iroh::address_lookup::PkarrPublisher::n0_dns());
        }
        Ok(b
            .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?
            .bind_addr_with_opts(
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
                BindOpts::default().set_is_required(false),
            )?)
    };
    let preferred = match opts.port {
        Some(0) => free_port()?,
        Some(p) => p,
        None => std::fs::read_to_string(dir.join(PORT_FILE))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(DEFAULT_PORT),
    };
    let endpoint = match mk(preferred)?.bind().await {
        Ok(ep) => ep,
        Err(err) if opts.port.is_none() => {
            let port = free_port()?;
            tracing::warn!("port {preferred} unavailable ({err}); using port {port}");
            mk(port)?.bind().await.context("binding endpoint")?
        }
        Err(err) => return Err(anyhow::Error::new(err).context("binding endpoint")),
    };
    if let Some(port) = v4_port(&endpoint) {
        if let Err(err) = std::fs::write(dir.join(PORT_FILE), port.to_string()) {
            tracing::warn!("saving port: {err}");
        }
    }
    Ok(endpoint)
}

/// Binds the phone endpoint (dial-only).
pub(super) async fn bind_client(key: SecretKey, opts: &NetOptions) -> Result<Endpoint> {
    let mut b = base_builder(opts, key)?;
    if opts.discovery {
        b = b
            .address_lookup(PkarrResolver::n0_dns())
            .address_lookup(DnsAddressLookup::n0_dns());
    }
    b.bind().await.context("binding endpoint")
}

fn v4_port(endpoint: &Endpoint) -> Option<u16> {
    endpoint
        .bound_sockets()
        .iter()
        .find(|a| a.is_ipv4())
        .map(|a| a.port())
}

/// Direct addresses for the pairing info, best first: (loopback), Tailscale IPv4, LAN IPv4,
/// iroh-discovered public addresses, other IPv4, Tailscale IPv6, global IPv6.
pub(super) fn gather_addrs(endpoint: &Endpoint, include_loopback: bool) -> Vec<SocketAddr> {
    let bound = endpoint.bound_sockets();
    let v4 = bound.iter().find(|a| a.is_ipv4()).map(|a| a.port());
    let v6 = bound.iter().find(|a| a.is_ipv6()).map(|a| a.port());
    let local: Vec<IpAddr> = match if_addrs::get_if_addrs() {
        Ok(ifs) => ifs.into_iter().map(|i| i.ip()).collect(),
        Err(err) => {
            tracing::warn!("enumerating interfaces: {err}");
            Vec::new()
        }
    };
    let iroh_addrs: Vec<SocketAddr> = endpoint.addr().ip_addrs().copied().collect();

    let mut ranked: Vec<(u8, SocketAddr)> = Vec::new();
    let mut add = |rank: u8, sa: SocketAddr| {
        if !ranked.iter().any(|(_, a)| *a == sa) {
            ranked.push((rank, sa));
        }
    };
    for ip in local {
        let port = match ip {
            IpAddr::V4(_) => v4,
            IpAddr::V6(_) => v6,
        };
        let Some(port) = port else { continue };
        let rank = match ip {
            IpAddr::V4(a) if a.is_loopback() => {
                if !include_loopback {
                    continue;
                }
                0
            }
            IpAddr::V4(a) if a.is_link_local() || a.is_unspecified() => continue,
            ip @ IpAddr::V4(_) if is_tailscale(ip) => 1,
            ip @ IpAddr::V4(_) if is_lan(ip) => 2,
            IpAddr::V4(_) => 4,
            ip if is_tailscale(ip) => 5,
            IpAddr::V6(a) if (a.segments()[0] & 0xe000) == 0x2000 => 6,
            IpAddr::V6(_) => continue,
        };
        add(rank, SocketAddr::new(ip, port));
    }
    for sa in iroh_addrs {
        let ip = sa.ip();
        if ip.is_loopback() || is_lan(ip) || is_tailscale(ip) {
            continue;
        }
        add(if ip.is_ipv4() { 3 } else { 6 }, sa);
    }
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, a)| a).take(MAX_ADDRS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let (k1, s1) = server_identity(dir.path()).unwrap();
        let (k2, s2) = server_identity(dir.path()).unwrap();
        assert_eq!(k1.public(), k2.public());
        assert_eq!(s1, s2);
        std::fs::write(dir.path().join(PAIRING_SECRET_FILE), b"short").unwrap();
        let (_, s3) = server_identity(dir.path()).unwrap();
        assert_ne!(s3, s1);
    }
}
