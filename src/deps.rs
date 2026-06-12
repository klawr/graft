use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

pub struct Deps {
    pub libs: Vec<PathBuf>,
    pub linker: Option<PathBuf>,
}

impl Deps {
    pub fn is_empty(&self) -> bool {
        self.libs.is_empty()
    }
}

pub fn resolve(binary: &str) -> Result<Deps> {
    let mut cmd = Command::new("ldd");
    cmd.arg(binary);
    crate::verbose::trace(&cmd);
    let output = cmd
        .output()
        .with_context(|| format!("running ldd on {binary}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.contains("statically linked") {
        return Ok(Deps {
            libs: vec![],
            linker: None,
        });
    }

    if !output.status.success() {
        bail!(
            "ldd failed on {binary}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let parsed = parse_ldd(&stdout);
    if !parsed.unresolved.is_empty() {
        eprintln!(
            "[graft] warning: {} could not be resolved on the host and won't be grafted: {}",
            if parsed.unresolved.len() == 1 {
                "a dependency"
            } else {
                "dependencies"
            },
            parsed.unresolved.join(", ")
        );
    }

    Ok(Deps {
        libs: parsed.libs,
        linker: parsed.linker,
    })
}

struct Parsed {
    libs: Vec<PathBuf>,
    linker: Option<PathBuf>,
    unresolved: Vec<String>,
}

// Parses `ldd` output into the set of libraries to copy (deduped by soname),
// the dynamic linker, and any "not found" entries. Pure, so it's unit-tested.
fn parse_ldd(stdout: &str) -> Parsed {
    let mut libs = vec![];
    let mut linker = None;
    let mut seen: HashSet<String> = HashSet::new();
    let mut unresolved = vec![];

    let mut push = |libs: &mut Vec<PathBuf>, path: &str| {
        // Dedup by filename — distinct paths to the same soname only need
        // copying once into /opt/graft/lib.
        let name = PathBuf::from(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        if let Some(name) = name
            && seen.insert(name)
        {
            libs.push(PathBuf::from(path));
        }
    };

    for line in stdout.lines() {
        let line = line.trim();

        if line.starts_with("linux-vdso") {
            continue;
        }

        // "/lib64/ld-linux-x86-64.so.2 (0x...)" — the dynamic linker
        if line.starts_with('/') {
            if let Some(path) = line.split_whitespace().next() {
                linker = Some(PathBuf::from(path));
                push(&mut libs, path);
            }
            continue;
        }

        // "libname.so => /path (0x...)"  or  "libname.so => not found"
        if let Some(pos) = line.find("=> ") {
            let rhs = line[pos + 3..].trim();
            let path = rhs.split_whitespace().next().unwrap_or("");
            if path.starts_with('/') {
                push(&mut libs, path);
            } else if rhs.starts_with("not found") {
                let name = line[..pos].trim();
                unresolved.push(name.to_string());
            }
        }
    }

    Parsed {
        libs,
        linker,
        unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_libs_and_linker() {
        let out = "\tlinux-vdso.so.1 (0x00007fff)\n\
                   \tlibc.so.6 => /usr/lib/libc.so.6 (0x00007f00)\n\
                   \t/lib64/ld-linux-x86-64.so.2 (0x00007f11)\n";
        let p = parse_ldd(out);
        assert_eq!(p.linker, Some(PathBuf::from("/lib64/ld-linux-x86-64.so.2")));
        assert!(p.libs.contains(&PathBuf::from("/usr/lib/libc.so.6")));
        assert!(
            p.libs
                .contains(&PathBuf::from("/lib64/ld-linux-x86-64.so.2"))
        );
        assert!(p.unresolved.is_empty());
    }

    #[test]
    fn reports_not_found() {
        let p = parse_ldd("\tlibfoo.so.1 => not found\n");
        assert_eq!(p.unresolved, vec!["libfoo.so.1".to_string()]);
        assert!(p.libs.is_empty());
    }

    #[test]
    fn dedups_by_soname() {
        let out = "\tlibc.so.6 => /lib/libc.so.6 (0x0)\n\
                   \tlibc.so.6 => /usr/lib/libc.so.6 (0x0)\n";
        let p = parse_ldd(out);
        let count = p.libs.iter().filter(|p| p.ends_with("libc.so.6")).count();
        assert_eq!(count, 1);
    }
}
