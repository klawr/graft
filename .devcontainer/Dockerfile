# Bookworm ships an older glibc than a typical host — which is the whole reason
# graft exists, so this devcontainer doubles as a realistic target to dogfood it.
FROM rust:1-bookworm

RUN rustup component add clippy rustfmt

WORKDIR /workspace
