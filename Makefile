PREFIX ?= /usr/local/bin
BINDIR  = $(PREFIX)

.PHONY: all build install uninstall clean dev test lint fmt check

all: build

build:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt

# What CI runs.
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

install: build
	install -Dm755 target/release/graft $(DESTDIR)$(BINDIR)/graft

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/graft

clean:
	cargo clean

dev:
	@./dev.sh
