mod backend;
mod cli;
mod project;

use std::io::IsTerminal;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command as Process, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;

use backend::{Kind, Memory, Mount, Network, RunSpec};
use cli::{BoxOpts, Cli, Command, NetMode};
use project::{Project, WORKSPACE};

const DEFAULT_IMAGE: &str = "debian:bookworm-slim";
const DEFAULT_CPUS: u32 = 4;
const DEFAULT_MEMORY: &str = "8g";
/// Exit code for AgentBox's own failures, following the Docker convention.
const EXIT_AGENTBOX_ERROR: u8 = 125;
const SHELL: &str = "command -v bash >/dev/null 2>&1 && exec bash -l || exec sh -l";

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("agentbox: {err:#}");
            ExitCode::from(EXIT_AGENTBOX_ERROR)
        }
    }
}

fn dispatch(cli: Cli) -> Result<ExitCode> {
    let outer = cli.opts;
    match cli.command {
        Command::Run { opts, command } => run_box(outer.merge(opts), command),
        Command::Shell { opts } => run_box(
            outer.merge(opts),
            vec!["sh".into(), "-c".into(), SHELL.into()],
        ),
        Command::Agent(args) => {
            let command = args
                .into_iter()
                .map(|a| {
                    a.into_string()
                        .map_err(|a| anyhow::anyhow!("argument {a:?} is not UTF-8"))
                })
                .collect::<Result<_>>()?;
            run_box(outer, command)
        }
        Command::Exec {
            backend,
            name,
            command,
        } => exec_box(backend.or(outer.backend).as_deref(), &name, &command),
        Command::Ps { backend } => ps(backend.or(outer.backend).as_deref()),
        Command::Stop { backend, name, all } => {
            stop(backend.or(outer.backend).as_deref(), name.as_deref(), all)
        }
        Command::Doctor => doctor(),
        Command::Policy => not_yet("policy", 4),
        Command::Report => not_yet("report", 5),
        Command::Login => not_yet("login", 3),
        Command::Clean => not_yet("clean", 5),
    }
}

fn not_yet(command: &str, milestone: u8) -> Result<ExitCode> {
    bail!("`{command}` is not implemented yet; it lands in milestone {milestone} (see plan.md)")
}

