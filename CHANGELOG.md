# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/klawr/graft/compare/v0.2.0...v0.2.1) - 2026-06-13

### Other

- stop useless PRs for release

## [0.2.0](https://github.com/klawr/graft/compare/v0.1.1...v0.2.0) - 2026-06-13

### Added

- Update readme and cli docs, add remote flags, update port forwarding
- properly expand ~ in path
- replace socat with built-in TCP proxy for port forwarding
- [**breaking**] initial release of graft

### Fixed

- use proper tmux session names
- Remote docker commands
- typo
- formatting
- restore Docker ENV PATH for login shells
- resolve symlinks and use container home for inject targets

### Other

- release v0.1.1
- fix release-plz stage
- add semantic versioning
- add release workflow and fix install script
- improve --help text for all commands

## [0.1.1](https://github.com/klawr/graft/compare/v0.1.0...v0.1.1) - 2026-06-13

### Fixed

- use proper tmux session names
