mod agents;
mod backend;
mod cli;
mod image;
mod kits;
mod project;
mod proxy;
mod user;

use std::io::IsTerminal;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, ExitCode, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;

use backend::{Kind, Memory, Mount, Network, RunSpec};
use cli::{BoxOpts, Cli, Command, NetMode};
use project::{Project, WORKSPACE};
use user::BoxUser;

const DEFAULT_CPUS: u32 = 4;
const DEFAULT_MEMORY: &str = "8g";
/// Exit code for Sandbox's own failures, following the Docker convention.
const EXIT_SANDBOX_ERROR: u8 = 125;
/// Debian/Ubuntu's bashrc builds the prompt from `debian_chroot` (see `box_env`);
/// other shells pick up the exported PS1.
const SHELL: &str =
    r"export PS1='(sandbox) \w \$ '; command -v bash >/dev/null 2>&1 && exec bash || exec sh";
/// TERM values whose terminfo ships in common images; anything else (xterm-ghostty,
/// alacritty, …) would leave TUIs complaining about an unknown terminal.
const KNOWN_TERMS: [&str; 9] = [
    "xterm",
    "xterm-256color",
    "screen",
    "screen-256color",
    "tmux",
    "tmux-256color",
    "vt100",
    "linux",
    "dumb",
];

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("sandbox: {err:#}");
            ExitCode::from(EXIT_SANDBOX_ERROR)
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
    // Hooks and config in .git run on the host, so the box may read but never write them.
    let git_dir = project.root.join(".git");
    if git_dir.is_dir() {
        mounts.push(Mount {
            source: git_dir,
            target: Path::new(WORKSPACE).join(".git"),
            readonly: true,
        });
    }
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

    let (uid, gid) = host_ids();
    let user = BoxUser::new(std::env::var("USER").ok().as_deref(), uid, gid);
    let cache = project_cache(&project)?;
    // Dotfiles, agent logins and history persist per project, and never cross
    // between projects (one project's transcripts stay out of another's box).
    mounts.push(cache_mount(&cache, "home", user.home())?);

    let agent = command.first().and_then(|c| agents::lookup(c));
    let mut rosetta = false;
    let mut kit_warnings = Vec::new();
    let (recipe, image_label) = match &opts.image {
        Some(image) => {
            if !opts.kit.is_empty() {
                bail!("--kit adds to the Sandbox base image, so it can't be combined with --image");
            }
            (None, image.clone())
        }
        None => {
            let mut kits = kits::resolve(&project.root, &opts.kit)?;
            let mut layers = Vec::new();
            if let Some(agent) = agent {
                if !kits.iter().any(|k| k.name == "node") {
                    // Node only for the agent: no node_modules/ mountpoint in a
                    // project that has none.
                    let mut node = kits::resolve(&project.root, &["node".into()])?;
                    node.iter_mut().for_each(|k| k.artifacts.clear());
                    kits.extend(node);
                }
                if let Some(node) = kits.iter().find(|k| k.name == "node") {
                    agent.check_node(node)?;
                }
                let version = agent.version(&image::cache_dir()?.join("agents"));
                layers.push((
                    format!("{} {version}", agent.name),
                    agent.dockerfile(&version),
                ));
            }
            mounts.extend(kit_mounts(&cache, &kits)?);
            rosetta = std::env::consts::ARCH == "aarch64" && kits.iter().any(|k| k.needs_x86_64);
            kit_warnings = kits.iter().flat_map(|k| k.warnings.clone()).collect();
            let label = std::iter::once("sandbox-base".to_string())
                .chain(kits.iter().map(kits::Kit::label))
                .chain(layers.iter().map(|(label, _)| label.clone()))
                .collect::<Vec<_>>()
                .join(" + ");
            (Some(image::with_kits(&kits, &layers)), label)
        }
    };

    let selection = backend::select(opts.backend.as_deref())?;
    // On Apple `container`, network traffic goes through a host-side proxy, so it
    // works even when a VPN breaks the VM network. Needs our images (for the bridge)
    // and the root setup step (to open up the socket).
    let host_proxy = selection.kind == Kind::AppleContainer
        && network == Network::Open
        && recipe.is_some()
        && backend::needs_user_setup(selection.kind);
    let needs_check = selection.kind == Kind::AppleContainer
        && network == Network::Open
        && !host_proxy
        && !opts.dry_run;
    let (kind, network_warning) = if needs_check {
        network_fallback(
            selection.explicit,
            backend::apple_network_ok(&image::cache_dir()?, true),
            || Kind::Docker.probe().is_ok(),
        )
    } else {
        (selection.kind, None)
    };
    let setup_user = backend::needs_user_setup(kind);
    let mut env = box_env(std::env::vars(), opts.env);
    // Passed by name so the token reaches the box through our environment,
    // never through argv (visible in `ps`) or --dry-run output.
    let gh_token = if opts.gh { Some(github_token()?) } else { None };
    if gh_token.is_some() {
        env.push("GH_TOKEN".into());
    }
    // The agent's own credentials, also by name. Other secrets still need -e.
    let agent_keys = agent.map(|a| a.present_keys()).unwrap_or_default();
    env.extend(agent_keys.iter().map(|k| k.to_string()));
    if host_proxy {
        env.extend(proxy::Proxy::env());
        env.push("SANDBOX_BRIDGE".into());
    }
    env.splice(
        0..0,
        [
            format!("USER={}", user.name),
            format!("LOGNAME={}", user.name),
        ],
    );
    let spec = RunSpec {
        name: format!("sandbox-{}-{}", project.slug(), short_id()),
        image: recipe
            .as_ref()
            .map(|r| r.tag.clone())
            .unwrap_or_else(|| image_label.clone()),
        labels: vec![
            (backend::LABEL.into(), "1".into()),
            backend::project_label(&project.root),
        ],
        mounts,
        workdir: project.workdir(),
        home: user.home().into(),
        rosetta,
        host_proxy,
        env,
        uid,
        gid,
        network,
        cpus: opts.cpus.unwrap_or(DEFAULT_CPUS),
        memory,
        publish: opts
            .publish
            .iter()
            .map(|p| parse_publish(p))
            .collect::<Result<_>>()?,
        tty: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        command: if setup_user {
            user.wrap(command)
        } else {
            command
        },
    };
    let args = backend::run_args(kind, &spec);

    if opts.dry_run {
        println!("{}", shell_line(kind.bin(), &args));
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(warning) = selection
        .warning
        .as_ref()
        .filter(|_| kind == selection.kind)
    {
        eprintln!("sandbox: warning: {warning}");
    }
    if let Some(warning) = &network_warning {
        eprintln!("sandbox: warning: {warning}");
    }
    if let Some(recipe) = &recipe {
        image::ensure(kind, recipe)?;
    }
    eprintln!("sandbox: {}", summary(kind, &image_label, &spec));
    if network == Network::Open {
        eprintln!(
            "sandbox: warning: network is unrestricted (--net allowlist lands in milestone 3; use --net none to cut it off)"
        );
    }
    for warning in &kit_warnings {
        eprintln!("sandbox: warning: {warning}");
    }
    if !agent_keys.is_empty() {
        eprintln!("sandbox: passing {} to the box", agent_keys.join(", "));
    }
    if gh_token.is_some() {
        eprintln!(
            "sandbox: warning: --gh: your GitHub token is readable by anything running in the box"
        );
    }

    let mut process = Process::new(kind.bin());
    process.args(&args);
    if let Some(token) = &gh_token {
        process.env("GH_TOKEN", token);
    }
    // Kept alive until the box exits; the socket is removed on drop.
    let _proxy = if host_proxy {
        let proxy = proxy::Proxy::start(&spec.name)?;
        // `container run --ssh` forwards whatever SSH_AUTH_SOCK names into the box.
        process.env("SSH_AUTH_SOCK", &proxy.socket);
        process.env("SANDBOX_BRIDGE", proxy::BRIDGE);
        Some(proxy)
    } else {
        None
    };
    let mut child = process
        .spawn()
        .with_context(|| format!("starting {}", kind.bin()))?;
    if setup_user {
        prepare_user(kind, &spec.name, &user, &mut child)?;
    }
    let status = child.wait().context("waiting for the box")?;
    Ok(exit_code(status))
}

