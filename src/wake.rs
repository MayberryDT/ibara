//! Waking a sleeping computer with a magic packet.
//!
//! A computer reports how it can be woken: the interface of its default
//! route, that interface's MAC, whether it is Wi-Fi (wake-on-WLAN, only from
//! sleep) or Ethernet (wake-on-LAN, also from off when the firmware allows),
//! its IPv4 subnet and, when known, its gateway's MAC (`gateway_mac`). Another
//! computer on that network sends the packet: six `0xff` bytes and the MAC
//! sixteen times, as a UDP broadcast to port 9. Home networks often share a
//! private range such as `192.168.1.0/24`, so the gateway's MAC tells a
//! computer at a café on the same range that it is not on the sleeping
//! computer's network ([`here`]).
//!
//! Addresses, routes, neighbours and the MAC come from `ip -j`, Wi-Fi
//! interfaces and their wake-up support from `iw`; an Ethernet card's
//! wake-on-LAN support from the `SIOCETHTOOL` ioctl (no `ethtool` program
//! needed; reading needs no root). Enabling wake-up needs root and happens in
//! `ibara power-system`.

use serde_json::{Value, json};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

/// `SIOCETHTOOL` and the wake-on-LAN commands (linux/ethtool.h, linux/sockios.h).
const SIOCETHTOOL: libc::c_ulong = 0x8946;
pub const ETHTOOL_GWOL: u32 = 0x5;
pub const ETHTOOL_SWOL: u32 = 0x6;
pub const WAKE_MAGIC: u32 = 1 << 5;

/// `struct ethtool_wolinfo`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WolInfo {
    pub cmd: u32,
    pub supported: u32,
    pub wolopts: u32,
    pub sopass: [u8; 6],
}

/// One `SIOCETHTOOL` wake-on-LAN call on `ifname` (`ETHTOOL_GWOL` or `ETHTOOL_SWOL`).
pub fn ethtool_wol(ifname: &str, mut info: WolInfo) -> std::io::Result<WolInfo> {
    if ifname.is_empty() || ifname.len() >= libc::IFNAMSIZ {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // SAFETY: a datagram socket we close below; `ifreq` is zeroed, its name is
    // NUL-terminated within IFNAMSIZ, and `ifr_data` points at `info`, which
    // outlives the call.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut request: libc::ifreq = std::mem::zeroed();
        for (slot, byte) in request.ifr_name.iter_mut().zip(ifname.bytes()) {
            *slot = byte as libc::c_char;
        }
        request.ifr_ifru.ifru_data = (&mut info as *mut WolInfo).cast();
        let status = libc::ioctl(fd, SIOCETHTOOL as _, &mut request);
        let error = std::io::Error::last_os_error();
        libc::close(fd);
        if status < 0 {
            return Err(error);
        }
    }
    Ok(info)
}

/// `aa:bb:cc:dd:ee:ff` (any case) as bytes; never all zeros or broadcast.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        if part.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    (mac != [0; 6] && mac != [0xff; 6]).then_some(mac)
}

/// `10.1.2.0/24` → network and prefix length (1–32). The host bits must be zero.
pub fn parse_subnet(text: &str) -> Option<(Ipv4Addr, u8)> {
    let (address, prefix) = text.split_once('/')?;
    let address: Ipv4Addr = address.parse().ok()?;
    let prefix: u8 = prefix.parse().ok().filter(|p| (1..=32).contains(p))?;
    (network(address, prefix) == address).then_some((address, prefix))
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix)) }
}

fn network(address: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(address) & mask(prefix))
}

fn broadcast(subnet: (Ipv4Addr, u8)) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(subnet.0) | !mask(subnet.1))
}

/// The magic packet for `mac`.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for copy in packet[6..].chunks_exact_mut(6) {
        copy.copy_from_slice(&mac);
    }
    packet
}

