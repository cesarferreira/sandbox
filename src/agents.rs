//! Agent profiles: how to install a coding agent into the box and which of its
//! credentials to pass through.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};

use crate::kits::Kit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    /// The command users type, e.g. `sandbox claude`.
    pub name: &'static str,
    pub package: &'static str,
    /// Oldest Node major the package supports.
    pub min_node: u32,
    /// Host variables holding the agent's own credentials, passed through by name.
    pub key_vars: &'static [&'static str],
}

pub const AGENTS: [Agent; 3] = [
    Agent {
        name: "claude",
        package: "@anthropic-ai/claude-code",
        min_node: 22,
        key_vars: &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
    },
    Agent {
        name: "codex",
        package: "@openai/codex",
        min_node: 18,
        key_vars: &["OPENAI_API_KEY"],
    },
    Agent {
        name: "gemini",
        package: "@google/gemini-cli",
        min_node: 20,
        key_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
    },
];

/// Re-check the registry for a new agent release at most this often.
const VERSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn lookup(command: &str) -> Option<Agent> {
    AGENTS.iter().copied().find(|a| a.name == command)
}

impl Agent {
    /// The key variables that are actually set on the host.
    pub fn present_keys(&self) -> Vec<&'static str> {
        self.key_vars
            .iter()
            .copied()
            .filter(|v| std::env::var(v).is_ok_and(|val| !val.is_empty()))
            .collect()
    }

    /// Fails early when the project pins a Node too old for the agent.
    pub fn check_node(&self, node: &Kit) -> Result<()> {
        let major = node
            .version
            .split('.')
            .next()
            .and_then(|m| m.parse::<u32>().ok());
        if let Some(major) = major
            && major < self.min_node
        {
            bail!(
                "{} needs Node {}+, but this project pins Node {} (from .nvmrc, .node-version, \
                 .tool-versions or engines.node)",
                self.name,
                self.min_node,
                node.version
            );
        }
        Ok(())
    }

    /// The image layer installing a specific version of the agent.
    pub fn dockerfile(&self, version: &str) -> String {
        format!(
            "RUN npm install -g --no-audit --no-fund {}@{version} \\\n \
             && chmod -R a+rwX /usr/local/lib/node_modules /usr/local/bin\n",
            self.package
        )
    }

    /// The version to install: the registry's latest, cached for a day so runs stay
    /// fast and offline-friendly. Falls back to the cached version, then `latest`.
    pub fn version(&self, cache: &Path) -> String {
        let file = cache.join(format!("{}.version", self.name));
        let cached = std::fs::read_to_string(&file)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| is_version(v));
        let fresh = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age < VERSION_TTL);
        if let (Some(v), true) = (&cached, fresh) {
            return v.clone();
        }
        match latest_version(self.package) {
            Some(v) => {
                let _ = std::fs::create_dir_all(cache);
                let _ = std::fs::write(&file, &v);
                v
            }
            None => cached.unwrap_or_else(|| "latest".into()),
        }
    }
}

fn latest_version(package: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "5"])
        .arg(format!("https://registry.npmjs.org/{package}/latest"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    json["version"]
        .as_str()
        .filter(|v| is_version(v))
        .map(String::from)
}

/// Registry data ends up in a Dockerfile `RUN` line, so accept only plain semver.
fn is_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 40
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
        && v.starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_kit(version: &str) -> Kit {
        let dir = std::env::temp_dir().join(format!("sandbox-agents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut kit = crate::kits::resolve(&dir, &["node".into()])
            .unwrap()
            .remove(0);
        kit.version = version.into();
        kit
    }

    #[test]
    fn looks_up_known_agents_only() {
        assert_eq!(
            lookup("claude").unwrap().package,
            "@anthropic-ai/claude-code"
        );
        assert_eq!(lookup("codex").unwrap().key_vars, ["OPENAI_API_KEY"]);
        assert!(lookup("bash").is_none());
    }

    #[test]
    fn rejects_too_old_node_pins() {
        let claude = lookup("claude").unwrap();
        assert!(claude.check_node(&node_kit("20")).is_err());
        assert!(claude.check_node(&node_kit("22.11.0")).is_ok());
        assert!(claude.check_node(&node_kit("lts")).is_ok());
    }

    #[test]
    fn layer_pins_the_version() {
        let layer = lookup("codex").unwrap().dockerfile("0.158.0");
        assert!(layer.contains("npm install -g --no-audit --no-fund @openai/codex@0.158.0"));
    }

    #[test]
    fn only_plain_versions_reach_the_dockerfile() {
        assert!(is_version("2.1.283"));
        assert!(is_version("1.0.0-beta.1"));
        assert!(!is_version("latest; rm -rf /"));
        assert!(!is_version("$(boom)"));
        assert!(!is_version(""));
    }

    #[test]
    fn uses_a_fresh_cached_version_without_the_network() {
        let dir = std::env::temp_dir().join(format!("sandbox-agent-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("claude.version"), "9.9.9\n").unwrap();
        assert_eq!(lookup("claude").unwrap().version(&dir), "9.9.9");
    }
}
