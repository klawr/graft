use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};

// Process-global verbosity level, set once from the parsed CLI. A global
// (rather than threading a level through every function) keeps the many
// command-building call sites untouched.
static LEVEL: AtomicU8 = AtomicU8::new(0);

pub fn set(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

pub fn enabled() -> bool {
    level() > 0
}

/// Prints the command (with any explicitly-set env vars) to stderr in
/// shell-pasteable form. No-op unless --verbose is set.
pub fn trace(cmd: &Command) {
    if !enabled() {
        return;
    }
    let mut line = String::new();
    for (k, v) in cmd.get_envs() {
        if let Some(v) = v {
            line.push_str(&format!(
                "{}={} ",
                k.to_string_lossy(),
                quote(&v.to_string_lossy())
            ));
        }
    }
    line.push_str(&cmd.get_program().to_string_lossy());
    for a in cmd.get_args() {
        line.push(' ');
        line.push_str(&quote(&a.to_string_lossy()));
    }
    eprintln!("[graft] $ {line}");
}

// Quote only when needed, so typical traces stay readable.
fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@' | ',')
        });
    if plain {
        s.to_string()
    } else {
        crate::docker::shell_quote(s)
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn plain_args_stay_unquoted() {
        assert_eq!(quote("docker"), "docker");
        assert_eq!(quote("ssh://user@host:22"), "ssh://user@host:22");
    }

    #[test]
    fn args_with_specials_are_quoted() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote(""), "''");
    }
}
