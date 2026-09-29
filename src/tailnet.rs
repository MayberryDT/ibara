//! Tailscale as ibara reads it: this computer and its tailnet
//! (`tailscale status --json`), who is at the other end of a connection
//! (`tailscale whois --json IP`), and the pairing port every computer that
//! hosts ibara listens on.
//!
//! `IBARA_TAILSCALE_BIN` names another `tailscale` program and
//! `IBARA_PAIRING_PORT` another port (tests use fixtures on loopback).

use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::{IpAddr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpSocket;

/// The pairing listener's TCP port on each Tailscale address.
pub const PAIRING_PORT: u16 = 24247;
/// One pairing request or reply line.
pub const LINE_LIMIT: usize = 16 * 1024;
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

pub fn pairing_port() -> u16 {
    std::env::var("IBARA_PAIRING_PORT").ok().and_then(|p| p.parse().ok()).filter(|p| *p > 0).unwrap_or(PAIRING_PORT)
}

fn tailscale_bin() -> OsString {
    std::env::var_os("IBARA_TAILSCALE_BIN").filter(|v| !v.is_empty()).unwrap_or_else(|| "tailscale".into())
}

/// Why `tailscale` gave no usable answer.
#[derive(Debug, Clone, PartialEq)]
pub enum CliError {
    NotInstalled,
    /// It ran and failed; its first stderr line (for example, the daemon is not running).
    Failed(String),
    TimedOut,
    Invalid,
}

async fn run_json(args: &[&str], timeout: Duration) -> Result<Value, CliError> {
    let mut command = tokio::process::Command::new(tailscale_bin());
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => CliError::NotInstalled,
        _ => CliError::Failed(e.to_string()),
    })?;
    let (mut stdout, mut stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let work = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut read_out = (&mut stdout).take(OUTPUT_LIMIT as u64 + 1);
        let mut read_err = (&mut stderr).take(64 * 1024);
        let _ = tokio::join!(read_out.read_to_end(&mut out), read_err.read_to_end(&mut err));
        (out, err, child.wait().await)
    };
    let (out, err, status) = tokio::time::timeout(timeout, work).await.map_err(|_| CliError::TimedOut)?;
    if !status.is_ok_and(|s| s.success()) {
        let first = String::from_utf8_lossy(&err).lines().next().unwrap_or("").trim().to_string();
        return Err(CliError::Failed(first));
    }
    if out.len() > OUTPUT_LIMIT {
        return Err(CliError::Invalid);
    }
    serde_json::from_slice(&out).map_err(|_| CliError::Invalid)
}

/// One computer in `tailscale status`.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    /// The first label of its MagicDNS name (`tulip1`), else its host name.
    pub node: String,
    /// The MagicDNS name without the trailing dot.
    pub dns_name: String,
    pub host_name: String,
    pub os: String,
    pub user_id: Option<u64>,
    pub ips: Vec<IpAddr>,
    pub tags: Vec<String>,
    pub online: bool,
}

