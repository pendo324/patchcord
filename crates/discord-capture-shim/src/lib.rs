//! discord-capture-shim: an `LD_PRELOAD`-loaded shared library that stops
//! Discord's native "Stream With Audio" feature from auto-linking its
//! per-app `discord_capture` audio-capture streams to every
//! audio-producing app on the system.
//!
//! # Background
//!
//! Discord's `discord_voice.node` addon creates one `PulseAudio` record
//! stream per app it detects audio from (`pa_stream_new(..., name="game
//! capture", ...)`), points it at that app's audio via
//! `pa_stream_set_monitor_stream(stream, sink_input_idx)` (the exact
//! `pactl list sink-inputs` index), then calls
//! `pa_stream_connect_record(stream, dev: NULL, ...)` to start capture.
//! `dev: NULL` is why the monitor-stream call is the only place the
//! per-app target is expressed.
//!
//! This is not the same mechanism as the video desktop-capture stream,
//! which uses `pw_stream_new`/`PipeWire`'s `target.object` directly --
//! `discord_capture` nodes exist in the `PipeWire` graph only because
//! `pipewire-pulse` creates them server-side once the `PulseAudio` record
//! stream connects. Discord's own process only speaks the `PulseAudio`
//! client API for this feature.
//!
//! There's no Discord UI to choose which app should be shared; every
//! detected app's `discord_capture` stream is live simultaneously.
//!
//! # Why plain symbol interposition, not a `dlsym` hook
//!
//! `discord_voice.node` links `libpulse.so.0` directly at build time
//! (ordinary versioned `@PULSE_0` symbols), unlike `libpipewire-0.3.so.0`
//! which it dlopen()s/dlsym()s at runtime. Because it's an ordinary
//! dynamic dependency, a same-named exported symbol from this
//! `LD_PRELOAD`'d/`DT_NEEDED`'d shared object is resolved ahead of the real
//! libpulse.so.0 by the normal dynamic linker search order.
//!
//! # Per-app targeting
//!
//! `is_allowed(sink_input_idx)` reads a plain-text allow-list file,
//! `$XDG_RUNTIME_DIR/patchcord/discord-capture-allowed.<pid>`, where
//! `<pid>` is this process's parent pid. `discord_voice.node` loads
//! inside Discord's renderer process, whose direct parent is Discord's
//! main Electron process -- the same process patchcord and this
//! plugin's `native.ts` already run in, so both sides derive the same
//! path independently with no IPC handshake needed.
//!
//! Format: one decimal sink-input index per line. Missing file or an
//! unlisted index means "not allowed" -- fails safe.
//!
//! # What this shim does NOT do
//!
//! No knowledge of patchcord's `PipeWire` graph management, no linking of
//! its own, and nothing outside the exact `pa_stream_set_monitor_stream`
//! call this file intercepts -- every other `PulseAudio`/`PipeWire` call
//! Discord makes is untouched.

use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------
// Real-symbol resolution.
//
// `discord_voice.node` links `libpulse.so.0` directly (ordinary
// versioned `@PULSE_0` symbols), so classic ELF symbol interposition --
// just exporting a same-named symbol from this LD_PRELOAD'd/DT_NEEDED'd
// shared object -- is enough; no dlsym hook is needed for this API.
//
// This shim must itself carry a real DT_NEEDED entry on libpulse.so.0
// (see build.rs) for `dlvsym(RTLD_NEXT, ...)` to reliably find the real
// implementation: without it, RTLD_NEXT has no vantage point to see
// libpulse.so.0 in the search order, and dlvsym fails with "undefined
// symbol ... version PULSE_0" even though libpulse is loaded elsewhere
// in the same process.
// ---------------------------------------------------------------------

type PaStreamSetMonitorStreamFn = unsafe extern "C" fn(*mut c_void, u32) -> c_int;
type PaStreamConnectRecordFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, c_int) -> c_int;

static REAL_PA_STREAM_SET_MONITOR_STREAM: OnceLock<usize> = OnceLock::new();
static REAL_PA_STREAM_CONNECT_RECORD: OnceLock<usize> = OnceLock::new();

fn real_pa_symbol(name: &CStr, version: &CStr) -> *mut c_void {
	// SAFETY: RTLD_NEXT + valid NUL-terminated C strings is the
	// documented way to reach the "real" implementation of a
	// specifically-versioned symbol from an interposing shared object.
	let ptr = unsafe { libc::dlvsym(libc::RTLD_NEXT, name.as_ptr(), version.as_ptr()) };
	assert!(
		!ptr.is_null(),
		"discord-capture-shim: failed to resolve real {name:?} via dlvsym(RTLD_NEXT, ..., \"PULSE_0\")"
	);
	ptr
}

