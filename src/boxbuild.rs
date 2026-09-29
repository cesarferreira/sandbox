//! Builds an image without Apple's builder, for when its VM has no network (VPNs).
//!
//! Running boxes still reach the network through the host proxy, so the
//! Dockerfile's steps run in a root box started from the parent image. Its
//! filesystem is then exported and loaded as a one-layer OCI image.
//! Only the instructions Sandbox's own Dockerfiles use are supported: FROM, ARG,
//! ENV and RUN.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::proxy;

const BIN: &str = "container";
const WORK: &str = "/sandbox-build";

#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    from: String,
    /// Build steps in order: `export` lines for ARG/ENV, and RUN commands.
    steps: Vec<Step>,
    /// Keys set by ENV, which end up in the image config.
    env_keys: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Export(String, String),
    Run(String),
}

pub fn build(tag: &str, dockerfile: &str, dir: &Path) -> Result<()> {
    let plan = parse(dockerfile)?;
    let (parent_env, parent_cmd) = parent_config(&plan.from)?;

    let work = dir.join("box-build");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    std::fs::write(work.join("bridge.pl"), proxy::BRIDGE)?;
    let mut keys: Vec<String> = parent_env
        .iter()
        .filter_map(|e| e.split_once('='))
        .map(|(k, _)| k.to_string())
        .collect();
    keys.extend(plan.env_keys.iter().cloned());
    keys.dedup();
    let cas = crate::certs::host_bundle(&crate::image::cache_dir()?)
        .and_then(|(dir, _)| std::fs::read_to_string(dir.join(crate::certs::FILE)).ok())
        .unwrap_or_default();
    std::fs::write(work.join("host-cas.pem"), &cas)?;
    std::fs::write(work.join("build.sh"), script(&plan, &keys))?;
    for (i, step) in plan.steps.iter().enumerate() {
        if let Step::Run(cmd) = step {
            std::fs::write(work.join(format!("step-{i}.sh")), cmd)?;
        }
    }

    let name = format!(
        "sandbox-build-{:06x}",
        crate::image::short_hash(tag) & 0xff_ffff
    );
    let _ = quiet(Command::new(BIN).args(["rm", "-f", &name]));
    let proxy = proxy::Proxy::start(&name)?;
    eprintln!("sandbox: building {tag} in a box (Apple's builder has no network)");
    let out = Command::new(BIN)
        .args([
            "run",
            "--progress",
            "none",
            "--name",
            &name,
            "--ssh",
            "--uid",
            "0",
            "--gid",
            "0",
        ])
        .args(["-e", &format!("SANDBOX_PROXY_SOCK={}", proxy::BOX_SOCKET)])
        .arg("-v")
        .arg(format!("{}:{WORK}", work.display()))
        .args([&plan.from, "sh", &format!("{WORK}/build.sh")])
        .env("SSH_AUTH_SOCK", &proxy.socket)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("running the build box")?;
    drop(proxy);
    let result = (|| {
        if !out.status.success() {
            bail!("a build step failed (see output above)");
        }
        let mut env = parse_env_output(&String::from_utf8_lossy(&out.stdout));
        if env.is_empty() {
            env = parent_env.clone();
        }
        let rootfs = work.join("rootfs.tar");
        run(Command::new(BIN).args(["export", &name, "-o"]).arg(&rootfs))?;
        load_oci(tag, &rootfs, &env, &parent_cmd, &work)
    })();
    let _ = quiet(Command::new(BIN).args(["rm", "-f", &name]));
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn parse(dockerfile: &str) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut logical = Vec::new();
    let mut current = String::new();
    for line in dockerfile.lines() {
        if current.is_empty() && (line.trim().is_empty() || line.trim_start().starts_with('#')) {
            continue;
        }
        // Keep the backslash: the step becomes a shell script, where it still continues.
        match line.strip_suffix('\\') {
            Some(_) => {
                current.push_str(line);
                current.push('\n');
            }
            None => {
                current.push_str(line);
                logical.push(std::mem::take(&mut current));
            }
        }
    }
    for instr in logical {
        let (op, rest) = instr
            .trim_start()
            .split_once(char::is_whitespace)
            .unwrap_or((&instr, ""));
        let rest = rest.trim();
        match op.to_ascii_uppercase().as_str() {
            "FROM" => plan.from = rest.to_string(),
            "RUN" => plan.steps.push(Step::Run(rest.to_string())),
            op @ ("ENV" | "ARG") => {
                for pair in rest.split_whitespace() {
                    let Some((k, v)) = pair.split_once('=') else {
                        continue; // `ARG NAME` without a default sets nothing
                    };
                    plan.steps.push(Step::Export(k.to_string(), v.to_string()));
                    if op == "ENV" {
                        plan.env_keys.push(k.to_string());
                    }
                }
            }
            other => bail!("box builds don't support `{other}` instructions"),
        }
    }
    if plan.from.is_empty() {
        bail!("Dockerfile has no FROM");
    }
    Ok(plan)
}

