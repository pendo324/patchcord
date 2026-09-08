use miniserde::json::Value;
use miniserde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShareableNode {
	pub id: u32,
	pub display_name: String,
	pub application_name: Option<String>,
	pub node_name: Option<String>,
	pub description: Option<String>,
	pub media_name: Option<String>,
	pub binary: Option<String>,
	pub process_id: Option<u32>,
	pub media_class: Option<String>,
	pub is_virtual: bool,
	pub is_device: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RouteFilter {
	/// When sharing many nodes at once, only keep nodes whose current
	/// output actually reaches a device (speaker) on the graph.
	pub only_speakers: bool,
	/// Like only_speakers, but must reach the *default* sink specifically.
	pub only_default_speakers: bool,
	/// Exclude device nodes (mic/speaker) from being routed.
	pub ignore_devices: bool,
	/// Exclude virtual nodes (node.virtual=true).
	pub ignore_virtual: bool,
	/// Exclude capture-oriented nodes (media.class Stream/Input/Audio).
	pub ignore_input_media: bool,
}

/// A best-effort hint correlating an in-progress KDE/KWin window-share
/// (portal ScreenCast session) with a likely audio-producing app, derived
/// from KWin's own PipeWire video node naming convention.
///
/// KWin names its screencast video capture node
/// `kwin-screencast-<desktopFileName>` (see kwin's
/// `screencastmanager.cpp`/`screencaststream.cpp`), where
/// `<desktopFileName>` is the shared window's desktop-file id (e.g.
/// `org.mozilla.firefox`, `steam`, `code`). This is emitted by the
/// compositor itself and is visible as a plain PipeWire node regardless of
/// what opaque source id Chromium/Electron's `getDisplayMedia()` picker
/// flow hands back to the page -- see `DesktopMediaID::IdType::
/// kNativePickerSession` in Chromium, which documents that id as opaque
/// under the native-portal-picker path. Correlating via this node name
/// sidesteps that opacity entirely.
///
/// This is KDE/KWin-specific by construction (GNOME's mutter, if it names
/// its own screencast nodes at all, almost certainly uses a different
/// convention that would need separate handling) and only ever fires for
/// window shares, never full-screen shares (which have no single target
/// app to correlate against). Callers must treat "no hint" as the normal
/// case, not an error, and fall back to showing the full unfiltered app
/// list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastHint {
	/// The raw desktop-file id KWin embedded in the node name, e.g.
	/// `org.mozilla.firefox` or `steam`.
	pub desktop_file_id: String,
	/// A lowercased, reverse-DNS-stripped fragment of `desktop_file_id`
	/// intended for substring matching against candidate audio nodes'
	/// `application.name`/`node.name`/`binary` fields, e.g. `firefox` for
	/// `org.mozilla.firefox`, or `steam` for `steam`.
	pub hint: String,
}

const KWIN_SCREENCAST_NODE_PREFIX: &str = "kwin-screencast-";

/// Scans a live node snapshot for a KWin window-screencast video node and
/// derives a [`ScreencastHint`] from it, if one is currently active.
/// Shared by both the legacy (`pw-dump`) and native (`libpipewire`)
/// backends, since both produce the same [`NodeRecord`] shape.
pub fn find_screencast_hint(nodes: &HashMap<u32, NodeRecord>) -> Option<ScreencastHint> {
	nodes.values().find_map(|node| {
		let name = node.prop_str("node.name")?;
		let desktop_file_id = name.strip_prefix(KWIN_SCREENCAST_NODE_PREFIX)?;
		if desktop_file_id.is_empty() {
			return None;
		}

		// Reverse-DNS ids (org.mozilla.firefox) carry their most specific,
		// most human-recognizable component last; plain ids (steam, code)
		// have only one component and are used as-is.
		let hint = desktop_file_id
			.rsplit('.')
			.next()
			.unwrap_or(desktop_file_id)
			.to_ascii_lowercase();

		if hint.is_empty() {
			return None;
		}

		Some(ScreencastHint {
			desktop_file_id: desktop_file_id.to_string(),
			hint,
		})
	})
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VirtualSinkInfo {
	pub sink_name: String,
	pub monitor_source: String,
	pub node_id: u32,
	pub virtual_mic_name: Option<String>,
	pub virtual_mic_description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDirection {
	Input,
	Output,
}

impl PortDirection {
	pub fn parse(value: &str) -> Option<Self> {
		match value {
			"in" | "input" => Some(Self::Input),
			"out" | "output" => Some(Self::Output),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Route {
	pub output_path: String,
	pub input_path: String,
}

#[derive(Debug, Clone)]
pub struct PortRecord {
	pub id: u32,
	pub direction: PortDirection,
	pub channel: Option<String>,
	pub port_index: Option<String>,
	pub path: Option<String>,
	pub port_name: Option<String>,
	pub object_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NodeRecord {
	pub id: u32,
	pub props: HashMap<String, Value>,
	pub ports: Vec<PortRecord>,
}

impl NodeRecord {
	pub fn prop_str(&self, key: &str) -> Option<&str> {
		match self.props.get(key) {
			Some(Value::String(s)) => Some(s.as_str()),
			_ => None,
		}
	}

	pub fn prop_num(&self, key: &str) -> Option<u32> {
		match self.props.get(key) {
			Some(Value::Number(n)) => n.to_string().parse().ok(),
			Some(Value::String(s)) => s.parse().ok(),
			_ => None,
		}
	}

	pub fn matches_prop(&self, key: &str, expected: &str) -> bool {
		self.prop_str(key) == Some(expected)
	}

	pub fn output_ports(&self) -> impl Iterator<Item = &PortRecord> {
		self.ports.iter().filter(|port| port.direction == PortDirection::Output)
	}

	pub fn input_ports(&self) -> impl Iterator<Item = &PortRecord> {
		self.ports.iter().filter(|port| port.direction == PortDirection::Input)
	}

	pub fn is_device(&self) -> bool {
		let has_device_id = self.prop_str("device.id").is_some_and(|value| !value.is_empty());

		let is_audio_device = self
			.prop_str("media.class")
			.is_some_and(|class| class.starts_with("Audio/Sink") || class.starts_with("Audio/Source"));

		has_device_id || is_audio_device
	}
}
