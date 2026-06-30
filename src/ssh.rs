use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::Command;

/// Splits a `--remote` destination into the ssh destination and an optional
/// port. Accepted forms: a `~/.ssh/config` Host alias, `user@host`,
/// `user@host:port`, `[v6addr]:port`, and any of those with an `ssh://`
/// prefix. A bare IPv6 address (more than one `:` and no brackets) is left
/// untouched rather than misread as host:port.
pub fn split(remote: &str) -> (String, Option<u16>) {
    let remote = remote.strip_prefix("ssh://").unwrap_or(remote);
    let host_start = remote.rfind('@').map(|i| i + 1).unwrap_or(0);
    let (user, host) = remote.split_at(host_start);

    // Bracketed IPv6: [addr] or [addr]:port. ssh wants the bare address.
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let addr = &rest[..end];
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse().ok());
            return (format!("{user}{addr}"), port);
        }
        return (remote.to_string(), None);
    }

    if let Some((h, p)) = host.rsplit_once(':')
        && !h.is_empty()
        && !h.contains(':')
        && let Ok(port) = p.parse::<u16>()
    {
        return (format!("{user}{h}"), Some(port));
    }
    (remote.to_string(), None)
}

/// Socket used to multiplex every ssh invocation against the same `--remote`
/// destination over one already-authenticated connection (`ControlMaster`),
/// keyed by the raw `remote` string so distinct hosts/ports get distinct
/// sockets. A `graft up` issues dozens of separate `ssh` calls (one per
/// `docker exec`/`cp`); without multiplexing each renegotiates its own TCP +
/// auth handshake, which is slow and, on a flaky or non-default-port hop,
/// occasionally just fails — silently degrading some operations (e.g.
/// chown'ing injected config) rather than the whole command.
fn control_path(remote: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    remote.hash(&mut hasher);
    let dir = std::env::temp_dir().join("graft-ssh");
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir);
    dir.join(format!("{:x}.sock", hasher.finish()))
}

/// Builds an `ssh` command for the remote with the port (from a `host:port`
/// destination), connection multiplexing, and `--verbose` flags applied, and
/// returns the destination separately — ssh options must precede the
/// destination, so callers add their own options first, then the
/// destination, then the remote command.
pub fn base_command(remote: &str) -> (Command, String) {
    let (dest, port) = split(remote);
    let mut c = Command::new("ssh");
    if let Some(p) = port {
        c.args(["-p", &p.to_string()]);
    }
    c.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=10m"]);
    c.arg("-o")
        .arg(format!("ControlPath={}", control_path(remote).display()));
    let v = crate::verbose::level().min(3);
    if v > 0 {
        c.arg(format!("-{}", "v".repeat(v as usize)));
    }
    (c, dest)
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn plain_alias_and_user_host_pass_through() {
        assert_eq!(split("myhost"), ("myhost".into(), None));
        assert_eq!(split("user@host"), ("user@host".into(), None));
    }

    #[test]
    fn trailing_port_is_extracted() {
        assert_eq!(split("host:2222"), ("host".into(), Some(2222)));
        assert_eq!(split("user@host:2222"), ("user@host".into(), Some(2222)));
    }

    #[test]
    fn ssh_url_prefix_is_stripped() {
        assert_eq!(
            split("ssh://user@host:2222"),
            ("user@host".into(), Some(2222))
        );
        assert_eq!(split("ssh://host"), ("host".into(), None));
    }

    #[test]
    fn bare_ipv6_is_not_misread_as_port() {
        assert_eq!(split("fe80::1"), ("fe80::1".into(), None));
        assert_eq!(split("user@fe80::1"), ("user@fe80::1".into(), None));
    }

    #[test]
    fn bracketed_ipv6_with_port() {
        assert_eq!(split("[fe80::1]:2222"), ("fe80::1".into(), Some(2222)));
        assert_eq!(
            split("user@[fe80::1]:2222"),
            ("user@fe80::1".into(), Some(2222))
        );
        assert_eq!(split("[fe80::1]"), ("fe80::1".into(), None));
    }

    #[test]
    fn non_numeric_suffix_is_not_a_port() {
        assert_eq!(split("host:notaport"), ("host:notaport".into(), None));
    }
}
