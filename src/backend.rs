use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    AppleContainer,
    Docker,
    Podman,
    Nerdctl,
}

/// Auto-detection order. Apple `container` only applies on macOS.
const AUTO_ORDER: [Kind; 4] = [
    Kind::AppleContainer,
    Kind::Docker,
    Kind::Podman,
    Kind::Nerdctl,
];

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::AppleContainer => "apple-container",
            Kind::Docker => "docker",
            Kind::Podman => "podman",
            Kind::Nerdctl => "nerdctl",
        }
    }

    pub fn bin(self) -> &'static str {
        match self {
            Kind::AppleContainer => "container",
            other => other.name(),
        }
    }

    /// `None` means auto-detect.
    pub fn parse(value: &str) -> Result<Option<Kind>> {
        Ok(Some(match value {
            "auto" => return Ok(None),
            "apple-container" | "apple" | "container" => Kind::AppleContainer,
            "docker" => Kind::Docker,
            "podman" => Kind::Podman,
            "nerdctl" => Kind::Nerdctl,
            other => bail!(
                "unknown backend `{other}` (expected auto, apple-container, docker, podman or nerdctl)"
            ),
        }))
    }

    /// Why this backend can't be used right now, if it can't.
    pub fn probe(self) -> Result<(), String> {
        if self == Kind::AppleContainer {
            probe_apple_platform()?;
        }
        if !on_path(self.bin()) {
            return Err(format!("`{}` is not installed", self.bin()));
        }
        let (args, hint): (&[&str], &str) = match self {
            Kind::AppleContainer => (
                &["system", "status"],
                "service not running; start it with `container system start`",
            ),
            _ => (&["info"], "daemon not reachable"),
        };
        if !succeeds(self.bin(), args) {
            return Err(hint.into());
        }
        Ok(())
    }
}

pub struct Selection {
    pub kind: Kind,
    /// Set when macOS fell back from Apple `container` to a weaker backend.
    pub warning: Option<String>,
}

pub fn select(preference: Option<&str>) -> Result<Selection> {
    if let Some(kind) = preference.map(Kind::parse).transpose()?.flatten() {
        kind.probe()
            .map_err(|why| anyhow::anyhow!("backend {} unavailable: {why}", kind.name()))?;
        return Ok(Selection {
            kind,
            warning: None,
        });
    }

    let mut reasons = Vec::new();
    for kind in AUTO_ORDER {
        if kind == Kind::AppleContainer && !cfg!(target_os = "macos") {
            continue;
        }
        match kind.probe() {
            Ok(()) => {
                let warning = reasons
                    .iter()
                    .find(|(k, _)| *k == Kind::AppleContainer)
                    .map(|(_, why): &(Kind, String)| {
                        format!(
                            "apple-container unavailable ({why}); {} shares one VM across all boxes, which is weaker isolation",
                            kind.name()
                        )
                    });
                return Ok(Selection { kind, warning });
            }
            Err(why) => reasons.push((kind, why)),
        }
    }
    let detail: Vec<String> = reasons
        .iter()
        .map(|(k, why)| format!("  {}: {why}", k.name()))
        .collect();
    bail!(
        "no usable isolation backend found:\n{}\nrun `agentbox doctor` for details",
        detail.join("\n")
    )
}

fn probe_apple_platform() -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("macOS only".into());
    }
    if std::env::consts::ARCH != "aarch64" {
        return Err("requires Apple silicon".into());
    }
    match macos_major() {
        Some(v) if v >= 26 => Ok(()),
        Some(v) => Err(format!(
            "requires macOS 26+, found {v} (older versions can't isolate the network)"
        )),
        None => Err("could not read the macOS version".into()),
    }
}

