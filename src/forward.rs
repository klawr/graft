use std::collections::{HashMap, HashSet};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Spawns a detached `graft _forward` daemon that manages port forwarding for
/// `container`. Returns immediately; the daemon outlives the calling process.
pub fn spawn_daemon(container: &str, remote: &Option<String>, static_ports: &[u16]) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut cmd = Command::new(exe);
    if let Some(r) = remote {
        cmd.args(["--remote", r]);
    }
    cmd.arg("_forward").arg(container);
    for p in static_ports {
        cmd.args(["--port", &p.to_string()]);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let _ = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Port-forwarding daemon. Polls `/proc/net/tcp` and `/proc/net/tcp6` every 2 s
/// and maintains one socat (local) or `ssh -L` (remote) child process per
/// listening port. Exits when the container stops.
pub fn run_daemon(container: &str, remote: &Option<String>, _static_ports: &[u16]) {
    let container_ip = match get_container_ip(container, remote) {
        Some(ip) => ip,
        None => return,
    };

    // Track active forwarder children (port → child).
    let mut active: HashMap<u16, Child> = HashMap::new();
    // Ports where spawn returned None (binary not found); don't retry these.
    let mut unspawnable: HashSet<u16> = HashSet::new();

    loop {
        if !is_container_running(container, remote) {
            break;
        }

        let listening = poll_listening_ports(container, remote);

        // Kill forwarders for ports that stopped listening or whose process died.
        active.retain(|&port, child| {
            let alive = listening.contains(&port)
                && child.try_wait().map(|s| s.is_none()).unwrap_or(false);
            if !alive {
                let _ = child.kill();
            }
            alive
        });

        // Spawn forwarders for newly-listening ports.
        for &port in &listening {
            if active.contains_key(&port) || unspawnable.contains(&port) {
                continue;
            }
            match if let Some(r) = remote {
                spawn_ssh_tunnel(port, &container_ip, r)
            } else {
                spawn_socat(port, &container_ip)
            } {
                Some(child) => {
                    active.insert(port, child);
                }
                None => {
                    unspawnable.insert(port);
                }
            }
        }

        std::thread::sleep(Duration::from_secs(2));
    }

    for (_, mut child) in active {
        let _ = child.kill();
    }
}

fn poll_listening_ports(container: &str, remote: &Option<String>) -> HashSet<u16> {
    let mut cmd = Command::new("docker");
    crate::docker::docker_host_env(&mut cmd, remote);
    cmd.args([
        "exec",
        container,
        "sh",
        "-c",
        "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null",
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.output()
        .ok()
        .map(|o| parse_proc_net_tcp(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

// /proc/net/tcp format (one header line, then one connection per line):
//   sl  local_address rem_address st ...
//   0:  00000000:1F90 00000000:0000 0A ...
// The port is the hex value after the colon in local_address; state 0A = LISTEN.
// tcp6 uses a 32-char hex address but the same port encoding.
fn parse_proc_net_tcp(data: &str) -> HashSet<u16> {
    let mut ports = HashSet::new();
    for line in data.lines() {
        let mut cols = line.split_whitespace();
        cols.next(); // sl / line index
        let local = match cols.next() {
            Some(v) => v,
            None => continue,
        };
        cols.next(); // rem_address
        let state = match cols.next() {
            Some(v) => v,
            None => continue,
        };
        if state != "0A" {
            continue;
        }
        if let Some(hex) = local.split(':').last() {
            if let Ok(port) = u16::from_str_radix(hex, 16) {
                if port > 0 {
                    ports.insert(port);
                }
            }
        }
    }
    ports
}

fn get_container_ip(container: &str, remote: &Option<String>) -> Option<String> {
    let mut cmd = Command::new("docker");
    crate::docker::docker_host_env(&mut cmd, remote);
    // Emit each network's IP on its own line; take the first non-empty one.
    cmd.args([
        "inspect",
        "--format",
        "{{range .NetworkSettings.Networks}}{{.IPAddress}}\n{{end}}",
        container,
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.output().ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from)
    })
}

fn is_container_running(container: &str, remote: &Option<String>) -> bool {
    let mut cmd = Command::new("docker");
    crate::docker::docker_host_env(&mut cmd, remote);
    cmd.args(["inspect", "--format", "{{.State.Running}}", container]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

fn spawn_socat(port: u16, container_ip: &str) -> Option<Child> {
    Command::new("socat")
        .args([
            &format!("TCP-LISTEN:{port},fork,reuseaddr"),
            &format!("TCP:{container_ip}:{port}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

fn spawn_ssh_tunnel(port: u16, container_ip: &str, remote: &str) -> Option<Child> {
    Command::new("ssh")
        .args([
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            &format!("{port}:{container_ip}:{port}"),
            remote,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_listen_ports() {
        let data = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0
   1: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0
   2: 0F02000A:C4E2 0202000A:1F90 01 00000000:00000000 00:00000000 00000000  1000
";
        let ports = parse_proc_net_tcp(data);
        assert!(ports.contains(&8080), "0x1F90 = 8080");
        assert!(ports.contains(&80), "0x0050 = 80");
        assert!(!ports.contains(&50402), "ESTABLISHED, not LISTEN");
    }

    #[test]
    fn parses_ipv6_listen_ports() {
        let data = "\
  sl  local_address                         remote_address                        st
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A
";
        let ports = parse_proc_net_tcp(data);
        assert!(ports.contains(&8080));
    }

    #[test]
    fn ignores_port_zero() {
        let data = "  sl local rem st\n   0: 00000000:0000 00000000:0000 0A\n";
        assert!(parse_proc_net_tcp(data).is_empty());
    }
}
