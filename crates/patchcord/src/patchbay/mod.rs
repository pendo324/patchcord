pub mod cmd;
pub mod pw_backend;
pub mod error;
pub mod models;
pub mod routing;
pub mod snapshot;
pub mod state;
pub mod state_native;

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

pub use error::{BackendError, Result};
pub use models::{RouteFilter, ScreencastHint, ShareableNode, VirtualSinkInfo};
pub use state_native::PatchbayStateNative;

use crate::logger;
use cmd::run_text;
use state::PatchbayState;

static PIPEWIRE_DETECTION_CACHE: OnceLock<Mutex<Option<(bool, Instant)>>> = OnceLock::new();
const PIPEWIRE_DETECTION_CACHE_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct PatchbayConfig {
	pub sink_prefix: String,
	pub sink_description: String,
	pub virtual_mic: bool,
	pub virtual_mic_name: Option<String>,
	pub virtual_mic_description: Option<String>,
	/// Creates the virtual sink with plain `Audio/Sink` media class instead
	/// of `Audio/Sink/Virtual`. Required for `set_default_sink_to_virtual`
	/// to actually work: WirePlumber's default-node policy
	/// (default-nodes/rescan.lua) only ever considers `Audio/Sink` and
	/// `Audio/Duplex` nodes as sink-default candidates, so an
	/// `Audio/Sink/Virtual` node can be *written* into
	/// `default.configured.audio.sink` (confirmed via `wpctl status`
	/// showing it under "Default Configured Devices") but never actually
	/// becomes the effective default (`default.audio.sink`), with and
	/// without a virtual mic attached.
	///
	/// Trade-off: plain `Audio/Sink` also changes the adapter's port
	/// negotiation such that `link_monitor_to_mic`'s 2in/2out topology
	/// assumption breaks, and mic linking silently times out.
	/// Since this flag exists specifically for app-only-audio screenshare,
	/// which has no need for a virtual mic at all, mutual exclusivity with
	/// `virtual_mic` is enforced by `AudioSharePatchbay::new` rather than
	/// attempting to support both at once.
	pub sink_becomes_default: bool,
}

impl Default for PatchbayConfig {
	fn default() -> Self {
		Self {
			sink_prefix: "audio-share".to_string(),
			sink_description: "Virtual Audio Share".to_string(),
			virtual_mic: false,
			virtual_mic_name: None,
			virtual_mic_description: None,
			sink_becomes_default: false,
		}
	}
}

pub fn has_pipewire() -> bool {
	let cache = PIPEWIRE_DETECTION_CACHE.get_or_init(|| Mutex::new(None));

	{
		let cached = *lock_unpoisoned(cache);
		if let Some((value, checked_at)) = cached
			&& checked_at.elapsed() < PIPEWIRE_DETECTION_CACHE_TTL
		{
			return value;
		}
	}

	let value = match detect_pipewire() {
		Ok(value) => value,
		Err(err) => {
			logger::warn(&format!("[patchbay] PipeWire detection failed: {err}"));
			false
		}
	};

	*lock_unpoisoned(cache) = Some((value, Instant::now()));
	value
}

pub fn ensure_pipewire() -> Result<()> {
	if has_pipewire() { Ok(()) } else { Err(BackendError::Unsupported) }
}

fn detect_pipewire() -> Result<bool> {
	let info = run_text("pactl", &["info"])?;

	let server_name = info
		.lines()
		.find_map(|line| line.strip_prefix("Server Name:"))
		.map(str::trim)
		.ok_or_else(|| BackendError::InvalidOutput("pactl", "missing `Server Name:` line".to_string()))?;

	let lowered = server_name.to_ascii_lowercase();
	logger::trace(&format!("[patchbay] pulse server: {lowered}"));

	Ok(lowered.contains("pipewire"))
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	match mutex.lock() {
		Ok(guard) => guard,
		Err(poisoned) => poisoned.into_inner(),
	}
}

enum BackendState {
	/// The original pw-dump/pw-link/pactl-shelling implementation.
	Legacy(PatchbayState),
	/// The native libpipewire implementation.
	Native(PatchbayStateNative),
}

pub struct AudioSharePatchbay {
	state: BackendState,
	native_events: Option<state_native::BackendEventReceiver>,
}

impl Default for AudioSharePatchbay {
	fn default() -> Self {
		Self::new(&PatchbayConfig::default(), false)
	}
}

impl AudioSharePatchbay {
	pub fn new(config: &PatchbayConfig, legacy: bool) -> Self {
		if !has_pipewire() {
			logger::warn("[patchbay] PipeWire was not detected as the active audio server");
		}

		let mut config = config.clone();
		if config.sink_becomes_default && config.virtual_mic {
			// See the doc comment on sink_becomes_default: the two features
			// require mutually incompatible sink media classes. Disable
			// virtual_mic rather than silently letting mic-link setup fail
			// later with a confusing timeout.
			logger::warn(
				"[patchbay] sink_becomes_default and virtual_mic are mutually exclusive; \
				 disabling virtual_mic for this session",
			);
			config.virtual_mic = false;
		}
		let config = &config;

		if legacy {
			logger::info("[patchbay] using legacy CLI backend");
			return Self {
				state: BackendState::Legacy(PatchbayState::new(config)),
				native_events: None,
			};
		}

		match PatchbayStateNative::spawn(config) {
			Ok((state, events)) => {
				logger::info("[patchbay] using native libpipewire backend");
				Self {
					state: BackendState::Native(state),
					native_events: Some(events),
				}
			}
			Err(err) => {
				logger::warn(&format!("[patchbay] native backend failed to start ({err}); using legacy backend"));
				Self {
					state: BackendState::Legacy(PatchbayState::new(config)),
					native_events: None,
				}
			}
		}
	}

