//! Native-backend implementation of the `PatchbayState` interface, using
//! `pw_backend` (direct `libpipewire` calls) instead of `pw-dump`/`pw-link`/
//! `pactl` shelling. Intended as a drop-in for `state.rs`'s methods; the
//! old CLI-based implementation is left in `state.rs` for reference and
//! A/B comparison via `--legacy-backend`.
//!
//! The backend returns *handles* for created objects (see `pw_backend`'s
//! Command docs). Registry-global ids are resolved here from snapshots by
//! matching node names, which is both what the old pw-dump path did and
//! the only reliable way to correlate a created proxy with its global.

use std::collections::HashMap;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::PatchbayConfig;
use super::error::{BackendError, Result};
use super::models::{self, NodeRecord, RouteFilter, ScreencastHint, ShareableNode, VirtualSinkInfo};
use super::pw_backend::{BackendEvent, GraphSnapshot, PipewireBackend};
use super::routing::map_ports;
use crate::logger;

pub type BackendEventReceiver = mpsc::Receiver<BackendEvent>;

static NEXT_SINK_ID: AtomicU64 = AtomicU64::new(1);

/// Fixed `node.name` for the always-silent virtual sink
/// `discord-capture-shim` redirects blocked `discord_capture` streams to.
/// Deliberately a fixed name (not per-session-unique like `sink_name`,
/// which embeds this process's own pid): `discord-capture-shim` hardcodes
/// this exact name as its redirect target (see its own
/// `NULL_SINK_MONITOR_SOURCE` constant), and the two sides have no other
/// coordination channel for it, so it must be a name both can agree on
/// without any handshake. One instance is shared by every patchcord
/// session on the same `PipeWire` server (harmless: it's just a silent
/// sink, "already exists" is treated as success, not an error), which
/// also means it doesn't need separate per-Discord-instance scoping the
/// way the sink-input allow-list file does -- unlike that file, this sink
/// carries no information about *which* apps are allowed, only a shared
/// place to discard audio nobody asked for.
const DISCORD_CAPTURE_NULL_SINK_NAME: &str = "discord-capture-null";

/// The objects we created for the current session, tracked by backend
/// handle (NOT registry id; see module docs).
pub struct NativeSession {
	pub sink_handle: Option<u32>,
	pub sink_name: String,
	pub sink_description: String,

	pub mic_handle: Option<u32>,
	pub mic_name: Option<String>,
	pub mic_description: Option<String>,

	/// Backend handles for links created to route app output to the sink.
	pub route_link_handles: Vec<u32>,

	/// Backend handles for the sink-monitor -> virtual-mic links created
	/// by `link_monitor_to_mic`. These are the virtual mic's own
	/// plumbing, not app routes, and must survive `route_nodes`/
	/// `clear_routes` calls (which only replace app->sink routes).
	/// Previously these were stored in `route_link_handles` too, so every
	/// `routeNodes` call (e.g. from `GoofCord`'s debounced graph-change
	/// handler) silently destroyed the virtual mic's audio path.
	pub mic_link_handles: Vec<u32>,

	/// The system default sink's name from just before
	/// `set_default_sink_to_virtual` last overrode it, if any. Used to
	/// restore the user's real default sink when the app-audio-only
	/// screenshare session ends, so we don't leave the virtual sink
	/// permanently claiming "default" after Discord's share stops.
	pub prior_default_sink: Option<String>,

	/// The node id(s) the user currently wants routed into every one of
	/// Discord's own `discord_capture` screenshare-audio nodes (see
	/// `NodeRecord::is_discord_capture`'s doc comment for the full
	/// background). Kept as its own session field, independent of
	/// `route_link_handles`/the virtual-sink routing path: linking
	/// straight into `discord_capture` needs no virtual sink or mic at
	/// all, and unlike the sink-routing path (a fixed set of destination
	/// ports that exists once `ensure_virtual_sink` has run), the actual
	/// set of `discord_capture` destination nodes can change over time
	/// as Discord creates a fresh one per newly-detected app -- so the
	/// *target selection* has to be remembered and re-applied whenever
	/// the graph changes, not just applied once.
	pub discord_capture_targets: Vec<u32>,

	/// The `RouteFilter` last passed to `set_discord_capture_targets`,
	/// remembered for the same reason `discord_capture_targets` is:
	/// `sync_discord_capture_links` re-applies both together on every
	/// graph change, not just at selection time.
	pub discord_capture_filter: RouteFilter,

	/// Backend handles for links created by `sync_discord_capture_links`,
	/// keyed by the `discord_capture` node id they were created for, so a
	/// stale `discord_capture` instance's own links can be torn down
	/// independently of any other instance's (e.g. when its target app
	/// closes and a fresh capture node appears for the same app later).
	pub discord_capture_link_handles: HashMap<u32, Vec<u32>>,

