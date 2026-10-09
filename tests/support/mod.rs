//! The shared world for end-to-end tests that pair computers: fixtures for a
//! fake tailnet, real target daemons and real operator consoles. See
//! `tests/pairing.rs` for how it works.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const IBARA: &str = env!("CARGO_BIN_EXE_ibara");
pub const IBARAD: &str = env!("CARGO_BIN_EXE_ibarad");
pub const RILEY: u64 = 1001;
pub const DANA: u64 = 2002;
pub const TAGGED: u64 = 3003;

/// `tailscale status --json` and `tailscale whois --json IP` from fixtures.
const FAKE_TAILSCALE: &str = r#"#!/bin/sh
dir="$FAKE_TAILSCALE_DIR"
case "$1" in
  status) exec cat "$dir/status-$FAKE_TAILSCALE_SELF.json" ;;
  whois)
    for ip in "$@"; do :; done
    [ -f "$dir/whois-$ip.json" ] && exec cat "$dir/whois-$ip.json"
    echo "peer not found" >&2
    exit 1 ;;
esac
echo "fake tailscale: unsupported $*" >&2
exit 2
"#;

/// The target's sshd: `-l ibara-op-P HOST operator-v1` with the pinned known
/// host and the enrolled key, then the forced command over the bearer route.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_SSH_LOG"
login= key= known= alias=
while [ $# -gt 2 ]; do
  case "$1" in
    -l) login=$2; shift 2 ;;
    -i) key=$2; shift 2 ;;
    -p) shift 2 ;;
    -o) case "$2" in
          UserKnownHostsFile=*) known=${2#UserKnownHostsFile=} ;;
          HostKeyAlias=*) alias=${2#HostKeyAlias=} ;;
        esac
        shift 2 ;;
    *) shift ;;
  esac
done
host=$1 command=$2
root=$(sed -n "s/^$host //p" "$FAKE_SSH_ROUTES" | head -n 1)
principal=${login#ibara-op-}
[ -n "$root" ] && [ "$principal" != "$login" ] || { echo "ssh: connect to host $host port 2222: Connection refused" >&2; exit 255; }
grep -qxF "$alias $(cut -d' ' -f1,2 "$root/host_ed25519.pub")" "$known" || { echo "Host key verification failed." >&2; exit 255; }
offered=$(ssh-keygen -y -f "$key" | cut -d' ' -f2)
enrolled=$(cut -d' ' -f2 "$root/authorized/$login" 2>/dev/null)
[ -n "$offered" ] && [ "$offered" = "$enrolled" ] || { echo "$login@$host: Permission denied (publickey)." >&2; exit 255; }
export SSH_ORIGINAL_COMMAND="$command"
exec "$FAKE_IBARA" agent-entry "$principal" "$root/run/controller.sock" "$root/gateway.key" "$root/state/operator-keys/$principal.key"
"#;

/// `ssh-keyscan … HOST` against the target's sshd: the host key it shows.
const FAKE_SSH_KEYSCAN: &str = r#"#!/bin/sh
for host in "$@"; do :; done
root=$(sed -n "s/^$host //p" "$FAKE_SSH_ROUTES" | head -n 1)
[ -n "$root" ] && [ -f "$root/host_ed25519.pub" ] || exit 1
echo "$host $(cut -d' ' -f1,2 "$root/host_ed25519.pub")"
"#;

pub struct Node {
    pub name: &'static str,
    pub host: &'static str,
    pub ip: &'static str,
    pub os: &'static str,
    pub user: u64,
    pub online: bool,
    pub tags: &'static [&'static str],
}

