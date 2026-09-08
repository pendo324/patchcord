//! discord-capture-shim: an `LD_PRELOAD`-loaded shared library whose only
//! job is to stop Discord's native "Stream With Audio" feature from
//! auto-linking its own per-app `discord_capture` `PipeWire` nodes to every
//! detected audio-producing app on the system.
//!
//! # Background
//!
//! Discord's `discord_voice.node` addon creates one `PipeWire`
//! `Stream/Input/Audio` node per app it detects audio from
//! (`node.name = "discord_capture"`, `media.name = "game capture"`,
//! `application.process.binary = "Discord"`), and links each one
//! *individually* to that specific app's output -- confirmed live via a
//! real screenshare test: disconnecting one app's link to its
//! `discord_capture` node silenced that app for a live viewer instantly,
//! and reconnecting it restored the audio. There is no existing Discord
//! UI to choose which single app should actually be shared; every
//! detected app's `discord_capture` link is live simultaneously.
//!
//! The per-app targeting is set via the `PW_KEY_TARGET_OBJECT` (and
//! `PW_KEY_NODE_AUTOCONNECT`) properties passed to `pw_stream_new` at the
//! moment Discord creates the stream -- confirmed live: each
//! `discord_capture` node's `target.object` property exactly matches one
//! specific app's `object.serial`.
//!
//! # Why interception has to happen via `dlsym`, not a normal symbol
//! override
//!
//! `discord_voice.node` does not link `libpipewire-0.3.so.0` at build
//! time (confirmed via `ldd`: absent from the needed-library list). It
//! `dlopen()`s it at runtime and resolves every individual `pw_*`
//! function via `dlsym()` (confirmed via ~88 distinct `dlsym@plt` call
//! sites and every `pw_stream_new`/`pw_properties_set`/etc. symbol name
//! present as a string in the binary -- the standard
//! `pipewire-rs`/`libpipewire-sys` runtime-loader pattern). A
//! conventional `LD_PRELOAD` symbol override relying on the dynamic
//! linker's normal PLT/GOT resolution therefore does **not** intercept
//! these calls: Discord never asks the dynamic linker to resolve
//! `pw_stream_new` by name at load time, it asks `dlsym()` explicitly,
//! at a call site *we* can intercept instead by overriding `dlsym`
//! itself.
//!
//! # What this shim does NOT do
//!
//! It has no knowledge of patchcord, performs no `PipeWire` graph
//! management or linking of its own, and makes no decision about which
//! app should actually be shared -- that's patchcord's job, reacting to
//! `discord_capture` nodes that (thanks to this shim) show up in the
//! graph with no target and no autoconnect. This shim's only effect is
//! to blank two string properties on one specific stream, immediately
//! before forwarding to the real, completely unmodified
//! `pw_stream_new` -- Discord's own capture/`pw_stream_dequeue_buffer`
//! code path, its stream lifecycle, and every other node it creates for
//! any other purpose, are all untouched.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::OnceLock;

/// Real `dlsym` resolved once via `RTLD_NEXT`, so we can both (a) look up
/// the real `pw_stream_new` to wrap, and (b) forward every *other*
/// symbol lookup made through us unmodified. `RTLD_NEXT` (rather than a
/// specific handle) is required here because our own `dlsym` override
/// intercepts calls regardless of which `dlopen` handle the caller
/// passed in, and the real implementation living "next" in the search
/// order after this shared object is exactly what an interposing
/// `LD_PRELOAD` library is supposed to chain to.
type DlsymFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