async fn run(program: &str, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(3), command.output()).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// This computer's IPv4 networks by interface, from `ip -j -4 addr`
/// (loopback and Tailscale left out).
async fn local_links() -> Vec<(String, (Ipv4Addr, u8))> {
    let Some(text) = run("ip", &["-j", "-4", "addr", "show"]).await else { return Vec::new() };
    let links: Vec<Value> = serde_json::from_str(&text).unwrap_or_default();
    let mut out = Vec::new();
    for link in &links {
        let name = link["ifname"].as_str().unwrap_or("");
        if name == "lo" || name.starts_with("tailscale") {
            continue;
        }
        for info in link["addr_info"].as_array().into_iter().flatten() {
            let address = info["local"].as_str().and_then(|a| a.parse::<Ipv4Addr>().ok());
            let prefix = info["prefixlen"].as_u64().and_then(|p| u8::try_from(p).ok()).filter(|p| (1..=32).contains(p));
            if let (Some(address), Some(prefix)) = (address, prefix) {
                out.push((name.to_string(), (network(address, prefix), prefix)));
            }
        }
    }
    out
}

/// This computer's IPv4 subnets (loopback and Tailscale left out).
pub async fn local_subnets() -> Vec<(Ipv4Addr, u8)> {
    local_links().await.into_iter().map(|(_, subnet)| subnet).collect()
}

/// This computer's IPv4 neighbours from `ip -j -4 neigh`: `(address, interface, MAC)`.
async fn neighbours() -> Vec<(String, String, String)> {
    let Some(text) = run("ip", &["-j", "-4", "neigh", "show"]).await else { return Vec::new() };
    let entries: Vec<Value> = serde_json::from_str(&text).unwrap_or_default();
    entries
        .iter()
        .filter_map(|e| {
            let mac = e["lladdr"].as_str().filter(|m| parse_mac(m).is_some())?.to_lowercase();
            Some((e["dst"].as_str()?.to_string(), e["dev"].as_str()?.to_string(), mac))
        })
        .collect()
}

/// Whether this computer is on a sleeping computer's network.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Here {
    /// On its subnet, and its gateway is a neighbour here.
    Same,
    /// On its subnet, but whether it is the same network cannot be told
    /// (no gateway recorded, or none seen here).
    Unsure,
    /// Not on its subnet, or on a network with the same range but another gateway.
    Elsewhere,
}

/// Whether this computer is on the network `subnet` whose gateway has the
/// MAC `gateway_mac` (when it was recorded).
pub async fn here(subnet: &str, gateway_mac: Option<&str>) -> Here {
    let Some(wanted) = parse_subnet(subnet) else { return Here::Elsewhere };
    let links: Vec<String> = local_links().await.into_iter().filter(|(_, s)| *s == wanted).map(|(name, _)| name).collect();
    if links.is_empty() {
        return Here::Elsewhere;
    }
    let Some(gateway_mac) = gateway_mac.filter(|m| parse_mac(m).is_some()).map(str::to_lowercase) else { return Here::Unsure };
    let neighbours = neighbours().await;
    if neighbours.iter().any(|(_, dev, mac)| links.contains(dev) && *mac == gateway_mac) {
        return Here::Same;
    }
    // This computer's own gateway on that network is known, and it is another one.
    let routes: Vec<Value> = run("ip", &["-j", "-4", "route", "show", "default"]).await.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let other = routes.iter().any(|r| {
        let (Some(gateway), Some(dev)) = (r["gateway"].as_str(), r["dev"].as_str()) else { return false };
        links.iter().any(|l| l == dev) && neighbours.iter().any(|(dst, d, mac)| dst == gateway && d == dev && *mac != gateway_mac)
    });
    if other { Here::Elsewhere } else { Here::Unsure }
}

