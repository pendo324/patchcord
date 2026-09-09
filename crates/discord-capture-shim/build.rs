//! Links `libpulse.so.0` directly into this shim's own `DT_NEEDED` list.
//!
//! Without this, the shim's own `pa_stream_new`/`pa_stream_connect_record`/
//! `pa_stream_set_monitor_stream` interposer functions call
//! `dlvsym(RTLD_NEXT, ..., "PULSE_0")` to reach the real implementation --
//! but `RTLD_NEXT` only searches shared objects loaded *after* this one in
//! the process's link map. When this shim is the very first object with a
//! versioned `pa_*` symbol anywhere in that search order, `RTLD_NEXT`
//! cannot find `libpulse.so.0` at all, even though it is definitely loaded
//! elsewhere in the process, and the lookup fails with "undefined symbol:
//! `pa_stream_new`, version `PULSE_0`".
//!
//! Explicitly linking against `libpulse` here gives this shared object its
//! own real `DT_NEEDED` entry for `libpulse.so.0`, which guarantees it is
//! present in the process's link map by the time our interposer functions
//! run, and gives the dynamic linker a real, no-quirks vantage point to
//! resolve the "real" symbol from via `RTLD_NEXT`.
//!
//! `--no-as-needed`/`-lpulse`/`--as-needed` are emitted as three separate
//! `cargo:rustc-link-arg` directives (not packed into one comma-joined
//! `-Wl,--no-as-needed,-lpulse,--as-needed` argument) so this also links
//! correctly under `cargo zigbuild` -- zig's `cc` linker driver rejects a
//! bare `-lpulse` when it's bundled inside a single `-Wl,` argument
//! ("unsupported linker arg: -lpulse"), but accepts it fine as its own
//! argument. Verified this ordering is preserved on the final linker
//! command line (readelf -d confirms the `DT_NEEDED` entry) under both plain
//! `cargo build` and `cargo zigbuild`.
fn main() {
	println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
	println!("cargo:rustc-link-arg=-lpulse");
	println!("cargo:rustc-link-arg=-Wl,--as-needed");
}