pub const NODES: &[Node] = &[
    Node {
        name: "vesper",
        host: "Vesper",
        ip: "127.0.0.3",
        os: "linux",
        user: RILEY,
        online: true,
        tags: &[],
    },
    Node {
        name: "tulip1",
        host: "tulip1",
        ip: "127.0.0.2",
        os: "linux",
        user: RILEY,
        online: true,
        tags: &[],
    },
    Node {
        name: "command",
        host: "command",
        ip: "127.0.0.4",
        os: "linux",
        user: DANA,
        online: true,
        tags: &[],
    },
    Node {
        name: "lab",
        host: "lab",
        ip: "127.0.0.8",
        os: "linux",
        user: DANA,
        online: true,
        tags: &[],
    },
    Node {
        name: "hazel",
        host: "hazel",
        ip: "127.0.0.5",
        os: "linux",
        user: RILEY,
        online: true,
        tags: &[],
    },
    Node {
        name: "oldbox",
        host: "oldbox",
        ip: "127.0.0.6",
        os: "linux",
        user: RILEY,
        online: false,
        tags: &[],
    },
    Node {
        name: "server",
        host: "server",
        ip: "127.0.0.7",
        os: "linux",
        user: TAGGED,
        online: true,
        tags: &["tag:server"],
    },
    Node {
        name: "pixel-8a",
        host: "Pixel 8a",
        ip: "127.0.0.9",
        os: "android",
        user: RILEY,
        online: true,
        tags: &[],
    },
    Node {
        name: "vesper-windows",
        host: "Vesper-Windows",
        ip: "127.0.0.10",
        os: "windows",
        user: RILEY,
        online: true,
        tags: &[],
    },
    // Tailscale lets a computer be called anything, including the name this
    // computer keeps for its own local owner.
    Node {
        name: "owner",
        host: "owner",
        ip: "127.0.0.11",
        os: "linux",
        user: RILEY,
        online: true,
        tags: &[],
    },
];

pub fn node(name: &str) -> &'static Node {
    NODES.iter().find(|n| n.name == name).unwrap()
}

pub fn login(user: u64) -> &'static str {
    match user {
        RILEY => "riley@example.com",
        DANA => "dana@example.net",
        _ => "tagged-devices",
    }
}

pub fn status_entry(n: &Node) -> Value {
    json!({
        "ID": format!("n{}", n.name), "HostName": n.host, "DNSName": format!("{}.tail0000.ts.net.", n.name), "OS": n.os,
        "UserID": n.user, "TailscaleIPs": [n.ip, format!("fd7a:115c:a1e0::{}", n.ip.rsplit('.').next().unwrap())],
        "Tags": if n.tags.is_empty() { Value::Null } else { json!(n.tags) }, "Online": n.online,
    })
}

pub fn status_for(own: &Node) -> Value {
    let peers: serde_json::Map<String, Value> = NODES
        .iter()
        .filter(|n| n.name != own.name)
        .map(|n| (format!("nodekey:{}", n.name), status_entry(n)))
        .collect();
    let users: serde_json::Map<String, Value> = [RILEY, DANA, TAGGED]
        .iter()
        .map(|id| {
            (
                id.to_string(),
                json!({"ID": id, "LoginName": login(*id), "DisplayName": login(*id)}),
            )
        })
        .collect();
    json!({
        "BackendState": "Running", "AuthURL": "", "Self": status_entry(own), "Peer": peers, "User": users,
        "CurrentTailnet": {"Name": "riley@example.com", "MagicDNSSuffix": "tail0000.ts.net", "MagicDNSEnabled": true},
    })
}

pub fn whois_for(n: &Node) -> Value {
    json!({
        "Node": {"ID": 1, "StableID": format!("stable-{}", n.name), "Name": format!("{}.tail0000.ts.net.", n.name),
                 "ComputedName": n.name, "User": n.user, "Tags": if n.tags.is_empty() { Value::Null } else { json!(n.tags) },
                 "Addresses": [format!("{}/32", n.ip)], "Hostinfo": {"Hostname": n.host, "OS": n.os}},
        "UserProfile": {"ID": n.user, "LoginName": login(n.user), "DisplayName": login(n.user)},
    })
}

