# Bookworm ships an older glibc than a typical host — which is the whole reason
# graft exists, so this devcontainer doubles as a realistic target to dogfood it.
FROM rust:1-bookworm

# The tools graft shells out to at runtime (so you can smoke-test it here), plus
# git for the usual workflow. Docker itself isn't installed: drive graft from the
# host, not from inside this container.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      tmux patchelf openssh-client ca-certificates git \
 && rm -rf /var/lib/apt/lists/*

# clippy + rustfmt for the build/lint/format loop.
RUN rustup component add clippy rustfmt

WORKDIR /workspace
