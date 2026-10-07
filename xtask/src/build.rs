//! Helpers shared by the xtask commands: the repository root and building xtask itself with
//! another cargo profile (bench measures a release build, fuzz runs an unwinding optimised one).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

pub fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask lives in the repository root")
}

/// The xtask executable built with cargo `profile`: this one if it already is that profile (or
/// `use_current` is set), otherwise a freshly built one in the same target directory. `why` is
/// printed while building, e.g. "bench: ... (measurements need an optimised build)".
pub fn profile_exe(profile: &str, use_current: bool, why: &str) -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe().context("locating the xtask executable")?;
    if use_current {
        return Ok(current);
    }
    let profile_dir = match profile {
        "dev" | "test" => "debug",
        "bench" => "release",
        p => p,
    };
    let exe_dir = current.parent().context("xtask executable has no parent directory")?;
    if exe_dir.file_name() == Some(OsStr::new(profile_dir)) {
        return Ok(current);
    }
    let target_dir = exe_dir.parent().context("xtask executable is not inside a cargo target directory")?;
    eprintln!("{why}");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--quiet", "--package", "xtask", "--profile", profile, "--target-dir"])
        .arg(target_dir)
        .current_dir(repo_root())
        .status()
        .context("running cargo build")?;
    if !status.success() {
        bail!("cargo build --profile {profile} failed ({status})");
    }
    let exe = target_dir.join(profile_dir).join(current.file_name().context("xtask executable has no file name")?);
    if !exe.is_file() {
        bail!("built xtask not found at {}", exe.display());
    }
    Ok(exe)
}
