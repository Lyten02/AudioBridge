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
/// Control protocol version carried in `Hello`. v2: remote control (state reports + requests).
pub const PROTOCOL_VERSION: u16 = 2;

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

/// Highest volume level; levels are percent (`0..=100`).
pub const MAX_LEVEL: u8 = 100;

/// A device volume: level in percent plus mute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    pub level: u8,
    pub muted: bool,
}

/// PC-side controls. The PC owns them; the phone sees the last report of each PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcControls {
    /// PC audio is streamed to the phone.
    pub audio: bool,
    /// The PC accepts the phone mic (plays it into the virtual cable).
    pub mic: bool,
    /// The virtual mic ("CABLE Output") is the Windows default recording device.
    pub mic_default: bool,
    /// Default playback device volume; `None` while unknown.
    pub volume: Option<Volume>,
}

impl Default for PcControls {
    fn default() -> Self {
        Self { audio: true, mic: true, mic_default: false, volume: None }
    }
}

/// Phone-side controls. The phone owns them; the PC sees the connected phone's last report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhoneControls {
    /// The user's mic switch on the phone.
    pub mic: bool,
    /// The phone may record right now (permission + Android's foreground-service mic access).
    pub mic_ready: bool,
    /// Media volume in percent; `None` while unknown.
    pub volume: Option<u8>,
}

impl PhoneControls {
    /// The phone mic can be sent to PCs.
    pub fn mic_allowed(&self) -> bool {
        self.mic && self.mic_ready
    }
}

/// Phone → PC: change a PC-side control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcRequest {
    Audio(bool),
    /// One-switch mic: on = accept the phone mic and make the virtual mic the default
    /// recording device; off = stop and restore the previous recording device.
    Mic(bool),
    /// Only the Windows default recording device part of [`PcRequest::Mic`].
    MicDefault(bool),
    Volume(u8),
    Mute(bool),
}

/// PC → phone: change a phone-side control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneRequest {
    Mic(bool),
    Volume(u8),
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
        phone: PhoneControls,
    },
    /// PC → phone, accepted. Carries the PC-side controls.
    Welcome {
        pc_name: String,
        pc: PcControls,
        mic_demanded: bool,
    },
    /// PC → phone, refused; the connection is closed afterwards.
    Reject { reason: String },
    /// PC → phone: PC-side controls changed.
    PcState(PcControls),
    /// PC → phone: whether a PC application is capturing the virtual microphone.
    MicDemand(bool),
    /// Phone → PC: phone-side controls changed.
    PhoneState(PhoneControls),
    /// Phone → PC: remote-control request.
    SetPc(PcRequest),
    /// PC → phone: remote-control request.
    SetPhone(PhoneRequest),
}

const MAX_CONTROL_LEN: usize = 1024;
const MAX_NAME_LEN: usize = 128;
/// Wire value of an unknown volume level.
const LEVEL_UNKNOWN: u8 = 0xFF;

