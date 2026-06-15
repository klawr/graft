use std::collections::{HashMap, HashSet};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// One port forward: local port `local` on the host tunnels to port `port` of
/// a target on the container network — the primary container itself when
/// `host` is None, or a named host (compose service, container name, network
/// alias) otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForward {
    pub local: u16,
    pub host: Option<String>,
    pub port: u16,
}

impl PortForward {
    /// Local port == target port on the primary container (the common case).
    pub fn same(port: u16) -> Self {
        Self {
            local: port,
            host: None,
            port,
        }
    }

    /// Serialized form for the internal `graft _forward --port` flag.
    pub fn encode(&self) -> String {
        match &self.host {
            Some(h) => format!("{}:{}:{}", self.local, h, self.port),
            None => format!("{}:{}", self.local, self.port),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.as_slice() {
            [l, p] => Some(Self {
                local: l.parse().ok()?,
                host: None,
                port: p.parse().ok()?,
            }),
            [l, h, p] => Some(Self {
                local: l.parse().ok()?,
                host: Some(h.to_string()),
                port: p.parse().ok()?,
            }),
            _ => None,
        }
    }
}

/// Spawns a detached `graft _forward` daemon that manages port forwarding for
/// `container`. Returns immediately; the daemon outlives the calling process.
/// Daemon stderr goes to a log file; the path is printed so the user can tail it.
pub fn spawn_daemon(container: &str, remote: &Option<String>, static_ports: &[PortForward]) {
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
    // Forward verbosity to the daemon so its ssh tunnels log into the log file.
    for _ in 0..crate::verbose::level() {
        cmd.arg("-v");
    }
    cmd.arg("_forward").arg(container);
    for p in static_ports {
        cmd.args(["--port", &p.encode()]);
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

/// Port-forwarding daemon. Forwards declared in `forwardPorts`
/// (`static_ports`) are set up eagerly, from startup — connections just fail
/// until the service listens. This also covers containers whose listeners
/// can't be inspected (no shell for `cat /proc/net/tcp`) and targets other
/// than the primary container (compose services). On top of that, the daemon
/// polls `/proc/net/tcp` and `/proc/net/tcp6` every 2 s and maintains one
/// forwarder per listening port. For local Docker, a built-in Rust TCP proxy
/// is used; for `--remote`, an `ssh -L` tunnel. Exits when the container stops.
pub fn run_daemon(container: &str, remote: &Option<String>, static_ports: &[PortForward]) {
    let container_ip = match get_container_ip(container, remote) {
        Some(ip) => ip,
        None => {
            eprintln!(
                "[graft forward] could not determine container IP — port forwarding disabled"
            );
            return;
        }
    };
    eprintln!("[graft forward] container IP: {container_ip}");

    let mut active: HashMap<u16, Forwarder> = HashMap::new();
    // Local ports whose forwarder failed to start, so the retry every 2 s
    // (likely failing the same way, e.g. host port occupied) doesn't spam the
    // log. Same warn-once treatment for named hosts that don't resolve (the
    // target service may simply not be up yet — keep trying quietly).
    let mut failed: HashSet<u16> = HashSet::new();
    let mut unresolved: HashSet<String> = HashSet::new();
    // Successful name → IP resolutions, so steady state needs no docker calls.
    let mut resolved: HashMap<String, String> = HashMap::new();

    loop {
        if !is_container_running(container, remote) {
            eprintln!("[graft forward] container stopped, exiting");
            break;
        }

        // Wanted forwards, keyed by local port: every listening port maps to
        // itself on the primary container; declared forwards are always wanted
        // and win a local-port collision. A forwarder that died (e.g. dropped
        // ssh tunnel) is retired below and recreated, since its local port
        // stays in the wanted set.
        let mut wanted: HashMap<u16, PortForward> = poll_listening_ports(container, remote)
            .into_iter()
            .map(|p| (p, PortForward::same(p)))
            .collect();
        for f in static_ports {
            wanted.insert(f.local, f.clone());
        }

        // Drop forwarders for ports that stopped listening or whose process died.
        active.retain(|&local, fwd| {
            let alive = wanted.contains_key(&local) && fwd.is_alive();
            if !alive {
                eprintln!("[graft forward] stopped forwarding port {local}");
            }
            alive
        });

        // Start forwarders for wanted ports that don't have one yet.
        for (&local, f) in &wanted {
            if active.contains_key(&local) {
                continue;
            }
            let target_ip = match &f.host {
                None => container_ip.clone(),
                Some(h) => match resolved.get(h) {
                    Some(ip) => ip.clone(),
                    None => match resolve_target_ip(container, remote, h) {
                        Some(ip) => {
                            eprintln!("[graft forward] resolved {h} → {ip}");
                            resolved.insert(h.clone(), ip.clone());
                            unresolved.remove(h);
                            ip
                        }
                        None => {
                            if unresolved.insert(h.clone()) {
                                eprintln!(
                                    "[graft forward] cannot resolve '{h}' on the container \
                                     network (service not up yet?) — will keep trying"
                                );
                            }
                            continue;
                        }
                    },
                },
            };
            let target = format!("{target_ip}:{}", f.port);
            let fwd = if let Some(r) = remote {
                eprintln!("[graft forward] port {local} — ssh -L {local}:{target} {r}");
                Forwarder::ssh_tunnel(local, &target, r)
            } else {
                eprintln!("[graft forward] port {local} — proxy 0.0.0.0:{local} → {target}");
                Forwarder::proxy(local, target)
            };
            match fwd {
                Some(fw) => {
                    eprintln!("[graft forward] forwarding port {local}");
                    active.insert(local, fw);
                    failed.remove(&local);
                }
                None => {
                    if failed.insert(local) {
                        eprintln!("[graft forward] could not forward port {local}");
                    }
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
    // `target` is "ip:port" as reachable from the Docker host.
    fn proxy(local: u16, target: String) -> Option<Self> {
        ProxyHandle::spawn(local, target).map(Forwarder::Proxy)
    }

    fn ssh_tunnel(local: u16, target: &str, remote: &str) -> Option<Self> {
        let (mut cmd, dest) = crate::ssh::base_command(remote);
        cmd.args([
            "-N",
            "-o",
            "BatchMode=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            &format!("{local}:{target}"),
            &dest,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // With --verbose, let ssh's own chatter land in the daemon log.
        .stderr(if crate::verbose::enabled() {
            Stdio::inherit()
        } else {
            Stdio::null()
        });
        crate::verbose::trace(&cmd);
        cmd.spawn().ok().map(Forwarder::Ssh)
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
            Forwarder::Ssh(child) => {
                let _ = child.kill();
            }
        }
    }
}

// ── Built-in TCP proxy ────────────────────────────────────────────────────────

struct ProxyHandle {
    stop: Arc<AtomicBool>,
}

impl ProxyHandle {
    // `target` is "ip:port" as reachable from the Docker host.
    fn spawn(local: u16, target: String) -> Option<Self> {
        let listener = TcpListener::bind(format!("0.0.0.0:{local}")).ok()?;
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
                        let target = target.clone();
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
    let mut cmd = crate::docker::command(
        remote,
        &[
            "exec",
            container,
            "sh",
            "-c",
            "cat /proc/net/tcp /proc/net/tcp6 2>/dev/null",
        ],
    );
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
// tcp6 uses a 32-char hex address but the same port encoding. Loopback-only
// listeners are skipped: they're unreachable via the container IP, and every
// container on a user-defined network has one (Docker's embedded DNS resolver
// on 127.0.0.11) that would otherwise be forwarded as noise.
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
        let mut parts = local.split(':');
        let (Some(addr), Some(hex)) = (parts.next(), parts.next()) else {
            continue;
        };
        if is_loopback_hex(addr) {
            continue;
        }
        if let Ok(port) = u16::from_str_radix(hex, 16)
            && port > 0
        {
            ports.insert(port);
        }
    }
    ports
}

// True if a /proc/net/tcp{,6} hex address is a loopback address. IPv4 is one
// little-endian u32 ("0100007F" = 127.0.0.1, "0B00007F" = 127.0.0.11); IPv6 is
// four such u32 groups, with ::1 being all-zero except a final "01000000".
fn is_loopback_hex(addr: &str) -> bool {
    match addr.len() {
        8 => u32::from_str_radix(addr, 16)
            .map(|v| std::net::Ipv4Addr::from(v.swap_bytes()).is_loopback())
            .unwrap_or(false),
        32 => addr.eq_ignore_ascii_case("00000000000000000000000001000000"),
        _ => false,
    }
}

fn get_container_ip(container: &str, remote: &Option<String>) -> Option<String> {
    let mut cmd = crate::docker::command(
        remote,
        &[
            "inspect",
            "--format",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}\n{{end}}",
            container,
        ],
    );
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.output().ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from)
    })
}

// Resolves a named forward target ("db" in forwardPorts: ["db:5432"]) to an IP
// reachable from the Docker host. Two strategies:
//   1. DNS inside the primary container — Docker's embedded DNS resolves
//      compose service names, container names, and network aliases on
//      user-defined networks; this is exactly what the devcontainer's own
//      processes see.
//   2. Containers on the primary's networks, matched by container name or
//      compose naming convention — covers the default bridge network (no
//      embedded DNS) and primary containers without a shell/getent.
fn resolve_target_ip(container: &str, remote: &Option<String>, host: &str) -> Option<String> {
    resolve_via_container_dns(container, remote, host)
        .or_else(|| resolve_via_networks(container, remote, host))
}

fn resolve_via_container_dns(
    container: &str,
    remote: &Option<String>,
    host: &str,
) -> Option<String> {
    let out = docker_stdout(remote, &["exec", container, "getent", "hosts", host])?;
    // getent output: "<addr>  <name> ..." per line; prefer an IPv4 address.
    let addrs: Vec<std::net::IpAddr> = out
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|a| a.parse().ok())
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .map(|a| a.to_string())
}

fn resolve_via_networks(container: &str, remote: &Option<String>, host: &str) -> Option<String> {
    let nets = docker_stdout(
        remote,
        &[
            "inspect",
            "--format",
            "{{range $k, $v := .NetworkSettings.Networks}}{{$k}}\n{{end}}",
            container,
        ],
    )?;
    for net in nets.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let containers = docker_stdout(
            remote,
            &[
                "network",
                "inspect",
                "--format",
                "{{range .Containers}}{{.Name}} {{.IPv4Address}}\n{{end}}",
                net,
            ],
        )?;
        for line in containers.lines() {
            let mut cols = line.split_whitespace();
            let (Some(name), Some(addr)) = (cols.next(), cols.next()) else {
                continue;
            };
            if name == host || compose_service_matches(name, host) {
                // IPv4Address is CIDR-form ("172.18.0.5/16").
                return addr.split('/').next().map(String::from);
            }
        }
    }
    None
}

// True if `name` follows the compose container naming convention
// <project>-<service>-<index> (or the legacy underscore form) for `service`.
fn compose_service_matches(name: &str, service: &str) -> bool {
    ['-', '_'].iter().any(|&sep| {
        name.rsplit_once(sep).is_some_and(|(stem, index)| {
            !index.is_empty()
                && index.chars().all(|c| c.is_ascii_digit())
                && stem
                    .strip_suffix(service)
                    .is_some_and(|project| project.ends_with(sep))
        })
    })
}

// Runs a docker command and returns stdout, or None on spawn/exit failure.
fn docker_stdout(remote: &Option<String>, args: &[&str]) -> Option<String> {
    let mut cmd = crate::docker::command(remote, args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_container_running(container: &str, remote: &Option<String>) -> bool {
    let mut cmd =
        crate::docker::command(remote, &["inspect", "--format", "{{.State.Running}}", container]);
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

    #[test]
    fn skips_loopback_listeners() {
        // 127.0.0.1:8080 (v4), 127.0.0.11:53 (docker DNS), ::1:9090 (v6) must
        // all be skipped; 0.0.0.0:8081 and [::]:9091 must be kept.
        let data = "\
  sl  local_address rem_address   st
   0: 0100007F:1F90 00000000:0000 0A
   1: 0B00007F:0035 00000000:0000 0A
   2: 00000000:1F91 00000000:0000 0A
   3: 00000000000000000000000001000000:2382 00000000000000000000000000000000:0000 0A
   4: 00000000000000000000000000000000:2383 00000000000000000000000000000000:0000 0A
";
        let ports = parse_proc_net_tcp(data);
        assert_eq!(ports, HashSet::from([8081, 9091]));
    }

    #[test]
    fn port_forward_encode_decode_roundtrip() {
        for f in [
            PortForward::same(8080),
            PortForward {
                local: 3000,
                host: None,
                port: 8080,
            },
            PortForward {
                local: 5432,
                host: Some("db".into()),
                port: 5432,
            },
        ] {
            assert_eq!(PortForward::decode(&f.encode()), Some(f));
        }
        assert_eq!(PortForward::decode("garbage"), None);
        assert_eq!(PortForward::decode("1:2:3:4"), None);
    }

    #[test]
    fn compose_naming_matches_service() {
        assert!(compose_service_matches("myproj-db-1", "db"));
        assert!(compose_service_matches("myproj_db_1", "db"));
        assert!(compose_service_matches("my-proj-db-12", "db"));
        assert!(!compose_service_matches("myproj-database-1", "db"));
        assert!(!compose_service_matches("myproj-db", "db"));
        assert!(!compose_service_matches("db", "db"));
    }
}