/// Adds the host user to the box's /etc/passwd as soon as the box is running, which
/// releases the wrapped command. On failure the command still starts, just nameless.
fn prepare_user(kind: Kind, name: &str, user: &BoxUser, child: &mut Child) -> Result<()> {
    // Generous, because the first run of an image includes pulling it.
    const MAX_ATTEMPTS: u32 = 3000;
    let setup = backend::exec_root_args(kind, name, &user.setup_script(&user.home()));
    for _ in 0..MAX_ATTEMPTS {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        let out = Process::new(kind.bin())
            .args(&setup)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {} exec", kind.bin()))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !user::is_not_ready(&stderr, name) {
            eprintln!(
                "sandbox: warning: couldn't add your user inside the box ({}); continuing without it",
                stderr.trim()
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Per-project state on the host: ~/.cache/sandbox/projects/<project>/. Plain host
/// directories (not backend volumes) keep ownership right on every backend.
fn project_cache(project: &Project) -> Result<PathBuf> {
    Ok(image::cache_dir()?.join("projects").join(format!(
        "{}-{:06x}",
        project.slug(),
        image::short_hash(&project.root.display().to_string()) & 0xff_ffff
    )))
}

fn cache_mount(cache: &Path, name: &str, target: impl Into<PathBuf>) -> Result<Mount> {
    let source = cache.join(name);
    std::fs::create_dir_all(&source).with_context(|| format!("creating {}", source.display()))?;
    Ok(Mount {
        source,
        target: target.into(),
        readonly: false,
    })
}

/// Kit caches, box-only build dirs and seeded files, kept per project so they survive
/// between runs.
fn kit_mounts(cache: &Path, kits: &[kits::Kit]) -> Result<Vec<Mount>> {
    let mut mounts = Vec::new();
    for kit in kits {
        for (name, target) in &kit.caches {
            mounts.push(cache_mount(cache, name, target.as_str())?);
        }
        for rel in &kit.artifacts {
            let name = format!("{}-{}", kit.name, rel.trim_start_matches('.'));
            mounts.push(cache_mount(cache, &name, Path::new(WORKSPACE).join(rel))?);
        }
        for (name, rel, contents, overwrite) in &kit.seeds {
            let path = cache.join(name).join(rel);
            if *overwrite || !path.exists() {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, contents)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
    }
    Ok(mounts)
}

/// The host's GitHub login: GH_TOKEN / GITHUB_TOKEN, else `gh auth token`.
fn github_token() -> Result<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(var)
            && !token.trim().is_empty()
        {
            return Ok(token.trim().to_string());
        }
    }
    let out = Process::new("gh")
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() && !out.stdout.trim_ascii().is_empty() => {
            Ok(String::from_utf8_lossy(out.stdout.trim_ascii()).into_owned())
        }
        _ => bail!(
            "--gh: no GitHub login found; run `gh auth login` on this machine or set GH_TOKEN"
        ),
    }
}

/// Given that Apple `container` was selected: keep it when its network works;
/// otherwise switch to Docker if Sandbox chose the backend itself and Docker is
/// usable, else keep it and warn.
fn network_fallback(
    explicit: bool,
    network_ok: bool,
    docker_ok: impl FnOnce() -> bool,
) -> (Kind, Option<String>) {
    if network_ok {
        return (Kind::AppleContainer, None);
    }
    if !explicit && docker_ok() {
        let warning = format!("{}. Using docker for this run", backend::APPLE_NETWORK_HINT);
        return (Kind::Docker, Some(warning));
    }
    (
        Kind::AppleContainer,
        Some(backend::APPLE_NETWORK_HINT.to_string()),
    )
}

fn exec_box(backend: Option<&str>, name: &str, command: &[String]) -> Result<ExitCode> {
    let kind = backend::select(backend)?.kind;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (uid, gid) = host_ids();
    let env = box_env(std::env::vars(), vec![]);
    let args = backend::exec_args(kind, name, tty, uid, gid, &env, command);
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
        "{:width$}  {:14}  {:32}  PROJECT",
        "NAME", "STATUS", "IMAGE"
    );
    for b in boxes {
        println!(
            "{:width$}  {:14}  {:32}  {}",
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
            bail!("no running Sandbox box named `{name}` (see `sandbox ps`)");
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
        if kind == Kind::AppleContainer && kind.probe().is_ok() {
            if backend::apple_network_ok(&image::cache_dir()?, false) {
                println!("    {:16} boxes can reach the internet", "");
            } else {
                println!("    {:16} ✗ {}", "", backend::APPLE_NETWORK_HINT);
            }
        }
    }
    println!();
    if let Ok(pinned) = std::env::var(backend::BACKEND_ENV) {
        println!("{}  {pinned}", backend::BACKEND_ENV);
    }
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
            eprintln!("sandbox: {err:#}");
            Ok(ExitCode::from(EXIT_SANDBOX_ERROR))
        }
    }
}

