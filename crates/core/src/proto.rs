//! Wire protocol: constants, audio datagram header and control-stream messages.

use anyhow::{bail, ensure, Context, Result};

/// QUIC ALPN identifying the AudioBridge protocol.
pub const ALPN: &[u8] = b"audiobridge/1";
/// Sample rate used everywhere at the handle boundary and on the wire.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in one 10 ms Opus frame.
pub const FRAME_SAMPLES: usize = 480;
/// Preferred UDP port of the PC endpoint (IPv4). Falls back to a random port when busy.
pub const DEFAULT_PORT: u16 = 47130;
/// Control protocol version carried in `Hello`.
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest Opus payload sent or accepted. The encoder is capped to it (≈ 512 kbit/s per 10 ms
/// frame, far above the configured bitrates), so jitter-buffer slots stay small.
pub const MAX_AUDIO_PAYLOAD: usize = 640;

const DATAGRAM_MAGIC: u8 = 0xAB;
/// Size of [`DatagramHeader`] on the wire.
pub const DATAGRAM_HEADER_LEN: usize = 8;

/// Media stream carried by a datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamId {
    /// PC system audio, PC → phone, stereo.
    PcAudio,
    /// Phone microphone, phone → PC, mono.
    Mic,
}

impl StreamId {
    /// Channel count of this stream.
    pub fn channels(self) -> usize {
        match self {
            StreamId::PcAudio => 2,
            StreamId::Mic => 1,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            StreamId::PcAudio => 0,
            StreamId::Mic => 1,
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(StreamId::PcAudio),
            1 => Some(StreamId::Mic),
            _ => None,
        }
    }
}

/// Datagram kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketKind {
    /// One 10 ms Opus frame follows the header.
    Audio,
    /// The sender went quiet (digital silence); no payload. `seq` is the next audio seq.
    Silence,
    /// Phone → PC keep-awake packet (no payload) that makes the phone transmit regularly so its
    /// Wi-Fi radio stays out of power save while it receives audio. Ignored by the receiver.
    Pace,
}

/// 8-byte header in front of every audio datagram:
/// `magic u8 | kind u8 | stream u8 | reserved u8 (0) | seq u32 LE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatagramHeader {
    pub kind: PacketKind,
    pub stream: StreamId,
    pub seq: u32,
}

impl DatagramHeader {
    /// Writes the header into the first [`DATAGRAM_HEADER_LEN`] bytes of `out`.
    pub fn write(&self, out: &mut [u8]) {
        out[0] = DATAGRAM_MAGIC;
        out[1] = match self.kind {
            PacketKind::Audio => 0,
            PacketKind::Silence => 1,
            PacketKind::Pace => 2,
        };
        out[2] = self.stream.to_u8();
        out[3] = 0;
        out[4..8].copy_from_slice(&self.seq.to_le_bytes());
    }

    /// Parses and validates a datagram; returns the header and the payload.
    pub fn parse(data: &[u8]) -> Result<(DatagramHeader, &[u8])> {
        ensure!(data.len() >= DATAGRAM_HEADER_LEN, "datagram too short");
        ensure!(data[0] == DATAGRAM_MAGIC, "bad datagram magic");
        ensure!(data[3] == 0, "bad reserved byte");
        let kind = match data[1] {
            0 => PacketKind::Audio,
            1 => PacketKind::Silence,
            2 => PacketKind::Pace,
            k => bail!("unknown datagram kind {k}"),
        };
        let stream = StreamId::from_u8(data[2]).context("unknown stream id")?;
        let seq = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let payload = &data[DATAGRAM_HEADER_LEN..];
        match kind {
            PacketKind::Audio => {
                ensure!(!payload.is_empty(), "empty audio payload");
                ensure!(payload.len() <= MAX_AUDIO_PAYLOAD, "audio payload too large");
            }
            PacketKind::Silence | PacketKind::Pace => {
                ensure!(payload.is_empty(), "marker with payload")
            }
        }
        Ok((DatagramHeader { kind, stream, seq }, payload))
    }
}