fn real_pa_stream_set_monitor_stream() -> PaStreamSetMonitorStreamFn {
	let addr = *REAL_PA_STREAM_SET_MONITOR_STREAM.get_or_init(|| real_pa_symbol(c"pa_stream_set_monitor_stream", c"PULSE_0") as usize);
	// SAFETY: addr was produced by a successful dlvsym call resolving
	// the real pa_stream_set_monitor_stream, so it matches that
	// function's documented signature.
	unsafe { std::mem::transmute::<usize, PaStreamSetMonitorStreamFn>(addr) }
}

fn real_pa_stream_connect_record() -> PaStreamConnectRecordFn {
	let addr = *REAL_PA_STREAM_CONNECT_RECORD.get_or_init(|| real_pa_symbol(c"pa_stream_connect_record", c"PULSE_0") as usize);
	// SAFETY: addr was produced by a successful dlvsym call resolving
	// the real pa_stream_connect_record, so it matches that function's
	// documented signature.
	unsafe { std::mem::transmute::<usize, PaStreamConnectRecordFn>(addr) }
}

// ---------------------------------------------------------------------
// Allow-list lookup.
// ---------------------------------------------------------------------

/// Cached once per process: the parent pid this shim's allow-list file is
/// scoped to. `getppid()` is cheap but not free, and this shim's parent
/// never changes for the lifetime of the process, so caching avoids a
/// syscall on every single interposed call.
fn allowlist_parent_pid() -> u32 {
	static PPID: AtomicU32 = AtomicU32::new(0);
	let cached = PPID.load(Ordering::Relaxed);
	if cached != 0 {
		return cached;
	}
	// SAFETY: getppid() takes no arguments and cannot fail.
	let ppid = unsafe { libc::getppid() }.cast_unsigned();
	PPID.store(ppid, Ordering::Relaxed);
	ppid
}

/// `$XDG_RUNTIME_DIR`, falling back to the POSIX-conventional
/// `/run/user/<uid>` if the environment variable isn't set.
fn runtime_dir() -> String {
	if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
		&& !dir.is_empty()
	{
		return dir;
	}
	// SAFETY: getuid() takes no arguments and cannot fail.
	let uid = unsafe { libc::getuid() };
	format!("/run/user/{uid}")
}

fn allowlist_path() -> std::path::PathBuf {
	std::path::Path::new(&runtime_dir())
		.join("patchcord")
		.join(format!("discord-capture-allowed.{}", allowlist_parent_pid()))
}

/// Reads the allow-list file fresh on every call (deliberately not
/// cached): this is a small, local plain-text file read once per
/// `discord_capture` stream setup, not a hot path, and re-reading it
/// live means a user changing their app selection takes effect on the
/// *next* app the `discord_capture` code path attempts to monitor, with no
/// need for this shim to be told about changes via any additional
/// mechanism.
fn is_allowed(sink_input_idx: u32) -> bool {
	let path = allowlist_path();
	let Ok(contents) = std::fs::read_to_string(&path) else {
		log(&format!(
			"allow-list file {} not readable; treating sink_input_idx={sink_input_idx} as not allowed",
			path.display()
		));
		return false;
	};
	contents
		.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty())
		.any(|line| line.parse::<u32>() == Ok(sink_input_idx))
}

// ---------------------------------------------------------------------
// Blocked-stream tracking + the null-sink redirect target.
//
// Suppressing the `pa_stream_set_monitor_stream` call alone is NOT
// sufficient to silence a disallowed app: a stream with no monitor
// target, connected via `pa_stream_connect_record(s, dev: NULL, ...)`
// (exactly what Discord does, unconditionally, right after the
// monitor-stream call), falls back to recording the system's actual
// default input device -- the user's real microphone -- which is
// strictly worse than the bug this shim exists to fix. Instead, every
// blocked stream is remembered (by its `pa_stream*` pointer) so the
// subsequent `pa_stream_connect_record` call for that stream can be
// redirected to a dedicated, silent null-sink's monitor instead of
// leaving `dev` as whatever Discord itself passed (always NULL in
// practice, but
// redirecting unconditionally for a blocked stream is correct
// regardless of what Discord passes).
// ---------------------------------------------------------------------

/// Name of an always-silent virtual sink this shim expects patchcord to
/// have already created (lazily, via `PatchbayStateNative::ensure_discord_capture_null_sink`,
/// the first time `set_discord_capture_targets` is called with a
/// non-empty selection) before any stream is ever blocked -- not created
/// by this shim itself, since a shared library injected into Discord's
/// own process is the wrong place to be managing global
/// `PipeWire` modules). Its `.monitor` source is permanently silent
/// (nothing is ever routed to the null sink itself), making it a safe,
/// always-available redirect target for any stream this shim blocks.
const NULL_SINK_MONITOR_SOURCE: &CStr = c"discord-capture-null.monitor";

fn blocked_streams() -> &'static Mutex<HashSet<usize>> {
	static BLOCKED: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
	BLOCKED.get_or_init(|| Mutex::new(HashSet::new()))
}

// ---------------------------------------------------------------------
// The actual interposed symbols.
// ---------------------------------------------------------------------