/// The script run as root in the build box. Step output goes to stderr; stdout
/// carries only the final environment, as `SANDBOX-ENV KEY=value` lines.
/// Host CAs (VPN TLS inspection) go in the Debian way, so a step that installs
/// ca-certificates picks them up too, and come out again before the export.
const TRUST_START: &str = r#"if [ -s /sandbox-build/host-cas.pem ]; then
  mkdir -p /usr/local/share/ca-certificates/sandbox-host
  awk '/BEGIN CERT/{n++} {print > ("/usr/local/share/ca-certificates/sandbox-host/host-" n ".crt")}' /sandbox-build/host-cas.pem
  command -v update-ca-certificates >/dev/null && update-ca-certificates >/dev/null 2>&1
  export NODE_EXTRA_CA_CERTS=/sandbox-build/host-cas.pem
fi"#;
const TRUST_END: &str = r#"if [ -d /usr/local/share/ca-certificates/sandbox-host ]; then
  rm -rf /usr/local/share/ca-certificates/sandbox-host
  command -v update-ca-certificates >/dev/null && update-ca-certificates --fresh >/dev/null 2>&1
fi"#;

fn script(plan: &Plan, keys: &[String]) -> String {
    let trust_start = TRUST_START;
    let url = format!("http://127.0.0.1:{}", proxy::BOX_PORT);
    let mut s = format!(
        "exec 3>&1 1>&2\n\
         chmod 666 \"$SANDBOX_PROXY_SOCK\" 2>/dev/null\n\
         (perl {WORK}/bridge.pl </dev/null >/dev/null 2>&1 &)\n\
         sleep 0.3\n\
         export http_proxy={url} https_proxy={url} HTTP_PROXY={url} HTTPS_PROXY={url}\n\
         {trust_start}\n"
    );
    for (i, step) in plan.steps.iter().enumerate() {
        match step {
            // Double quotes so values can refer to earlier variables ($PATH).
            Step::Export(k, v) => s.push_str(&format!("export {k}=\"{v}\"\n")),
            Step::Run(_) => s.push_str(&format!(
                "sh {WORK}/step-{i}.sh || {{ echo \"build step {i} failed\"; exit 1; }}\n"
            )),
        }
    }
    s.push_str(TRUST_END);
    s.push_str("\nrm -rf /tmp/* 2>/dev/null\n");
    for k in keys {
        s.push_str(&format!(
            "[ -n \"${{{k}+x}}\" ] && printf 'SANDBOX-ENV %s=%s\\n' {k} \"${k}\" >&3\n"
        ));
    }
    s
}

fn parse_env_output(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("SANDBOX-ENV "))
        .map(String::from)
        .collect()
}

/// Env and Cmd of the parent image, pulling it first if needed.
fn parent_config(image: &str) -> Result<(Vec<String>, Vec<String>)> {
    if !quiet(Command::new(BIN).args(["image", "inspect", image])) {
        run(Command::new(BIN).args(["image", "pull", image]))?;
    }
    let out = Command::new(BIN)
        .args(["image", "inspect", image])
        .output()?;
    let json: Value = serde_json::from_slice(&out.stdout).context("reading image config")?;
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    let variant = json[0]["variants"]
        .as_array()
        .and_then(|vs| vs.iter().find(|v| v["platform"]["architecture"] == arch))
        .context("parent image has no variant for this architecture")?;
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let config = &variant["config"]["config"];
    Ok((strings(&config["Env"]), strings(&config["Cmd"])))
}

