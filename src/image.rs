use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::backend::Kind;
use crate::kits::Kit;

const BASE_DOCKERFILE: &str = include_str!("../images/base/Dockerfile");

/// An image Sandbox builds itself, optionally on top of another one it builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub tag: String,
    dockerfile: String,
    parent: Option<Box<Recipe>>,
}

/// The default image, tagged by a hash of its Dockerfile so edits trigger a rebuild.
pub fn base() -> Recipe {
    Recipe {
        tag: format!("sandbox-base:{:012x}", short_hash(BASE_DOCKERFILE)),
        dockerfile: BASE_DOCKERFILE.into(),
        parent: None,
    }
}

/// The base image plus the given kits; just the base image when there are none.
pub fn with_kits(kits: &[Kit]) -> Recipe {
    let base = base();
    if kits.is_empty() {
        return base;
    }
    let mut dockerfile = format!("FROM {}\n", base.tag);
    for kit in kits {
        dockerfile.push_str(&format!("\n# kit: {}\n{}", kit.label(), kit.dockerfile));
    }
    Recipe {
        tag: format!("sandbox-env:{:012x}", short_hash(&dockerfile)),
        dockerfile,
        parent: Some(Box::new(base)),
    }
}

/// Builds `recipe` (and its parents) for `kind` unless it already exists.
pub fn ensure(kind: Kind, recipe: &Recipe) -> Result<()> {
    if exists(kind, &recipe.tag) {
        return Ok(());
    }
    if let Some(parent) = &recipe.parent {
        ensure(kind, parent)?;
    }
    let tag = &recipe.tag;
    let dir = cache_dir()?.join("images").join(tag.replace(':', "-"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("Dockerfile"), &recipe.dockerfile)?;

    eprintln!("sandbox: building image {tag} (first run only, this can take a few minutes)");
    if build(kind, tag, &dir).is_ok() {
        return Ok(());
    }
    // Apple's builder runs in its own VM; if it can't build (no network, builder
    // broken), a working Docker can build the image and hand it over.
    if kind == Kind::AppleContainer && Kind::Docker.probe().is_ok() {
        eprintln!("sandbox: apple-container build failed; building with docker and importing");
        ensure(Kind::Docker, recipe)?;
        let tar = dir.join("image.tar");
        run(Command::new("docker").args(["save", tag, "-o"]).arg(&tar))?;
        let loaded = run(Command::new("container")
            .args(["image", "load", "-i"])
            .arg(&tar));
        let _ = std::fs::remove_file(&tar);
        return loaded;
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

pub fn cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("sandbox"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cache").join("sandbox"))
}

pub fn short_hash(s: &str) -> u64 {
    fnv1a(s) & 0xffff_ffff_ffff
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
    fn tags_are_stable_and_content_addressed() {
        assert_eq!(base(), base());
        assert!(base().tag.starts_with("sandbox-base:"));
        assert_eq!(base().tag.len(), "sandbox-base:".len() + 12);
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a("a"), fnv1a("b"));
    }

    #[test]
    fn kits_layer_on_the_base_image() {
        assert_eq!(with_kits(&[]), base());
        let dir = std::env::temp_dir().join(format!("sandbox-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        let recipe = with_kits(&crate::kits::detect(&dir));
        assert!(recipe.tag.starts_with("sandbox-env:"));
        assert!(
            recipe
                .dockerfile
                .starts_with(&format!("FROM {}\n", base().tag))
        );
        assert!(recipe.dockerfile.contains("# kit: rust stable"));
        assert_eq!(recipe.parent.as_deref(), Some(&base()));
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
