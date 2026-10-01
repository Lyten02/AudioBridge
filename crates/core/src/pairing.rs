//! QR pairing payload: everything a phone needs to dial the PC.
//!
//! Binary layout (version 1), base64url (no padding) in `audiobridge://pair?d=...`:
//! `ver u8 | endpoint id [32] | secret [16] | name_len u8 | name | n_addrs u8 |
//!  n × (family u8 (4|6) | ip [4|16] | port u16 LE) | relay_len u8 | relay url`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{bail, ensure, Context, Result};
use iroh::{EndpointAddr, EndpointId, RelayUrl, TransportAddr};

use crate::proto::{truncate_utf8, Reader};

const URI_PREFIX: &str = "audiobridge://pair?d=";
const FORMAT_VERSION: u8 = 1;
const MAX_PC_NAME: usize = 48;
/// Upper bound on direct addresses carried in the QR.
pub const MAX_ADDRS: usize = 6;

/// Content of the QR code. Opaque to platform code except `pc_name()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingInfo {
    endpoint_id: EndpointId,
    secret: [u8; 16],
    pc_name: String,
    addrs: Vec<SocketAddr>,
    relay_url: Option<RelayUrl>,
}

impl PairingInfo {
    /// Builds pairing info. `pc_name` is truncated to 48 bytes and `addrs` to [`MAX_ADDRS`].
    pub fn new(
        endpoint_id: EndpointId,
        secret: [u8; 16],
        pc_name: &str,
        addrs: Vec<SocketAddr>,
        relay_url: Option<RelayUrl>,
    ) -> Self {
        let mut addrs = addrs;
        addrs.truncate(MAX_ADDRS);
        Self {
            endpoint_id,
            secret,
            pc_name: truncate_utf8(pc_name, MAX_PC_NAME).to_owned(),
            addrs,
            relay_url,
        }
    }

    /// `audiobridge://pair?d=<base64url>`.
    pub fn to_uri(&self) -> String {
        format!(
            "{URI_PREFIX}{}",
            data_encoding::BASE64URL_NOPAD.encode(&self.to_bytes())
        )
    }

    /// Parses a URI produced by [`PairingInfo::to_uri`].
    pub fn from_uri(s: &str) -> Result<Self> {
        let data = s
            .trim()
            .strip_prefix(URI_PREFIX)
            .context("not an AudioBridge pairing code")?;
        let bytes = data_encoding::BASE64URL_NOPAD
            .decode(data.as_bytes())
            .context("pairing code is not valid base64url")?;
        Self::from_bytes(&bytes)
    }

    pub fn pc_name(&self) -> &str {
        &self.pc_name
    }

    /// Stable identifier of the PC (its endpoint id as a string).
    pub fn peer_id(&self) -> String {
        self.endpoint_id.to_string()
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint_id
    }

    pub fn secret(&self) -> &[u8; 16] {
        &self.secret
    }

    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    pub fn relay_url(&self) -> Option<&RelayUrl> {
        self.relay_url.as_ref()
    }

