use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::backend::Kind;

const BASE_DOCKERFILE: &str = include_str!("../images/base/Dockerfile");

/// The default image, tagged by a hash of its Dockerfile so edits trigger a rebuild.
pub fn base_tag() -> String {
    format!(
        "sandbox-base:{:012x}",
        fnv1a(BASE_DOCKERFILE) & 0xffff_ffff_ffff
    )
}

/// Builds the base image for `kind` unless it already exists.
pub fn ensure_base(kind: Kind) -> Result<String> {
    let tag = base_tag();
    if exists(kind, &tag) {
        return Ok(tag);
    }
    let dir = cache_dir()?.join("images").join(tag.replace(':', "-"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("Dockerfile"), BASE_DOCKERFILE)?;

    eprintln!("sandbox: building the base image {tag} (first run only, about a minute)");
    if build(kind, &tag, &dir).is_ok() {
        return Ok(tag);
    }
    // Apple's builder runs in its own VM; if it can't build (no network, builder
    // broken), a working Docker can build it and hand the image over.
    if kind == Kind::AppleContainer && Kind::Docker.probe().is_ok() {
        eprintln!("sandbox: apple-container build failed; building with docker and importing");
        if !exists(Kind::Docker, &tag) {
            build(Kind::Docker, &tag, &dir)?;
        }
        let tar = dir.join("image.tar");
        run(Command::new("docker").args(["save", &tag, "-o"]).arg(&tar))?;
        let loaded = run(Command::new("container")
            .args(["image", "load", "-i"])
            .arg(&tar));
        let _ = std::fs::remove_file(&tar);
        loaded?;
        return Ok(tag);
    }
    bail!(
        "building {tag} with {} failed (see output above)",
        kind.name()
    )
}

fn exists(kind: Kind, tag: &str) -> bool {
    Command::new(kind.bin())
        .args(["image", "inspect", tag])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn build(kind: Kind, tag: &str, dir: &std::path::Path) -> Result<()> {
    run(Command::new(kind.bin()).args(["build", "-t", tag]).arg(dir))
}

/// Runs a command with its output on our stderr, keeping stdout for the box.
fn run(cmd: &mut Command) -> Result<()> {
    let stderr = std::io::stderr().as_fd().try_clone_to_owned()?;
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(stderr))
        .status()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} failed", cmd.get_program());
    }
    Ok(())
}

fn cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("sandbox"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cache").join("sandbox"))
}

/// FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_is_stable_and_content_addressed() {
        assert_eq!(base_tag(), base_tag());
        assert!(base_tag().starts_with("sandbox-base:"));
        assert_eq!(base_tag().len(), "sandbox-base:".len() + 12);
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a("a"), fnv1a("b"));
    }

    #[test]
    fn base_image_ships_agent_tools() {
        for tool in ["git", "ripgrep", "fd-find", "tree", "jq", "gh", "curl"] {
            assert!(
                BASE_DOCKERFILE.contains(tool),
                "{tool} missing from base image"
            );
        }
    }
}