/// Messages on the single bi-directional control stream (phone opens it).
/// Framing: `len u16 LE | tag u8 | body`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlMsg {
    /// Phone → PC, first message.
    Hello {
        version: u16,
        secret: [u8; 16],
        device_name: String,
        mic_allowed: bool,
    },
    /// PC → phone, accepted. Carries the PC-side toggles.
    Welcome {
        pc_name: String,
        pc_audio_enabled: bool,
        mic_enabled: bool,
        mic_demanded: bool,
    },
    /// PC → phone, refused; the connection is closed afterwards.
    Reject { reason: String },
    /// PC → phone: PC-side stream toggles changed.
    Toggles { pc_audio_enabled: bool, mic_enabled: bool },
    /// PC → phone: whether a PC application is capturing the virtual microphone.
    MicDemand(bool),
    /// Phone → PC: phone-side microphone permission/toggle changed.
    MicAllowed(bool),
}

const MAX_CONTROL_LEN: usize = 1024;
const MAX_NAME_LEN: usize = 128;

impl ControlMsg {
    /// Encodes the message including its length prefix.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(64);
        match self {
            ControlMsg::Hello {
                version,
                secret,
                device_name,
                mic_allowed,
            } => {
                body.push(1);
                body.extend_from_slice(&version.to_le_bytes());
                body.extend_from_slice(secret);
                put_str(&mut body, device_name);
                body.push(*mic_allowed as u8);
            }
            ControlMsg::Welcome {
                pc_name,
                pc_audio_enabled,
                mic_enabled,
                mic_demanded,
            } => {
                body.push(2);
                put_str(&mut body, pc_name);
                body.push(*pc_audio_enabled as u8);
                body.push(*mic_enabled as u8);
                body.push(*mic_demanded as u8);
            }
            ControlMsg::Reject { reason } => {
                body.push(3);
                put_str(&mut body, reason);
            }
            ControlMsg::Toggles {
                pc_audio_enabled,
                mic_enabled,
            } => {
                body.push(4);
                body.push(*pc_audio_enabled as u8);
                body.push(*mic_enabled as u8);
            }
            ControlMsg::MicDemand(on) => {
                body.push(5);
                body.push(*on as u8);
            }
            ControlMsg::MicAllowed(on) => {
                body.push(6);
                body.push(*on as u8);
            }
        }
        let mut out = Vec::with_capacity(body.len() + 2);
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Decodes one message body (without the length prefix).
    pub fn decode(body: &[u8]) -> Result<ControlMsg> {
        let mut r = Reader(body);
        let msg = match r.u8()? {
            1 => ControlMsg::Hello {
                version: u16::from_le_bytes(r.array()?),
                secret: r.array()?,
                device_name: r.string()?,
                mic_allowed: r.bool()?,
            },
            2 => ControlMsg::Welcome {
                pc_name: r.string()?,
                pc_audio_enabled: r.bool()?,
                mic_enabled: r.bool()?,
                mic_demanded: r.bool()?,
            },
            3 => ControlMsg::Reject { reason: r.string()? },
            4 => ControlMsg::Toggles {
                pc_audio_enabled: r.bool()?,
                mic_enabled: r.bool()?,
            },
            5 => ControlMsg::MicDemand(r.bool()?),
            6 => ControlMsg::MicAllowed(r.bool()?),
            t => bail!("unknown control message tag {t}"),
        };
        ensure!(r.0.is_empty(), "trailing bytes in control message");
        Ok(msg)
    }

    /// Writes one framed message to a QUIC send stream.
    pub async fn write_to(&self, send: &mut iroh::endpoint::SendStream) -> Result<()> {
        send.write_all(&self.encode()).await?;
        Ok(())
    }

    /// Reads one framed message from a QUIC receive stream.
    pub async fn read_from(recv: &mut iroh::endpoint::RecvStream) -> Result<ControlMsg> {
        let mut len = [0u8; 2];
        recv.read_exact(&mut len).await?;
        let len = u16::from_le_bytes(len) as usize;
        ensure!(len > 0 && len <= MAX_CONTROL_LEN, "bad control message length {len}");
        let mut body = vec![0u8; len];
        recv.read_exact(&mut body).await?;
        ControlMsg::decode(&body)
    }
}