/// Wraps a rootfs tar as a one-layer OCI image and loads it.
fn load_oci(tag: &str, rootfs: &Path, env: &[String], cmd: &[String], work: &Path) -> Result<()> {
    let oci = work.join("oci");
    let blobs = oci.join("blobs/sha256");
    std::fs::create_dir_all(&blobs)?;
    let layer = sha256_file(rootfs)?;
    let layer_size = std::fs::metadata(rootfs)?.len();
    std::fs::rename(rootfs, blobs.join(&layer))?;

    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    let config = json!({
        "architecture": arch,
        "os": "linux",
        "config": { "Env": env, "Cmd": cmd, "WorkingDir": "/" },
        "rootfs": { "type": "layers", "diff_ids": [format!("sha256:{layer}")] },
    });
    let config_digest = write_blob(&blobs, &serde_json::to_vec(&config)?)?;
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": format!("sha256:{}", config_digest.0), "size": config_digest.1 },
        "layers": [{ "mediaType": "application/vnd.oci.image.layer.v1.tar",
                     "digest": format!("sha256:{layer}"), "size": layer_size }],
    });
    let manifest_digest = write_blob(&blobs, &serde_json::to_vec(&manifest)?)?;
    let (repo, version) = tag.split_once(':').unwrap_or((tag, "latest"));
    let index = json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", manifest_digest.0), "size": manifest_digest.1,
            "annotations": {
                "io.containerd.image.name": format!("docker.io/library/{repo}:{version}"),
                "org.opencontainers.image.ref.name": version,
            },
        }],
    });
    std::fs::write(oci.join("index.json"), serde_json::to_vec(&index)?)?;
    std::fs::write(oci.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    let archive = work.join("image.tar");
    run(Command::new("tar")
        .arg("-cf")
        .arg(&archive)
        .arg("-C")
        .arg(&oci)
        .arg("."))?;
    run(Command::new(BIN)
        .args(["image", "load", "-i"])
        .arg(&archive))
}

fn write_blob(blobs: &Path, bytes: &[u8]) -> Result<(String, u64)> {
    let tmp = blobs.join("tmp");
    std::fs::write(&tmp, bytes)?;
    let digest = sha256_file(&tmp)?;
    std::fs::rename(&tmp, blobs.join(&digest))?;
    Ok((digest, bytes.len() as u64))
}

fn sha256_file(path: &Path) -> Result<String> {
    let out = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .context("running shasum")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let digest = text.split_whitespace().next().unwrap_or_default();
    if !out.status.success() || digest.len() != 64 {
        bail!("hashing {} failed", path.display());
    }
    Ok(digest.to_string())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.stdin(Stdio::null()).stdout(Stdio::null()).status()?;
    if !status.success() {
        bail!("{:?} failed", cmd.get_program());
    }
    Ok(())
}

fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sandbox_dockerfiles() {
        let plan = parse(
            "FROM sandbox-base:abc\n\n# kit: rust stable\nENV A=/x B=/y:$PATH\nARG DEBIAN_FRONTEND=noninteractive\nARG NOVALUE\n\
             RUN one \\\n && two\nRUN three\n",
        )
        .unwrap();
        assert_eq!(plan.from, "sandbox-base:abc");
        assert_eq!(plan.env_keys, ["A", "B"]);
        assert_eq!(
            plan.steps,
            [
                Step::Export("A".into(), "/x".into()),
                Step::Export("B".into(), "/y:$PATH".into()),
                Step::Export("DEBIAN_FRONTEND".into(), "noninteractive".into()),
                Step::Run("one \\\n && two".into()),
                Step::Run("three".into()),
            ]
        );
        assert!(parse("FROM x\nCOPY a b\n").is_err());
        assert!(parse("RUN x\n").is_err());
    }

    #[test]
    fn script_runs_steps_in_order_and_reports_env() {
        let plan = parse("FROM x\nENV P=/a:$PATH\nRUN do-it\n").unwrap();
        let s = script(&plan, &["PATH".into(), "P".into()]);
        let export = s.find("export P=\"/a:$PATH\"").unwrap();
        let step = s.find("sh /sandbox-build/step-1.sh").unwrap();
        assert!(export < step);
        assert!(s.contains("export http_proxy=http://127.0.0.1:3128"));
        let (start, end) = (
            s.find("sandbox-host/host-").unwrap(),
            s.find("update-ca-certificates --fresh").unwrap(),
        );
        assert!(
            start < step && step < end,
            "host CAs are added before the steps and removed after"
        );
        assert!(s.contains("printf 'SANDBOX-ENV %s=%s\\n' P \"$P\" >&3"));
        assert_eq!(
            parse_env_output("noise\nSANDBOX-ENV PATH=/a:/b\nSANDBOX-ENV P=/a\n"),
            ["PATH=/a:/b", "P=/a"]
        );
    }

    #[test]
    fn builds_the_real_base_and_kit_dockerfiles() {
        assert!(parse(include_str!("../images/base/Dockerfile")).is_ok());
        let dir = std::env::temp_dir().join(format!("sandbox-boxbuild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["Cargo.toml", "package.json"] {
            std::fs::write(dir.join(name), "{}").unwrap();
        }
        let kits =
            crate::kits::resolve(&dir, &["rust".into(), "node".into(), "android".into()]).unwrap();
        let recipe = crate::image::with_kits(&kits, &[]);
        assert!(parse(&recipe.dockerfile).is_ok());
    }
}