fn parse_mount(raw: &str) -> Result<Mount> {
    let (path, readonly) = match raw.strip_suffix(":rw") {
        Some(p) => (p, false),
        None => (raw.strip_suffix(":ro").unwrap_or(raw), true),
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

/// Accepts `PORT`, `HOST:BOX` or `IP:HOST:BOX` (plus an optional `/tcp` or `/udp`).
/// Without an IP the port binds to 127.0.0.1, so the box isn't exposed to the network.
fn parse_publish(raw: &str) -> Result<String> {
    let (ports, proto) = match raw.split_once('/') {
        Some((ports, proto @ ("tcp" | "udp"))) => (ports, Some(proto)),
        Some(_) => bail!("--publish {raw}: protocol must be tcp or udp"),
        None => (raw, None),
    };
    let parts: Vec<&str> = ports.split(':').collect();
    let (ip, host, boxed) = match parts[..] {
        [port] => ("127.0.0.1", port, port),
        [host, boxed] => ("127.0.0.1", host, boxed),
        [ip, host, boxed] => (ip, host, boxed),
        _ => bail!("--publish {raw}: expected PORT, HOST:BOX or IP:HOST:BOX"),
    };
    for port in [host, boxed] {
        port.parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .with_context(|| format!("--publish {raw}: `{port}` is not a valid port"))?;
    }
    ip.parse::<std::net::IpAddr>()
        .with_context(|| format!("--publish {raw}: `{ip}` is not an IP address"))?;
    let spec = format!("{ip}:{host}:{boxed}");
    Ok(match proto {
        Some(proto) => format!("{spec}/{proto}"),
        None => spec,
    })
}

/// The box's environment: a few safe host values, fixed defaults, then the user's `-e`.
/// Nothing else from the host environment is passed in, since it often holds secrets.
fn box_env(host: impl Iterator<Item = (String, String)>, user: Vec<String>) -> Vec<String> {
    let mut env = Vec::new();
    for (key, value) in host {
        match key.as_str() {
            "TERM" if KNOWN_TERMS.contains(&value.as_str()) => env.push(format!("TERM={value}")),
            "TERM" => env.push("TERM=xterm-256color".into()),
            "COLORTERM" | "TZ" => env.push(format!("{key}={value}")),
            _ => {}
        }
    }
    env.sort();
    // Host locales (en_GB.UTF-8, …) are rarely installed in images; C.UTF-8 always is.
    env.push("LANG=C.UTF-8".into());
    env.push("debian_chroot=sandbox".into());
    env.extend(user);
    env
}

fn summary(kind: Kind, image: &str, spec: &RunSpec) -> String {
    let cache_root = image::cache_dir().ok();
    let is_cache = |m: &&Mount| cache_root.as_ref().is_some_and(|c| m.source.starts_with(c));
    let mounts: Vec<String> = spec
        .mounts
        .iter()
        .filter(|m| !is_cache(m))
        .map(|m| {
            let mode = if m.readonly { "ro" } else { "rw" };
            if m.source == m.target {
                format!("{} ({mode})", m.source.display())
            } else {
                format!("{} → {} ({mode})", m.source.display(), m.target.display())
            }
        })
        .collect();
    let caches: Vec<String> = spec
        .mounts
        .iter()
        .filter(is_cache)
        .filter_map(|m| {
            m.source
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
    let net = match spec.network {
        Network::None => "none",
        Network::Open if spec.host_proxy => "open (via host proxy)",
        Network::Open => "open",
    };
    let mut line = format!("{} · {image} · {}", kind.name(), mounts.join(", "));
    if !caches.is_empty() {
        line.push_str(&format!(" · caches {}", caches.join(", ")));
    }
    line.push_str(&format!(" · net {net}"));
    line
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code.clamp(0, 255) as u8),
        (None, Some(sig)) => ExitCode::from((128 + sig).clamp(0, 255) as u8),
        _ => ExitCode::from(EXIT_SANDBOX_ERROR),
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
    fn mounts_are_read_only_unless_rw() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let raw = tmp.display().to_string();
        let plain = parse_mount(&raw).unwrap();
        assert!(plain.readonly);
        assert_eq!(plain.source, tmp);
        assert_eq!(plain.target, tmp);
        assert!(parse_mount(&format!("{raw}:ro")).unwrap().readonly);
        assert!(!parse_mount(&format!("{raw}:rw")).unwrap().readonly);
        assert!(parse_mount("/definitely/not/here").is_err());
    }

    #[test]
    fn apple_network_fallback() {
        assert_eq!(
            network_fallback(false, true, || true),
            (Kind::AppleContainer, None)
        );
        let (kind, warning) = network_fallback(false, false, || true);
        assert_eq!(kind, Kind::Docker);
        assert!(warning.unwrap().ends_with("Using docker for this run"));
        // An explicit choice is respected; so is a missing Docker.
        assert_eq!(
            network_fallback(true, false, || true).0,
            Kind::AppleContainer
        );
        assert_eq!(
            network_fallback(false, false, || false).0,
            Kind::AppleContainer
        );
    }

    #[test]
    fn publish_binds_localhost_by_default() {
        assert_eq!(parse_publish("3000").unwrap(), "127.0.0.1:3000:3000");
        assert_eq!(parse_publish("8080:80").unwrap(), "127.0.0.1:8080:80");
        assert_eq!(parse_publish("0.0.0.0:8080:80").unwrap(), "0.0.0.0:8080:80");
        assert_eq!(parse_publish("53:53/udp").unwrap(), "127.0.0.1:53:53/udp");
        for bad in ["http", "0", "70000", "1:2:3:4", "3000/sctp", "nope:1:2"] {
            assert!(parse_publish(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn env_passes_only_safe_host_vars() {
        let host = [
            ("TERM", "xterm-ghostty"),
            ("COLORTERM", "truecolor"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("LANG", "en_GB.UTF-8"),
            ("PATH", "/opt/homebrew/bin"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let env = box_env(host.into_iter(), vec!["FOO=bar".into()]);
        assert_eq!(
            env,
            [
                "COLORTERM=truecolor",
                "TERM=xterm-256color",
                "LANG=C.UTF-8",
                "debian_chroot=sandbox",
                "FOO=bar"
            ]
        );
    }

    #[test]
    fn known_terms_pass_through() {
        let host = [("TERM".to_string(), "tmux-256color".to_string())];
        assert!(box_env(host.into_iter(), vec![]).contains(&"TERM=tmux-256color".to_string()));
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("--memory"), "--memory");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
