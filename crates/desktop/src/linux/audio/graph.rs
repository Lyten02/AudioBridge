//! What the registry and the `default` metadata tell about the PipeWire graph, and how that maps
//! to the platform-neutral [`DeviceSummary`].

use std::collections::BTreeMap;

use crate::endpoints::{DeviceSummary, EndpointInfo};

use super::MIC_NODE_NAME;

/// A node of interest: an audio device (or a virtual device) apps can play to or record from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeEntry {
    pub name: String,
    pub description: String,
    pub class: String,
}

impl NodeEntry {
    fn is_sink(&self) -> bool {
        matches!(self.class.as_str(), "Audio/Sink" | "Audio/Duplex")
    }

    fn is_source(&self) -> bool {
        matches!(self.class.as_str(), "Audio/Source" | "Audio/Source/Virtual" | "Audio/Duplex")
    }

    fn label(&self) -> String {
        if self.description.is_empty() { self.name.clone() } else { self.description.clone() }
    }
}

/// `default.*` keys of the `default` metadata object (values are node names).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    /// `default.audio.sink`: the sink the session manager actually uses.
    pub sink: Option<String>,
    /// `default.audio.source`: the source the session manager actually uses.
    pub source: Option<String>,
    /// `default.configured.audio.source`: the user's choice (may name an absent node).
    pub configured_source: Option<String>,
}

impl Defaults {
    /// Applies one metadata property; returns whether it was one of ours. `value` is `None`
    /// when the key is removed.
    pub fn apply(&mut self, key: &str, value: Option<&str>) -> bool {
        let slot = match key {
            "default.audio.sink" => &mut self.sink,
            "default.audio.source" => &mut self.source,
            "default.configured.audio.source" => &mut self.configured_source,
            _ => return false,
        };
        *slot = value.and_then(node_name_from_json);
        true
    }
}