	/// Backend handle for the always-silent virtual sink
	/// `discord-capture-shim` redirects blocked/disallowed
	/// `discord_capture` streams to (via its `.monitor` source), created
	/// lazily the first time `set_discord_capture_targets` is called with
	/// a non-empty selection. `None` until then. Uses `media.class =
	/// "Audio/Sink/Virtual"` (the same class `ensure_virtual_sink` uses
	/// for its non-default-sink case) specifically because a plain
	/// `"Audio/Sink"` node gets enumerated by Discord's own "new audio
	/// device detected, switch to it?" prompt -- confirmed live: a
	/// manually `pactl load-module module-null-sink`'d equivalent (no
	/// `media.class` override, defaults to a real-looking sink)
	/// immediately triggered that exact prompt the first time a stream
	/// connected to it.
	pub discord_capture_null_sink_handle: Option<u32>,
}

pub struct PatchbayStateNative {
	config: PatchbayConfig,
	backend: PipewireBackend,
	session: NativeSession,
}

impl PatchbayStateNative {
	pub fn spawn(config: &PatchbayConfig) -> Result<(Self, BackendEventReceiver)> {
		let (event_tx, event_rx) = mpsc::channel::<BackendEvent>();
		let backend = PipewireBackend::spawn(event_tx)?;

		// Wait for the registry's initial burst so the first snapshot a
		// caller sees is populated (see PipewireBackend::wait_ready).
		backend.wait_ready(Duration::from_secs(3));

		let unique = NEXT_SINK_ID.fetch_add(1, Ordering::Relaxed);

		let sink_name = format!("{}-{}-{}", config.sink_prefix, process::id(), unique);
		let sink_description = config.sink_description.clone();

		let (mic_name, mic_description) = if config.virtual_mic || config.virtual_mic_name.is_some() {
			let name = config
				.virtual_mic_name
				.clone()
				.unwrap_or_else(|| format!("{}-mic", config.sink_prefix));
			let desc = config
				.virtual_mic_description
				.clone()
				.unwrap_or_else(|| format!("{} (Virtual Mic)", config.sink_description));
			(Some(name), Some(desc))
		} else {
			(None, None)
		};

		let session = NativeSession {
			sink_handle: None,
			sink_name,
			sink_description,
			mic_handle: None,
			mic_name,
			mic_description,
			route_link_handles: Vec::new(),
			mic_link_handles: Vec::new(),
			prior_default_sink: None,
			discord_capture_targets: Vec::new(),
			discord_capture_filter: RouteFilter::default(),
			discord_capture_link_handles: HashMap::new(),
			discord_capture_null_sink_handle: None,
		};

		Ok((
			Self {
				config: config.clone(),
				backend,
				session,
			},
			event_rx,
		))
	}

	/// Resolves a node's registry id by node.name from the live snapshot.
	fn find_node_id_by_name(&self, name: &str) -> Result<Option<u32>> {
		let snapshot = self.backend.snapshot()?;
		Ok(snapshot
			.nodes
			.values()
			.find(|n| n.prop_str("node.name") == Some(name))
			.map(|n| n.id))
	}

	/// Polls until a node with `name` exists with at least `min_inputs`
	/// input ports (used to wait for a freshly created sink to be ready).
	fn wait_for_node_ready(&self, name: &str, min_inputs: usize) -> Result<u32> {
		let deadline = Instant::now() + Duration::from_secs(3);
		loop {
			let snapshot = self.backend.snapshot()?;
			if let Some(node) = snapshot.nodes.values().find(|n| n.prop_str("node.name") == Some(name))
				&& node.input_ports().filter(|p| p.path.is_some()).count() >= min_inputs
			{
				return Ok(node.id);
			}
			if Instant::now() >= deadline {
				return Err(BackendError::Timeout(name.to_string()));
			}
			std::thread::sleep(Duration::from_millis(200));
		}
	}

	pub fn list_shareable_nodes(&self, include_devices: bool) -> Result<Vec<ShareableNode>> {
		let snapshot = self.backend.snapshot()?;

		let mut nodes = snapshot
			.nodes
			.values()
			.filter(|node| !self.is_our_virtual_audio_object(node))
			.filter(|node| !node.is_discord_capture())
			// Discord's own WEBRTC VoiceEngine record/playback streams
			// (its own microphone input and voice-channel playback) --
			// never a meaningful "app to share the audio of", and
			// including them let a user select Discord's own voice
			// audio as a screenshare-audio source, which is at best
			// useless and at worst a feedback loop.
			.filter(|node| node.prop_str("node.name") != Some("WEBRTC VoiceEngine"))
			.filter(|node| node.output_ports().any(|port| port.path.is_some()))
			.filter(|node| {
				let is_app = node.prop_str("application.name").is_some_and(|v| !v.is_empty())
					|| node.prop_str("application.process.binary").is_some_and(|v| !v.is_empty());

				if node.is_device() { include_devices } else { is_app }
			})
			.map(to_shareable_node)
			.collect::<Vec<_>>();

		nodes.sort_by(|left, right| {
			left.display_name
				.to_ascii_lowercase()
				.cmp(&right.display_name.to_ascii_lowercase())
				.then_with(|| left.id.cmp(&right.id))
		});

		Ok(nodes)
	}