pub fn macos_version() -> Option<String> {
    let out = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

fn macos_major() -> Option<u32> {
    macos_version()?.split('.').next()?.parse().ok()
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn succeeds(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    None,
    Open,
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub labels: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub workdir: PathBuf,
    pub env: Vec<String>,
    pub uid: u32,
    pub gid: u32,
    pub network: Network,
    pub cpus: u32,
    pub memory: Memory,
    pub publish: Vec<String>,
    pub tty: bool,
    pub command: Vec<String>,
}

pub const LABEL: &str = "agentbox";
pub const PROJECT_LABEL: &str = "agentbox.project";

pub fn run_args(kind: Kind, spec: &RunSpec) -> Vec<String> {
    let mut a = args(&["run", "--rm", "--init", "-i", "--cap-drop", "ALL"]);
    if spec.tty {
        a.push("-t".into());
    }
    a.extend(["--name".into(), spec.name.clone()]);
    for (k, v) in &spec.labels {
        a.extend(["--label".into(), format!("{k}={v}")]);
    }

    match kind {
        Kind::AppleContainer => a.extend([
            "--progress".into(),
            "none".into(),
            "--uid".into(),
            spec.uid.to_string(),
            "--gid".into(),
            spec.gid.to_string(),
        ]),
        // Rootless podman maps the host user itself; --user would land on a subuid.
        Kind::Podman => a.push("--userns=keep-id".into()),
        Kind::Docker | Kind::Nerdctl => {
            a.extend(["--user".into(), format!("{}:{}", spec.uid, spec.gid)]);
        }
    }
    if kind != Kind::AppleContainer {
        a.extend(args(&[
            "--security-opt",
            "no-new-privileges",
            "--pids-limit",
            "4096",
        ]));
    }

    // Arbitrary UIDs have no passwd entry, so give tools a writable HOME.
    a.extend(["-e".into(), "HOME=/tmp".into()]);
    for e in &spec.env {
        a.extend(["-e".into(), e.clone()]);
    }

    for m in &spec.mounts {
        let (src, dst) = (m.source.display(), m.target.display());
        match (kind, m.readonly) {
            (Kind::AppleContainer, true) => a.extend([
                "--mount".into(),
                format!("type=bind,source={src},target={dst},readonly"),
            ]),
            (_, true) => a.extend(["-v".into(), format!("{src}:{dst}:ro")]),
            (_, false) => a.extend(["-v".into(), format!("{src}:{dst}")]),
        }
    }
    a.extend(["-w".into(), spec.workdir.display().to_string()]);

    if spec.network == Network::None {
        a.extend(args(&["--network", "none"]));
    }
    a.extend(["--cpus".into(), spec.cpus.to_string()]);
    a.extend(["--memory".into(), spec.memory.for_backend(kind)]);
    for p in &spec.publish {
        a.extend(["-p".into(), p.clone()]);
    }

    a.push(spec.image.clone());
    a.extend(spec.command.iter().cloned());
    a
}

pub fn exec_args(
    kind: Kind,
    name: &str,
    tty: bool,
    uid: u32,
    gid: u32,
    cmd: &[String],
) -> Vec<String> {
    let mut a = args(&["exec", "-i"]);
    if tty {
        a.push("-t".into());
    }
    match kind {
        Kind::AppleContainer => a.extend([
            "--uid".into(),
            uid.to_string(),
            "--gid".into(),
            gid.to_string(),
        ]),
        Kind::Podman => {}
        Kind::Docker | Kind::Nerdctl => a.extend(["--user".into(), format!("{uid}:{gid}")]),
    }
    a.push(name.into());
    a.extend(cmd.iter().cloned());
    a
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// A memory size in whole mebibytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory(u64);

impl Memory {
    pub fn parse(value: &str) -> Result<Memory> {
        let v = value.trim().to_ascii_lowercase();
        let (num, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
        let n: u64 = num
            .parse()
            .with_context(|| format!("invalid memory size `{value}` (try 8g or 512m)"))?;
        let mib = match unit.trim_end_matches(['b', 'i']) {
            "m" | "" => n,
            "g" => n * 1024,
            "t" => n * 1024 * 1024,
            _ => bail!("invalid memory unit in `{value}` (use m, g or t)"),
        };
        if mib < 128 {
            bail!("memory `{value}` is too small; use at least 128m");
        }
        Ok(Memory(mib))
    }

    fn for_backend(self, kind: Kind) -> String {
        match kind {
            Kind::AppleContainer => format!("{}M", self.0),
            _ => format!("{}m", self.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxInfo {
    pub name: String,
    pub project: String,
    pub image: String,
    pub status: String,
}

pub fn list(kind: Kind) -> Result<Vec<BoxInfo>> {
    let args: Vec<String> = match kind {
        Kind::AppleContainer => args(&["ls", "--format", "json"]),
        _ => args(&["ps", "--filter", "label=agentbox=1", "--format", "json"]),
    };
    let out = Command::new(kind.bin())
        .args(&args)
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running {} {}", kind.bin(), args.join(" ")))?;
    if !out.status.success() {
        bail!("{} failed listing containers", kind.bin());
    }
    parse_list(kind, &String::from_utf8_lossy(&out.stdout))
}

fn parse_list(kind: Kind, text: &str) -> Result<Vec<BoxInfo>> {
    let text = text.trim();
    let items: Vec<Value> = if text.is_empty() {
        vec![]
    } else if text.starts_with('[') {
        serde_json::from_str(text).context("parsing container list")?
    } else {
        // docker/nerdctl print one JSON object per line
        text.lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .context("parsing container list")?
    };

    let mut boxes = Vec::new();
    for item in items {
        let info = match kind {
            Kind::AppleContainer => {
                let labels = &item["configuration"]["labels"];
                if labels[LABEL].as_str() != Some("1") {
                    continue;
                }
                BoxInfo {
                    name: str_of(&item["configuration"]["id"]),
                    project: str_of(&labels[PROJECT_LABEL]),
                    image: str_of(&item["configuration"]["image"]["reference"]),
                    status: str_of(&item["status"]["state"]),
                }
            }
            _ => {
                let labels = docker_labels(&item["Labels"]);
                if labels.iter().all(|(k, v)| !(k == LABEL && v == "1")) {
                    continue;
                }
                let name = match &item["Names"] {
                    Value::Array(names) => names.first().map(str_of).unwrap_or_default(),
                    other => str_of(other),
                };
                BoxInfo {
                    name,
                    project: labels
                        .into_iter()
                        .find(|(k, _)| k == PROJECT_LABEL)
                        .map(|(_, v)| v)
                        .unwrap_or_default(),
                    image: str_of(&item["Image"]),
                    status: str_of(&item["Status"]),
                }
            }
        };
        boxes.push(info);
    }
    Ok(boxes)
}

fn str_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// Docker prints labels as "a=b,c=d"; podman prints a map.
fn docker_labels(v: &Value) -> Vec<(String, String)> {
    match v {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), str_of(v))).collect(),
        Value::String(s) => s
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        _ => vec![],
    }
}

pub fn stop(kind: Kind, names: &[String]) -> Result<()> {
    let status = Command::new(kind.bin())
        .arg("stop")
        .args(names)
        .stdout(Stdio::null())
        .status()
        .with_context(|| format!("running {} stop", kind.bin()))?;
    if !status.success() {
        bail!("{} stop failed", kind.bin());
    }
    Ok(())
}

pub fn project_label(root: &Path) -> (String, String) {
    (PROJECT_LABEL.into(), root.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec {
            name: "agentbox-repo-abc123".into(),
            image: "debian:bookworm-slim".into(),
            labels: vec![("agentbox".into(), "1".into())],
            mounts: vec![
                Mount {
                    source: "/code/repo".into(),
                    target: "/workspace".into(),
                    readonly: false,
                },
                Mount {
                    source: "/code/main/.git".into(),
                    target: "/code/main/.git".into(),
                    readonly: true,
                },
            ],
            workdir: "/workspace/app".into(),
            env: vec!["FOO=bar".into()],
            uid: 501,
            gid: 20,
            network: Network::None,
            cpus: 4,
            memory: Memory::parse("8g").unwrap(),
            publish: vec!["3000:3000".into()],
            tty: true,
            command: vec!["bash".into(), "-l".into()],
        }
    }

    fn has_pair(a: &[String], flag: &str, value: &str) -> bool {
        a.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn apple_args() {
        let a = run_args(Kind::AppleContainer, &spec());
        assert_eq!(
            &a[..6],
            ["run", "--rm", "--init", "-i", "--cap-drop", "ALL"]
        );
        assert!(a.contains(&"-t".to_string()));
        assert!(has_pair(&a, "--uid", "501") && has_pair(&a, "--gid", "20"));
        assert!(has_pair(&a, "--progress", "none"));
        assert!(has_pair(&a, "-v", "/code/repo:/workspace"));
        assert!(has_pair(
            &a,
            "--mount",
            "type=bind,source=/code/main/.git,target=/code/main/.git,readonly"
        ));
        assert!(has_pair(&a, "--network", "none"));
        assert!(has_pair(&a, "--memory", "8192M"));
        assert!(has_pair(&a, "-w", "/workspace/app"));
        assert!(!a.contains(&"--pids-limit".to_string()));
        assert_eq!(&a[a.len() - 3..], ["debian:bookworm-slim", "bash", "-l"]);
    }

    #[test]
    fn docker_args() {
        let a = run_args(Kind::Docker, &spec());
        assert!(has_pair(&a, "--user", "501:20"));
        assert!(has_pair(&a, "-v", "/code/main/.git:/code/main/.git:ro"));
        assert!(has_pair(&a, "--security-opt", "no-new-privileges"));
        assert!(has_pair(&a, "--memory", "8192m"));
        assert!(has_pair(&a, "--label", "agentbox=1"));
    }

    #[test]
    fn podman_keeps_host_user() {
        let a = run_args(Kind::Podman, &spec());
        assert!(a.contains(&"--userns=keep-id".to_string()));
        assert!(!a.contains(&"--user".to_string()));
    }

    #[test]
    fn open_network_adds_no_flag() {
        let mut s = spec();
        s.network = Network::Open;
        s.tty = false;
        let a = run_args(Kind::Docker, &s);
        assert!(!a.contains(&"--network".to_string()));
        assert!(!a.contains(&"-t".to_string()));
    }

    #[test]
    fn memory_units() {
        assert_eq!(Memory::parse("8g").unwrap(), Memory(8192));
        assert_eq!(Memory::parse("512M").unwrap(), Memory(512));
        assert_eq!(Memory::parse("2GiB").unwrap(), Memory(2048));
        assert!(Memory::parse("lots").is_err());
        assert!(Memory::parse("8x").is_err());
        assert!(Memory::parse("64m").is_err());
    }

    #[test]
    fn parses_apple_list_and_skips_foreign_containers() {
        let json = r#"[
          {"configuration":{"id":"agentbox-repo-1","labels":{"agentbox":"1","agentbox.project":"/code/repo"},
            "image":{"reference":"docker.io/library/alpine:latest"}},"status":{"state":"running"}},
          {"configuration":{"id":"postgres","labels":{},"image":{"reference":"postgres"}},"status":{"state":"running"}}
        ]"#;
        let boxes = parse_list(Kind::AppleContainer, json).unwrap();
        assert_eq!(
            boxes,
            [BoxInfo {
                name: "agentbox-repo-1".into(),
                project: "/code/repo".into(),
                image: "docker.io/library/alpine:latest".into(),
                status: "running".into(),
            }]
        );
    }

    #[test]
    fn parses_docker_lines_and_podman_arrays() {
        let docker = r#"{"Names":"agentbox-a","Labels":"agentbox=1,agentbox.project=/p","Image":"alpine","Status":"Up 2 minutes"}"#;
        let podman = r#"[{"Names":["agentbox-b"],"Labels":{"agentbox":"1","agentbox.project":"/q"},"Image":"alpine","Status":"running"}]"#;
        assert_eq!(parse_list(Kind::Docker, docker).unwrap()[0].project, "/p");
        assert_eq!(
            parse_list(Kind::Podman, podman).unwrap()[0].name,
            "agentbox-b"
        );
        assert!(parse_list(Kind::Docker, "").unwrap().is_empty());
    }

    #[test]
    fn parses_backend_names() {
        assert_eq!(Kind::parse("auto").unwrap(), None);
        assert_eq!(Kind::parse("apple").unwrap(), Some(Kind::AppleContainer));
        assert!(Kind::parse("vbox").is_err());
    }
}