impl Peer {
    fn parse(value: &Value) -> Peer {
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let dns_name = text("DNSName").trim_end_matches('.').to_string();
        let host_name = text("HostName");
        let node = match dns_name.split('.').next().filter(|n| !n.is_empty()) {
            Some(first) => first.to_string(),
            None => host_name.to_lowercase(),
        };
        Peer {
            node,
            dns_name,
            host_name,
            os: text("OS"),
            user_id: value.get("UserID").and_then(Value::as_u64),
            ips: strings(value.get("TailscaleIPs")).iter().filter_map(|ip| ip.parse().ok()).collect(),
            tags: strings(value.get("Tags")),
            online: value.get("Online").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    /// Its first IPv4 address, else its first address.
    pub fn address(&self) -> Option<IpAddr> {
        self.ips.iter().find(|ip| ip.is_ipv4()).or(self.ips.first()).copied()
    }

    /// Does `name` name this computer (node, MagicDNS name, host name or address)?
    pub fn named(&self, name: &str) -> bool {
        let name = name.trim_end_matches('.');
        [&self.node, &self.dns_name, &self.host_name].iter().any(|n| !n.is_empty() && n.eq_ignore_ascii_case(name))
            || self.ips.iter().any(|ip| ip.to_string() == name)
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

/// `tailscale status --json`.
#[derive(Debug, Clone)]
pub struct Status {
    /// `Running`, `Stopped`, `NeedsLogin`, `NeedsMachineAuth`, …
    pub backend: String,
    pub auth_url: Option<String>,
    pub own: Option<Peer>,
    pub peers: Vec<Peer>,
    logins: BTreeMap<u64, String>,
}

impl Status {
    pub fn parse(value: &Value) -> Status {
        let logins = value
            .get("User")
            .and_then(Value::as_object)
            .map(|users| {
                users
                    .values()
                    .filter_map(|u| Some((u.get("ID")?.as_u64()?, u.get("LoginName")?.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Status {
            backend: value.get("BackendState").and_then(Value::as_str).unwrap_or("").to_string(),
            auth_url: value.get("AuthURL").and_then(Value::as_str).filter(|u| !u.is_empty()).map(str::to_string),
            own: value.get("Self").filter(|s| s.is_object()).map(Peer::parse),
            peers: value.get("Peer").and_then(Value::as_object).map(|p| p.values().map(Peer::parse).collect()).unwrap_or_default(),
            logins,
        }
    }

    pub fn running(&self) -> bool {
        self.backend == "Running"
    }

    /// The Tailscale login of a computer; none for a tagged computer.
    pub fn owner(&self, peer: &Peer) -> Option<&str> {
        if !peer.tags.is_empty() {
            return None;
        }
        self.logins.get(&peer.user_id?).map(String::as_str)
    }

    /// This computer's login.
    pub fn login(&self) -> Option<&str> {
        self.owner(self.own.as_ref()?)
    }
}

pub async fn status() -> Result<Status, CliError> {
    run_json(&["status", "--json"], Duration::from_secs(5)).await.map(|v| Status::parse(&v))
}

/// Who is at `ip`, as Tailscale authenticated it.
#[derive(Debug, Clone, PartialEq)]
pub struct Whois {
    pub stable_id: String,
    /// The first label of its MagicDNS name.
    pub node: String,
    /// The name the computer gives itself (`Bench`), else its node name.
    pub host_name: String,
    pub user_id: u64,
    pub login: String,
    pub tags: Vec<String>,
}

pub async fn whois(ip: IpAddr) -> Result<Whois, CliError> {
    let value = run_json(&["whois", "--json", &ip.to_string()], Duration::from_secs(5)).await?;
    let node = value.get("Node").ok_or(CliError::Invalid)?;
    let text = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or("").to_string();
    let name = text(node.get("Name"));
    let node_name = name.split('.').next().unwrap_or("").to_string();
    let host_name = text(node.get("Hostinfo").and_then(|h| h.get("Hostname")));
    let whois = Whois {
        stable_id: text(node.get("StableID")),
        host_name: if host_name.trim().is_empty() { node_name.clone() } else { host_name.trim().chars().filter(|c| !c.is_control()).take(64).collect() },
        node: node_name,
        user_id: node.get("User").and_then(Value::as_u64).unwrap_or(0),
        login: text(value.get("UserProfile").and_then(|p| p.get("LoginName"))),
        tags: strings(node.get("Tags")),
    };
    if whois.stable_id.is_empty() || whois.node.is_empty() || whois.user_id == 0 {
        return Err(CliError::Invalid);
    }
    Ok(whois)
}

/// Two computers belong to the same person when both are signed in to
/// Tailscale with the same login and neither is a tagged computer.
pub fn same_owner(a: (Option<u64>, &[String]), b: (Option<u64>, &[String])) -> bool {
    a.0.is_some() && a.0 == b.0 && a.1.is_empty() && b.1.is_empty()
}

/// One pairing exchange: connect to `to` (from `from` when given, so the other
/// computer sees this computer's own Tailscale address), send one JSON line,
/// read one back.
pub async fn exchange(to: SocketAddr, from: Option<IpAddr>, request: &Value, timeout: Duration) -> std::io::Result<Value> {
    let socket = if to.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    if let Some(from) = from.filter(|f| f.is_ipv4() == to.is_ipv4()) {
        // Unbound, the kernel still picks the Tailscale address for a Tailscale peer.
        let _ = socket.bind(SocketAddr::new(from, 0));
    }
    let work = async {
        let mut stream = socket.connect(to).await?;
        stream.write_all(format!("{request}\n").as_bytes()).await?;
        let mut line = Vec::new();
        BufReader::new(&mut stream).take(LINE_LIMIT as u64).read_until(b'\n', &mut line).await?;
        serde_json::from_slice::<Value>(&line).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not a pairing reply"))
    };
    tokio::time::timeout(timeout, work).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}
