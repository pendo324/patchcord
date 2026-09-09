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
//! Usage:
//!   discord-capture-setup <path-to-`discord_voice.node`> <path-to-shim-dir>
//!   discord-capture-setup --restore <path-to-`discord_voice.node`>
//!
//! `<path-to-shim-dir>` is added to RUNPATH (alongside the existing
//! `$ORIGIN`, not replacing it) so the dynamic linker can actually find
//! `discord-capture-shim.so` at load time regardless of where Discord's
//! own modules directory is.
//!
//! # Backup / restore
//!
//! Before ever modifying `discord_voice.node`, a copy of the original,
//! untouched file is written alongside it as
//! `discord_voice.node.discord-capture-setup.orig` -- created once (the
//! first successful patch only; a second patch run against an
//! already-patched file is a no-op per the idempotency above, so it
//! never overwrites a good backup with an already-patched copy). This
//! makes the whole operation genuinely reversible: `--restore` copies
//! the backup back over the live file (also via a write-to-temp-then-
//! rename, for the same crash-safety reason as the normal patch path)
//! and leaves the backup in place afterward, so `--restore` can be run
//! more than once safely.
//!
//! Without this, a bad shim build, an incompatible future Discord
//! update, or simply the user wanting to uninstall the plugin would have
//! no way back to a known-good `discord_voice.node` short of a full
//! Discord reinstall -- unacceptable for something that unconditionally
//! patches a binary outside the plugin's own directory on every launch.

use std::env;
use std::fs;
use std::process::ExitCode;

use arwen::elf::ElfContainer;

const SHIM_SONAME: &str = "discord-capture-shim.so";
const BACKUP_SUFFIX: &str = ".discord-capture-setup.orig";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if args.len() == 3 && args[1] == "--restore" {
        return match restore(&args[2]) {
            Ok(true) => {
                println!("discord-capture-setup: restored {} from backup", args[2]);
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("discord-capture-setup: no backup found for {}, nothing to restore", args[2]);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("discord-capture-setup: failed to restore {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        };
    }

    let [_, target_path, shim_dir] = args.as_slice() else {
        eprintln!(
            "usage: discord-capture-setup <path-to-`discord_voice.node`> <path-to-shim-dir>\n       discord-capture-setup --restore <path-to-`discord_voice.node`>"
        );
        return ExitCode::FAILURE;
    };

    match run(target_path, shim_dir) {
        Ok(Outcome::Patched) => {
            println!("discord-capture-setup: patched {target_path} (backup saved as {target_path}{BACKUP_SUFFIX})");
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

/// Restores `target_path` from its `BACKUP_SUFFIX` sidecar, if one
/// exists. Returns `Ok(false)` (not an error) when no backup exists --
/// e.g. `--restore` run on a file that was never patched.
fn restore(target_path: &str) -> Result<bool, String> {
    let backup_path = format!("{target_path}{BACKUP_SUFFIX}");
    if !std::path::Path::new(&backup_path).exists() {
        return Ok(false);
    }

    let tmp_path = format!("{target_path}.discord-capture-setup.restoretmp");
    fs::copy(&backup_path, &tmp_path).map_err(|e| format!("copy backup to temp: {e}"))?;

    if let Ok(meta) = fs::metadata(target_path) {
        let _ = fs::set_permissions(&tmp_path, meta.permissions());
    }

    fs::rename(&tmp_path, target_path).map_err(|e| format!("rename into place: {e}"))?;
    Ok(true)
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

    // Save a backup of the genuinely-untouched original before making
    // any change -- only reached when we've just confirmed the file is
    // NOT already patched, so this can never save an already-patched
    // copy as the "original". Written before the ELF is modified at all
    // (not after), so a failure partway through add_needed/set_runpath
    // below still leaves a valid backup in place.
    let backup_path = format!("{target_path}{BACKUP_SUFFIX}");
    if !std::path::Path::new(&backup_path).exists() {
        fs::write(&backup_path, &original).map_err(|e| format!("write backup: {e}"))?;
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

