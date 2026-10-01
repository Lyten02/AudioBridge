//! Pure mapping from core `HubStatus` to the Kotlin-facing statusJson (v2), plus the listener notification gate.
//!
//! Schema (all keys always present):
//! `{"state","micEnabled","micCapturing","micWanted","pcAudioActive","peers":[{"id","name","state","path","rttMs",
//! "pcAudioEnabled","micEnabled","micDemanded","pcAudio":{...},"mic":{...},"error"}]}`

use std::time::{Duration, Instant};

use audiobridge_core::session::{ConnState, HubStatus, PathKind, Status, StreamStats};
use serde::Serialize;

/// Minimum spacing between two listener calls.
pub const MIN_NOTIFY_INTERVAL: Duration = Duration::from_millis(250);
/// Minimum spacing between listener calls caused only by statistics changes.
pub const STATS_NOTIFY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamView {
    pub active: bool,
    pub buffer_ms: f32,
    pub underruns: u64,
    pub lost: u64,
    pub kbps: f32,
}

impl StreamView {
    const IDLE: StreamView = StreamView { active: false, buffer_ms: 0.0, underruns: 0, lost: 0, kbps: 0.0 };

    fn from_stats(s: &StreamStats) -> Self {
        StreamView {
            active: s.active,
            buffer_ms: round1(s.buffer_ms),
            underruns: s.underruns,
            lost: s.lost_packets,
            kbps: round1(s.kbps),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub state: &'static str,
    pub path: Option<&'static str>,
    pub rtt_ms: Option<f32>,
    pub pc_audio_enabled: bool,
    pub mic_enabled: bool,
    pub mic_demanded: bool,
    pub pc_audio: StreamView,
    pub mic: StreamView,
    pub error: Option<String>,
}

impl PeerView {
    /// `pairing_name` is the PC name from the pairing URI, used until the PC introduces itself.
    pub fn from_status(id: &str, pairing_name: &str, s: &Status) -> Self {
        PeerView {
            id: id.to_owned(),
            name: s.peer_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| pairing_name.to_owned()),
            state: state_str(s.state),
            path: s.path.map(path_str),
            rtt_ms: s.rtt_ms.filter(|v| v.is_finite()).map(round1),
            pc_audio_enabled: s.pc_audio_enabled,
            mic_enabled: s.mic_enabled,
            mic_demanded: s.mic_demanded,
            pc_audio: StreamView::from_stats(&s.pc_audio),
            mic: StreamView::from_stats(&s.mic),
            error: s.last_error.clone(),
        }
    }

    /// A paired PC that has no connection task yet (the hub is still starting or failed to start).
    pub fn starting(id: &str, name: &str, error: Option<String>) -> Self {
        PeerView {
            id: id.to_owned(),
            name: name.to_owned(),
            state: "starting",
            path: None,
            rtt_ms: None,
            pc_audio_enabled: false,
            mic_enabled: false,
            mic_demanded: false,
            pc_audio: StreamView::IDLE,
            mic: StreamView::IDLE,
            error,
        }
    }

    /// Fields the UI reacts to immediately (everything except throughput/latency statistics).
    fn same_meaning(&self, o: &Self) -> bool {
        self.id == o.id
            && self.name == o.name
            && self.state == o.state
            && self.path == o.path
            && self.pc_audio_enabled == o.pc_audio_enabled
            && self.mic_enabled == o.mic_enabled
            && self.mic_demanded == o.mic_demanded
            && self.pc_audio.active == o.pc_audio.active
            && self.mic.active == o.mic.active
            && self.error == o.error
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    pub state: &'static str,
    pub mic_enabled: bool,
    pub mic_capturing: bool,
    pub mic_wanted: bool,
    pub pc_audio_active: bool,
    pub peers: Vec<PeerView>,
}

impl StatusView {
    /// No hub and no paired PCs.
    pub fn idle() -> Self {
        StatusView {
            state: "idle",
            mic_enabled: false,
            mic_capturing: false,
            mic_wanted: false,
            pc_audio_active: false,
            peers: Vec::new(),
        }
    }

    /// PCs are configured but the hub is not running yet; every peer is shown as starting.
    pub fn starting<'a>(
        peers: impl IntoIterator<Item = (&'a str, &'a str)>,
        mic_enabled: bool,
        error: Option<&str>,
    ) -> Self {
        StatusView {
            state: "running",
            mic_enabled,
            mic_capturing: false,
            mic_wanted: false,
            pc_audio_active: false,
            peers: peers.into_iter().map(|(id, name)| PeerView::starting(id, name, error.map(str::to_owned))).collect(),
        }
    }

    /// One entry per configured peer `(id, pairing pc_name)`, in configured order. A configured peer the hub
    /// does not report yet is shown as starting; peers the hub still reports after removal are omitted.
    pub fn from_hub<'a>(
        s: &HubStatus,
        configured: impl IntoIterator<Item = (&'a str, &'a str)>,
        mic_capturing: bool,
    ) -> Self {
        StatusView {
            state: "running",
            mic_enabled: s.mic_enabled,
            mic_capturing,
            mic_wanted: s.mic_wanted,
            pc_audio_active: s.pc_audio_active,
            peers: configured
                .into_iter()
                .map(|(id, name)| match s.peers.iter().find(|p| p.id == id) {
                    Some(p) => PeerView::from_status(id, name, &p.status),
                    None => PeerView::starting(id, name, None),
                })
                .collect(),
        }
    }

