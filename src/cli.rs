use std::ffi::OsString;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "sandbox",
    version,
    about = "Run AI coding agents in isolated, disposable sandboxes",
    allow_external_subcommands = true,
    after_help = "Shorthand: `sandbox [OPTIONS] <AGENT> [ARGS...]` runs <AGENT> in a fresh box,\n\
                  e.g. `sandbox codex --yolo`. Everything after <AGENT> goes to it untouched."
)]
pub struct Cli {
    #[command(flatten)]
    pub opts: BoxOpts,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run a command (or agent) in a fresh, disposable box
    Run {
        #[command(flatten)]
        opts: BoxOpts,
        /// Command and arguments to run inside the box
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Open an interactive shell in a fresh box
    Shell {
        #[command(flatten)]
        opts: BoxOpts,
    },
    /// Run a command in a box that is already running
    Exec {
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
        /// Box name (see `sandbox ps`)
        #[arg(name = "BOX")]
        name: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List running boxes
    Ps {
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
    },
    /// Stop a box (default: the one running for this project)
    Stop {
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
        /// Box name (see `sandbox ps`)
        #[arg(name = "BOX", conflicts_with = "all")]
        name: Option<String>,
        /// Stop every Sandbox box (other containers are left alone)
        #[arg(long)]
        all: bool,
    },
    /// Inspect and manage the permission manifest and approvals
    Policy,
    /// Show the post-run report
    Report,
    /// Store an agent credential in the OS keychain for the broker
    Login,
    /// Check backends and environment health
    Doctor,
    /// Remove cached images, caches or agent state
    Clean,
    #[command(external_subcommand)]
    Agent(Vec<OsString>),
}

#[derive(Args, Debug, Default, Clone)]
pub struct BoxOpts {
    /// Isolation backend: auto, apple-container, docker, podman, nerdctl
    #[arg(long, value_name = "BACKEND")]
    pub backend: Option<String>,

    /// Image to run [default: the Sandbox base image, built on first use]
    #[arg(long)]
    pub image: Option<String>,

    /// Network mode [default: open]
    #[arg(long, value_enum)]
    pub net: Option<NetMode>,

    /// Extra host path to mount at the same path; read-only unless suffixed with :rw
    #[arg(long, value_name = "PATH[:rw]")]
    pub mount: Vec<String>,

    /// Publish a box port on the host's 127.0.0.1: PORT, HOST:BOX, or IP:HOST:BOX to bind elsewhere
    #[arg(long, short = 'p', value_name = "SPEC")]
    pub publish: Vec<String>,

    /// Set an environment variable in the box (KEY=VALUE, or KEY to copy from host).
    /// Only TERM, COLORTERM and TZ are passed through otherwise
    #[arg(long, short = 'e', value_name = "KEY[=VALUE]")]
    pub env: Vec<String>,

    /// CPUs available to the box [default: 4]
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory limit, e.g. 8g or 512m [default: 8g]
    #[arg(long)]
    pub memory: Option<String>,

    /// Let `gh` in the box use your GitHub login (GH_TOKEN; the token is readable inside the box)
    #[arg(long)]
    pub gh: bool,

    /// Print the backend command instead of running it
    #[arg(long)]
    pub dry_run: bool,
}

impl BoxOpts {
    /// Options given after the subcommand take precedence over ones given before it.
    pub fn merge(self, inner: BoxOpts) -> BoxOpts {
        BoxOpts {
            backend: inner.backend.or(self.backend),
            image: inner.image.or(self.image),
            net: inner.net.or(self.net),
            mount: [self.mount, inner.mount].concat(),
            publish: [self.publish, inner.publish].concat(),
            env: [self.env, inner.env].concat(),
            cpus: inner.cpus.or(self.cpus),
            memory: inner.memory.or(self.memory),
            gh: self.gh || inner.gh,
            dry_run: self.dry_run || inner.dry_run,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetMode {
    /// No network at all (loopback only)
    None,
    /// Only allowlisted hosts, through the Sandbox proxy
    Allowlist,
    /// Unrestricted egress
    Open,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn shorthand_passes_agent_args_through() {
        let cli = parse(&["sandbox", "--net", "none", "codex", "--yolo", "-m", "x"]);
        assert_eq!(cli.opts.net, Some(NetMode::None));
        match cli.command {
            Command::Agent(args) => assert_eq!(args, ["codex", "--yolo", "-m", "x"]),
            other => panic!("expected agent shorthand, got {other:?}"),
        }
    }

    #[test]
    fn run_keeps_flags_after_command() {
        let cli = parse(&["sandbox", "run", "--image", "alpine", "--", "ls", "-la"]);
        match cli.command {
            Command::Run { opts, command } => {
                assert_eq!(opts.image.as_deref(), Some("alpine"));
                assert_eq!(command, ["ls", "-la"]);
            }
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn subcommand_names_are_reserved() {
        assert!(matches!(
            parse(&["sandbox", "ps"]).command,
            Command::Ps { .. }
        ));
        assert!(matches!(
            parse(&["sandbox", "doctor"]).command,
            Command::Doctor
        ));
    }

    #[test]
    fn inner_options_win_on_merge() {
        let outer = BoxOpts {
            image: Some("a".into()),
            env: vec!["X=1".into()],
            ..Default::default()
        };
        let inner = BoxOpts {
            image: Some("b".into()),
            env: vec!["Y=2".into()],
            ..Default::default()
        };
        let merged = outer.merge(inner);
        assert_eq!(merged.image.as_deref(), Some("b"));
        assert_eq!(merged.env, ["X=1", "Y=2"]);
    }
}