fn run_box(opts: BoxOpts, command: Vec<String>) -> Result<ExitCode> {
    let network = match opts.net.unwrap_or(NetMode::Open) {
        NetMode::None => Network::None,
        NetMode::Open => Network::Open,
        NetMode::Allowlist => bail!(
            "--net allowlist needs the egress proxy, which lands in milestone 3; use --net none or --net open"
        ),
    };
    let memory = Memory::parse(opts.memory.as_deref().unwrap_or(DEFAULT_MEMORY))?;

    let cwd = std::env::current_dir().context("reading current directory")?;
    let project = Project::detect(&cwd)?;

    let mut mounts = vec![Mount {
        source: project.root.clone(),
        target: WORKSPACE.into(),
        readonly: false,
    }];
    if let Some(git_dir) = &project.external_git_dir {
        mounts.push(Mount {
            source: git_dir.clone(),
            target: git_dir.clone(),
            readonly: true,
        });
    }
    for raw in &opts.mount {
        mounts.push(parse_mount(raw)?);
    }

    let selection = backend::select(opts.backend.as_deref())?;
    let kind = selection.kind;
    let (uid, gid) = host_ids();
    let spec = RunSpec {
        name: format!("agentbox-{}-{}", project.slug(), short_id()),
        image: opts.image.unwrap_or_else(|| DEFAULT_IMAGE.into()),
        labels: vec![
            (backend::LABEL.into(), "1".into()),
            backend::project_label(&project.root),
        ],
        mounts,
        workdir: project.workdir(),
        env: opts.env,
        uid,
        gid,
        network,
        cpus: opts.cpus.unwrap_or(DEFAULT_CPUS),
        memory,
        publish: opts.publish,
        tty: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        command,
    };
    let args = backend::run_args(kind, &spec);

    if opts.dry_run {
        println!("{}", shell_line(kind.bin(), &args));
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(warning) = &selection.warning {
        eprintln!("agentbox: warning: {warning}");
    }
    eprintln!("agentbox: {}", summary(kind, &spec));
    if network == Network::Open {
        eprintln!(
            "agentbox: warning: network is unrestricted (--net allowlist lands in milestone 3; use --net none to cut it off)"
        );
    }

    let status = Process::new(kind.bin())
        .args(&args)
        .status()
        .with_context(|| format!("starting {}", kind.bin()))?;
    Ok(exit_code(status))
}

fn exec_box(backend: Option<&str>, name: &str, command: &[String]) -> Result<ExitCode> {
    let kind = backend::select(backend)?.kind;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (uid, gid) = host_ids();
    let args = backend::exec_args(kind, name, tty, uid, gid, command);
    let status = Process::new(kind.bin())
        .args(&args)
        .status()
        .with_context(|| format!("starting {}", kind.bin()))?;
    Ok(exit_code(status))
}

fn ps(backend: Option<&str>) -> Result<ExitCode> {
    let kind = backend::select(backend)?.kind;
    let boxes = backend::list(kind)?;
    if boxes.is_empty() {
        eprintln!("no running boxes ({})", kind.name());
        return Ok(ExitCode::SUCCESS);
    }
    let width = boxes.iter().map(|b| b.name.len()).max().unwrap_or(4).max(4);
    println!(
        "{:width$}  {:10}  {:30}  PROJECT",
        "NAME", "STATUS", "IMAGE"
    );
    for b in boxes {
        println!(
            "{:width$}  {:10}  {:30}  {}",
            b.name, b.status, b.image, b.project
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn stop(backend: Option<&str>, name: Option<&str>, all: bool) -> Result<ExitCode> {
    let kind = backend::select(backend)?.kind;
    let boxes = backend::list(kind)?;
    let targets: Vec<String> = if all {
        boxes.into_iter().map(|b| b.name).collect()
    } else if let Some(name) = name {
        if !boxes.iter().any(|b| b.name == name) {
            bail!("no running AgentBox box named `{name}` (see `agentbox ps`)");
        }
        vec![name.to_string()]
    } else {
        let project = Project::detect(&std::env::current_dir()?)?;
        let root = project.root.display().to_string();
        let mine: Vec<String> = boxes
            .into_iter()
            .filter(|b| b.project == root)
            .map(|b| b.name)
            .collect();
        match mine.len() {
            0 => bail!("no box is running for {root}"),
            1 => mine,
            _ => bail!(
                "several boxes are running for {root}: {}; name one, or pass --all",
                mine.join(", ")
            ),
        }
    };
    if targets.is_empty() {
        eprintln!("no running boxes");
        return Ok(ExitCode::SUCCESS);
    }
    backend::stop(kind, &targets)?;
    for t in &targets {
        eprintln!("stopped {t}");
    }
    Ok(ExitCode::SUCCESS)
}

fn doctor() -> Result<ExitCode> {
    println!(
        "platform   {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if cfg!(target_os = "macos") {
        println!(
            "macOS      {}",
            backend::macos_version().unwrap_or_else(|| "unknown".into())
        );
    }
    println!();
    for kind in [
        Kind::AppleContainer,
        Kind::Docker,
        Kind::Podman,
        Kind::Nerdctl,
    ] {
        match kind.probe() {
            Ok(()) => println!("  ✓ {:16} ready", kind.name()),
            Err(why) => println!("  ✗ {:16} {why}", kind.name()),
        }
    }
    println!();
    match backend::select(None) {
        Ok(sel) => {
            println!("selected   {}", sel.kind.name());
            if let Some(w) = sel.warning {
                println!("warning    {w}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            println!("selected   none");
            eprintln!("agentbox: {err:#}");
            Ok(ExitCode::from(EXIT_AGENTBOX_ERROR))
        }
    }
}

fn parse_mount(raw: &str) -> Result<Mount> {
    let (path, readonly) = match raw.strip_suffix(":ro") {
        Some(p) => (p, true),
        None => (raw.strip_suffix(":rw").unwrap_or(raw), false),
    };
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(rest)
        }
        None => PathBuf::from(path),
    };
    let source = expanded
        .canonicalize()
        .with_context(|| format!("--mount {raw}: {} does not exist", expanded.display()))?;
    Ok(Mount {
        target: source.clone(),
        source,
        readonly,
    })
}

fn summary(kind: Kind, spec: &RunSpec) -> String {
    let mounts: Vec<String> = spec
        .mounts
        .iter()
        .map(|m| {
            let mode = if m.readonly { "ro" } else { "rw" };
            if m.source == m.target {
                format!("{} ({mode})", m.source.display())
            } else {
                format!("{} → {} ({mode})", m.source.display(), m.target.display())
            }
        })
        .collect();
    let net = match spec.network {
        Network::None => "none",
        Network::Open => "open",
    };
    format!(
        "{} · {} · {} · net {net}",
        kind.name(),
        spec.image,
        mounts.join(", ")
    )
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code.clamp(0, 255) as u8),
        (None, Some(sig)) => ExitCode::from((128 + sig).clamp(0, 255) as u8),
        _ => ExitCode::from(EXIT_AGENTBOX_ERROR),
    }
}

fn host_ids() -> (u32, u32) {
    // SAFETY: getuid/getgid have no preconditions and cannot fail.
    unsafe { (libc::getuid(), libc::getgid()) }
}

fn short_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    format!(
        "{:06x}",
        (nanos ^ std::process::id().rotate_left(16)) & 0xff_ffff
    )
}

fn shell_line(bin: &str, args: &[String]) -> String {
    std::iter::once(bin.to_string())
        .chain(args.iter().map(|a| shell_quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@%+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_flags() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let raw = tmp.display().to_string();
        let ro = parse_mount(&format!("{raw}:ro")).unwrap();
        assert!(ro.readonly);
        assert_eq!(ro.source, tmp);
        assert_eq!(ro.target, tmp);
        assert!(!parse_mount(&raw).unwrap().readonly);
        assert!(parse_mount("/definitely/not/here").is_err());
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("--memory"), "--memory");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