/// How this computer can be woken, or `None` when its network card cannot
/// wake it (or it has no default route to a local network).
pub async fn detect() -> Option<Value> {
    let routes: Vec<Value> = serde_json::from_str(&run("ip", &["-j", "-4", "route", "show", "default"]).await?).ok()?;
    let route = routes.iter().min_by_key(|r| r["metric"].as_u64().unwrap_or(0))?;
    let ifname = route["dev"]
        .as_str()
        .filter(|name| !name.is_empty() && name.len() < libc::IFNAMSIZ && !name.starts_with("tailscale"))?
        .to_string();
    let gateway = route["gateway"].as_str().unwrap_or("").to_string();
    let links: Vec<Value> = serde_json::from_str(&run("ip", &["-j", "link", "show", "dev", &ifname]).await?).ok()?;
    let mac = links.first()?["address"].as_str().filter(|m| parse_mac(m).is_some())?.to_lowercase();
    let addresses: Vec<Value> = serde_json::from_str(&run("ip", &["-j", "-4", "addr", "show", "dev", &ifname]).await?).ok()?;
    let info = addresses.first()?["addr_info"].as_array()?.iter().find(|i| i["family"] == "inet")?.clone();
    let address: Ipv4Addr = info["local"].as_str()?.parse().ok()?;
    let prefix = u8::try_from(info["prefixlen"].as_u64()?).ok().filter(|p| (1..=30).contains(p))?;
    let subnet = format!("{}/{prefix}", network(address, prefix));
    let (kind, supported, from_off) = match wifi_phy(&ifname).await {
        Some(phy) => {
            let info = run("iw", &["phy", &phy, "info"]).await.unwrap_or_default();
            ("wifi", info.contains("wake up on magic packet"), false)
        }
        None => {
            let wol = ethtool_wol(&ifname, WolInfo { cmd: ETHTOOL_GWOL, ..Default::default() }).ok();
            let magic = wol.is_some_and(|w| w.supported & WAKE_MAGIC != 0);
            ("ethernet", magic, magic)
        }
    };
    let gateway_mac = neighbours().await.into_iter().find(|(dst, dev, _)| *dst == gateway && *dev == ifname).map(|(.., mac)| mac);
    let mut wake = json!({"mac": mac, "ifname": ifname, "kind": kind, "subnet": subnet, "from_off": from_off});
    if let Some(gateway_mac) = gateway_mac {
        wake["gateway_mac"] = json!(gateway_mac);
    }
    supported.then_some(wake)
}

/// The wireless phy of `ifname` from `iw dev` (`phy#0` → `phy0`), if it is Wi-Fi.
async fn wifi_phy(ifname: &str) -> Option<String> {
    let text = run("iw", &["dev"]).await?;
    let mut phy = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(number) = line.strip_prefix("phy#") {
            phy = Some(format!("phy{number}"));
        } else if line.strip_prefix("Interface ") == Some(ifname) {
            return phy;
        }
    }
    None
}

/// Send the magic packet for `mac` to the broadcast address of `subnet`, and
/// to the limited broadcast. `IBARA_TEST_WAKE_ADDRESS` (tests) replaces both
/// with one address.
pub fn send(mac: [u8; 6], subnet: (Ipv4Addr, u8)) -> std::io::Result<()> {
    let packet = magic_packet(mac);
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    if let Some(test) = std::env::var("IBARA_TEST_WAKE_ADDRESS").ok().and_then(|a| a.parse::<SocketAddr>().ok()) {
        socket.send_to(&packet, test)?;
        return Ok(());
    }
    socket.set_broadcast(true)?;
    socket.send_to(&packet, (broadcast(subnet), 9))?;
    socket.send_to(&packet, (Ipv4Addr::BROADCAST, 9))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnets_and_macs_are_exact() {
        assert_eq!(parse_subnet("10.20.30.0/24"), Some(("10.20.30.0".parse().unwrap(), 24)));
        assert_eq!(broadcast(parse_subnet("10.20.30.0/24").unwrap()), "10.20.30.255".parse::<Ipv4Addr>().unwrap());
        for bad in ["10.20.30.7/24", "10.0.0.0/33", "10.0.0.0", "x/24"] {
            assert_eq!(parse_subnet(bad), None, "{bad}");
        }
        for bad in ["00:00:00:00:00:00", "ff:ff:ff:ff:ff:ff", "02:00:5e:71:00", "02-00-5e-71-00-01", "02:00:5e:71:00:0"] {
            assert_eq!(parse_mac(bad), None, "{bad}");
        }
    }
}
