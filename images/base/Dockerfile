# The default image for `sandbox`: Debian plus the tools coding agents reach for.
# Sandbox tags it by a hash of this file, so edits here trigger a rebuild.
FROM debian:bookworm-slim

ARG DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      bash-completion build-essential ca-certificates curl diffutils fd-find file git \
      gnupg jq less make openssh-client patch procps python3 python3-venv ripgrep \
      sqlite3 tree unzip wget xz-utils zip \
 && ln -s /usr/bin/fdfind /usr/local/bin/fd \
 && install -d -m 0755 /etc/apt/keyrings \
 && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

# The box runs as the host user with HOME=/tmp; git would otherwise refuse the
# bind-mounted repo as "dubious ownership" when UIDs differ.
RUN git config --system safe.directory '*'