fn real_dlsym() -> DlsymFn {
    static REAL_DLSYM: OnceLock<usize> = OnceLock::new();
    let addr = *REAL_DLSYM.get_or_init(|| {
        // SAFETY: RTLD_NEXT + a well-known libc symbol name/version is
        // the standard, documented way for an LD_PRELOAD interposer to
        // reach the "real" implementation of a function it is itself
        // overriding.
        //
        // This MUST be dlvsym, not plain dlsym: this shared object
        // itself exports a symbol named "dlsym" (see the `dlsym`
        // function below), so a plain `libc::dlsym(RTLD_NEXT, "dlsym")`
        // call resolves back to *our own* exported dlsym (the dynamic
        // linker has no notion of "this call originates from inside a
        // dlsym implementation" -- it just resolves the symbol name
        // through the normal search order, which now includes this .so
        // ahead of glibc's real one). That self-recursion was confirmed
        // live: it manifests as this exact call deadlocking inside
        // OnceLock::get_or_init (the recursive call re-enters this same
        // closure before the first call has returned to store a value,
        // so the second get_or_init call blocks on the first's
        // in-progress initialization forever) -- not a crash, not an
        // error return, an unrecoverable hang on every single dlsym call
        // made anywhere in the process for its entire lifetime.
        // dlvsym's explicit GLIBC_2.2.5 version tag requests a symbol
        // this shared object itself does not export (we only export
        // unversioned "dlsym"), which is why it correctly reaches
        // glibc's real implementation instead.
        let ptr = unsafe { libc::dlvsym(libc::RTLD_NEXT, c"dlsym".as_ptr(), c"GLIBC_2.2.5".as_ptr()) };
        assert!(!ptr.is_null(), "discord-capture-shim: failed to resolve real dlsym via dlvsym(RTLD_NEXT, \"dlsym\", \"GLIBC_2.2.5\")");
        ptr as usize
    });
    // SAFETY: addr was produced by a successful dlvsym(RTLD_NEXT, "dlsym",
    // "GLIBC_2.2.5") call above, so it is a valid function pointer with
    // dlsym's own documented signature.
    unsafe { std::mem::transmute::<usize, DlsymFn>(addr) }
}

type PwStreamNewFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void;
type PwPropertiesGetFn = unsafe extern "C" fn(*const c_void, *const c_char) -> *const c_char;
type PwPropertiesSetFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;

struct PwPropertiesFns {
    get: PwPropertiesGetFn,
    set: PwPropertiesSetFn,
}

/// Resolves `pw_properties_get`/`pw_properties_set` via the *real* dlsym
/// (not our own override -- these names aren't ones we intercept, so
/// going through our own `dlsym_wrapper` would just bounce straight back
/// to `real_dlsym` anyway, but calling `real_dlsym` directly here is more
/// obviously correct and avoids any risk of accidental recursion).
/// `handle` is whatever handle the caller's own `dlopen`d
/// libpipewire-0.3.so.0 module produced -- passed through from the same
/// `dlsym(handle, "pw_stream_new")` call this shim is intercepting, so
/// it's guaranteed to be the correct handle to resolve sibling symbols
/// from.
fn resolve_properties_fns(handle: *mut c_void) -> Option<PwPropertiesFns> {
    let dlsym = real_dlsym();
    // SAFETY: `handle` came from the caller's own dlopen call (forwarded
    // to us via the intercepted dlsym call), and c"..." string literals
    // are valid NUL-terminated C strings for the lifetime of this call.
    unsafe {
        let get = dlsym(handle, c"pw_properties_get".as_ptr());
        let set = dlsym(handle, c"pw_properties_set".as_ptr());
        if get.is_null() || set.is_null() {
            return None;
        }
        Some(PwPropertiesFns {
            get: std::mem::transmute::<*mut c_void, PwPropertiesGetFn>(get),
            set: std::mem::transmute::<*mut c_void, PwPropertiesSetFn>(set),
        })
    }
}

const PW_KEY_APPLICATION_PROCESS_BINARY: &CStr = c"application.process.binary";
const PW_KEY_NODE_NAME: &CStr = c"node.name";
const PW_KEY_MEDIA_NAME: &CStr = c"media.name";
const PW_KEY_TARGET_OBJECT: &CStr = c"target.object";
const PW_KEY_NODE_AUTOCONNECT: &CStr = c"node.autoconnect";