	/// Best-effort correlation of an in-progress KDE/KWin window-share;
	/// see [`ScreencastHint`]'s doc comment. `None` is the normal case
	/// (no active window share, or not on `KWin`).
	pub fn find_screencast_hint(&self) -> Result<Option<ScreencastHint>> {
		let snapshot = self.backend.snapshot()?;
		Ok(models::find_screencast_hint(&snapshot.nodes))
	}

	pub fn ensure_virtual_sink(&mut self) -> Result<VirtualSinkInfo> {
		if let Some(info) = self.virtual_sink_info()? {
			self.ensure_virtual_mic()?;
			return Ok(info);
		}

		logger::info(&format!("[patchbay] creating virtual sink {}", self.session.sink_name));
		let handle = self.backend.create_virtual_node(
			self.session.sink_name.clone(),
			self.session.sink_description.clone(),
			if self.config.sink_becomes_default {
				"Audio/Sink"
			} else {
				"Audio/Sink/Virtual"
			},
		)?;
		self.session.sink_handle = Some(handle);

		self.ensure_virtual_mic()?;

		let sink_id = self.wait_for_node_ready(&self.session.sink_name, 2)?;
		logger::info(&format!(
			"[patchbay] virtual sink ready: {} (node id {sink_id})",
			self.session.sink_name
		));

		if self.config.sink_becomes_default {
			self.set_default_sink_to_virtual()?;
		}

		self.virtual_sink_info()?
			.ok_or_else(|| BackendError::Message("virtual sink not found after creation".to_string()))
	}

	fn ensure_virtual_mic(&mut self) -> Result<()> {
		let (Some(mic_name), Some(mic_desc)) = (self.session.mic_name.clone(), self.session.mic_description.clone()) else {
			return Ok(());
		};
		if self.session.mic_handle.is_some() {
			return Ok(());
		}

		logger::info(&format!("[patchbay] creating virtual mic wrapper {mic_name}"));
		let handle = self.backend.create_virtual_node(mic_name, mic_desc, "Audio/Source/Virtual")?;
		self.session.mic_handle = Some(handle);

		self.link_monitor_to_mic()
	}

	/// Links the sink's own output ports (a `support.null-audio-sink`
	/// adapter exposes its monitor as regular output ports on the sink
	/// node itself -- there is no separate `.monitor` node, unlike a
	/// `PulseAudio` module-null-sink) to the mic source adapter's input
	/// ports. Mirrors venmic's `create_mic`, which creates both sides via
	/// the identical `support.null-audio-sink` factory (`kind::sink` vs
	/// `kind::source` only changes `media.class`) and links
	/// receiver-output -> source-input matched by `audio.channel`.
	fn link_monitor_to_mic(&mut self) -> Result<()> {
		let Some(mic_name) = self.session.mic_name.clone() else {
			return Ok(());
		};
		// Wait for both freshly-created adapter nodes to actually appear
		// in the graph (registry globals for just-created objects can
		// lag a beat behind the create() call returning) before looking
		// up their ports below.
		let mic_id = self.wait_for_node_ready(&mic_name, 2)?;
		let sink_id = self.wait_for_node_ready(&self.session.sink_name, 2)?;

		// Wait (up to ~3s) for the sink's output ports and the mic's
		// input ports to be visible, then create the FL/FR links. Without
		// them the virtual mic captures silence. Port globals can arrive
		// asynchronously after node creation, so poll the live graph
		// rather than giving up on the first snapshot.
		let deadline = Instant::now() + Duration::from_secs(3);
		loop {
			let snapshot = self.backend.snapshot()?;
			let sink = snapshot.nodes.get(&sink_id);
			let mic = snapshot.nodes.get(&mic_id);

			if let (Some(m), Some(mic)) = (sink, mic) {
				let outputs = m.output_ports().filter(|p| p.path.is_some()).cloned().collect::<Vec<_>>();
				let inputs = mic.input_ports().filter(|p| p.path.is_some()).cloned().collect::<Vec<_>>();

				if !outputs.is_empty() && !inputs.is_empty() {
					for (output, input) in map_ports(&outputs, &inputs) {
						let handle = self.backend.create_link(m.id, output.id, mic_id, input.id)?;
						// Monitor->mic links are part of the virtual mic's
						// identity, not "routes"; tracked separately so
						// route_nodes/clear_routes never touches them.
						self.session.mic_link_handles.push(handle);
					}
					logger::info(&format!(
						"[patchbay] linked sink monitor -> virtual mic ({outputs} outputs -> {inputs} inputs)",
						outputs = outputs.len(),
						inputs = inputs.len()
					));
					return Ok(());
				}
			}

			if Instant::now() >= deadline {
				return Err(BackendError::Message("timed out linking sink monitor to virtual mic".to_string()));
			}
			std::thread::sleep(Duration::from_millis(200));
		}
	}

