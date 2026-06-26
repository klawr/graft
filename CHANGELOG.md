# Changelog

All notable changes to this project will be documented in this file.
## [0.4.0] - 2026-06-26

### Added
- Add version cli flag

### Fixed
- Select workspace if devcontainer config is incorrect
- Formatting
## [0.3.2] - 2026-06-17

### Fixed
- Use ssh -tt to enforce tty
## [0.3.1] - 2026-06-15

### Fixed
- Permission on unprivileged images
## [0.3.0] - 2026-06-15

### Added
- Remove docker cli dependency for remote usage
- Use git-cliff instead of release-plz

### Fixed
- Quality
- Ci
## [0.2.2] - 2026-06-15

### Fixed
- Release ci
## [0.1.1] - 2026-06-13

### Fixed
- Use proper tmux session names
## [0.1.0] - 2026-06-12

### Added
- Initial release of graft
- Replace socat with built-in TCP proxy for port forwarding
- Properly expand ~ in path
- Update readme and cli docs, add remote flags, update port forwarding

### Documentation
- Improve --help text for all commands

### Fixed
- Resolve symlinks and use container home for inject targets
- Restore Docker ENV PATH for login shells
- Formatting
- Typo
- Remote docker commands