    pub fn to_json(&self) -> String {
        // Serializing plain strings/numbers/bools into a String cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }

    fn same_meaning(&self, o: &Self) -> bool {
        self.state == o.state
            && self.mic_enabled == o.mic_enabled
            && self.mic_capturing == o.mic_capturing
            && self.mic_wanted == o.mic_wanted
            && self.pc_audio_active == o.pc_audio_active
            && self.peers.len() == o.peers.len()
            && self.peers.iter().zip(&o.peers).all(|(a, b)| a.same_meaning(b))
    }
}

/// `parsePairing` result: `{"id":"<peer_id>","name":"<pc name>"}`.
#[derive(Serialize)]
pub struct PairingView<'a> {
    pub id: &'a str,
    pub name: &'a str,
}

impl PairingView<'_> {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

pub fn state_str(s: ConnState) -> &'static str {
    match s {
        ConnState::Starting => "starting",
        // Server-only state; a phone never reports it, but map it to its closest client meaning.
        ConnState::WaitingForPeer | ConnState::Connecting => "connecting",
        ConnState::Connected => "connected",
        ConnState::Reconnecting => "reconnecting",
        ConnState::Stopped => "stopped",
    }
}

pub fn path_str(p: PathKind) -> &'static str {
    match p {
        PathKind::Lan => "lan",
        PathKind::Tailscale => "tailscale",
        PathKind::Direct => "direct",
        PathKind::Relay => "relay",
    }
}