	pub fn route_nodes(&mut self, node_ids: Vec<u32>, filter: &RouteFilter) -> Result<VirtualSinkInfo> {
		if node_ids.is_empty() {
			self.clear_routes()?;
			return self.ensure_virtual_sink();
		}

		let sink_info = self.ensure_virtual_sink()?;
		let sink_id = self
			.find_sink_id()?
			.ok_or_else(|| BackendError::Message("virtual sink missing after ensure".to_string()))?;

		let snapshot = self.backend.snapshot()?;
		let sink_inputs = snapshot
			.nodes
			.get(&sink_id)
			.map(|n| n.input_ports().filter(|p| p.path.is_some()).cloned().collect::<Vec<_>>())
			.unwrap_or_default();

		if sink_inputs.len() < 2 {
			return Err(BackendError::Message("virtual sink has no usable stereo input ports".to_string()));
		}

		let default_sink_id = filter.only_default_speakers.then(|| resolve_default_sink_id(&snapshot)).flatten();

		let mut new_handles = Vec::<u32>::new();
		let mut failed = Vec::<u32>::new();

		for node_id in dedupe_node_ids(node_ids) {
			if node_id == sink_id {
				logger::warn("[patchbay] refusing to link the virtual sink to itself");
				continue;
			}

			let Some(node) = snapshot.nodes.get(&node_id) else {
				logger::warn(&format!("[patchbay] node {node_id} does not exist"));
				continue;
			};

			if self.is_our_virtual_audio_object(node) {
				logger::warn(&format!("[patchbay] refusing to route helper-owned virtual node {node_id}"));
				continue;
			}

			if !should_link(&snapshot, node, filter, default_sink_id) {
				logger::debug(&format!(
					"[patchbay] node {node_id} filtered out by route filter (only_speakers={}, only_default={}, ignore_devices={}, ignore_virtual={}, ignore_input_media={})",
					filter.only_speakers,
					filter.only_default_speakers,
					filter.ignore_devices,
					filter.ignore_virtual,
					filter.ignore_input_media
				));
				continue;
			}

			let outputs = node.output_ports().filter(|p| p.path.is_some()).cloned().collect::<Vec<_>>();
			if outputs.is_empty() {
				logger::debug(&format!("[patchbay] node {node_id} has no usable output ports"));
				continue;
			}

			for (output, input) in map_ports(&outputs, &sink_inputs) {
				match self.backend.create_link(node_id, output.id, sink_id, input.id) {
					Ok(handle) => new_handles.push(handle),
					Err(err) => {
						logger::warn(&format!(
							"[patchbay] failed to link node {node_id} port {} to sink: {err}",
							output.id
						));
						failed.push(node_id);
					}
				}
			}
		}

		if new_handles.is_empty() {
			return Err(BackendError::Message("none of the selected nodes could be linked".to_string()));
		}

		// Replace old routes with the new set: destroy links that are no
		// longer desired.
		for handle in std::mem::take(&mut self.session.route_link_handles) {
			let _ = self.backend.destroy_link(handle);
		}
		self.session.route_link_handles = new_handles;

		if !failed.is_empty() {
			logger::warn(&format!("[patchbay] {failed:?} nodes could not be linked"));
		}

		Ok(sink_info)
	}

	pub fn clear_routes(&mut self) -> Result<()> {
		let handles = std::mem::take(&mut self.session.route_link_handles);
		let mut failures = 0usize;
		for handle in handles {
			match self.backend.destroy_link(handle) {
				Ok(()) => {}
				Err(err) => {
					failures += 1;
					logger::warn(&format!("[patchbay] failed to remove link {handle}: {err}"));
				}
			}
		}
		if failures == 0 {
			Ok(())
		} else {
			Err(BackendError::Message(format!("failed to remove {failures} route(s)")))
		}
	}

	/// Sets which node id(s) should be routed directly into every one of
	/// Discord's own `discord_capture` screenshare-audio nodes (see
	/// `NodeRecord::is_discord_capture`), then immediately reconciles the
	/// live graph to match. Pass an empty `Vec` to stop routing anything
	/// into `discord_capture` (any existing links this created are torn
	/// down; Discord's own `discord_capture` nodes remain, just
	/// unlinked -- discord-capture-shim already prevents them from
	/// auto-linking themselves back to anything on their own).
	///
	/// This is a completely separate path from `route_nodes`/the virtual
	/// sink: no sink or mic is created or needed here, since the whole
	/// point of `discord_capture` is that Discord already reads directly
	/// from it in-process -- there is nothing for a virtual mic to swap
	/// into on this path, unlike the mic-based routing `route_nodes`
	/// still supports for other consumers.
	///
	/// `filter` is applied with the exact same `should_link` decision
	/// `route_nodes` uses (`only_speakers`/`only_default_speakers`/
	/// `ignore_devices`/`ignore_virtual`/`ignore_input_media`) -- shared
	/// exactly rather than reimplemented, so "Entire System" mode's
	/// filters behave identically whether the audio ends up going to a
	/// virtual sink or straight into `discord_capture`.
	pub fn set_discord_capture_targets(&mut self, node_ids: Vec<u32>, filter: RouteFilter) -> Result<()> {
		let deduped = dedupe_node_ids(node_ids);
		if !deduped.is_empty() {
			// Only created when actually needed: a fresh session that
			// never calls this with a non-empty selection never gets a
			// discord-capture-null sink node at all.
			self.ensure_discord_capture_null_sink()?;
		}
		self.session.discord_capture_targets = deduped;
		self.session.discord_capture_filter = filter;
		self.write_discord_capture_allowlist()?;
		self.sync_discord_capture_links()
	}

