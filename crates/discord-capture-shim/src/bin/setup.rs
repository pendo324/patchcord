//! `discord-capture-setup`: patches a real `discord_voice.node` file so it
//! loads `discord-capture-shim.so` as an ordinary `DT_NEEDED` dependency,
//! without requiring the system `patchelf` package.
//!
//! This exists purely to avoid an external `patchelf` dependency in the
//! distributed Equicord plugin. A single-file shim+installer isn't
//! possible on glibc -- `PT_INTERP` and `dlopen`/`DT_NEEDED`-loadability
//! are mutually exclusive on one ELF file -- so this ships as a second,
//! ordinary binary alongside `discord-capture-shim.so` instead.
//!
//! Equivalent to:
//!   patchelf --add-needed discord-capture-shim.so \
//!            --add-rpath <dir containing discord-capture-shim.so> \
//!            <`discord_voice.node`>
//!
//! Uses `arwen` (pure Rust ELF rewriter) instead of shelling out to
//! `patchelf`. Idempotent: if `discord-capture-shim.so` is already listed
//! in `DT_NEEDED`, this is a no-op (exit 0) rather than adding a duplicate
//! entry or erroring, so `native.ts` can call this unconditionally on
//! every plugin start without tracking whether it already ran.
//!
//! Usage: discord-capture-setup <path-to-`discord_voice.node`> <path-to-shim-dir>
//!
//! `<path-to-shim-dir>` is added to RUNPATH (alongside the existing
//! `$ORIGIN`, not replacing it) so the dynamic linker can actually find
//! `discord-capture-shim.so` at load time regardless of where Discord's
//! own modules directory is.

use std::env;
use std::fs;
use std::process::ExitCode;

use arwen::elf::ElfContainer;

const SHIM_SONAME: &str = "discord-capture-shim.so";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let [_, target_path, shim_dir] = args.as_slice() else {
        eprintln!(
            "usage: discord-capture-setup <path-to-`discord_voice.node`> <path-to-shim-dir>"
        );
        return ExitCode::FAILURE;
    };

    match run(target_path, shim_dir) {
        Ok(Outcome::Patched) => {
            println!("discord-capture-setup: patched {target_path}");
            ExitCode::SUCCESS
        }
        Ok(Outcome::AlreadyPatched) => {
            println!("discord-capture-setup: {target_path} already patched, nothing to do");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("discord-capture-setup: failed to patch {target_path}: {e}");
            ExitCode::FAILURE
        }
    }
}

enum Outcome {
    Patched,
    AlreadyPatched,
}

fn run(target_path: &str, shim_dir: &str) -> Result<Outcome, String> {
    let original = fs::read(target_path).map_err(|e| format!("read: {e}"))?;

    let mut container =
        ElfContainer::parse(&original).map_err(|e| format!("parse ELF: {e}"))?;

    let already_needed = container
        .inner
        .elf_needed()
        .any(|n| n == SHIM_SONAME.as_bytes());

    if already_needed {
        return Ok(Outcome::AlreadyPatched);
    }

    container
        .add_needed(vec![SHIM_SONAME.to_string()])
        .map_err(|e| format!("add DT_NEEDED: {e}"))?;

    // Preserve the existing RUNPATH (e.g. "$ORIGIN") rather than
    // clobbering it -- discord_voice.node's own other DT_NEEDED entries
    // (libmediapipe.so etc.) still need to resolve via $ORIGIN.
    let mut runpath_entries = container.get_rpath();
    let shim_dir_owned = shim_dir.to_string();
    if !runpath_entries.iter().any(|p| p == &shim_dir_owned) {
        runpath_entries.push(shim_dir_owned);
    }
    let new_runpath = runpath_entries.join(":");
    container
        .set_runpath(new_runpath)
        .map_err(|e| format!("set RUNPATH: {e}"))?;

    // Write to a temp file in the same directory then rename over the
    // original atomically, so a crash/failure mid-write never leaves
    // discord_voice.node truncated or corrupt (that would break Discord
    // voice entirely, not just this feature).
    let tmp_path = format!("{target_path}.discord-capture-setup.tmp");
    container
        .write_to_path(std::path::Path::new(&tmp_path))
        .map_err(|e| format!("write patched file: {e}"))?;

    // Preserve the original file's permissions (patchelf-equivalent tools
    // can otherwise leave the output non-executable).
    if let Ok(meta) = fs::metadata(target_path) {
        let _ = fs::set_permissions(&tmp_path, meta.permissions());
    }

    fs::rename(&tmp_path, target_path).map_err(|e| format!("rename into place: {e}"))?;

    Ok(Outcome::Patched)
}
