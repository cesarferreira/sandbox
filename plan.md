# Sandbox — Revised Plan

Version: 0.2 (revision of PRD 0.1)
Author: Cesar Ferreira
Status: Draft

---

## Part 1 — Review of v0.1

### What v0.1 gets right (kept)

- **It works with any agent.** The agent is just a command, and Sandbox doesn't care which one.
- **Disposable by default.** Every run starts clean.
- **One command, no setup.** Prefixing a normal command with `sandbox` is the right UX.
- **The permission manifest as the signature feature.** This is the best idea in the draft. v0.2 builds the product around it, with one rule: every line in it must be enforced by something.

### Holes, in order of severity

#### Security model

1. **There is no threat model.** The draft lists things an agent *could* do, but never names the adversary. There are two realistic ones:
   - *agent error*: `rm -rf`, broken dotfiles, runaway processes;
   - *prompt injection*: a malicious README, issue, dependency or web page telling the agent to exfiltrate data.

   The second is the one that matters. With v0.1 defaults (`network = internet`, the project mounted read-write, the agent's API key inside the box), an injected agent can `curl` your proprietary code and your API key to anywhere. The box stops it from reading `~/.ssh`, but it leaks everything the box does contain.
   **Fix:** make the default network mode an **egress allowlist** (the agent's model API plus the package registries), not `internet`.

2. **The agent's own credentials are never addressed.** Codex, Claude Code and Gemini all need to authenticate, and the draft never says how. Putting `ANTHROPIC_API_KEY` or `~/.codex/auth.json` into the sandbox makes it the first thing an injected agent steals. OAuth login flows that open a browser and wait on a localhost callback also break inside containers.
   **Fix:** add a host-side **egress proxy and credential broker**. It enforces the allowlist and injects auth headers for known API hosts, so the real credential never enters the sandbox; the sandbox only sees a placeholder token.

3. **A read-write project mount lets the agent run code on the host.** The agent can write files that the *host* executes later:
   - `.git/hooks/*`, and `.git/config` settings like `core.hooksPath`, `core.fsmonitor` and `core.sshCommand`;
   - `.envrc` (direnv), `.vscode/tasks.json` / `settings.json`, `.idea/`;
   - `package.json` scripts, `Makefile`, `gradlew`, `build.gradle`, `.cargo/config.toml`.

   The next time you run `git status` or open the project in your IDE, the agent's code runs on your laptop, outside the sandbox. This is the most important hole in v0.1.
   **Fix:** mount the obvious host-executed paths read-only (`.git/hooks`, `.git/config`, `.envrc`, `.vscode`, `.idea`). Add a **review mode**, where the agent works on a copy and changes come back as a branch or diff, and flag any change to a build or script file in the post-run report.

4. **Secrets inside the project are exposed by default.** `.env`, `local.properties`, `google-services.json`, `*.keystore` and `terraform.tfstate` all live inside "just the project".
   **Fix:** add an `.sandboxignore` (with sensible defaults) that masks these paths with empty files or tmpfs.

5. **Some manifest permissions can't be enforced.**
   - `git.push: false` can't be enforced while the box has network access and a token; the agent can call `git push` or `curl` the GitHub API directly. The only honest version is "no push credential is given to the box, and the host pushes after you review".
   - `processes.spawn: false` would stop every agent from working, since they all spawn shells.
   - `host.clipboard` and `host.notifications` don't exist inside a container.

   A permission you can't enforce is **security theater**, and it undermines the signature feature.
   **Fix:** the manifest may only contain permissions that map to a concrete enforcement mechanism (see Part 2 §5).

6. **A manifest committed to the repo is an attack vector.** If `sandbox.yaml` lives in the repo, a malicious repo can ship one that asks for `~/.ssh` plus the open internet, and a user tired of prompts presses Y.
   **Fix:** a repo manifest is only a *request*. The user's global policy caps what it can get. Approvals are stored per project, keyed by a hash of the manifest, and you're asked again only when the manifest changes. This also fixes the prompt fatigue v0.1 would cause by asking on every launch.

7. **The example config contradicts "secure by default".** `mount_gitconfig = true` breaks "nothing is mounted except the project". A `~/.gitconfig` can carry credential helpers, `url.insteadOf` rules with embedded tokens, and `includeIf` paths.
   **Fix:** generate a minimal gitconfig in the box that only sets `user.name` and `user.email`. Also, v0.1 uses two config formats (TOML for config, YAML for the manifest); pick one.

8. **Shared caches can be poisoned.** Mounting the host's `~/.npm`, `~/.cargo` or `~/.gradle` means a malicious postinstall script in project A can poison a cache that project B, *or your host*, later uses.
   **Fix:** use caches that Sandbox manages as named volumes, never the host's caches, and allow scoping them per project.

9. **There are no resource limits.** Nothing caps CPU, memory, pids, disk, or run time, so a fork bomb or runaway `cargo build` still hurts the host.

#### Feasibility and usability

10. **The toolchain gap.** "Base image: ubuntu" contains neither the agent (`codex`, `claude`) nor your toolchain (the Android SDK, a Rust toolchain, a specific Node version). On macOS, host binaries can't run in a Linux container at all. This is the biggest usability problem, and v0.1 doesn't mention it.
    **Fix:** build images in layers (base → project toolchain → agent), with the toolchain layer taken from `devcontainer.json`, a Dockerfile, or `mise.toml` / `.tool-versions` when they exist. v0.1 dismissed Dev Containers, but reusing their *spec* costs nothing and covers a lot of real projects.

11. **Build artifacts break across the host/Linux boundary.** If the agent runs `npm install` inside the box, Linux native modules land in the host's `node_modules` and break the host, and the reverse happens too. The same goes for `target/`, `.venv/`, `build/` and `.gradle/`.
    **Fix:** mask these directories with volumes that live only in the container. Also run the container as the host's UID/GID, so files the agent creates aren't owned by root.

12. **The backend list is scope explosion, and it contains a factual error.** Twelve backends means twelve sets of bugs. "Docker: Linux only" is wrong: Docker Desktop, OrbStack and Colima all run Docker on macOS in a VM. Bubblewrap is also a different *model*: it sandboxes host binaries with no image at all.
    **Fix:** the MVP has two drivers.
    - **Apple `container`** is the preferred macOS backend: one VM per box, no daemon, no socket, stable since 1.0 (June 2026).
    - **Docker-compatible CLI** (Docker Desktop, OrbStack, Colima, Podman) covers Linux, plus Macs that can't run Apple `container`.

    Sandbox must never mount the docker socket.

13. **The CLI grammar is ambiguous.**
    - `sandbox codex --yolo` collides with the subcommands: is `sandbox list` an agent called `list`?
    - `sandbox stop` doesn't say *which* box when several are running.
    - `--network localhost` could mean the host's localhost (reaching your dev DB) or the container's.
    - The draft doesn't cover exit code and signal propagation, TTY resize, port forwarding for dev servers the agent starts, or image paste.

14. **"Disposable" conflicts with agent state.** Agents keep logins, settings, MCP config, skills and session history in `~/.claude`, `~/.codex` and similar. If every run starts from nothing, you log in every time and can never resume a session.
    **Fix:** keep a persistent **agent state volume** per agent, separate from the host's home directory. The workspace is disposable; the agent's own state is not.

15. **Git worktrees and monorepos.** In a git worktree, `.git` is a *file* pointing outside the mounted directory, so git breaks inside the box. And running from a subdirectory of a monorepo: should the box mount the subdirectory or the repo root?
    **Fix:** mount the git top level by default, and resolve and mount the worktree's common git dir read-only.

16. **The audit log is too thin, and replay is too far away.** "Timestamp, command, exit code" says nothing about what the agent actually *did*. A **post-run report** is cheap to build and valuable: files changed, domains contacted, requests blocked, and flagged changes to sensitive files. It belongs in the MVP. Full replay, GUI, snapshots and the VS Code extension can stay deferred.

17. **Other tools already do this, and the draft doesn't mention them.**
    - **Docker Sandboxes**: microVM per agent, a private Docker engine, network allow and deny lists, and support for Claude Code, Codex, Copilot CLI and Gemini CLI.
    - **Dagger container-use**: containerized worktrees per agent over MCP, with git-branch review.
    - **Anthropic sandbox-runtime (`srt`)**: an OS-level sandbox (Seatbelt / bubblewrap) plus a network-filtering proxy, with no container.
    - **Built-in agent sandboxes**: Codex and Claude Code both ship their own.
    - **The name**: the CLI is `sandbox`, published as the crate `agent-sandbox-cli` (`sandbox` and `agent-sandbox` are both taken on crates.io). The word is generic (Docker Sandboxes, Apple's App Sandbox, `sandbox-exec`).

    The plan needs a "why this instead of those" section (Part 2 §2).

---

## Part 2 — Revised Plan

### 1. Vision

Unchanged from v0.1: **Run any AI coding agent with full autonomy, and know exactly what it can touch.**

```bash
cd my-project
sandbox codex --yolo
```

The trust question moves from *"Can I trust this model?"* to *"What does this box allow?"*, and the answer is a short, human-readable manifest that the system actually enforces.

### 2. Positioning (why this, not the alternatives)

| | Docker Sandboxes | container-use | sandbox-runtime | Agent built-in sandboxes | **Sandbox** |
|---|---|---|---|---|---|
| Works with any agent | several | MCP agents | yes | one agent each | **yes** |
| Enforced, reviewable manifest | partial (network lists) | no | config file | per agent | **headline feature** |
| Credentials never enter the box | ? | no | no | n/a | **yes (broker)** |
| Protects host-executed files | ? | via branches | path rules | varies | **yes** |
| Post-run report | ? | git diff | no | no | **yes** |
| Open, local, vendor-neutral | Docker product | yes | yes | n/a | **yes** |

"?" means not verified yet; filling these in is a **Milestone 0** task. If Docker Sandboxes already covers most of the Sandbox column, the right move might be a policy and manifest layer *on top of* existing sandboxes instead of a new runtime. Decide this before writing code.

### 3. Threat model

**In scope (Sandbox defends against):**
- *Agent error*: destructive commands, dependency pollution, runaway processes.
- *Prompt injection*: an agent steered by untrusted content in the repo, its dependencies, or the web, trying to:
  - read data outside the project;
  - exfiltrate project code or credentials;
  - get code executed on the host;
  - persist across runs.
- *A malicious repo*: a repository whose committed config tries to grant itself more access.

**Out of scope (documented, not defended):**
- Kernel or hypervisor escapes. We rely on the backend's boundary: a VM on macOS and Windows, namespaces on Linux, with rootless Podman or gVisor recommended.
- Exfiltration through *allowed* channels. The agent's model API is necessarily reachable, so an injected agent could send code to the model provider. The report shows this; it can't be prevented.
- A human approving a dangerous manifest.
- Malicious agent binaries. We sandbox them; we don't verify them.

### 4. Principles

1. **Secure by default.** The project is mounted minus masked secrets; the network is an allowlist; no credentials are passed in.
2. **Enforceable or absent.** Every permission shown to the user corresponds to a mechanism.
3. **Nothing the box writes runs on the host unreviewed.**
4. **Disposable workspace, persistent agent state.**
5. **Fast.** Under 3 s to a prompt on a warm cache. A cold first run is allowed to take longer, and it shows progress.
6. **Works with any agent through data, not code.** A new agent means a new profile file, not a new release.

### 5. The permission manifest (headline feature)

The same format (TOML) is used at three levels:

| Level | Location | Role |
|---|---|---|
| Global policy | `~/.config/sandbox/policy.toml` | **Caps**: the most any project can get |
| Project request | `<repo>/.sandbox.toml` (committed) | **Request**: what this project asks for |
| Approval record | `~/.local/share/sandbox/approvals/<project-hash>.toml` | What the user approved, keyed by manifest hash |

The effective permissions are `min(request, policy)` plus any CLI flags. The user is asked on the first run and again only when the effective permissions change.

```toml
[filesystem]
project = "rw"                     # "rw" | "ro" | "review" (copy; changes returned as a branch)
protect = [".git", ".envrc", ".vscode", ".idea"]   # read-only in the box
mask    = [".env*", "*.keystore", "local.properties", "*.tfstate"]     # appear as empty
extra   = []                       # e.g. ["~/Downloads:ro"]; each entry is shown in the prompt

[network]
mode  = "allowlist"                # "none" | "allowlist" | "open"
allow = ["registry.npmjs.org", "crates.io", "static.crates.io", "pypi.org", "files.pythonhosted.org"]
# The agent profile adds its model API hosts automatically.
host_ports = []                    # e.g. [5432] to reach a Postgres on the host
publish    = []                    # e.g. [3000] to see a dev server running in the box

[credentials]
agent_auth = "broker"              # "broker" (injected by the proxy, never in the box) | "env" | "none"
github     = "none"                # "none" | "read" (read-only fine-grained token via the broker)
ssh_agent  = false                 # forward an SSH agent socket, never the key files

[git]
push = false                       # enforced by giving the box no push credential; the host pushes after review

[resources]
cpus     = 4
memory   = "8g"
pids     = 2048
disk     = "20g"
timeout  = "4h"
```

**Every field maps to how it is enforced:**

| Permission | Enforced by |
|---|---|
| `filesystem.project`, `protect`, `extra` | bind-mount flags (rw/ro); `review` mode uses a copy or worktree |
| `filesystem.mask` | tmpfs or empty-file overlay mounts |
| `network.mode` / `allow` | the box has no direct route out; all egress goes through the Sandbox proxy (HTTP CONNECT/SNI allowlist) |
| `credentials.*` | the broker injects headers for the matching host; the box holds only a placeholder |
| `git.push` | no push credential enters the box |
| `resources.*` | container runtime limits plus a Sandbox watchdog |

What the user sees at launch (first run, or when the manifest changed):

```
Launching codex in Sandbox  (backend: apple-container, image: sandbox/codex:node20)

Filesystem
  ✓ read/write   ~/code/my-project
  🔒 read-only   .git/hooks  .git/config  .envrc  .vscode
  ◌ masked       .env  .env.local
  ✗ everything else on this machine

Network  (allowlist)
  ✓ api.openai.com          (agent)
  ✓ registry.npmjs.org      (project)
  ✗ everything else

Credentials
  ✓ OpenAI auth             (brokered, never enters the box)
  ✗ GitHub, SSH

Limits   4 CPU · 8 GB · 4h

Approve for this project? [Y/n/details]
```

On later runs, a single line: `sandbox: using approved policy for my-project (codex, allowlist, 2 hosts)`.

### 6. Architecture

```
┌──────────────────────── Host ─────────────────────────┐
│                                                        │
│  sandbox CLI                                          │
│   ├── policy engine     (request ∩ policy → approved)  │
│   ├── agent registry    (profiles: install, auth, env) │
│   ├── image builder     (base → toolchain → agent)     │
│   ├── backend driver    (docker-compatible | apple)    │
│   ├── egress proxy +    (allowlist, header injection,  │
│   │   credential broker  request log)                  │
│   └── reporter          (post-run report, run log)     │
│                                                        │
│         │ run container (internal network only)        │
│         ▼                                              │
│  ┌──────────── Sandbox ────────────┐                   │
│  │ /workspace   project (masks,    │                   │
│  │              ro protects)       │                   │
│  │ /home/agent  agent state volume │                   │
│  │ /cache       managed caches     │                   │
│  │ HTTPS_PROXY → host proxy        │                   │
│  └─────────────────────────────────┘                   │
└────────────────────────────────────────────────────────┘
```

**Components:**

- **CLI.** A Rust single binary. It fits the existing `new-rust-cli` template and gives fast startup.
- **Backend driver.** A trait with methods like `ensure_image`, `create`, `attach_tty`, `exec`, `stop` and `remove`.
  - **`apple-container`** (macOS, preferred). Each box runs in its own lightweight VM with its own kernel, so boxes don't share a VM the way they do under Docker Desktop, OrbStack or Colima. There is no daemon or socket to leak.
    - Requires Apple silicon and **macOS 26**. On macOS 15 only the single default network exists and `--network` errors out, so the internal-network-plus-proxy design (below) can't be enforced there.
  - **`docker`** (Linux; fallback on macOS). Works with any Docker-compatible CLI (`docker` / `podman` / `nerdctl`).
    - On macOS, all boxes share one Linux VM, which is a weaker boundary.
    - On Linux, prefer rootless Podman.
  - **Selection order:**
    1. config or `--backend`;
    2. on macOS, `apple-container` if it's installed on Apple silicon with macOS ≥ 26;
    3. the first Docker-compatible CLI found.

    The chosen backend is always printed. On a macOS fallback, Sandbox also prints why (for example "macOS 15: Apple container networking unavailable") and that isolation is weaker.
  - **Validate early (M0 spike).** Before committing to the `apple-container` driver, check three things (see §11):
    - that it can enforce "the only way out is the proxy";
    - bind-mount I/O speed;
    - how long a warm start takes.
- **Network isolation.** The box is attached to an *internal* network with no default route. Its only way out is the Sandbox proxy (a sidecar container or a host process), so `HTTPS_PROXY` is a convenience rather than the enforcement: tools that ignore it simply fail.
- **Credential broker.** Credentials are stored in the OS keychain (macOS Keychain, or libsecret on Linux).
  - The proxy does the TLS for allowlisted API hosts that need credentials and injects the auth header. Doing this means the box has to trust a Sandbox CA; *open question*.
  - Everything else passes through as SNI-filtered CONNECT.
- **Agent profiles.** TOML files, built in and user-extensible, for example:
  ```toml
  name = "codex"
  install = "npm i -g @openai/codex@{version}"
  command = "codex"
  state_dirs = ["~/.codex"]
  api_hosts = ["api.openai.com", "chatgpt.com"]
  auth = { kind = "header", host = "api.openai.com", header = "Authorization", format = "Bearer {secret}" }
  yolo_flag = "--yolo"
  ```
- **Image builder.** Images are built in three layers:
  1. **base**: a Debian-slim image with git, curl, a shell and common tools;
  2. **toolchain**: detected from the project, in this order: `.sandbox.toml` image or Dockerfile → `.devcontainer/devcontainer.json` → `mise.toml` / `.tool-versions` → nothing;
  3. **agent**: the agent profile's install step.

  Layers are content-addressed and cached. `--image` overrides everything.
- **Workspace modes.**
  - `rw` (default): bind mount with protects and masks.
  - `review`: `git worktree add` into a temp directory. Only that worktree and the common git dir (read-only) are mounted. At the end, Sandbox shows a diffstat and asks to keep the branch or discard it.
- **Artifact isolation.** `node_modules`, `target`, `.venv`, `build` and `.gradle` are placed on named volumes in the container, based on the detected project type. The container runs as the host's UID/GID.
- **Reporter.** Writes a run record to `~/.local/share/sandbox/runs/<id>/`: the approved manifest, the command, timing, exit code, the egress log (allowed and blocked), and a git diffstat. Files are mode `0600`.

### 6a. Toolchain kits (Milestone 2 design)

A plain Linux image lacks the project's toolchain, and the host's toolchain (macOS binaries) can't run in the box. Sandbox therefore composes the image per project:

1. **Choose the toolchain layer**, strongest first:
   - `--image` (used as-is, no kits);
   - `.sandbox/Dockerfile` (`FROM sandbox-base`) committed in the repo;
   - an existing `.devcontainer/devcontainer.json`;
   - **auto-detected kits**, overridable with `--kit NAME` / `--kit none`.
2. **Kits are Dockerfile snippets** layered on the base image. Each reads the project's own version pins (`rust-toolchain.toml`, `.nvmrc`, `.bazelversion`, `compileSdk`…). Values taken from repo files are restricted to safe characters before they reach a `RUN` line.
3. **The combined image is tagged by a hash of its Dockerfile.** It is built once, and rebuilt when a pin changes.
4. **Every kit declares caches** (package registries, SDKs) and **build-output dirs** (`target/`, `build/`, `.gradle/`). Both are backed by host directories under `~/.cache/sandbox/projects/<project>/`. They are per project to avoid cross-project poisoning, and plain host directories so ownership is right on every backend.

| Kit | Status | Notes |
|---|---|---|
| rust | **done** | rustup + pinned toolchain; registry, git and `target/` cached |
| node | **done** | nodejs.org tarball verified against SHASUMS256; pin from `.nvmrc` / `.node-version` / `.tool-versions` / `engines.node`, else LTS; corepack; npm/pnpm/yarn/corepack caches and `node_modules/` box-only. Only the root `node_modules` is isolated for now (workspace packages' nested ones are not). |
| android | **done** | JDK 17 + pinned cmdline-tools; AGP fetches platforms and build-tools into a per-project SDK cache. Host-accepted licences are copied in, never auto-accepted. A Gradle init script moves build dirs into a cache, so box and Android Studio builds don't thrash. The SDK is mirrored at `local.properties`' `sdk.dir`. On Apple silicon, the x86_64-only `aapt2` / platform-tools run via Rosetta (`container --rosetta`) or Docker Desktop's emulation, with `libc6:amd64` in the image. Verified: AGP 9.4.1 `assembleDebug` on both backends. Not covered yet: JDK other than 17, NDK/CMake, emulators and devices (use host `adb` via `host_ports`), a shared SDK cache across projects. |
| bazel | planned | bazelisk; persistent output base, ideally a remote cache |
| go, python (uv) | planned | |
| iOS / Xcode | not possible in a Linux box | needs the native OS-sandbox backend (§10) |

**Agent layer (done):** `sandbox claude|codex|gemini` adds the node kit if needed and installs the agent from npm on top. The version is pinned to the registry's latest and re-checked at most daily. A project pinning a too-old Node fails early. The agent's own key variables pass through by name. Agent state lives in a per-project persistent home, so one project's transcripts never reach another project's box. The credential broker (M3) will replace key passthrough.

### 7. CLI

```
sandbox [OPTIONS] <AGENT> [AGENT_ARGS...]   # shorthand; everything after <AGENT> goes to the agent untouched
sandbox run [OPTIONS] <AGENT|--> [ARGS...]  # explicit form; `--` runs an arbitrary command
sandbox shell [--agent <name>]              # interactive shell in a fresh box with the same policy
sandbox exec <BOX> <CMD...>                 # run a command in a box that is already running
sandbox ps                                  # list running boxes (id, agent, project, uptime)
sandbox stop [<BOX>|--all]                  # with no argument: the box for this project, or an error if there are several
sandbox policy [show|edit|approve|revoke]   # inspect and manage the manifest and approvals
sandbox report [<RUN>]                      # post-run report (default: the last run)
sandbox login <AGENT>                       # store the agent's credential in the keychain for the broker
sandbox doctor                              # backend, proxy, keychain, and image cache health
sandbox clean [--images|--caches|--state]   # clean up, scoped by what you name
```

- **Reserved names:** `run shell exec ps stop policy report login doctor clean help`. An agent profile can't use any of these.
- **Exit code:** Sandbox exits with the agent's exit code. It uses 125–127 for its own errors, following the Docker convention.
- **Signals and terminal:** signals, TTY resize and raw mode pass through to the agent.
- **Common options:**
  - `--net none|allowlist|open`, `--allow <host>`;
  - `--mount <path>[:ro]`, `--publish <port>`, `--host-port <port>`;
  - `--review`, `--image`, `--yes` (non-interactive, fails if approval is needed), `--dry-run` (print the effective policy and exit).

### 8. MVP scope (v0.1 release)

**In:**
- **macOS:** Apple `container` on Apple silicon with macOS 26, falling back to OrbStack, Docker Desktop or Colima through the Docker-compatible driver.
- **Linux:** Docker or rootless Podman through the Docker-compatible driver.
- **Parity:** both drivers must pass the same adversarial security suite.
- Agent profiles for `claude`, `codex` and `gemini`, plus `run -- <cmd>` for anything else.
- Base and agent image layers, plus toolchain from `devcontainer.json` or `--image`. `mise` detection comes later.
- A `rw` workspace with protects, masks and artifact volumes; UID/GID mapping; git top-level and worktree handling.
- Network modes `none`, `allowlist` and `open`, all through the proxy, with an egress log.
- The credential broker for the agent's API key only, using the API-key auth path. OAuth or subscription login is an open question.
- Persistent agent state volumes; managed cache volumes.
- The manifest with policy caps, the hash-based approval flow, `--dry-run` and `--yes`.
- Resource limits and a timeout.
- Commands: `run`/shorthand, `shell`, `ps`, `stop`, `policy`, `report`, `login`, `doctor`, `clean`.
- The post-run report.

**Out (deferred):** see §10.

### 9. Milestones

| # | Milestone | Exit criteria |
|---|---|---|
| 0 | **Validate** | The competitor matrix (§2) is filled in from hands-on trials of Docker Sandboxes, container-use and srt. A written go / pivot decision. A name check. An Apple `container` spike on macOS 26 answers three things: whether an internal network with egress only through the proxy works, bind-mount I/O on a `node_modules`-heavy build, and whether a warm start takes under 3 s. |
| 1 | **Walking skeleton** | `sandbox run -- bash` works on macOS (`apple-container`) and Linux (`docker`), behind one driver trait: project mounted, UID mapping, TTY and exit code pass through, the box is removed afterwards. |
| 2 | **Agents** | Profiles and a layered image build. `sandbox claude` and `sandbox codex` work end to end, with persistent state and the key passed by env (temporarily). |
| 3 | **Network and broker** | Internal network plus the proxy, the allowlist, and the egress log. The key moves to the keychain and broker, and a test proves it is absent from the box's env and filesystem. |
| 4 | **Manifest** | The policy, request and approval model; the launch prompt; protects and masks; resource limits; `--dry-run`. |
| 5 | **Report and polish** | The post-run report, `doctor`, `clean`, artifact volumes, `devcontainer.json` toolchains, docs. |
| 6 | **Review mode** | The worktree-based `--review` flow with keep or discard. |

**Security tests** are a first-class deliverable from Milestone 3 on. Each is an adversarial script run as "the agent", and each asserts the attempt fails:
- read `~/.ssh`;
- `curl` a non-allowlisted host;
- write `.git/hooks/pre-commit`;
- read `.env`;
- find the API key in env, `/proc`, or files;
- fork bomb;
- reach the host's localhost without `host_ports`.

### 10. Deferred / stretch

- **Backends:** Windows (WSL2 + Docker), Firecracker, gVisor or Kata runtime options.
- **Native OS-sandbox backend** (Seatbelt / Landlock / bubblewrap, as in srt). It keeps the host toolchain but gives weaker isolation; it could be the "light" tier.
- **Scoped short-lived GitHub tokens** through the broker, and a host-side "push after review" command.
- **Full session replay** (TTY recording plus the egress log).
- **Snapshots**, `sandbox top` TUI, GUI, VS Code extension.
- **Export/import** of a reproducible box spec. This is mostly free, since it is the manifest plus the image digest.
- **Org-managed policies**: a company-wide `policy.toml` distributed by MDM.

### 11. Open questions

1. **Build, pivot or layer?** If Docker Sandboxes already covers most of this, is Sandbox better as the manifest, policy and report layer running *on* existing sandboxes? This is decided in M0.
2. **Containers or OS sandbox as the default on macOS?**
   - Containers give strong isolation, but they need a Linux toolchain in the image and bind-mount I/O is slower.
   - Seatbelt keeps the host toolchain and adds no I/O cost, but `sandbox-exec` is deprecated and the boundary is weaker.
   - *Recommendation:* containers are the default, through Apple `container` where available. The OS sandbox is a later "light" backend.
3. **Can Apple `container` enforce egress only through the proxy?**
   - Options to test on macOS 26:
     - an internal network with no route out, plus the proxy as a second container on it;
     - a host-only network with the proxy on the host;
     - firewall rules inside the box's VM, as a last resort, because root in the box could undo them.
   - If none works cleanly, the fallback is `docker` on macOS for `allowlist` mode, and `apple-container` only for `none` and `open`.
   - Also check which resource limits it supports (pids and disk in particular) and how to map the host UID/GID.
4. **TLS interception for header injection.** The box must trust a Sandbox CA, and some agents pin certificates or ship their own CA bundles.
   - Alternative: the box gets a placeholder key, and the proxy swaps it for the real one on the way out.
   - Either way the proxy must read the request. Test each agent early (M3).
5. **Subscription or OAuth logins** (a Claude Pro/Max login, ChatGPT sign-in for Codex). Can the broker hold those refresh tokens, or do we allow an "env" mode with a clear warning?
6. **Network mode for web-research agents.** Agents that fetch docs or search the web need broad egress. Should there be a read-only "open with log" mode, and how is it labeled so users don't treat it as safe?
7. **Default workspace mode:** `rw` (lower friction) or `review` (safer)? *Recommendation:* `rw` with protects in the MVP; revisit once review mode exists.
8. **Distribution name.** The crate is `agent-sandbox-cli` and the binary is `sandbox`. Check that `sandbox` doesn't clash in Homebrew or with other tools on users' `PATH`.

### 12. Success metrics

- **Time to first run:** under 60 s from install to an agent prompt with a pulled image; under 3 s warm.
- **Security:** 100% of the adversarial test suite fails as intended, on every supported backend, in CI.
- **Friction:** fewer than 1 approval prompt per project per week in normal use. Measure this by dogfooding.
- **Compatibility:** the three MVP agents pass a scripted "make a change, run the tests" task inside the box on macOS and Linux.
- **Adoption signal:** at least 5 external users running it daily without asking for `--net open` as their default.