	/// Writes `discord-capture-shim`'s allow-list file: the set of
	/// `PulseAudio` sink-input indices (`object.serial` -- confirmed live
	/// this session to be the exact index `pa_stream_set_monitor_stream`
	/// receives and `pactl list sink-inputs` reports, NOT the `PipeWire`
	/// registry id `ShareableNode.id`/`discord_capture_targets` itself
	/// uses) the shim should allow `discord_capture` streams to actually
	/// monitor. See `discord-capture-shim`'s own module doc comment for
	/// the full mechanism and path-derivation this must match exactly.
	///
	/// Deliberately computed from `getppid()`, exactly like the shim
	/// does: this process is always spawned directly by the plugin's
	/// `native.ts` running in Discord's own main Electron process (see
	/// `node/patchcord.js`'s use of `child_process.spawn`), so this
	/// process's parent pid and `discord_voice.node`'s (running inside a
	/// child *renderer* process of that same main process) are the exact
	/// same pid -- no IPC or coordination needed for the two sides to
	/// agree on the file path independently.
	fn write_discord_capture_allowlist(&self) -> Result<()> {
		let snapshot = self.backend.snapshot()?;
		let mut serials = Vec::<u32>::new();
		for &target_id in &self.session.discord_capture_targets {
			if let Some(node) = snapshot.nodes.get(&target_id) {
				if let Some(serial) = node.prop_num("object.serial") {
					serials.push(serial);
				} else {
					logger::warn(&format!(
						"[patchbay] discord_capture target node {target_id} has no object.serial; cannot allow-list it for discord-capture-shim"
					));
				}
			}
		}

		let path = discord_capture_allowlist_path();
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)
				.map_err(|err| BackendError::Message(format!("failed to create {}: {err}", parent.display())))?;
		}
		let contents = serials.iter().map(u32::to_string).collect::<Vec<_>>().join("\n");
		// Atomic write-then-rename: discord-capture-shim reads this file
		// fresh on every pa_stream_set_monitor_stream call, so a reader
		// must never observe a partially-written file.
		let tmp_path = path.with_extension("tmp");
		std::fs::write(&tmp_path, contents)
			.map_err(|err| BackendError::Message(format!("failed to write {}: {err}", tmp_path.display())))?;
		std::fs::rename(&tmp_path, &path)
			.map_err(|err| BackendError::Message(format!("failed to rename {} to {}: {err}", tmp_path.display(), path.display())))?;
		logger::debug(&format!(
			"[patchbay] wrote discord-capture-shim allow-list {}: {} app(s)",
			path.display(),
			serials.len()
		));
		Ok(())
	}

	/// Creates the always-silent `discord-capture-null` virtual sink
	/// `discord-capture-shim` redirects blocked streams to, if it doesn't
	/// already exist (from this session or a previous/concurrent one --
	/// see `DISCORD_CAPTURE_NULL_SINK_NAME`'s own doc comment for why a
	/// fixed, shared name is intentional here). Safe/cheap to call
	/// repeatedly.
	fn ensure_discord_capture_null_sink(&mut self) -> Result<()> {
		if self.session.discord_capture_null_sink_handle.is_some() {
			return Ok(());
		}
		if self.find_node_id_by_name(DISCORD_CAPTURE_NULL_SINK_NAME)?.is_some() {
			// Already exists (created by a previous session on the same
			// PipeWire server that's still running, or another
			// concurrent patchcord instance) -- nothing to create, and
			// no handle of our own to track since we don't own its
			// lifecycle in that case.
			return Ok(());
		}
		logger::info(&format!(
			"[patchbay] creating discord_capture null-sink redirect target {DISCORD_CAPTURE_NULL_SINK_NAME}"
		));
		let handle = self.backend.create_virtual_node(
			DISCORD_CAPTURE_NULL_SINK_NAME.to_string(),
			"Discord Capture (blocked)".to_string(),
			// Audio/Sink/Virtual, not plain Audio/Sink: see
			// discord_capture_null_sink_handle's own doc comment for why
			// this specific class avoids Discord's "new audio device
			// detected" prompt.
			"Audio/Sink/Virtual",
		)?;
		self.session.discord_capture_null_sink_handle = Some(handle);
		self.wait_for_node_ready(DISCORD_CAPTURE_NULL_SINK_NAME, 2)?;
		Ok(())
	}

	/// Reconciles `discord_capture` links against the current
	/// `discord_capture_targets` selection and the current live graph.
	/// Called both right after `set_discord_capture_targets` changes the
	/// selection, and on every `graphChanged` event (see main.rs) so a
	/// freshly-created `discord_capture` node -- e.g. Discord detecting a
	/// newly-launched app, or the user restarting their target app --
	/// gets linked without waiting for the client to notice and issue a
	/// new request. Safe/cheap to call when nothing has actually changed
	/// (compares against already-created link handles per node before
	/// creating anything new).
	pub fn sync_discord_capture_links(&mut self) -> Result<()> {
		let snapshot = self.backend.snapshot()?;

		let capture_node_ids: Vec<u32> = snapshot.nodes.values().filter(|n| n.is_discord_capture()).map(|n| n.id).collect();

		// Drop bookkeeping for any discord_capture node that no longer
		// exists (its target app closed, or Discord recreated it under a
		// new registry id) -- the PipeWire server already destroyed the
		// links along with the node itself, so there's nothing to
		// destroy here, just stale handles to forget.
		self.session
			.discord_capture_link_handles
			.retain(|node_id, _| capture_node_ids.contains(node_id));

		if self.session.discord_capture_targets.is_empty() {
			// Nothing selected: tear down any links we previously made
			// (e.g. the user just switched to "None"/"Skip"), but leave
			// discord_capture nodes themselves alone.
			for (_, handles) in std::mem::take(&mut self.session.discord_capture_link_handles) {
				for handle in handles {
					let _ = self.backend.destroy_link(handle);
				}
			}
			return Ok(());
		}

		let mut errors = Vec::<String>::new();

		for capture_id in capture_node_ids {
			let Some(capture_node) = snapshot.nodes.get(&capture_id) else {
				continue;
			};
			let capture_inputs = capture_node.input_ports().filter(|p| p.path.is_some()).cloned().collect::<Vec<_>>();
			if capture_inputs.is_empty() {
				logger::debug(&format!(
					"[patchbay] discord_capture node {capture_id} has no usable input ports yet"
				));
				continue;
			}

			// A discord_capture node's shim-redirected link to
			// discord-capture-null (see discord-capture-shim's own doc
			// comment) can form asynchronously, independently of --
			// and, confirmed live, sometimes strictly *after* -- this
			// exact sync running and adding the real target link(s) for
			// that same node. Because a discord_capture node's real
			// links are only ever added once (see the "already linked"
			// check below, which then permanently skips all future
			// syncs for that id), a null-sink link that only shows up
			// afterward would otherwise never get cleaned up, leaving
			// silence permanently mixed in alongside the real target for
			// that node's whole lifetime. This call is deliberately
			// placed OUTSIDE (before) the "already linked" skip, and
			// looks specifically for links *from* discord-capture-null
			// (not any pre-existing link at all), so it's safe and cheap
			// to run unconditionally on every single sync for every
			// discord_capture node, including ones already fully linked.
			self.remove_stray_null_sink_links(&snapshot, capture_id, &capture_inputs);

			// Already linked (from a previous sync) and nothing else to
			// redo -- discord_capture nodes are 1:1 with a single target
			// app for their whole lifetime (Discord creates a fresh node
			// rather than re-targeting an existing one when the set of
			// detected apps changes), so once linked, a given
			// discord_capture id's *real target* links never need to
			// change again during this sync unless the user's selection
			// itself changes -- which set_discord_capture_targets
			// already handles by calling this method fresh. (The
			// discord-capture-null cleanup above is the one exception
			// that still needs to run every time, which is why it's
			// ordered before this check rather than skipped by it.)
			if self.session.discord_capture_link_handles.contains_key(&capture_id) {
				continue;
			}

			let mut handles_for_node = Vec::<u32>::new();

			let default_sink_id = self
				.session
				.discord_capture_filter
				.only_default_speakers
				.then(|| resolve_default_sink_id(&snapshot))
				.flatten();

			for &target_id in &self.session.discord_capture_targets {
				if target_id == capture_id {
					continue;
				}
				let Some(target_node) = snapshot.nodes.get(&target_id) else {
					logger::warn(&format!("[patchbay] discord_capture target node {target_id} does not exist"));
					continue;
				};
				if self.is_our_virtual_audio_object(target_node) {
					logger::warn(&format!(
						"[patchbay] refusing to route helper-owned virtual node {target_id} into discord_capture"
					));
					continue;
				}
				if !should_link(&snapshot, target_node, &self.session.discord_capture_filter, default_sink_id) {
					logger::debug(&format!(
						"[patchbay] node {target_id} filtered out of discord_capture routing by RouteFilter"
					));
					continue;
				}
				let outputs = target_node.output_ports().filter(|p| p.path.is_some()).cloned().collect::<Vec<_>>();
				if outputs.is_empty() {
					continue;
				}

				for (output, input) in map_ports(&outputs, &capture_inputs) {
					match self.backend.create_link(target_id, output.id, capture_id, input.id) {
						Ok(handle) => handles_for_node.push(handle),
						Err(err) => {
							errors.push(format!(
								"failed to link node {target_id} port {} to discord_capture {capture_id}: {err}",
								output.id
							));
						}
					}
				}
			}

			if !handles_for_node.is_empty() {
				logger::info(&format!(
					"[patchbay] linked {} target(s) into discord_capture node {capture_id}",
					self.session.discord_capture_targets.len()
				));
				self.session.discord_capture_link_handles.insert(capture_id, handles_for_node);
			}
		}

		if errors.is_empty() {
			Ok(())
		} else {
			Err(BackendError::Message(errors.join("; ")))
		}
	}

	/// Destroys any live link into `capture_id`'s input ports whose
	/// source is `discord-capture-null` -- see the call site's own
	/// comment in `sync_discord_capture_links` for why this needs to run
	/// unconditionally, every sync, even for nodes already fully linked.
	/// Split out into its own method purely to keep
	/// `sync_discord_capture_links` under clippy's line-count limit; not
	/// meant to be called from anywhere else.
	fn remove_stray_null_sink_links(&self, snapshot: &GraphSnapshot, capture_id: u32, capture_inputs: &[models::PortRecord]) {
		let capture_input_ids: std::collections::HashSet<u32> = capture_inputs.iter().map(|p| p.id).collect();
		for (&link_id, link) in &snapshot.links {
			if link.input_node != capture_id || !capture_input_ids.contains(&link.input_port) {
				continue;
			}
			let is_from_null_sink =
				snapshot.nodes.get(&link.output_node).and_then(|n| n.prop_str("node.name")) == Some(DISCORD_CAPTURE_NULL_SINK_NAME);
			if is_from_null_sink {
				logger::debug(&format!(
					"[patchbay] removing stray discord-capture-null link {link_id} into discord_capture {capture_id}"
				));
				if let Err(err) = self.backend.destroy_link_by_id(link_id) {
					logger::warn(&format!(
						"[patchbay] failed to remove stray discord-capture-null link {link_id} into discord_capture {capture_id}: {err}"
					));
				}
			}
		}
	}

	pub fn dispose(&mut self) -> Result<()> {
		let mut errors = Vec::<String>::new();

		// Restore the real default sink first, before tearing down the
		// virtual sink itself: doing it after would leave a brief window
		// (or, if this errors, potentially forever) where the system
		// default points at a sink that no longer exists.
		if let Err(err) = self.restore_default_sink() {
			errors.push(err.to_string());
		}

		if let Err(err) = self.clear_routes() {
			errors.push(err.to_string());
		}

		self.session.discord_capture_targets.clear();
		for (_, handles) in std::mem::take(&mut self.session.discord_capture_link_handles) {
			for handle in handles {
				if let Err(err) = self.backend.destroy_link(handle) {
					errors.push(err.to_string());
				}
			}
		}

		for handle in std::mem::take(&mut self.session.mic_link_handles) {
			if let Err(err) = self.backend.destroy_link(handle) {
				errors.push(err.to_string());
			}
		}

		if let Some(handle) = self.session.mic_handle.take()
			&& let Err(err) = self.backend.destroy_node(handle)
		{
			errors.push(err.to_string());
		}
		if let Some(handle) = self.session.sink_handle.take()
			&& let Err(err) = self.backend.destroy_node(handle)
		{
			errors.push(err.to_string());
		}

		if errors.is_empty() {
			logger::info("[patchbay] native session disposed");
			Ok(())
		} else {
			Err(BackendError::Message(errors.join("; ")))
		}
	}

	fn find_sink_id(&self) -> Result<Option<u32>> {
		self.find_node_id_by_name(&self.session.sink_name)
	}

	/// Mutes or unmutes the virtual mic node itself (matches venmic's
	/// "Initial Mute" toggle: mute right after linking to swallow the
	/// startup audio spike Chromium produces when a new input device
	/// appears, then unmute once the share is actually live).
	pub fn set_virtual_mic_mute(&self, mute: bool) -> Result<()> {
		let Some(mic_id) = self.find_mic_id()? else {
			return Err(BackendError::Message("virtual mic is not active".to_string()));
		};
		self.backend.set_mute(mic_id, mute)
	}

	/// Makes the virtual sink the system default, remembering the prior
	/// default so it can be restored later. Needed because Discord's real
	/// "Stream With Audio" screenshare capture always grabs the *default*
	/// sink's monitor, not a specific chosen node -- so routing only the
	/// selected app(s) into the virtual sink isn't enough on its own; the
	/// virtual sink must also become "the" default for the duration of the
	/// share for Discord to actually pick its audio up.
	pub fn set_default_sink_to_virtual(&mut self) -> Result<()> {
		let snapshot = self.backend.snapshot()?;
		if self.session.prior_default_sink.is_none() {
			self.session.prior_default_sink = snapshot.default_sink_name;
		}
		self.backend.set_default_sink(Some(self.session.sink_name.clone()))
	}

	/// Restores the system default sink to whatever it was before
	/// `set_default_sink_to_virtual` was called, if anything was recorded.
	/// Safe to call even if the default was never overridden (no-op).
	pub fn restore_default_sink(&mut self) -> Result<()> {
		let Some(prior) = self.session.prior_default_sink.take() else {
			return Ok(());
		};
		self.backend.set_default_sink(Some(prior))
	}

	fn find_mic_id(&self) -> Result<Option<u32>> {
		self.session
			.mic_name
			.as_deref()
			.map(|name| self.find_node_id_by_name(name))
			.transpose()
			.map(std::option::Option::flatten)
	}

	fn virtual_sink_info(&self) -> Result<Option<VirtualSinkInfo>> {
		let Some(sink_id) = self.find_sink_id()? else {
			return Ok(None);
		};

		let snapshot = self.backend.snapshot()?;
		let Some(node) = snapshot.nodes.get(&sink_id) else {
			return Ok(None);
		};

		if node.input_ports().filter(|p| p.path.is_some()).count() < 2 {
			return Ok(None);
		}

		Ok(Some(VirtualSinkInfo {
			sink_name: self.session.sink_name.clone(),
			monitor_source: format!("{}.monitor", self.session.sink_name),
			node_id: sink_id,
			virtual_mic_name: self.session.mic_name.clone(),
			virtual_mic_description: self.session.mic_description.clone(),
		}))
	}

	fn is_our_virtual_audio_object(&self, node: &NodeRecord) -> bool {
		node.matches_prop("node.name", &self.session.sink_name)
			|| node.matches_prop("node.name", &format!("{}.monitor", self.session.sink_name))
			|| self.session.mic_name.as_ref().is_some_and(|m| node.matches_prop("node.name", m))
	}
}

