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
| `rust` | `Cargo.toml` | `rustup`, the toolchain from `rust-toolchain.toml` (else stable), `clippy`, `rustfmt`, pinned components and targets | cargo registry and git checkouts, `target/` |
| `node` | `package.json` | Node from nodejs.org (checksum-verified): the version from `.nvmrc`, `.node-version`, `.tool-versions` or `engines.node`, else the current LTS; `corepack` for pnpm and yarn | npm, pnpm, yarn and corepack caches, `node_modules/` |

The combined image is built once and tagged by its contents, so later runs start instantly and changing `rust-toolchain.toml` rebuilds it. Caches live on your machine under `~/.cache/sandbox/projects/<project>/`. The box's `target/` and `node_modules/` are kept there too, separate from yours, so Linux builds and native modules never overwrite your macOS ones. A project with both `Cargo.toml` and `package.json` gets both kits.

```bash
sandbox run -- cargo test                # in a Rust project: rust kit detected
sandbox run -- npm test                  # in a Node project: node kit, pinned version
sandbox --kit rust --kit node shell      # force kits when detection misses them
sandbox --kit none shell                 # base image only
sandbox --image rust:1 run -- cargo test # your own image; kits are skipped
```

More kits (Android/Gradle, Bazel, Go, Python) are on the roadmap in [plan.md](plan.md).

## Examples

**Poke around safely.** Open a shell in a throwaway copy of your environment:

```bash
sandbox shell
(sandbox)you@box:/workspace$ rg TODO
(sandbox)you@box:/workspace$ cat ~/.ssh/id_ed25519   # No such file or directory
```

**Run an agent unattended**, with its API key passed in explicitly:

```bash
sandbox -e ANTHROPIC_API_KEY run --image my-claude-image -- claude --dangerously-skip-permissions
sandbox -e OPENAI_API_KEY codex --yolo
```

(Agent images come with milestone 2. Until then, use an image that has the agent installed.)

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
- **You are you.** The box runs as your UID/GID with your username (so `whoami`, prompts and file ownership match your Mac), and `sandbox shell` prompts are prefixed with `(sandbox)`. Images need `sh` for this; the entry is added just before your command starts.
- **Exit codes pass through.** Sandbox's own errors use `125`.
- **Backend choice.** On Apple silicon with macOS 26, Sandbox uses Apple [`container`](https://github.com/apple/container), which gives one VM per box. Otherwise it falls back to Docker, Podman or nerdctl, and prints a warning that isolation is weaker.
- **Network.** `--net none` cuts all networking. The default is currently `open`; the `allowlist` mode and the credential broker are still to come.

Options: `--image`, `--net none|open`, `--mount PATH[:rw]`, `-p/--publish`, `-e/--env`, `--gh`, `--kit`, `--cpus`, `--memory`, `--backend`, `--dry-run`. See [plan.md](plan.md) for the roadmap.

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
