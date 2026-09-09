//! Links `libpulse.so.0` directly into this shim's own `DT_NEEDED` list.
//!
//! Without this, the shim's own `pa_stream_new`/`pa_stream_connect_record`/
//! `pa_stream_set_monitor_stream` interposer functions call
//! `dlvsym(RTLD_NEXT, ..., "PULSE_0")` to reach the real implementation --
//! but `RTLD_NEXT` only searches shared objects loaded *after* this one in
//! the process's link map. When this shim is the very first object with a
//! versioned `pa_*` symbol anywhere in that search order (confirmed live:
//! reproduced standalone with a minimal `dlopen`-loaded interposer that
//! does not itself depend on `libpulse`), `RTLD_NEXT` cannot find
//! `libpulse.so.0` at all, even though it is definitely loaded elsewhere in
//! the process, and the lookup fails with "undefined symbol: `pa_stream_new`,
//! version `PULSE_0`".
//!
//! Explicitly linking against `libpulse` here gives this shared object its
//! own real `DT_NEEDED` entry for `libpulse.so.0`, which guarantees it is
//! present in the process's link map by the time our interposer functions
//! run, and gives the dynamic linker a real, no-quirks vantage point to
//! resolve the "real" symbol from via `RTLD_NEXT`.
fn main() {
    // Order matters here and normal cargo:rustc-link-lib/-arg combinations
    // cannot express it: cargo always places -l flags emitted via
    // rustc-link-lib *before* any rustc-link-arg flags on the final
    // linker command line, so a plain `--no-as-needed` / `-lpulse` /
    // `--as-needed` split (as separate directives) ends up on the
    // command line in the wrong order (`-lpulse --no-as-needed
    // --as-needed`), which still drops the NEEDED entry (confirmed live
    // via `readelf -d`: libpulse.so.0 absent even with the split
    // directives present in the actual invocation). Passing the whole
    // `--no-as-needed -lpulse --as-needed` sequence as a single
    // rustc-link-arg keeps the ordering intact.
    println!("cargo:rustc-link-arg=-Wl,--no-as-needed,-lpulse,--as-needed");
}