impl Drop for PatchbayStateNative {
	fn drop(&mut self) {
		if let Err(err) = self.dispose() {
			logger::warn(&format!("[patchbay] cleanup failed during drop: {err}"));
		}
	}
}

/// Path to `discord-capture-shim`'s per-Discord-instance allow-list file
/// -- see `write_discord_capture_allowlist`'s own doc comment for the
/// exact derivation this must mirror. Computed fresh on every call
/// (`getppid()` is cheap and this process's parent never changes anyway,
/// so caching would save essentially nothing).
fn discord_capture_allowlist_path() -> std::path::PathBuf {
	// SAFETY: getppid() takes no arguments and cannot fail.
	let ppid = unsafe { libc::getppid() };
	let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
		// SAFETY: getuid() takes no arguments and cannot fail.
		let uid = unsafe { libc::getuid() };
		format!("/run/user/{uid}")
	});
	std::path::Path::new(&runtime_dir)
		.join("patchcord")
		.join(format!("discord-capture-allowed.{ppid}"))
}

fn dedupe_node_ids(node_ids: Vec<u32>) -> Vec<u32> {
	let mut seen = std::collections::BTreeSet::new();
	let mut deduped = Vec::new();
	for node_id in node_ids {
		if seen.insert(node_id) {
			deduped.push(node_id);
		}
	}
	deduped
}