/// One decimal place; non-finite values become 0 so the JSON always carries a number.
fn round1(v: f32) -> f32 {
    if v.is_finite() {
        (v * 10.0).round() / 10.0
    } else {
        0.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    None,
    Stats,
    Meaningful,
}

pub fn classify(prev: &StatusView, next: &StatusView) -> Change {
    if !prev.same_meaning(next) {
        Change::Meaningful
    } else if prev != next {
        Change::Stats
    } else {
        Change::None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// Deliver now.
    Send,
    /// A deliverable change exists; deliver at this instant unless it is superseded.
    WaitUntil(Instant),
    /// Nothing worth delivering.
    Idle,
}

/// Decides when the listener may be called: never more than once per 250 ms, and for
/// statistics-only changes at most once per 2 s. Compares against the last delivered view.
#[derive(Debug, Default)]
pub struct NotifyGate {
    last: Option<(Instant, StatusView)>,
}

impl NotifyGate {
    pub fn poll(&self, now: Instant, next: &StatusView) -> Gate {
        let Some((sent_at, prev)) = &self.last else {
            return Gate::Send;
        };
        let interval = match classify(prev, next) {
            Change::None => return Gate::Idle,
            Change::Stats => STATS_NOTIFY_INTERVAL,
            Change::Meaningful => MIN_NOTIFY_INTERVAL,
        };
        let due = *sent_at + interval;
        if now >= due {
            Gate::Send
        } else {
            Gate::WaitUntil(due)
        }
    }

    pub fn mark_sent(&mut self, now: Instant, view: StatusView) {
        self.last = Some((now, view));
    }

    /// Forget the last delivery (a new listener must receive the current state immediately).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiobridge_core::session::PeerStatus;

    fn stats(active: bool, buffer_ms: f32, kbps: f32) -> StreamStats {
        StreamStats { active, buffer_ms, underruns: 3, lost_packets: 7, kbps }
    }

    fn connected(name: Option<&str>) -> Status {
        Status {
            state: ConnState::Connected,
            peer_name: name.map(str::to_owned),
            path: Some(PathKind::Tailscale),
            rtt_ms: Some(4.234),
            pc_audio_enabled: true,
            mic_enabled: true,
            mic_demanded: false,
            pc_audio: stats(true, 31.04, 190.46),
            mic: stats(false, 0.0, 0.0),
            last_error: None,
        }
    }

    fn hub(peers: Vec<(&str, Status)>) -> HubStatus {
        HubStatus {
            peers: peers.into_iter().map(|(id, status)| PeerStatus { id: id.into(), status }).collect(),
            mic_enabled: true,
            mic_wanted: false,
            pc_audio_active: true,
        }
    }

    const CONFIGURED: [(&str, &str); 2] = [("a", "DESKTOP"), ("b", "LAPTOP")];

    fn view(peers: Vec<(&str, Status)>) -> StatusView {
        StatusView::from_hub(&hub(peers), CONFIGURED, false)
    }

    #[test]
    fn hub_status_matches_schema() {
        let json: serde_json::Value =
            serde_json::from_str(&StatusView::from_hub(&hub(vec![("a", connected(Some("LYTEN")))]), [("a", "DESKTOP")], true).to_json())
                .unwrap();
        let expected = serde_json::json!({
            "state": "running",
            "micEnabled": true,
            "micCapturing": true,
            "micWanted": false,
            "pcAudioActive": true,
            "peers": [{
                "id": "a",
                "name": "LYTEN",
                "state": "connected",
                "path": "tailscale",
                "rttMs": 4.2,
                "pcAudioEnabled": true,
                "micEnabled": true,
                "micDemanded": false,
                "pcAudio": {"active": true, "bufferMs": 31.0, "underruns": 3, "lost": 7, "kbps": 190.5},
                "mic": {"active": false, "bufferMs": 0.0, "underruns": 3, "lost": 7, "kbps": 0.0},
                "error": null
            }]
        });
        // Compare as text: f32 values serialize with f32 precision (4.2f32 != 4.2f64).
        assert_eq!(json.to_string(), expected.to_string());
    }

    #[test]
    fn name_falls_back_to_pairing_name_until_welcome() {
        let v = StatusView::from_hub(
            &hub(vec![("b", connected(None)), ("c", connected(Some(""))), ("a", connected(Some("LYTEN")))]),
            [("b", "LAPTOP"), ("c", ""), ("a", "DESKTOP")],
            false,
        );
        let got: Vec<&str> = v.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(got, ["LAPTOP", "", "LYTEN"]);
    }

    #[test]
    fn peers_follow_configured_list() {
        // Hub reports a removed PC ("x") and has not picked up the new "b" yet; order comes from the config.
        let v = StatusView::from_hub(
            &hub(vec![("x", connected(Some("OLD"))), ("a", connected(Some("LYTEN")))]),
            [("b", "LAPTOP"), ("a", "DESKTOP")],
            false,
        );
        let got: Vec<(&str, &str, &str)> = v.peers.iter().map(|p| (p.id.as_str(), p.name.as_str(), p.state)).collect();
        assert_eq!(got, [("b", "LAPTOP", "starting"), ("a", "LYTEN", "connected")]);
        assert_eq!(v.peers[0].error, None);
    }

    #[test]
    fn idle_has_every_key() {
        let v: serde_json::Value = serde_json::from_str(&StatusView::idle().to_json()).unwrap();
        let obj = v.as_object().unwrap();
        for key in ["state", "micEnabled", "micCapturing", "micWanted", "pcAudioActive", "peers"] {
            assert!(obj.contains_key(key), "missing {key}");
        }
        assert_eq!(v["state"], "idle");
        assert_eq!(v["peers"], serde_json::json!([]));
    }

    #[test]
    fn starting_lists_configured_peers_with_error() {
        let v: serde_json::Value = serde_json::from_str(
            &StatusView::starting([("a", "DESKTOP"), ("b", "LAPTOP")], true, Some("bind failed")).to_json(),
        )
        .unwrap();
        assert_eq!(v["state"], "running");
        assert_eq!(v["micEnabled"], true);
        let peers = v["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[1]["name"], "LAPTOP");
        assert_eq!(peers[1]["state"], "starting");
        assert_eq!(peers[1]["error"], "bind failed");
        assert!(peers[0]["path"].is_null() && peers[0]["rttMs"].is_null());
        assert_eq!(peers[0]["pcAudio"]["active"], false);
    }

    #[test]
    fn pairing_json() {
        let v: serde_json::Value =
            serde_json::from_str(&PairingView { id: "abc", name: "LY\"TEN" }.to_json()).unwrap();
        assert_eq!(v, serde_json::json!({"id": "abc", "name": "LY\"TEN"}));
    }

    #[test]
    fn state_and_path_names() {
        let mut s = connected(None);
        for (state, name) in [
            (ConnState::Starting, "starting"),
            (ConnState::Connecting, "connecting"),
            (ConnState::WaitingForPeer, "connecting"),
            (ConnState::Reconnecting, "reconnecting"),
            (ConnState::Stopped, "stopped"),
        ] {
            s.state = state;
            assert_eq!(PeerView::from_status("a", "", &s).state, name);
        }
        for (path, name) in [(PathKind::Lan, "lan"), (PathKind::Direct, "direct"), (PathKind::Relay, "relay")] {
            s.path = Some(path);
            assert_eq!(PeerView::from_status("a", "", &s).path, Some(name));
        }
        s.path = None;
        assert_eq!(PeerView::from_status("a", "", &s).path, None);
    }

    #[test]
    fn non_finite_numbers_never_reach_json() {
        let mut s = connected(None);
        s.rtt_ms = Some(f32::NAN);
        s.pc_audio.kbps = f32::INFINITY;
        s.pc_audio.buffer_ms = f32::NAN;
        let v: serde_json::Value = serde_json::from_str(&view(vec![("a", s)]).to_json()).unwrap();
        assert!(v["peers"][0]["rttMs"].is_null());
        assert_eq!(v["peers"][0]["pcAudio"]["kbps"], 0.0);
        assert_eq!(v["peers"][0]["pcAudio"]["bufferMs"], 0.0);
    }

    #[test]
    fn classify_separates_stats_from_meaning() {
        let base = view(vec![("a", connected(Some("LYTEN"))), ("b", connected(None))]);
        assert_eq!(classify(&base, &base.clone()), Change::None);

        let mut stats_only = base.clone();
        stats_only.peers[1].rtt_ms = Some(9.0);
        stats_only.peers[0].pc_audio.kbps = 100.0;
        stats_only.peers[1].mic.underruns += 1;
        assert_eq!(classify(&base, &stats_only), Change::Stats);

        let edits: Vec<fn(&mut StatusView)> = vec![
            |v| v.mic_capturing = true,
            |v| v.mic_wanted = true,
            |v| v.pc_audio_active = false,
            |v| v.mic_enabled = false,
            |v| v.state = "idle",
            |v| v.peers.truncate(1),
            |v| v.peers.swap(0, 1),
            |v| v.peers[1].state = "reconnecting",
            |v| v.peers[1].path = Some("relay"),
            |v| v.peers[1].name = "X".into(),
            |v| v.peers[1].pc_audio_enabled = false,
            |v| v.peers[1].mic_enabled = false,
            |v| v.peers[1].mic_demanded = true,
            |v| v.peers[1].pc_audio.active = false,
            |v| v.peers[1].mic.active = true,
            |v| v.peers[1].error = Some("x".into()),
        ];
        for (i, edit) in edits.into_iter().enumerate() {
            let mut v = base.clone();
            edit(&mut v);
            assert_eq!(classify(&base, &v), Change::Meaningful, "edit #{i}");
        }
    }

    #[test]
    fn gate_debounces_meaningful_changes_to_250ms() {
        let t0 = Instant::now();
        let mut gate = NotifyGate::default();
        let a = StatusView::idle();
        assert_eq!(gate.poll(t0, &a), Gate::Send);
        gate.mark_sent(t0, a.clone());
        assert_eq!(gate.poll(t0 + Duration::from_millis(10), &a), Gate::Idle);

        let b = StatusView::starting([("a", "DESKTOP")], false, None);
        assert_eq!(gate.poll(t0 + Duration::from_millis(100), &b), Gate::WaitUntil(t0 + MIN_NOTIFY_INTERVAL));
        assert_eq!(gate.poll(t0 + MIN_NOTIFY_INTERVAL, &b), Gate::Send);
        // A change that reverted before delivery is not delivered.
        assert_eq!(gate.poll(t0 + Duration::from_secs(1), &a), Gate::Idle);
    }

    #[test]
    fn gate_limits_stats_changes_to_2s() {
        let t0 = Instant::now();
        let mut gate = NotifyGate::default();
        let a = view(vec![("a", connected(Some("LYTEN")))]);
        gate.mark_sent(t0, a.clone());
        let mut b = a.clone();
        b.peers[0].pc_audio.buffer_ms = 45.0;
        assert_eq!(gate.poll(t0 + Duration::from_millis(300), &b), Gate::WaitUntil(t0 + STATS_NOTIFY_INTERVAL));
        assert_eq!(gate.poll(t0 + STATS_NOTIFY_INTERVAL, &b), Gate::Send);
        // A meaningful change on top of stats is only held for the short interval.
        b.peers[0].mic_demanded = true;
        assert_eq!(gate.poll(t0 + Duration::from_millis(300), &b), Gate::Send);
    }

    #[test]
    fn gate_reset_sends_immediately() {
        let t0 = Instant::now();
        let mut gate = NotifyGate::default();
        let a = StatusView::idle();
        gate.mark_sent(t0, a.clone());
        gate.reset();
        assert_eq!(gate.poll(t0, &a), Gate::Send);
    }
}