/// Truncates `s` to at most `max` bytes on a char boundary.
pub(crate) fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let s = truncate_utf8(s, MAX_NAME_LEN);
    out.push(s.len() as u8);
    out.extend_from_slice(s.as_bytes());
}

pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "truncated message");
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => bail!("bad bool {v}"),
        }
    }
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }
    pub(crate) fn string(&mut self) -> Result<String> {
        let len = self.u8()? as usize;
        Ok(std::str::from_utf8(self.take(len)?)
            .context("invalid utf-8")?
            .to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = DatagramHeader {
            kind: PacketKind::Audio,
            stream: StreamId::Mic,
            seq: 0xDEAD_BEEF,
        };
        let mut buf = [0u8; DATAGRAM_HEADER_LEN + 3];
        h.write(&mut buf);
        buf[DATAGRAM_HEADER_LEN..].copy_from_slice(&[1, 2, 3]);
        let (parsed, payload) = DatagramHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn header_rejects_garbage() {
        let mut good = [0u8; DATAGRAM_HEADER_LEN + 1];
        DatagramHeader {
            kind: PacketKind::Audio,
            stream: StreamId::PcAudio,
            seq: 1,
        }
        .write(&mut good);
        assert!(DatagramHeader::parse(&good).is_ok());

        assert!(DatagramHeader::parse(&[]).is_err());
        assert!(DatagramHeader::parse(&good[..5]).is_err());
        // audio without payload
        assert!(DatagramHeader::parse(&good[..DATAGRAM_HEADER_LEN]).is_err());
        for (idx, val) in [(0usize, 0x00u8), (1, 7), (2, 9), (3, 1)] {
            let mut bad = good;
            bad[idx] = val;
            assert!(DatagramHeader::parse(&bad).is_err(), "byte {idx}={val} accepted");
        }
        // markers (silence, pace) must not carry a payload
        for kind in [1u8, 2] {
            let mut marker = good;
            marker[1] = kind;
            assert!(DatagramHeader::parse(&marker).is_err());
            assert!(DatagramHeader::parse(&marker[..DATAGRAM_HEADER_LEN]).is_ok());
        }
        // payload above the cap; the cap itself is fine
        let mut big = vec![0u8; DATAGRAM_HEADER_LEN + MAX_AUDIO_PAYLOAD + 1];
        big[..DATAGRAM_HEADER_LEN].copy_from_slice(&good[..DATAGRAM_HEADER_LEN]);
        assert!(DatagramHeader::parse(&big).is_err());
        assert!(DatagramHeader::parse(&big[..DATAGRAM_HEADER_LEN + MAX_AUDIO_PAYLOAD]).is_ok());
    }

    #[test]
    fn control_roundtrip_and_garbage() {
        let msgs = [
            ControlMsg::Hello {
                version: PROTOCOL_VERSION,
                secret: [7; 16],
                device_name: "POCO F5".into(),
                mic_allowed: true,
            },
            ControlMsg::Welcome {
                pc_name: "LYTEN".into(),
                pc_audio_enabled: true,
                mic_enabled: false,
                mic_demanded: true,
            },
            ControlMsg::Reject {
                reason: "bad secret".into(),
            },
            ControlMsg::Toggles {
                pc_audio_enabled: false,
                mic_enabled: true,
            },
            ControlMsg::MicDemand(true),
            ControlMsg::MicAllowed(false),
        ];
        for m in msgs {
            let enc = m.encode();
            let len = u16::from_le_bytes([enc[0], enc[1]]) as usize;
            assert_eq!(len, enc.len() - 2);
            assert_eq!(ControlMsg::decode(&enc[2..]).unwrap(), m);
            // truncated and trailing garbage are rejected
            assert!(ControlMsg::decode(&enc[2..enc.len() - 1]).is_err());
            let mut extra = enc[2..].to_vec();
            extra.push(0);
            assert!(ControlMsg::decode(&extra).is_err());
        }
        assert!(ControlMsg::decode(&[99]).is_err());
        assert!(ControlMsg::decode(&[5, 2]).is_err());
    }
}