	/// The receiver for async graph events emitted by the native backend,
	/// if it is in use. main.rs maps these to `graphChanged` protocol
	/// events, replacing the old `pw-mon` subprocess.
	pub fn take_native_events(&mut self) -> Option<state_native::BackendEventReceiver> {
		self.native_events.take()
	}

	pub fn list_shareable_nodes(&self, include_devices: bool) -> Result<Vec<ShareableNode>> {
		match &self.state {
			BackendState::Legacy(state) => state.list_shareable_nodes(include_devices),
			BackendState::Native(state) => state.list_shareable_nodes(include_devices),
		}
	}

	/// Best-effort correlation of an in-progress KDE/KWin window-share with
	/// a likely audio-producing app; see [`ScreencastHint`]'s doc comment
	/// for the mechanism and its limits. `None` means "no active window
	/// share detected" or "not on KWin" -- callers should treat that as
	/// the normal case and fall back to the full unfiltered node list.
	pub fn find_screencast_hint(&self) -> Result<Option<ScreencastHint>> {
		match &self.state {
			BackendState::Legacy(state) => state.find_screencast_hint(),
			BackendState::Native(state) => state.find_screencast_hint(),
		}
	}

	pub fn ensure_virtual_sink(&mut self) -> Result<VirtualSinkInfo> {
		match &mut self.state {
			BackendState::Legacy(state) => state.ensure_virtual_sink(),
			BackendState::Native(state) => state.ensure_virtual_sink(),
		}
	}

	pub fn route_nodes(&mut self, node_ids: Vec<u32>, filter: RouteFilter) -> Result<VirtualSinkInfo> {
		match &mut self.state {
			BackendState::Legacy(state) => state.route_nodes(node_ids),
			BackendState::Native(state) => state.route_nodes(node_ids, filter),
		}
	}

	pub fn clear_routes(&mut self) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(state) => state.clear_routes(),
			BackendState::Native(state) => state.clear_routes(),
		}
	}

	/// Mutes or unmutes the virtual mic node. Only supported by the
	/// native backend, since the legacy CLI backend has no direct handle
	/// to the created PulseAudio/PipeWire objects to apply a mute param
	/// to; callers should treat an error here as "not supported" and
	/// fall back to muting/unmuting the consuming getUserMedia track
	/// instead.
	pub fn set_virtual_mic_mute(&self, mute: bool) -> Result<()> {
		match &self.state {
			BackendState::Legacy(_) => Err(BackendError::Message(
				"setVirtualMicMute is not supported by the legacy backend".to_string(),
			)),
			BackendState::Native(state) => state.set_virtual_mic_mute(mute),
		}
	}

	/// Makes the virtual sink the system default audio sink, remembering
	/// the prior default. Only supported by the native backend (the legacy
	/// CLI backend has no `pactl set-default-sink` equivalent wired up).
	pub fn set_default_sink_to_virtual(&mut self) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(_) => Err(BackendError::Message(
				"setDefaultSinkToVirtual is not supported by the legacy backend".to_string(),
			)),
			BackendState::Native(state) => state.set_default_sink_to_virtual(),
		}
	}

	/// Restores the system default sink overridden by
	/// `set_default_sink_to_virtual`, if any.
	pub fn restore_default_sink(&mut self) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(_) => Ok(()),
			BackendState::Native(state) => state.restore_default_sink(),
		}
	}

	/// Routes the given node id(s) directly into every one of Discord's
	/// own `discord_capture` screenshare-audio nodes, replacing whatever
	/// selection (if any) was previously routed there. Pass an empty
	/// list to stop routing anything (existing links torn down, no
	/// virtual sink/mic involved -- see `state_native`'s own doc comment
	/// for why this needs no sink at all, unlike `route_nodes`). Only
	/// supported by the native backend: the legacy CLI backend has no
	/// live graph-change reactivity to keep this correctly synced as
	/// Discord creates fresh `discord_capture` nodes over time.
	pub fn set_discord_capture_targets(&mut self, node_ids: Vec<u32>, filter: RouteFilter) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(_) => Err(BackendError::Message(
				"setDiscordCaptureTargets is not supported by the legacy backend".to_string(),
			)),
			BackendState::Native(state) => state.set_discord_capture_targets(node_ids, filter),
		}
	}

	/// Re-applies the current `set_discord_capture_targets` selection
	/// against the live graph; called on every `graphChanged` event (see
	/// main.rs) so a freshly-created `discord_capture` node gets linked
	/// without a client round-trip. No-op (`Ok(())`) on the legacy
	/// backend, which doesn't support the `discord_capture` path at all --
	/// deliberately not an error here, since main.rs calls this
	/// unconditionally on every graph change regardless of which backend
	/// ended up active.
	pub fn sync_discord_capture_links(&mut self) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(_) => Ok(()),
			BackendState::Native(state) => state.sync_discord_capture_links(),
		}
	}

	pub fn dispose(&mut self) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(state) => state.dispose(),
			BackendState::Native(state) => state.dispose(),
		}
	}
}