pub fn write_executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Everything one scenario shares: fixtures, fake tools, the pairing port, evidence.
pub struct World {
    pub root: PathBuf,
    pub port: u16,
    pub evidence: Option<PathBuf>,
    pub tag: &'static str,
    /// Where magic packets go instead of a broadcast (`IBARA_TEST_WAKE_ADDRESS`).
    pub wake_address: Option<String>,
}

impl World {
    pub fn new(tag: &'static str) -> World {
        let root = std::env::temp_dir().join(format!("ibara-pair-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["bin", "tailscale"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        write_executable(&root.join("bin/tailscale"), FAKE_TAILSCALE);
        write_executable(&root.join("bin/ssh"), FAKE_SSH);
        write_executable(&root.join("bin/ssh-keyscan"), FAKE_SSH_KEYSCAN);
        for n in NODES {
            fs::write(
                root.join(format!("tailscale/status-{}.json", n.name)),
                status_for(n).to_string(),
            )
            .unwrap();
            fs::write(
                root.join(format!("tailscale/whois-{}.json", n.ip)),
                whois_for(n).to_string(),
            )
            .unwrap();
        }
        fs::write(root.join("ssh-routes"), "").unwrap();
        fs::create_dir_all(root.join("omarchy")).unwrap();
        for n in NODES {
            fs::create_dir_all(root.join("machines").join(n.name).join("bin")).unwrap();
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let evidence = std::env::var_os("IBARA_E2E_EVIDENCE")
            .map(PathBuf::from)
            .inspect(|dir| fs::create_dir_all(dir).unwrap());
        World {
            root,
            port,
            evidence,
            tag,
            wake_address: None,
        }
    }

    /// One computer's own programs, first on its daemons' `PATH` (fake `ip`,
    /// `iw`, `journalctl`, Omarchy's scripts).
    pub fn machine_bin(&self, name: &str) -> PathBuf {
        self.root.join("machines").join(name).join("bin")
    }

    pub fn record(&self, entry: Value) {
        if let Some(dir) = &self.evidence {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{}.jsonl", self.tag)))
                .unwrap();
            writeln!(file, "{entry}").unwrap();
        }
    }

    /// Environment every daemon of one computer shares.
    pub fn machine_env(&self, command: &mut Command, n: &Node) {
        // Nothing reaches this computer's real Omarchy, settings or session:
        // Omarchy's scripts come only from the machine's own fake bin.
        command
            .env(
                "PATH",
                format!(
                    "{}:{}:/usr/bin:/bin",
                    self.machine_bin(n.name).display(),
                    self.root.join("bin").display()
                ),
            )
            .env("OMARCHY_PATH", self.root.join("omarchy"))
            .env_remove("XDG_CONFIG_HOME")
            .env("IBARA_TAILSCALE_BIN", self.root.join("bin/tailscale"))
            .env("IBARA_PAIRING_PORT", self.port.to_string())
            .env("FAKE_TAILSCALE_DIR", self.root.join("tailscale"))
            .env("FAKE_TAILSCALE_SELF", n.name)
            .env("FAKE_SSH_ROUTES", self.root.join("ssh-routes"))
            .env("FAKE_SSH_LOG", self.root.join("ssh.log"))
            .env("FAKE_IBARA", IBARA)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("XDG_STATE_HOME")
            .env_remove("IBARA_STATION_DESCRIPTOR")
            .env_remove("IBARA_OPERATOR_DIRECTORY_DB");
        if let Some(address) = &self.wake_address {
            command.env("IBARA_TEST_WAKE_ADDRESS", address);
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub fn wait_ready(child: &mut Child, marker: &str) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut log = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(line) if line.contains(marker) => return rx,
            Ok(line) => log.push(line),
            Err(_) => panic!(
                "never ready ({marker}): {log:#?} (exit {:?})",
                child.try_wait()
            ),
        }
    }
}

/// A target computer: a real `ibarad` with a stub desktop and a stub root
/// access projection that records every request and "installs" each enrolled
/// key where the fake sshd looks for it.
pub struct Target {
    pub child: Child,
    restart_command: Command,
    pub root: PathBuf,
    pub projections: PathBuf,
}

impl Target {
    pub fn start(world: &World, n: &Node, station_label: Option<&str>, window_ms: u64) -> Target {
        Target::start_with(world, n, station_label, window_ms, &[])
    }

    /// `start`, with `env` set last (so it can replace a stub, such as
    /// `IBARA_TEST_HYPRCTL`, or give the computer a desktop session).
    pub fn start_with(
        world: &World,
        n: &Node,
        station_label: Option<&str>,
        window_ms: u64,
        env: &[(&str, &std::ffi::OsStr)],
    ) -> Target {
        let root = world.root.join(format!("target-{}", n.name));
        for dir in [
            "home",
            "install",
            "operators",
            "bin",
            "authorized",
            "xdg-run",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("policy.json"), r#"{"principals":[]}"#).unwrap();
        fs::write(root.join("gateway.key"), "gateway-e2e-secret\n").unwrap();
        let admin_digest = ibara::server::policy::sha256_hex(b"admin-e2e-secret");
        fs::write(root.join("admin.sha256"), format!("{admin_digest}\n")).unwrap();
        fs::write(root.join("admin.key"), "admin-e2e-secret\n").unwrap();
        fs::write(root.join("fingerprints.json"), r#"{"fingerprints":{}}"#).unwrap();
        if let Some(label) = station_label {
            fs::write(
                root.join("station.json"),
                json!({"schema_version": 1, "display_label": label}).to_string(),
            )
            .unwrap();
        }
        let keygen = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "", "-f"])
            .arg(root.join("host_ed25519"))
            .status()
            .unwrap();
        assert!(keygen.success());
        let projections = root.join("projections.jsonl");
        let listener = UnixListener::bind(root.join("access.sock")).unwrap();
        let (log, authorized) = (projections.clone(), root.join("authorized"));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log)
                    .unwrap();
                writeln!(file, "{request}").unwrap();
                for (principal, key) in request["keys"].as_object().into_iter().flatten() {
                    fs::write(
                        authorized.join(format!("ibara-op-{principal}")),
                        key.as_str().unwrap(),
                    )
                    .unwrap();
                }
                let _ = writeln!(reader.get_mut(), "{{\"ok\":true}}");
            }
        });
        let bin = root.join("bin");
        let stub = |name: &str, script: &str| {
            let path = bin.join(name);
            write_executable(&path, &format!("#!/bin/sh\n{script}\n"));
            path
        };
        let (hyprctl, grim, cua) = (
            stub("hyprctl", "echo '[]'"),
            stub("grim", "exit 1"),
            stub("cua-driver", "exit 1"),
        );
        let mut command = Command::new(IBARAD);
        world.machine_env(&mut command, n);
        command
            .env("HOME", root.join("home"))
            .env("IBARA_STATE_DIR", root.join("state"))
            .env("IBARA_DATA_DIR", root.join("data"))
            .env("IBARA_RUNTIME_DIR", root.join("run"))
            .env("IBARA_INSTALL_ROOT", root.join("install"))
            .env("IBARA_ACCESS_FINGERPRINTS", root.join("fingerprints.json"))
            .env("IBARA_ACCESS_SOCKET", root.join("access.sock"))
            .env("IBARA_POLICY", root.join("policy.json"))
            .env("IBARA_GATEWAY_KEY", root.join("gateway.key"))
            .env("IBARA_ADMIN_HASH", root.join("admin.sha256"))
            .env(
                "IBARA_OPERATOR_ACCOUNTS",
                root.join("absent-operator-accounts.json"),
            )
            .env("IBARA_OPERATOR_SOCKET_DIR", root.join("operators"))
            .env("IBARA_SSH_HOST_KEY", root.join("host_ed25519.pub"))
            .env("IBARA_STATION_FILE", root.join("station.json"))
            .env("IBARA_TEST_PAIRING_WINDOW_MS", window_ms.to_string())
            .env("IBARA_TEST_PREVIEW_TOOLS", "1")
            .env("IBARA_TEST_HYPRCTL", hyprctl)
            .env("IBARA_TEST_GRIM", grim)
            .env("IBARA_TEST_CUA", cua)
            .env("IBARA_POWER_SOCKET", root.join("power.sock"))
            .env("XDG_RUNTIME_DIR", root.join("xdg-run"))
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let lines = wait_ready(&mut child, "\"pairing_listening\"");
        std::thread::spawn(move || for _ in lines.iter() {});
        let mut routes = fs::OpenOptions::new()
            .append(true)
            .open(world.root.join("ssh-routes"))
            .unwrap();
        writeln!(routes, "{} {}", n.ip, root.display()).unwrap();
        Target {
            child,
            restart_command: command,
            root,
            projections,
        }
    }

    /// Restart the actual daemon over the same journal/identity, without fixture
    /// reinitialization. Used to prove durable intent across process loss.
    pub fn restart(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.child=self.restart_command.spawn().unwrap();
        let lines=wait_ready(&mut self.child,"\"pairing_listening\"");
        std::thread::spawn(move || for _ in lines.iter() {});
    }

    /// The root power helper as systemd runs it (`Accept=yes`): each
    /// connection to this target's power socket becomes the real `ibara
    /// power-system` on that connection, with the programs in `fixture/bin`
    /// (fakes that log) and the `fixture/sys` and `fixture/proc` trees.
    pub fn serve_power(&self, fixture: &Path) {
        let listener = UnixListener::bind(self.root.join("power.sock")).unwrap();
        let fixture = fixture.to_path_buf();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let input: std::os::fd::OwnedFd = stream.try_clone().unwrap().into();
                let output: std::os::fd::OwnedFd = stream.into();
                let mut helper = Command::new(IBARA)
                    .arg("power-system")
                    .env("IBARA_POWER_BIN_DIR", fixture.join("bin"))
                    .env("IBARA_POWER_SYSFS", fixture.join("sys"))
                    .env("IBARA_POWER_PROC", fixture.join("proc"))
                    .stdin(Stdio::from(input))
                    .stdout(Stdio::from(output))
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                std::thread::spawn(move || helper.wait());
            }
        });
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    pub fn projections(&self) -> Vec<Value> {
        fs::read_to_string(&self.projections)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub fn authority(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.join("state/operator-authority.json")).unwrap())
            .unwrap()
    }

    /// `computerctl` on this computer: `ibara admin ARGS…` as its local owner.
    /// The exit code and the printed JSON.
    pub fn admin(&self, args: &[&str]) -> (Option<i32>, Value) {
        let out = Command::new(IBARA)
            .arg("admin")
            .args(args)
            .env("IBARA_ADMIN_SOCKET", self.root.join("run/admin.sock"))
            .env("IBARA_ADMIN_KEY", self.root.join("admin.key"))
            .output()
            .unwrap();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        let reply = serde_json::from_str(&stdout)
            .unwrap_or_else(|_| json!({"stdout": stdout, "stderr": stderr}));
        (out.status.code(), reply)
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An operator console on one computer: `ibarad --role operator` with its own
/// home (so its own `~/.ssh/ibara_agent_ed25519` and directory).
pub struct Console<'w> {
    pub world: &'w World,
    pub name: String,
    pub child: Child,
    pub reader: BufReader<UnixStream>,
    pub writer: UnixStream,
    pub home: PathBuf,
    pub next_id: u32,
}