fn to_shareable_node(node: &NodeRecord) -> ShareableNode {
	let application_name = node.prop_str("application.name").map(str::to_string);
	let node_name = node.prop_str("node.name").map(str::to_string);
	let description = node
		.prop_str("node.description")
		.or_else(|| node.prop_str("device.description"))
		.map(str::to_string);
	let media_name = node.prop_str("media.name").map(str::to_string);
	let binary = node.prop_str("application.process.binary").map(str::to_string);
	let process_id = node.prop_num("application.process.id");

	let display_name = description
		.clone()
		.or_else(|| application_name.clone())
		.or_else(|| media_name.clone())
		.or_else(|| node_name.clone())
		.unwrap_or_else(|| format!("Node {}", node.id));

	ShareableNode {
		id: node.id,
		display_name,
		application_name,
		node_name,
		description,
		media_name,
		binary,
		process_id,
		media_class: node.prop_str("media.class").map(str::to_string),
		is_virtual: node.prop_str("node.virtual") == Some("true"),
		is_device: node.is_device(),
	}
}
/// Resolves the registry id of the default sink from the live metadata
/// (`default.audio.sink` -> node.name match), if known.
fn resolve_default_sink_id(snapshot: &GraphSnapshot) -> Option<u32> {
	let name = snapshot.default_sink_name.as_ref()?;
	snapshot
		.nodes
		.values()
		.find(|n| n.prop_str("node.name") == Some(name.as_str()))
		.map(|n| n.id)
}