/// True only for the exact node identity confirmed live in this
/// session's `PipeWire` investigation: `node.name = "discord_capture"`,
/// `media.name = "game capture"`, `application.process.binary =
/// "Discord"`. Checking all three (not just `node.name`) keeps this from
/// ever matching an unrelated stream that happens to reuse the
/// `discord_capture` name in some other application -- unlikely, but
/// cheap to guard against given how much this shim's blast radius matters
/// (it runs inside every stream creation in Discord's whole process).
fn is_discord_capture_stream(props: *const c_void, fns: &PwPropertiesFns) -> bool {
    // SAFETY: props is the pw_properties pointer Discord itself is about
    // to pass to the real pw_stream_new; pw_properties_get is a
    // documented read-only accessor safe to call with any valid
    // pw_properties pointer and NUL-terminated key.
    unsafe fn get(fns: &PwPropertiesFns, props: *const c_void, key: &CStr) -> Option<String> {
        // SAFETY: see this outer function's own SAFETY comment above --
        // callers of `get` uphold the same preconditions.
        unsafe {
            let ptr = (fns.get)(props, key.as_ptr());
            if ptr.is_null() {
                return None;
            }
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }

    if props.is_null() {
        return false;
    }

    // SAFETY: see get()'s own SAFETY comment; each call is independently
    // sound for the same reasons.
    unsafe {
        get(fns, props, PW_KEY_NODE_NAME).as_deref() == Some("discord_capture")
            && get(fns, props, PW_KEY_MEDIA_NAME).as_deref() == Some("game capture")
            && get(fns, props, PW_KEY_APPLICATION_PROCESS_BINARY).as_deref() == Some("Discord")
    }
}

/// The actual `pw_stream_new` wrapper installed in place of the real
/// symbol. Blanks `target.object`/`node.autoconnect` on exactly the
/// `discord_capture` stream identity, forwards every other stream's
/// properties completely unmodified, then calls straight through to the
/// real `pw_stream_new` either way -- stream creation, ownership, and
/// Discord's own subsequent use of the returned `pw_stream*` (including
/// `pw_stream_dequeue_buffer`, `pw_stream_connect`, etc.) are entirely
/// untouched by this shim.
unsafe extern "C" fn pw_stream_new_wrapper(
    core: *mut c_void,
    name: *const c_char,
    props: *mut c_void,
) -> *mut c_void {
    // Catches any panic so it can never unwind across this extern "C"
    // boundary into Discord's own C++ call stack (unwinding across an
    // FFI boundary is undefined behavior) -- worst case on a bug here is
    // "this one stream's properties weren't modified, fall through to
    // the real call unchanged", never a crash.
    let result = std::panic::catch_unwind(|| {
        // SAFETY: this closure only reads `props` through the properties
        // accessor functions (never touches its layout directly), and
        // only after checking it against the real function pointer table
        // resolved once the first pw_stream_new lookup was intercepted
        // (see properties_fns's own doc comment for how/when that
        // happens).
        if let Some(fns) = properties_fns()
            && is_discord_capture_stream(props, &fns)
        {
            log("blanking target.object/node.autoconnect on a discord_capture stream");
            // SAFETY: props is a valid pw_properties* per this
            // function's own contract (it's what's about to be
            // passed to the real pw_stream_new), and pw_properties_set
            // is documented safe to call with a NULL value to clear a
            // key.
            unsafe {
                (fns.set)(props, PW_KEY_TARGET_OBJECT.as_ptr(), std::ptr::null());
                (fns.set)(props, PW_KEY_NODE_AUTOCONNECT.as_ptr(), std::ptr::null());
            }
        }
    });
    if result.is_err() {
        log("panic caught in pw_stream_new wrapper; forwarding to real pw_stream_new unmodified");
    }

    let real = real_pw_stream_new();
    // SAFETY: forwarding the caller's own arguments, unmodified except
    // for the in-place property edits already applied above (which use
    // the same real pw_properties_set the caller itself would have used),
    // to the real pw_stream_new resolved from the same library.
    unsafe { real(core, name, props) }
}

/// Resolved once, the first time `dlsym(handle, "pw_stream_new")` is
/// actually intercepted (see `dlsym`'s own doc comment for why it can't
/// be resolved any earlier -- this shared object has no reliable
/// constructor-time hook into when Discord's own `dlopen` of
/// libpipewire-0.3.so.0 happens). Stored as raw addresses (usize) rather
/// than the function-pointer types directly so a plain `OnceLock` can hold
/// them without needing an unsafe Sync impl for a wrapper struct --
/// function pointers are already Sync/Send, but a struct of them isn't
/// automatically inferred as such without deriving/asserting it, and
/// storing addresses sidesteps that entirely.
static REAL_PW_STREAM_NEW: OnceLock<usize> = OnceLock::new();
static PW_PROPERTIES_GET: OnceLock<usize> = OnceLock::new();
static PW_PROPERTIES_SET: OnceLock<usize> = OnceLock::new();

fn real_pw_stream_new() -> PwStreamNewFn {
    #[allow(clippy::missing_panics_doc)]
    let addr = *REAL_PW_STREAM_NEW.get().expect(
        "discord-capture-shim: pw_stream_new wrapper invoked before REAL_PW_STREAM_NEW was set",
    );
    // SAFETY: addr was produced by a successful real dlsym(handle,
    // "pw_stream_new") call in dlsym below, so it is a valid function
    // pointer matching pw_stream_new's documented signature.
    unsafe { std::mem::transmute::<usize, PwStreamNewFn>(addr) }
}

fn properties_fns() -> Option<PwPropertiesFns> {
    let get = *PW_PROPERTIES_GET.get()?;
    let set = *PW_PROPERTIES_SET.get()?;
    if get == 0 || set == 0 {
        return None;
    }
    // SAFETY: get/set were produced by successful real dlsym calls in
    // resolve_properties_fns, stored as usize purely so they can live in
    // a plain OnceLock<usize>.
    unsafe {
        Some(PwPropertiesFns {
            get: std::mem::transmute::<usize, PwPropertiesGetFn>(get),
            set: std::mem::transmute::<usize, PwPropertiesSetFn>(set),
        })
    }
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

/// The actual symbol this shared library exports to satisfy `LD_PRELOAD`
/// interposition.
///
/// Discord's own libc `dlsym` PLT entry resolves to this
/// function instead of glibc's real one, for every single `dlsym` call
/// made anywhere in Discord's process for the remainder of its lifetime
/// -- not just the ones related to `PipeWire`. Every symbol name other than
/// `"pw_stream_new"` must be forwarded completely unmodified for the
/// process to keep working at all.
///
/// # Safety
///
/// `symbol` must be a valid, NUL-terminated C string for the duration of
/// this call, exactly as required by the real `dlsym` this function
/// stands in for -- any caller invoking the real `dlsym` with this
/// pointer would already need to uphold the same precondition, so this
/// wrapper adds no additional safety requirement beyond what `dlsym`
/// itself already documents.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void {
    // SAFETY: symbol is whatever the caller (Discord's own code, or any
    // other code in the process) passed to a real dlsym call; per dlsym's
    // own contract it must already be a valid NUL-terminated C string, or
    // the caller's own use of libc's dlsym would already be unsound
    // regardless of this wrapper's presence.
    let name = unsafe { CStr::from_ptr(symbol) };

    if name.to_bytes() == b"pw_stream_new" {
        // Resolve and cache the real pw_stream_new (and the
        // pw_properties accessor functions from the same handle) the
        // first time this specific lookup happens, then always return
        // our own wrapper's address from then on -- including on this
        // very first call, so the immediate caller gets the wrapper too.
        if REAL_PW_STREAM_NEW.get().is_none() {
            let real = real_dlsym();
            // SAFETY: forwarding the caller's own handle/symbol
            // arguments to the real dlsym, which is always sound to call
            // with a valid handle and NUL-terminated symbol name.
            let real_addr = unsafe { real(handle, symbol) };
            if real_addr.is_null() {
                log("pw_stream_new not found via real dlsym; not intercepting");
                return real_addr;
            }
            let _ = REAL_PW_STREAM_NEW.set(real_addr as usize);

            let props_fns = resolve_properties_fns(handle);
            match props_fns {
                Some(fns) => {
                    let _ = PW_PROPERTIES_GET.set(fns.get as usize);
                    let _ = PW_PROPERTIES_SET.set(fns.set as usize);
                    log("intercepting pw_stream_new");
                }
                None => {
                    log(
                        "pw_properties_get/pw_properties_set not resolvable; \
                         discord_capture streams will NOT be intercepted",
                    );
                }
            }
        }
        return pw_stream_new_wrapper as *mut c_void;
    }

    let real = real_dlsym();
    // SAFETY: forwarding the caller's own handle/symbol arguments,
    // unmodified, to the real dlsym.
    unsafe { real(handle, symbol) }
}

/// Sanity marker export.
///
/// Lets `nm`/`objdump` (or a future test harness) confirm this exact
/// shim built and is the one actually loaded, without needing to trigger
/// a real `dlsym` lookup of `"pw_stream_new"` first.
#[unsafe(no_mangle)]
pub extern "C" fn discord_capture_shim_version() -> *const c_char {
    static VERSION: OnceLock<CString> = OnceLock::new();
    VERSION
        .get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).unwrap_or_default())
        .as_ptr()
}