impl<'w> Console<'w> {
    pub fn start(world: &'w World, name: &str, n: &Node, target: Option<&Target>) -> Console<'w> {
        Self::start_with(world, name, n, target, &[])
    }
    pub fn start_with(
        world: &'w World,
        name: &str,
        n: &Node,
        target: Option<&Target>,
        env: &[(&str, &std::ffi::OsStr)],
    ) -> Console<'w> {
        let root = world.root.join(format!("console-{name}"));
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("run")).unwrap();
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(IBARAD);
        world.machine_env(&mut command, n);
        command
            .args(["--role", "operator"])
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"));
        match target {
            Some(target) => command.env("IBARA_RUNTIME_DIR", target.root.join("run")),
            None => command.env("IBARA_RUNTIME_DIR", root.join("no-target")),
        };
        command.envs(env.iter().copied());
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = wait_ready(&mut child, "operator console serving");
        std::thread::spawn(move || for _ in lines.iter() {});
        let stream = UnixStream::connect(root.join("run/ibara/ibarad.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(40)))
            .unwrap();
        Console {
            world,
            name: name.into(),
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            child,
            home: root.join("home"),
            next_id: 0,
        }
    }

    pub fn ask(&mut self, command: &str, args: &[&str]) -> Value {
        self.next_id += 1;
        let id = format!("{}-{}", self.name, self.next_id);
        writeln!(
            self.writer,
            "{}",
            json!({"id": id, "command": command, "args": args})
        )
        .unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
        assert_eq!(reply["id"], id, "{reply}");
        let envelope = reply["envelope"].clone();
        self.world.record(
            json!({"console": self.name, "command": command, "args": args, "envelope": envelope}),
        );
        envelope
    }

    /// `ask`, requiring an ok envelope; its data.
    pub fn ok(&mut self, command: &str, args: &[&str]) -> Value {
        let envelope = self.ask(command, args);
        assert!(
            envelope["error"].is_null(),
            "{command} {args:?}: {envelope}"
        );
        envelope["data"].clone()
    }

    /// `operator-observe`, which answers as a `preview` event: its envelope.
    pub fn preview(&mut self, args: &[&str]) -> Value {
        self.next_id += 1;
        let id = format!("{}-{}", self.name, self.next_id);
        writeln!(
            self.writer,
            "{}",
            json!({"id": id, "command": "operator-observe", "args": args})
        )
        .unwrap();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            let event: Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
            if event["event"] == "preview" && event["data"]["request_id"] == id {
                let envelope = event["data"].clone();
                self.world.record(json!({"console": self.name, "command": "operator-observe", "args": args, "envelope": envelope}));
                return envelope;
            }
        }
    }