/// Mirrors venmic's `should_link()` decision for a candidate node:
/// whether it should be routed to the virtual sink given the active route
/// filter. `only_speakers`/`only_default_speakers` inspect where the
/// node's audio is *currently* connected on the graph, exactly like
/// venmic, rather than filtering on static node type alone.
fn should_link(snapshot: &GraphSnapshot, node: &NodeRecord, filter: &RouteFilter, default_sink_id: Option<u32>) -> bool {
	if node.is_device() && filter.ignore_devices {
		return false;
	}
	if node.prop_str("node.virtual") == Some("true") && filter.ignore_virtual {
		return false;
	}
	if matches!(node.prop_str("media.class"), Some(c) if c.starts_with("Stream/Input/Audio")) && filter.ignore_input_media {
		return false;
	}

	if filter.only_speakers || filter.only_default_speakers {
		// Which nodes does this node's output currently terminate at?
		let targets = snapshot.link_targets_of(node.id);

		let reaches_device = targets.iter().any(|target_id| {
			snapshot
				.nodes
				.get(target_id)
				.is_some_and(|t| t.prop_str("device.id").is_some_and(|v| !v.is_empty()))
		});

		if filter.only_speakers && !reaches_device {
			return false;
		}

		if filter.only_default_speakers {
			match default_sink_id {
				Some(sink_id) if targets.contains(&sink_id) => {}
				_ => return false,
			}
		}
	}

	true
}
