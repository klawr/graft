# Bookworm ships an older glibc than a typical host — which is the whole reason
# graft exists, so this devcontainer doubles as a realistic target to dogfood it.
FROM rust:1-bookworm

RUN rustup component add clippy rustfmt

ARG UID=1000
RUN useradd -m -u $UID dev \
    && mkdir -p /workspace /usr/local/cargo \
    && chown -R dev:dev /workspace /usr/local/cargo /usr/local/rustup

USER dev
WORKDIR /workspace