/// Interposed `pa_stream_set_monitor_stream(stream, sink_input_idx)`.
///
/// If `sink_input_idx` is in the current allow-list, forwards to the real
/// implementation unmodified -- the resulting `discord_capture` stream
/// (and the `PipeWire` node `pipewire-pulse` creates for it server-side)
/// comes up pointed at that app's audio exactly as Discord intended.
///
/// If it is not allowed, this call is skipped (never forwarded) and `0`
/// (success) is returned so Discord's own error handling doesn't treat
/// this as a failure worth surfacing to the user; the `stream` pointer is
/// recorded in `blocked_streams()` so the subsequent
/// `pa_stream_connect_record` call for the same stream (see that
/// function's own doc comment) knows to redirect it to a silent source
/// instead of whatever `dev` Discord itself passes.
///
/// # Safety
/// Same preconditions as the real `pa_stream_set_monitor_stream`: `stream`
/// must be a valid, non-connected `pa_stream*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pa_stream_set_monitor_stream(stream: *mut c_void, sink_input_idx: u32) -> c_int {
	if is_allowed(sink_input_idx) {
		log(&format!(
			"pa_stream_set_monitor_stream: sink_input_idx={sink_input_idx} allowed, forwarding"
		));
		// A stream pointer can be reused by the allocator after an
		// earlier stream on the same address was destroyed; clear any
		// stale "blocked" marking for this address before allowing it
		// through, so a previously-blocked-then-freed stream's address
		// being reused for a now-allowed stream can never accidentally
		// redirect it to the null sink.
		if let Ok(mut blocked) = blocked_streams().lock() {
			blocked.remove(&(stream as usize));
		}
		// SAFETY: forwarding the caller's own arguments, unmodified, to
		// the real pa_stream_set_monitor_stream.
		return unsafe { real_pa_stream_set_monitor_stream()(stream, sink_input_idx) };
	}
	log(&format!(
		"pa_stream_set_monitor_stream: sink_input_idx={sink_input_idx} NOT allowed, suppressing call and marking stream for null-sink redirect"
	));
	if let Ok(mut blocked) = blocked_streams().lock() {
		blocked.insert(stream as usize);
	}
	0
}

/// Interposed `pa_stream_connect_record(stream, dev, attr, flags)`.
///
/// For a stream previously marked blocked by
/// `pa_stream_set_monitor_stream` above, `dev` is replaced with
/// `discord_capture_null.monitor` (a permanently silent source) before
/// forwarding, regardless of what Discord itself passed -- this is the
/// actual point of capture start, and is what prevents the stream from
/// falling back to the system's real default microphone (see
/// `blocked_streams`'s own doc comment for why that fallback is the
/// specific failure mode this exists to prevent).
///
/// Every other stream (allowed `discord_capture` streams, and any other
/// `PulseAudio` record stream Discord's process happens to open, such as
/// its own microphone input for voice chat) passes through completely
/// unmodified.
///
/// # Safety
/// Same preconditions as the real `pa_stream_connect_record`: `stream`
/// must be a valid `pa_stream*` in the correct state to connect, and
/// `dev` (if non-null) must be a valid NUL-terminated C string for the
/// duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pa_stream_connect_record(stream: *mut c_void, dev: *const c_char, attr: *const c_void, flags: c_int) -> c_int {
	let is_blocked = blocked_streams().lock().is_ok_and(|blocked| blocked.contains(&(stream as usize)));

	if is_blocked {
		log("pa_stream_connect_record: stream previously blocked, redirecting dev to discord_capture_null.monitor");
		// SAFETY: forwarding stream/attr/flags unmodified, and
		// NULL_SINK_MONITOR_SOURCE is a valid `'static` NUL-terminated C
		// string literal, sound to pass in place of the caller's own
		// `dev` argument.
		return unsafe { real_pa_stream_connect_record()(stream, NULL_SINK_MONITOR_SOURCE.as_ptr(), attr, flags) };
	}

	// SAFETY: forwarding the caller's own arguments, completely
	// unmodified, to the real pa_stream_connect_record.
	unsafe { real_pa_stream_connect_record()(stream, dev, attr, flags) }
}

fn log(msg: &str) {
	// Deliberately plain eprintln rather than a logging crate: this is a
	// tiny, dependency-minimal shim injected into another process's
	// address space, and stderr is already redirected/captured by
	// Discord's own process supervision (or the terminal it was launched
	// from) -- no separate log file/config needed for something this
	// small.
	eprintln!("[discord-capture-shim] {msg}");
}

/// Sanity marker export.
///
/// Lets `nm`/`objdump` (or a future test harness) confirm this exact
/// shim built and is the one actually loaded, without needing to trigger
/// a real interposed call first.
#[unsafe(no_mangle)]
pub extern "C" fn discord_capture_shim_version() -> *const c_char {
	static VERSION: OnceLock<CString> = OnceLock::new();
	VERSION
		.get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).unwrap_or_default())
		.as_ptr()
}