impl ControlMsg {
    /// Encodes the message including its length prefix.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(64);
        match self {
            ControlMsg::Hello {
                version,
                secret,
                device_name,
                phone,
            } => {
                body.push(1);
                body.extend_from_slice(&version.to_le_bytes());
                body.extend_from_slice(secret);
                put_str(&mut body, device_name);
                put_phone(&mut body, phone);
            }
            ControlMsg::Welcome {
                pc_name,
                pc,
                mic_demanded,
            } => {
                body.push(2);
                put_str(&mut body, pc_name);
                put_pc(&mut body, pc);
                body.push(*mic_demanded as u8);
            }
            ControlMsg::Reject { reason } => {
                body.push(3);
                put_str(&mut body, reason);
            }
            ControlMsg::PcState(pc) => {
                body.push(4);
                put_pc(&mut body, pc);
            }
            ControlMsg::MicDemand(on) => {
                body.push(5);
                body.push(*on as u8);
            }
            ControlMsg::PhoneState(phone) => {
                body.push(6);
                put_phone(&mut body, phone);
            }
            ControlMsg::SetPc(req) => {
                body.push(7);
                let (kind, value) = match *req {
                    PcRequest::Audio(on) => (0, on as u8),
                    PcRequest::Mic(on) => (1, on as u8),
                    PcRequest::MicDefault(on) => (2, on as u8),
                    PcRequest::Volume(level) => (3, level.min(MAX_LEVEL)),
                    PcRequest::Mute(on) => (4, on as u8),
                };
                body.extend_from_slice(&[kind, value]);
            }
            ControlMsg::SetPhone(req) => {
                body.push(8);
                let (kind, value) = match *req {
                    PhoneRequest::Mic(on) => (0, on as u8),
                    PhoneRequest::Volume(level) => (1, level.min(MAX_LEVEL)),
                };
                body.extend_from_slice(&[kind, value]);
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
            1 => {
                let version = u16::from_le_bytes(r.array()?);
                let secret = r.array()?;
                let device_name = r.string()?;
                // Other versions carry other fields; keep only what the PC needs to reject them
                // with a readable reason.
                let phone = if version == PROTOCOL_VERSION {
                    r.phone()?
                } else {
                    r.0 = &[];
                    PhoneControls::default()
                };
                ControlMsg::Hello { version, secret, device_name, phone }
            }
            2 => ControlMsg::Welcome {
                pc_name: r.string()?,
                pc: r.pc()?,
                mic_demanded: r.bool()?,
            },
            3 => ControlMsg::Reject { reason: r.string()? },
            4 => ControlMsg::PcState(r.pc()?),
            5 => ControlMsg::MicDemand(r.bool()?),
            6 => ControlMsg::PhoneState(r.phone()?),
            7 => ControlMsg::SetPc(match r.u8()? {
                0 => PcRequest::Audio(r.bool()?),
                1 => PcRequest::Mic(r.bool()?),
                2 => PcRequest::MicDefault(r.bool()?),
                3 => PcRequest::Volume(r.level()?),
                4 => PcRequest::Mute(r.bool()?),
                k => bail!("unknown PC request {k}"),
            }),
            8 => ControlMsg::SetPhone(match r.u8()? {
                0 => PhoneRequest::Mic(r.bool()?),
                1 => PhoneRequest::Volume(r.level()?),
                k => bail!("unknown phone request {k}"),
            }),
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

fn put_level(out: &mut Vec<u8>, level: Option<u8>) {
    out.push(level.map_or(LEVEL_UNKNOWN, |l| l.min(MAX_LEVEL)));
}

/// `audio | mic | mic_default | level (0xFF unknown) | muted` (all `u8`).
fn put_pc(out: &mut Vec<u8>, pc: &PcControls) {
    out.extend_from_slice(&[pc.audio as u8, pc.mic as u8, pc.mic_default as u8]);
    put_level(out, pc.volume.map(|v| v.level));
    out.push(pc.volume.is_some_and(|v| v.muted) as u8);
}

/// `mic | mic_ready | level (0xFF unknown)` (all `u8`).
fn put_phone(out: &mut Vec<u8>, phone: &PhoneControls) {
    out.extend_from_slice(&[phone.mic as u8, phone.mic_ready as u8]);
    put_level(out, phone.volume);
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
    fn level(&mut self) -> Result<u8> {
        let v = self.u8()?;
        ensure!(v <= MAX_LEVEL, "bad volume level {v}");
        Ok(v)
    }
    fn opt_level(&mut self) -> Result<Option<u8>> {
        match self.u8()? {
            LEVEL_UNKNOWN => Ok(None),
            v if v <= MAX_LEVEL => Ok(Some(v)),
            v => bail!("bad volume level {v}"),
        }
    }
    fn pc(&mut self) -> Result<PcControls> {
        let (audio, mic, mic_default) = (self.bool()?, self.bool()?, self.bool()?);
        let level = self.opt_level()?;
        let muted = self.bool()?;
        ensure!(level.is_some() || !muted, "mute without a volume");
        Ok(PcControls { audio, mic, mic_default, volume: level.map(|level| Volume { level, muted }) })
    }
    fn phone(&mut self) -> Result<PhoneControls> {
        Ok(PhoneControls { mic: self.bool()?, mic_ready: self.bool()?, volume: self.opt_level()? })
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
        let pc = PcControls {
            audio: true,
            mic: false,
            mic_default: true,
            volume: Some(Volume { level: 42, muted: true }),
        };
        let phone = PhoneControls { mic: true, mic_ready: false, volume: Some(100) };
        let msgs = [
            ControlMsg::Hello {
                version: PROTOCOL_VERSION,
                secret: [7; 16],
                device_name: "POCO F5".into(),
                phone,
            },
            ControlMsg::Welcome {
                pc_name: "LYTEN".into(),
                pc,
                mic_demanded: true,
            },
            ControlMsg::Reject {
                reason: "bad secret".into(),
            },
            ControlMsg::PcState(PcControls::default()),
            ControlMsg::PcState(pc),
            ControlMsg::MicDemand(true),
            ControlMsg::PhoneState(PhoneControls::default()),
            ControlMsg::PhoneState(phone),
            ControlMsg::SetPc(PcRequest::Audio(false)),
            ControlMsg::SetPc(PcRequest::Mic(true)),
            ControlMsg::SetPc(PcRequest::MicDefault(false)),
            ControlMsg::SetPc(PcRequest::Volume(0)),
            ControlMsg::SetPc(PcRequest::Mute(true)),
            ControlMsg::SetPhone(PhoneRequest::Mic(false)),
            ControlMsg::SetPhone(PhoneRequest::Volume(100)),
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
        // volume levels above 100 and unknown request kinds are rejected
        assert!(ControlMsg::decode(&[7, 3, 101]).is_err());
        assert!(ControlMsg::decode(&[7, 9, 0]).is_err());
        assert!(ControlMsg::decode(&[8, 1, 0xFF]).is_err());
        assert!(ControlMsg::decode(&[6, 1, 1, 101]).is_err());
        // mute without a known volume is not canonical
        assert!(ControlMsg::decode(&[4, 1, 1, 0, 0xFF, 1]).is_err());
        assert_eq!(ControlMsg::decode(&[4, 1, 1, 0, 0xFF, 0]).unwrap(), ControlMsg::PcState(PcControls::default()));
    }

    #[test]
    fn hello_of_another_version_still_decodes_for_a_readable_reject() {
        // v1: tag | version | secret | name | mic_allowed
        let mut v1 = vec![1, 1, 0];
        v1.extend_from_slice(&[7; 16]);
        v1.extend_from_slice(&[2, b'P', b'5', 1]);
        let ControlMsg::Hello { version, device_name, phone, .. } = ControlMsg::decode(&v1).unwrap() else {
            panic!("not a Hello");
        };
        assert_eq!((version, device_name.as_str(), phone), (1, "P5", PhoneControls::default()));
    }
}