    /// Poll `pair-status` until the request leaves `waiting`.
    pub fn settled(&mut self, request_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.ok("pair-status", &[request_id]);
            if status["state"] != "waiting" {
                return status;
            }
            assert!(Instant::now() < deadline, "still waiting: {status}");
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    pub fn public_key(&self) -> String {
        fs::read_to_string(self.home.join(".ssh/ibara_agent_ed25519.pub"))
            .unwrap()
            .trim()
            .to_string()
    }
}

impl Drop for Console<'_> {
    fn drop(&mut self) {
        // SAFETY: signalling a child we spawned.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

pub fn six_digits(code: &Value) -> bool {
    code.as_str().is_some_and(|c| {
        c.len() == 7
            && c.as_bytes()[3] == b' '
            && c.bytes()
                .enumerate()
                .all(|(i, b)| i == 3 || b.is_ascii_digit())
    })
}

pub fn computers(tailnet: &Value) -> BTreeMap<String, Value> {
    tailnet["computers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["node"].as_str().unwrap().to_string(), c.clone()))
        .collect()
}

/// Selected-route reads through the new directory row reach the real target.
pub fn route_works(console: &mut Console, computer_id: &str) {
    let session = console.ok("operator-session", &["--computer", computer_id]);
    let epoch = session["controller_epoch"].as_str().unwrap().to_string();
    let status = console.ok(
        "operator-status",
        &["--computer", computer_id, "--epoch", &epoch],
    );
    assert_eq!(status["computer_id"], computer_id, "{status}");
}
