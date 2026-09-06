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
pub use models::{ShareableNode, VirtualSinkInfo};
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
}

impl Default for PatchbayConfig {
	fn default() -> Self {
		Self {
			sink_prefix: "audio-share".to_string(),
			sink_description: "Virtual Audio Share".to_string(),
			virtual_mic: false,
			virtual_mic_name: None,
			virtual_mic_description: None,
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

	pub fn ensure_virtual_sink(&mut self) -> Result<VirtualSinkInfo> {
		match &mut self.state {
			BackendState::Legacy(state) => state.ensure_virtual_sink(),
			BackendState::Native(state) => state.ensure_virtual_sink(),
		}
	}

	pub fn route_nodes(&mut self, node_ids: Vec<u32>) -> Result<VirtualSinkInfo> {
		match &mut self.state {
			BackendState::Legacy(state) => state.route_nodes(node_ids),
			BackendState::Native(state) => state.route_nodes(node_ids),
		}
	}

	pub fn clear_routes(&mut self) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(state) => state.clear_routes(),
			BackendState::Native(state) => state.clear_routes(),
		}
	}

	pub fn dispose(&mut self) -> Result<()> {
		match &mut self.state {
			BackendState::Legacy(state) => state.dispose(),
			BackendState::Native(state) => state.dispose(),
		}
	}
}