/// Values of the `default` metadata are JSON objects: `{ "name": "alsa_output.pci-…" }`.
pub fn node_name_from_json(value: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;
    parsed.get("name")?.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// The JSON value for `default.configured.audio.{sink,source}`.
pub fn node_name_json(name: &str) -> String {
    serde_json::json!({ "name": name }).to_string()
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    /// Audio device nodes by global id.
    pub nodes: BTreeMap<u32, NodeEntry>,
    pub defaults: Defaults,
    /// Links by global id, including readers (which are not device nodes).
    links: BTreeMap<u32, (u32, u32)>,
}

impl Graph {
    /// Records a node global if it is an audio device; others are ignored.
    pub fn add_node(&mut self, id: u32, name: Option<&str>, description: Option<&str>, class: Option<&str>) {
        let (Some(name), Some(class)) = (name, class) else { return };
        let entry = NodeEntry { name: name.to_owned(), description: description.unwrap_or_default().to_owned(), class: class.to_owned() };
        if entry.is_sink() || entry.is_source() {
            self.nodes.insert(id, entry);
        }
    }

    pub fn add_link(&mut self, id: u32, output: Option<&str>, input: Option<&str>) {
        if let (Some(output), Some(input)) = (
            output.and_then(|v| v.parse().ok()),
            input.and_then(|v| v.parse().ok()),
        ) {
            self.links.insert(id, (output, input));
        }
    }

    pub fn remove(&mut self, id: u32) -> bool {
        self.links.remove(&id);
        self.links.retain(|_, (output, input)| *output != id && *input != id);
        self.nodes.remove(&id).is_some()
    }

    /// A source adapter can stay Streaming after its last consumer is unlinked.
    /// Demand therefore also requires a live outgoing graph link, not just stream state.
    pub fn mic_has_readers(&self) -> bool {
        self.find(MIC_NODE_NAME).is_some_and(|(mic, _)| {
            self.links.values().any(|(output, _)| *output == mic)
        })
    }

    pub fn find(&self, name: &str) -> Option<(u32, &NodeEntry)> {
        self.nodes.iter().find(|(_, n)| n.name == name).map(|(id, n)| (*id, n))
    }

    /// The sink the session manager uses for playback (what the loopback captures), if present.
    pub fn default_sink(&self) -> Option<(u32, &NodeEntry)> {
        let name = self.defaults.sink.as_deref()?;
        self.find(name).filter(|(_, n)| n.is_sink())
    }

    pub fn summary(&self) -> DeviceSummary {
        let sinks = self.nodes.values().filter(|n| n.is_sink() && n.name != MIC_NODE_NAME);
        let sources = self.nodes.values().filter(|n| n.is_source());
        let render_endpoints: Vec<_> =
            sinks.map(|n| EndpointInfo { id: n.name.clone(), name: n.label(), is_cable: false }).collect();
        let capture_endpoints: Vec<_> = sources
            .map(|n| EndpointInfo { id: n.name.clone(), name: n.label(), is_cable: n.name == MIC_NODE_NAME })
            .collect();
        let sink = self.default_sink().map(|(_, n)| n);
        // The user's choice wins: it is what comes back when that device (or our mic) appears.
        let capture_id = self.defaults.configured_source.clone().or_else(|| self.defaults.source.clone());
        let capture_name = capture_id.as_deref().map(|id| {
            self.find(id).map_or_else(|| id.to_owned(), |(_, n)| n.label())
        });
        let mic_is_default = [&self.defaults.configured_source, &self.defaults.source]
            .into_iter()
            .any(|n| n.as_deref() == Some(MIC_NODE_NAME));
        DeviceSummary {
            default_render_id: sink.map(|n| n.name.clone()),
            default_render_name: sink.map(NodeEntry::label),
            // Our virtual mic is a source; it can never become the playback default.
            default_is_cable: false,
            // The virtual mic is AudioBridge's own node: always available while the service runs,
            // and a default set to it applies as soon as it appears.
            cable_render_id: Some(MIC_NODE_NAME.to_owned()),
            render_endpoints,
            default_capture_id: capture_id,
            default_capture_name: capture_name,
            default_capture_is_cable: mic_is_default,
            cable_capture_id: Some(MIC_NODE_NAME.to_owned()),
            capture_endpoints,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Graph {
        let mut g = Graph::default();
        g.add_node(40, Some("alsa_output.speakers"), Some("Speakers"), Some("Audio/Sink"));
        g.add_node(41, Some("alsa_input.mic"), Some("Built-in Mic"), Some("Audio/Source"));
        g.add_node(42, Some(MIC_NODE_NAME), Some("AudioBridge"), Some("Audio/Source/Virtual"));
        g.add_node(43, Some("firefox"), None, Some("Stream/Output/Audio"));
        g.defaults.apply("default.audio.sink", Some(r#"{"name":"alsa_output.speakers"}"#));
        g.defaults.apply("default.audio.source", Some(r#"{"name":"alsa_input.mic"}"#));
        g
    }

    #[test]
    fn streams_are_not_endpoints_and_our_mic_is_marked() {
        let s = graph().summary();
        assert_eq!(s.render_endpoints.len(), 1);
        assert_eq!(s.default_render_id.as_deref(), Some("alsa_output.speakers"));
        assert_eq!(s.default_render_name.as_deref(), Some("Speakers"));
        assert!(!s.default_capture_is_cable);
        let cable: Vec<_> = s.capture_endpoints.iter().filter(|e| e.is_cable).map(|e| e.id.as_str()).collect();
        assert_eq!(cable, [MIC_NODE_NAME]);
        assert_eq!(s.capture_restore_candidate(None).map(|e| e.id.as_str()), Some("alsa_input.mic"));
    }

    #[test]
    fn configured_mic_counts_as_default_even_while_absent() {
        let mut g = graph();
        g.remove(42);
        g.defaults.apply("default.configured.audio.source", Some(&node_name_json(MIC_NODE_NAME)));
        let s = g.summary();
        // WirePlumber fell back to the real mic, but the user's choice is still the virtual mic:
        // turning AudioBridge off must restore the real one as the configured default.
        assert!(s.default_capture_is_cable);
        assert_eq!(s.default_capture_id.as_deref(), Some(MIC_NODE_NAME));
        assert_eq!(s.capture_restore_candidate(Some("alsa_input.mic")).map(|e| e.id.as_str()), Some("alsa_input.mic"));
    }

    #[test]
    fn missing_default_sink_means_no_loopback_source() {
        let mut g = graph();
        g.remove(40);
        assert!(g.summary().default_render_id.is_none());
        g.defaults.apply("default.audio.sink", None);
        assert!(g.default_sink().is_none());
    }

    #[test]
    fn mic_demand_ends_only_when_the_last_reader_link_is_removed() {
        let mut g = graph();
        g.add_link(60, Some("41"), Some("50")); // unrelated microphone
        assert!(!g.mic_has_readers());
        g.add_link(61, Some("42"), Some("51"));
        g.add_link(62, Some("42"), Some("52"));
        assert!(g.mic_has_readers());
        g.remove(61);
        assert!(g.mic_has_readers());
        g.remove(62);
        assert!(!g.mic_has_readers());
        g.add_link(63, Some("42"), Some("53"));
        assert!(g.mic_has_readers());
        g.remove(53); // reader node removal can precede its link removal
        assert!(!g.mic_has_readers());
    }

    #[test]
    fn removed_mic_and_reused_node_ids_do_not_keep_demand() {
        let mut g = graph();
        g.add_link(60, Some("42"), Some("51"));
        assert!(g.mic_has_readers());
        g.remove(42);
        assert!(!g.mic_has_readers());
        g.add_node(42, Some(MIC_NODE_NAME), None, Some("Audio/Source"));
        assert!(!g.mic_has_readers());
        g.add_link(61, Some("42"), Some("51"));
        assert!(g.mic_has_readers());
        // A new PipeWire session starts with an empty graph, never stale links.
        assert!(!Graph::default().mic_has_readers());
    }
}