    /// Full iroh address (direct addrs + relay) for dialing.
    pub fn endpoint_addr(&self) -> EndpointAddr {
        EndpointAddr::from_parts(
            self.endpoint_id,
            self.addrs
                .iter()
                .copied()
                .map(TransportAddr::Ip)
                .chain(self.relay_url.iter().cloned().map(TransportAddr::Relay)),
        )
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        out.push(FORMAT_VERSION);
        out.extend_from_slice(self.endpoint_id.as_bytes());
        out.extend_from_slice(&self.secret);
        out.push(self.pc_name.len() as u8);
        out.extend_from_slice(self.pc_name.as_bytes());
        out.push(self.addrs.len() as u8);
        for addr in &self.addrs {
            match addr.ip() {
                IpAddr::V4(ip) => {
                    out.push(4);
                    out.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    out.push(6);
                    out.extend_from_slice(&ip.octets());
                }
            }
            out.extend_from_slice(&addr.port().to_le_bytes());
        }
        match &self.relay_url {
            Some(url) => {
                let s = url.to_string();
                out.push(s.len() as u8);
                out.extend_from_slice(s.as_bytes());
            }
            None => out.push(0),
        }
        out
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(bytes);
        let ver = r.u8()?;
        ensure!(ver == FORMAT_VERSION, "unsupported pairing format {ver}");
        let endpoint_id =
            EndpointId::from_bytes(&r.array::<32>()?).context("invalid endpoint id")?;
        let secret = r.array::<16>()?;
        let pc_name = r.string()?;
        let n = r.u8()? as usize;
        ensure!(n <= MAX_ADDRS, "too many addresses");
        let mut addrs = Vec::with_capacity(n);
        for _ in 0..n {
            let ip = match r.u8()? {
                4 => IpAddr::V4(Ipv4Addr::from(r.array::<4>()?)),
                6 => IpAddr::V6(Ipv6Addr::from(r.array::<16>()?)),
                f => bail!("bad address family {f}"),
            };
            let port = u16::from_le_bytes(r.array()?);
            addrs.push(SocketAddr::new(ip, port));
        }
        let relay = r.string()?;
        let relay_url = if relay.is_empty() {
            None
        } else {
            Some(relay.parse::<RelayUrl>().context("invalid relay url")?)
        };
        ensure!(r.0.is_empty(), "trailing bytes in pairing code");
        Ok(Self {
            endpoint_id,
            secret,
            pc_name,
            addrs,
            relay_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n_addrs: usize, name: &str) -> PairingInfo {
        let key = iroh::SecretKey::from_bytes(&[3u8; 32]);
        let mut addrs = vec![
            "100.64.0.10:47130".parse().unwrap(),
            "192.168.1.20:47130".parse().unwrap(),
            "[fd7a:115c:a1e0::1234:5678]:47130".parse().unwrap(),
        ];
        while addrs.len() < n_addrs {
            addrs.push(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(0x2a02, 0x6b8, 1, 2, 3, 4, 5, addrs.len() as u16)),
                47131,
            ));
        }
        addrs.truncate(n_addrs);
        PairingInfo::new(
            key.public(),
            [0x5A; 16],
            name,
            addrs,
            Some("https://euc1-1.relay.n0.iroh-canary.iroh.link./".parse().unwrap()),
        )
    }

    #[test]
    fn uri_roundtrip() {
        let p = sample(3, "LYTEN");
        let uri = p.to_uri();
        assert!(uri.starts_with("audiobridge://pair?d="));
        let back = PairingInfo::from_uri(&uri).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.pc_name(), "LYTEN");
        let addr = back.endpoint_addr();
        assert_eq!(addr.ip_addrs().count(), 3);
        assert_eq!(addr.relay_urls().count(), 1);
    }

    #[test]
    fn worst_case_uri_fits_qr() {
        // Maximum address count (all IPv6) and a maximum-length non-ASCII name.
        let p = sample(MAX_ADDRS, &"Компьютер-Константина".repeat(5));
        assert_eq!(p.addrs().len(), MAX_ADDRS);
        assert!(p.pc_name().len() <= MAX_PC_NAME);
        let uri = p.to_uri();
        assert!(uri.len() <= 400, "uri too long: {}", uri.len());
        assert_eq!(PairingInfo::from_uri(&uri).unwrap(), p);
        // Typical PC: Tailscale + LAN + one IPv6.
        let typical = sample(3, "LYTEN").to_uri();
        assert!(typical.len() <= 230, "typical uri too long: {}", typical.len());
    }

    #[test]
    fn rejects_garbage() {
        assert!(PairingInfo::from_uri("").is_err());
        assert!(PairingInfo::from_uri("https://example.com").is_err());
        assert!(PairingInfo::from_uri("audiobridge://pair?d=!!!").is_err());
        assert!(PairingInfo::from_uri("audiobridge://pair?d=AQID").is_err());
        let uri = sample(2, "PC").to_uri();
        assert!(PairingInfo::from_uri(&uri[..uri.len() - 4]).is_err());
        assert!(PairingInfo::from_uri(&format!("{uri}AA")).is_err());
    }
}
