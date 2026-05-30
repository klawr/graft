use std::collections::{HashMap, HashSet};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Spawns a detached `graft _forward` daemon that manages port forwarding for
/// `container`. Returns immediately; the daemon outlives the calling process.
/// Daemon stderr goes to a log file; the path is printed so the user can tail it.
pub fn spawn_daemon(container: &str, remote: &Option<String>, static_ports: &[u16]) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    let log = log_path(container);
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let log_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[graft] could not open forward log {}: {e}", log.display());
            return;
        }
    };

    println!("[graft] port forwarding log: {}", log.display());

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
        .stderr(log_file)
        .spawn();
}

/// Port-forwarding daemon. Polls `/proc/net/tcp` and `/proc/net/tcp6` every 2 s
/// and maintains one forwarder per listening port. For local Docker, a built-in
/// Rust TCP proxy is used; for `--remote`, an `ssh -L` tunnel. Exits when the
/// container stops.
pub fn run_daemon(container: &str, remote: &Option<String>, _static_ports: &[u16]) {
    let container_ip = match get_container_ip(container, remote) {
        Some(ip) => ip,
        None => {
            eprintln!("[graft forward] could not determine container IP — port forwarding disabled");
            return;
        }
    };
    eprintln!("[graft forward] container IP: {container_ip}");

    let mut active: HashMap<u16, Forwarder> = HashMap::new();

    loop {
        if !is_container_running(container, remote) {
            eprintln!("[graft forward] container stopped, exiting");
            break;
        }

        let listening = poll_listening_ports(container, remote);

        // Drop forwarders for ports that stopped listening or whose process died.
        active.retain(|&port, fwd| {
            let alive = listening.contains(&port) && fwd.is_alive();
            if !alive {
                eprintln!("[graft forward] stopped forwarding port {port}");
            }
            alive
        });

        // Start forwarders for newly-listening ports.
        for &port in &listening {
            if active.contains_key(&port) {
                continue;
            }
            let fwd = if let Some(r) = remote {
                eprintln!("[graft forward] port {port} — ssh -L {port}:{container_ip}:{port} {r}");
                Forwarder::ssh_tunnel(port, &container_ip, r)
            } else {
                eprintln!("[graft forward] port {port} — proxy 0.0.0.0:{port} → {container_ip}:{port}");
                Forwarder::proxy(port, container_ip.clone())
            };
            match fwd {
                Some(f) => {
                    eprintln!("[graft forward] forwarding port {port}");
                    active.insert(port, f);
                }
                None => {
                    eprintln!("[graft forward] could not forward port {port}");
                }
            }
        }

        std::thread::sleep(Duration::from_secs(2));
    }
}

// ── Forwarder ─────────────────────────────────────────────────────────────────

enum Forwarder {
    Proxy(ProxyHandle),
    Ssh(Child),
}

impl Forwarder {
    fn proxy(port: u16, container_ip: String) -> Option<Self> {
        ProxyHandle::spawn(port, container_ip).map(Forwarder::Proxy)
    }

    fn ssh_tunnel(port: u16, container_ip: &str, remote: &str) -> Option<Self> {
        Command::new("ssh")
            .args([
                "-N",
                "-o", "BatchMode=yes",
                "-o", "ExitOnForwardFailure=yes",
                "-L", &format!("{port}:{container_ip}:{port}"),
                remote,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .map(Forwarder::Ssh)
    }

    fn is_alive(&mut self) -> bool {
        match self {
            Forwarder::Proxy(h) => !h.stop.load(Ordering::Relaxed),
            Forwarder::Ssh(child) => child.try_wait().map(|s| s.is_none()).unwrap_or(false),
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        match self {
            Forwarder::Proxy(h) => h.stop.store(true, Ordering::Relaxed),
            Forwarder::Ssh(child) => { let _ = child.kill(); }
        }
    }
}

// ── Built-in TCP proxy ────────────────────────────────────────────────────────

struct ProxyHandle {
    stop: Arc<AtomicBool>,
}

impl ProxyHandle {
    fn spawn(port: u16, container_ip: String) -> Option<Self> {
        let listener = TcpListener::bind(format!("0.0.0.0:{port}")).ok()?;
        listener.set_nonblocking(true).ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        std::thread::spawn(move || {
            loop {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((client, _)) => {
                        let target = format!("{container_ip}:{port}");
                        std::thread::spawn(move || {
                            if let Ok(server) = TcpStream::connect(&target) {
                                proxy_conn(client, server);
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break,
                }
            }
        });

        Some(ProxyHandle { stop })
    }
}

// Bidirectional copy between two TCP streams. Runs client→server in a spawned
// thread and server→client in the current thread; each half shuts down its write
// end on EOF so the peer sees a clean close.
fn proxy_conn(client: TcpStream, server: TcpStream) {
    let mut c_r = client;
    let mut s_r = server;
    let mut s_w = match s_r.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut c_w = match c_r.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };

    let t = std::thread::spawn(move || {
        std::io::copy(&mut c_r, &mut s_w).ok();
        s_w.shutdown(std::net::Shutdown::Write).ok();
    });

    std::io::copy(&mut s_r, &mut c_w).ok();
    c_w.shutdown(std::net::Shutdown::Write).ok();
    t.join().ok();
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn log_path(container: &str) -> PathBuf {
    let short = container.get(..12).unwrap_or(container);
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share"))
        .join("graft")
        .join(format!("forward-{short}.log"))
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
