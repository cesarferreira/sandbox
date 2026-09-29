<div align="center">
  <h1>sandbox</h1>

  <p><strong>Run AI coding agents in isolated, disposable sandboxes</strong></p>

  <p>
    <img alt="License" src="https://img.shields.io/badge/license-MIT-green">
    <img alt="Rust" src="https://img.shields.io/badge/rust-1.85%2B-orange">
    <img alt="Edition" src="https://img.shields.io/badge/edition-2024-blue">
  </p>

  <p>
    <a href="#install">Install</a>
    &nbsp;·&nbsp;
    <a href="#quickstart">Quickstart</a>
    &nbsp;·&nbsp;
    <a href="#development">Development</a>
  </p>
</div>

---

## Install

Requires [Rust](https://rustup.rs) **1.85+** and `~/.cargo/bin` on your `PATH`.

```bash
cargo install agent-sandbox-cli   # installs the `sandbox` binary
# or, before the first crates.io release:
cargo install --git https://github.com/cesarferreira/sandbox.git
```

Verify:

```bash
sandbox --help
```

<details>
<summary><strong>Build from source</strong> — for development or unreleased changes</summary>

```bash
git clone https://github.com/cesarferreira/sandbox.git
cd sandbox
cargo install --path . --locked
# or
make install-release
```

Debug install (faster compile, larger binary):

```bash
make install
```

Run without installing:

```bash
make build-release
./target/release/sandbox
```

</details>

<a id="quickstart"></a>
## Quickstart

```bash
cd my-project
sandbox doctor                          # which backend will be used, and why
sandbox run -- bash -lc 'ls; id'        # any command, in a fresh box
sandbox --net none make test            # shorthand: everything after the command goes to it
sandbox shell                           # interactive shell in a fresh box
sandbox ps                              # running boxes
sandbox stop                            # stop this project's box (or: stop <name> / --all)
```

The first run builds the base image (Debian with `git`, `rg`, `fd`, `tree`, `jq`, `gh`, `curl`, `python3`, `build-essential` and friends), which takes about a minute. Later runs reuse it.

## Toolchains

Sandbox looks at the project and adds the toolchain it needs on top of the base image:

| Kit | Detected from | Installs | Cached between runs |
|---|---|---|---|
| `rust` | `Cargo.toml` | `rustup`, the toolchain from `rust-toolchain.toml` (else stable), `clippy`, `rustfmt`, pinned components and targets, and `cargo-nextest` (pinned, checksum-verified) | cargo registry and git checkouts, `target/` |
| `node` | `package.json` | Node from nodejs.org (checksum-verified): the version from `.nvmrc`, `.node-version`, `.tool-versions` or `engines.node`, else the current LTS; `corepack` for pnpm and yarn | npm, pnpm, yarn and corepack caches, `node_modules/` |
| `android` | a build, settings or version-catalog file (root or one level down) that mentions the Android Gradle plugin | JDK 17, Android command-line tools (checksum-pinned, with `sdkmanager` pointed at the cache), the x86_64 libc that Google's `aapt2` needs on arm64. AGP downloads platforms and build-tools itself. | the Android SDK, `~/.gradle`, Gradle build output, `.gradle/` |

The combined image is built once and tagged by its contents, so later runs start instantly and changing `rust-toolchain.toml` rebuilds it. Caches live on your machine under `~/.cache/sandbox/projects/<project>/`. The box's `target/` and `node_modules/` are kept there too, separate from yours, so Linux builds and native modules never overwrite your macOS ones. A project with both `Cargo.toml` and `package.json` gets both kits.

```bash
sandbox run -- cargo nextest run         # in a Rust project: rust kit detected
sandbox run -- npm test                  # in a Node project: node kit, pinned version
sandbox run -- ./gradlew assembleDebug   # in an Android project: android kit
sandbox --kit rust --kit node shell      # force kits when detection misses them
sandbox --kit none shell                 # base image only
sandbox --image rust:1 run -- cargo test # your own image; kits are skipped
```

**Android notes:**
- **Licences.** SDK licences you already accepted on this machine (Android Studio, or `sdkmanager --licenses`) are reused. Sandbox never accepts them for you. If there are none, run `sandbox --kit android run -- sdkmanager --licenses` once to read and accept them.
- **No clash with Android Studio.** The box's build output goes to the cache, not the project's `build/` dirs, so box builds and Android Studio builds don't keep invalidating each other. The APK ends up under `~/.cache/sandbox/projects/<project>/gradle-build/app/build/outputs/`.
- **`local.properties`.** If `sdk.dir` points at your machine's SDK, the box's own SDK is shown at that same path, so AGP finds it. Your real SDK is never mounted.
- **Apple silicon.** Google ships `aapt2` and `platform-tools` for x86_64 Linux only, so the box runs them through Rosetta (Apple `container`) or Docker Desktop's emulation. Everything else, including the JDK and Gradle, runs natively.
- **Disk.** Expect roughly 1 GB of SDK and Gradle caches per project.

More kits (Bazel, Go, Python) are on the roadmap in [plan.md](plan.md).

## Examples

**Poke around safely.** Open a shell in a throwaway copy of your environment:

```bash
sandbox shell
(sandbox)you@box:/workspace$ rg TODO
(sandbox)you@box:/workspace$ cat ~/.ssh/id_ed25519   # No such file or directory
```

**Run a coding agent** with full autonomy, inside the box:

```bash
sandbox claude --dangerously-skip-permissions
sandbox codex --yolo
sandbox gemini
```

The agent is installed into the image on first use, on top of the project's toolchain kits. It's the latest npm release, re-checked at most once a day, so agents stay current without slowing every run. The agent's own key (`ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`, `GEMINI_API_KEY` / `GOOGLE_API_KEY`) is passed through when it's set on your machine. Other secrets still need `-e`. If you log in inside the box instead, the login is kept in that project's home (see below).

| Agent | Package | Needs |
|---|---|---|
| `claude` | `@anthropic-ai/claude-code` | Node 22+ |
| `codex` | `@openai/codex` | Node 18+ |
| `gemini` | `@google/gemini-cli` | Node 20+ |

If the project pins an older Node (say `.nvmrc` = 20), `sandbox claude` stops with a clear message instead of failing inside the box.

**Use `gh` with your login**: read issues, check CI, review PRs from inside the box:

```bash
sandbox --gh run -- gh pr checks 42
sandbox --gh run -- gh issue view 17 --comments
sandbox --gh shell                       # gh is logged in for the whole session
```

`--gh` passes your token (from `GH_TOKEN`, `GITHUB_TOKEN` or `gh auth token`) as `GH_TOKEN`. Anything in the box can read it, so use it when the task needs GitHub. `.git` stays read-only, so pushing still happens on your machine after you review.

**Cut the network** for code you don't trust, like a fresh clone or an unknown `postinstall`:

```bash
sandbox --net none run -- make test
sandbox --net none shell
```

**Try a dev server** without exposing it beyond your machine:

```bash
sandbox -p 3000 run -- python3 -m http.server 3000   # http://127.0.0.1:3000
```

**Give read-only access to one extra folder:**

```bash
sandbox --mount ~/Downloads/dataset run -- python3 analyze.py ~/Downloads/dataset
sandbox --mount ~/notes:rw shell         # writable only because of :rw
```

**Work in a git worktree.** The main repo's `.git` is mounted read-only automatically:

```bash
git worktree add ../feature-x && cd ../feature-x
sandbox shell
```

**Look inside a running box**, or clean up:

```bash
sandbox ps
sandbox exec sandbox-myproj-1a2b3c -- git status
sandbox stop --all
```

**See exactly what would run**, without running it:

```bash
sandbox --dry-run --net none -p 8080 shell
```

## What the box can and can't do

Every run gets a new box that is removed on exit:

- **Only the project is visible.** The git top level is mounted read-write at `/workspace`, except `.git`, which is read-only: hooks and git config run on your machine, so the box must not be able to change them. For a git worktree, the main repo's `.git` is also mounted read-only. Nothing else from your machine exists in the box.
- **Extra mounts are read-only.** `--mount ~/Downloads` is read-only; add `:rw` to make it writable.
- **Ports stay local.** `-p 3000` publishes on `127.0.0.1:3000`. Pass an IP (`-p 0.0.0.0:3000:3000`) to expose it further.
- **No host environment by default.** Only `TERM` (normalized to one the image knows), `COLORTERM` and `TZ` are passed in, plus `LANG=C.UTF-8`. Anything else, including API keys, needs an explicit `-e`.
- **A home per project.** `~` in the box is `/home/<you>`, kept under `~/.cache/sandbox/projects/<project>/home`. Agent logins, settings, history and dotfiles survive between runs, but one project's home is never visible from another project's box.
- **You are you.** The box runs as your UID/GID with your username (so `whoami`, prompts and file ownership match your Mac), and `sandbox shell` prompts are prefixed with `(sandbox)`. Images need `sh` for this; the entry is added just before your command starts.
- **Exit codes pass through.** Sandbox's own errors use `125`.
- **Backend choice.** On Apple silicon with macOS 26, Sandbox uses Apple [`container`](https://github.com/apple/container), which gives one VM per box. Otherwise it falls back to Docker, Podman or nerdctl, and prints a warning that isolation is weaker. Set `SANDBOX_BACKEND=docker` (or any other backend) to change the default; `--backend` still wins.
- **VPNs and Apple `container`.** VPN clients often break the virtual network that Apple `container` boxes use. So on Apple `container`, a box's traffic goes through a small proxy that Sandbox runs on your machine. The box reaches it over the VM's private channel rather than the network, and the proxy's own connections go through your VPN like any other app's. `HTTP(S)_PROXY` are set in the box, so `cargo`, `npm`, `git`, `curl`, `gh`, `pip`, `apt` and the agents all work with the VPN on. The summary line shows `net open (via host proxy)`. Image builds work too: when Apple's builder has no network, Sandbox uses Docker if it's running, and otherwise runs the build steps in a box through the same proxy. Only tools that ignore proxy settings still need the VM network; `sandbox doctor` shows its state.
- **VPN TLS inspection.** Some VPNs (Cloudflare WARP Gateway, Zscaler, …) re-sign HTTPS traffic with a corporate CA that your Mac trusts. Sandbox adds the CA certificates from the macOS System keychain to each box's trust store, so TLS works in the box too (`NODE_EXTRA_CA_CERTS` is set for Node). Box builds use them while building but don't keep them in the image.
- **Network.** `--net none` cuts all networking. The default is currently `open`; the `allowlist` mode and the credential broker are still to come.

Options: `--image`, `--net none|open`, `--mount PATH[:rw]`, `-p/--publish`, `-e/--env`, `--gh`, `--kit`, `--cpus`, `--memory`, `--backend` (or `SANDBOX_BACKEND`), `--dry-run`. See [plan.md](plan.md) for the roadmap.

<a id="development"></a>
## Development

Common tasks via the `Makefile`:

```bash
make              # check + build + test
make build        # debug build
make build-release
make install      # install debug binary
make install-release
make run ARGS="doctor"
make check        # cargo check + clippy
make fmt          # format
make lint         # fmt check + clippy
make test
make clean
make demo         # install + show --help
```

Releasing (requires [cargo-release](https://github.com/crate-ci/cargo-release) and [git-cliff](https://github.com/orhun/git-cliff)):

```bash
make release                  # default minor bump
make release LEVEL=patch      # patch bump
make release LEVEL=major      # major bump
```

The pre-release hook regenerates `CHANGELOG.md` with `git-cliff` from your conventional-commit history (grouped into Features, Bug Fixes, etc. per `cliff.toml`) and commits it alongside the version bump. Pushing the resulting `v*` tag triggers the release workflow, which builds the multi-platform binaries and publishes a GitHub Release whose notes are generated by `git-cliff` from the same config.

## License

MIT
