//! Everyday features over the pairing route, end to end.
//!
//! The same world as `tests/pairing.rs`: real target daemons and real
//! consoles, a fake tailnet, a fake sshd that runs the real forced command.
//! Each computer also has its own fake `ip` and `iw` (its network), a fake
//! `journalctl`, and fake Omarchy scripts that log what they were asked; the
//! target's root power helper is the real `ibara power-system`, run per
//! connection as systemd would, with fake `systemctl`, `iw` and
//! `systemd-cryptenroll` and Tulip1's disk layout. Magic packets go to a UDP
//! socket this test reads.
//!
//! Failure cases this must catch:
//! 1. An everyday command still needs an administrator route, or a station.
//! 2. Health misses memory, disk, uptime, repair, how to wake it, or whether a
//!    restart stops at the disk password; the console does not keep the wake
//!    information for when the computer is asleep.
//! 3. Logs read the wrong unit, too many lines, or none.
//! 4. A setting is stored in the wrong table or file, a bad value is kept, a
//!    hand edit is not picked up without a restart, or a console edit loses
//!    the person's comments; a setting has no effect (name, wake on network).
//! 5. Sleep does not turn on wake-up (on the adapter's own phy) first, or
//!    turns it on when the setting is off; restart, shut down or lock
//!    do not reach their program; restart does not warn about the disk password.
//! 6. A theme the other computer lacks is not sent, arrives changed, or Omarchy
//!    is not asked to apply it; applying the theme it already has runs again.
//! 7. A repair claims success it did not have; restarting ibara does not end the process.
//! 8. Approvals from two computers are not merged; someone who may not answer
//!    them sees them; an answered one stays listed; an approval does not say
//!    which action it holds and on what; a computer that stops answering
//!    loses its approvals from the list.
//! 9. A computer that denies a command is not said plainly.
//! 10. "While you were away" shows events already seen, misses new ones, or
//!    shows computers with nothing new; `ibara away` differs.
//! 11. Wake does not send the right bytes, is not sent from another computer
//!    on the sleeping one's network, or claims to have been sent when no
//!    computer there is on, or when this computer is on another network that
//!    uses the same private range.
//!
//! `IBARA_E2E_EVIDENCE=DIR` keeps every console exchange as JSON lines in DIR.

mod support;

use serde_json::{Value, json};
use std::fs;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use support::*;

const TULIP1_MAC: &str = "02:00:5e:71:00:01";
const HAZEL_MAC: &str = "10:20:30:40:50:60";
/// The routers of Tulip1's home network, Vesper's own network and a café
/// that happens to use Tulip1's home range.
const HOME_GATEWAY: &str = "02:00:5e:10:00:01";
const VESPER_GATEWAY: &str = "02:00:5e:50:00:01";
const CAFE_GATEWAY: &str = "02:00:5e:ca:fe:01";

/// `ip` answering from `../net` next to it: the default route, one link, its
/// IPv4 addresses, every address, or the neighbours.
const FAKE_IP: &str = r#"#!/bin/sh
d="$(dirname "$0")/../net"
case "$*" in
  "-j -4 route show default") cat "$d/route.json" ;;
  "-j link show dev "*) cat "$d/link-$5.json" ;;
  "-j -4 addr show dev "*) cat "$d/addr-$6.json" ;;
  "-j -4 addr show") cat "$d/addr.json" ;;
  "-j -4 neigh show") cat "$d/neigh.json" ;;
  *) echo "fake ip: $*" >&2; exit 1 ;;
esac
"#;

/// `iw dev` from `../net/iw-dev.txt`; every phy can wake on a magic packet.
const FAKE_IW: &str = r#"#!/bin/sh
d="$(dirname "$0")/../net"
case "$*" in
  dev) cat "$d/iw-dev.txt" ;;
  "phy "*" info") printf 'Wiphy %s\n\tWoWLAN support:\n\t\t * wake up on magic packet\n' "$2" ;;
  *) echo "fake iw: $*" >&2; exit 1 ;;
esac
"#;

/// A program that logs its name and arguments to `../log`.
fn logging(name: &str, extra: &str) -> String {
    format!("#!/bin/sh\nprintf '%s\\n' \"{name} $*\" >> \"$(dirname \"$0\")/../log\"\n{extra}\n")
}

/// One computer's network: `(ifname, mac, address, prefix, gateway's mac)`,
/// each link's gateway at `.1`; the first is the default route.
fn network(world: &World, name: &str, links: &[(&str, &str, &str, u8, &str)]) {
    let net = world.machine_bin(name).join("../net");
    fs::create_dir_all(&net).unwrap();
    let gateway = |address: &str| format!("{}.1", address.rsplit_once('.').unwrap().0);
    let (first, _, first_address, ..) = links[0];
    let route = json!([{"dst": "default", "gateway": gateway(first_address), "dev": first, "metric": 600}]);
    fs::write(net.join("route.json"), route.to_string()).unwrap();
    let mut all = Vec::new();
    let mut neighbours = Vec::new();
    let mut iw = String::new();
    for (index, (ifname, mac, address, prefix, gateway_mac)) in links.iter().enumerate() {
        let addr = json!({"ifname": ifname, "addr_info": [{"family": "inet", "local": address, "prefixlen": prefix, "scope": "global"}]});
        fs::write(net.join(format!("addr-{ifname}.json")), json!([addr]).to_string()).unwrap();
        fs::write(net.join(format!("link-{ifname}.json")), json!([{"ifname": ifname, "link_type": "ether", "address": mac}]).to_string()).unwrap();
        all.push(addr);
        neighbours.push(json!({"dst": gateway(address), "dev": ifname, "lladdr": gateway_mac, "state": ["REACHABLE"]}));
        iw.push_str(&format!("phy#{index}\n\tInterface {ifname}\n\t\ttype managed\n"));
    }
    all.push(json!({"ifname": "lo", "addr_info": [{"family": "inet", "local": "127.0.0.1", "prefixlen": 8}]}));
    fs::write(net.join("addr.json"), json!(all).to_string()).unwrap();
    fs::write(net.join("neigh.json"), json!(neighbours).to_string()).unwrap();
    fs::write(net.join("iw-dev.txt"), iw).unwrap();
}

/// The programs a computer runs for everyday commands, as logging fakes.
fn machine(world: &World, name: &str) {
    let bin = world.machine_bin(name);
    write_executable(&bin.join("ip"), FAKE_IP);
    write_executable(&bin.join("iw"), FAKE_IW);
    write_executable(&bin.join("journalctl"), &logging("journalctl", "for n in 1 2 3 4 5; do echo \"2026-09-27T10:0$n:00+0000 tulip1 ibarad[42]: line $n\"; done"));
    let theme_set = "mkdir -p \"$HOME/.local/state/omarchy/current\" && printf '%s\\n' \"$1\" > \"$HOME/.local/state/omarchy/current/theme.name\"\n\
         echo \"OMARCHY_PATH=$OMARCHY_PATH\" >> \"$(dirname \"$0\")/../log\"";
    write_executable(&bin.join("omarchy-theme-set"), &logging("omarchy-theme-set", theme_set));
    for program in ["omarchy-system-lock", "systemctl", "loginctl"] {
        write_executable(&bin.join(program), &logging(program, ""));
    }
}

fn machine_log(world: &World, name: &str) -> Vec<String> {
    fs::read_to_string(world.machine_bin(name).join("../log")).unwrap_or_default().lines().map(str::to_string).collect()
}

/// Tulip1's root power helper world: its adapters and disk layout as
/// inspected, and fake `systemctl`, `iw` and `systemd-cryptenroll`.
fn power_fixture(world: &World, name: &str) -> PathBuf {
    let root = world.root.join(format!("power-{name}"));
    for dir in ["bin", "sys/class/net/wlp2s0/device", "sys/class/net/wlp2s0/phy80211", "sys/block", "sys/dev/block", "sys/class/block", "proc/self"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::write(root.join("sys/class/net/wlp2s0/phy80211/name"), "phy0\n").unwrap();
    for program in ["systemctl", "iw"] {
        write_executable(&root.join("bin").join(program), &logging(program, ""));
    }
    write_executable(&root.join("bin/systemd-cryptenroll"), &logging("systemd-cryptenroll", "printf 'SLOT TYPE\\n   0 password\\n'"));
    let block = |name: &str, numbers: &str, dm: Option<(&str, &str)>, slaves: &[&str]| {
        let dir = root.join("sys/block").join(name);
        fs::create_dir_all(dir.join("slaves")).unwrap();
        fs::write(dir.join("dev"), format!("{numbers}\n")).unwrap();
        if let Some((dm_name, uuid)) = dm {
            fs::create_dir_all(dir.join("dm")).unwrap();
            fs::write(dir.join("dm/name"), format!("{dm_name}\n")).unwrap();
            fs::write(dir.join("dm/uuid"), format!("{uuid}\n")).unwrap();
        }
        for slave in slaves {
            std::os::unix::fs::symlink(root.join("sys/block").join(slave), dir.join("slaves").join(slave)).unwrap();
        }
        std::os::unix::fs::symlink(&dir, root.join("sys/dev/block").join(numbers)).unwrap();
        std::os::unix::fs::symlink(&dir, root.join("sys/class/block").join(name)).unwrap();
    };
    block("mmcblk2p2", "179:2", None, &[]);
    block("dm-0", "253:0", Some(("root", "CRYPT-LUKS2-5e2d9c413a7b4c1d9e8f0a1b2c3d4e5f-root")), &["mmcblk2p2"]);
    fs::write(root.join("proc/self/mountinfo"), "32 2 0:29 /@ / rw,relatime shared:1 - btrfs /dev/mapper/root rw,subvol=/@\n").unwrap();
    fs::write(root.join("proc/cmdline"), "cryptdevice=PARTUUID=3c1e5a2b-7d40-4f86-9a1b-2e6c0d8f4b17:root root=/dev/mapper/root rw\n").unwrap();
    root
}

/// The helper answers first and then runs its command: wait for `lines` entries.
fn power_log_at(fixture: &Path, lines: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = power_log(fixture);
        if log.len() >= lines || Instant::now() > deadline {
            return log;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn power_log(fixture: &Path) -> Vec<String> {
    fs::read_to_string(fixture.join("log")).unwrap_or_default().lines().map(str::to_string).collect()
}

/// Pair `console` to `node` as the same person's computer; its computer id.
fn add_own(console: &mut Console, node: &str) -> String {
    let started = console.ok("pair-start", &[node]);
    let paired = console.settled(started["request_id"].as_str().unwrap());
    assert_eq!(paired["state"], "paired", "{paired}");
    paired["computer_id"].as_str().unwrap().to_string()
}

/// Pair `console` to `node`, accepted by `there` (a console on that computer).
fn add_accepted(console: &mut Console, there: &mut Console, node: &str) -> String {
    let started = console.ok("pair-start", &[node]);
    let request = started["request_id"].as_str().unwrap().to_string();
    assert_eq!(there.ok("pair-answer", &[&request, "accept"])["state"], "paired");
    let paired = console.settled(&request);
    paired["computer_id"].as_str().unwrap().to_string()
}

fn epoch(console: &mut Console, computer: &str) -> String {
    console.ok("operator-session", &["--computer", computer])["controller_epoch"].as_str().unwrap().to_string()
}

/// A per-computer command's envelope.
fn on(console: &mut Console, computer: &str, epoch: &str, command: &str, extra: &[&str]) -> Value {
    let mut args = vec!["--computer", computer, "--epoch", epoch];
    args.extend_from_slice(extra);
    console.ask(command, &args)
}

/// A per-computer command that must succeed; the other computer's result.
fn ok_on(console: &mut Console, computer: &str, epoch: &str, command: &str, extra: &[&str]) -> Value {
    let envelope = on(console, computer, epoch, command, extra);
    assert!(envelope["error"].is_null(), "{command} {extra:?}: {envelope}");
    envelope["data"]["result"].clone()
}

fn setting<'a>(sections: &'a Value, key: &str) -> &'a Value {
    sections["sections"].as_array().unwrap().iter().flat_map(|s| s["settings"].as_array().unwrap()).find(|s| s["key"] == key).unwrap()
}

fn directory_row(console: &mut Console, computer: &str) -> Value {
    console.ok("directory", &[])["computers"].as_array().unwrap().iter().find(|c| c["computer_id"] == computer).unwrap().clone()
}

#[test]
fn a_computer_is_looked_after_from_another_over_the_pairing_route() {
    let world = World::new("everyday");
    for name in ["tulip1", "vesper"] {
        machine(&world, name);
    }
    network(&world, "tulip1", &[("wlp2s0", TULIP1_MAC, "10.20.30.196", 24, HOME_GATEWAY)]);
    network(&world, "vesper", &[("wlp46s0", "02:00:5e:76:00:01", "192.168.50.20", 24, VESPER_GATEWAY)]);
    let power = power_fixture(&world, "tulip1");
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    tulip1.serve_power(&power);
    // Vesper's own ibara vouches for its console's request, so Tulip1 accepts it at once.
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let id = add_own(&mut vesper, "tulip1");
    let e = epoch(&mut vesper, &id);

    // Health: the computer as it is, how to wake it, and whether a restart stops at the disk password.
    let health = ok_on(&mut vesper, &id, &e, "operator-health", &[]);
    assert_eq!(health["name"], Value::Null, "no name set in its settings: {health}");
    assert!(health["load"].is_number() && health["uptime_s"].is_u64() && health["cpus"].as_u64().is_some_and(|n| n >= 1), "{health}");
    assert!(health["memory"]["total_mb"].as_f64().unwrap() > health["memory"]["used_mb"].as_f64().unwrap(), "{health}");
    assert!(health["disk"]["total_gb"].as_f64().unwrap() > 0.0, "{health}");
    assert_eq!(health["repair"], json!({"last": null, "needs_person": null}));
    let wake = json!({"mac": TULIP1_MAC, "ifname": "wlp2s0", "kind": "wifi", "subnet": "10.20.30.0/24", "from_off": false, "gateway_mac": HOME_GATEWAY});
    assert_eq!(health["wake"], wake);
    assert_eq!(health["disk_password"], true, "Tulip1's disk asks for its password at boot");
    assert_eq!(directory_row(&mut vesper, &id)["wake"], wake, "kept for when Tulip1 sleeps");
    let status = ok_on(&mut vesper, &id, &e, "operator-status", &[]);
    assert_eq!((status["paused"].as_bool(), status["pause_origin"].as_str(), &status["disk_password"]), (Some(true), Some("system"), &json!(true)));

    // Logs: the newest lines of ibara's own unit, or of its screen sharing (the stream's own log).
    let logs = ok_on(&mut vesper, &id, &e, "operator-logs", &["--which", "ibara", "--lines", "3"]);
    let lines: Vec<&str> = logs["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    assert!(lines.len() == 3 && lines[2].ends_with("line 5") && lines[0].ends_with("line 3"), "{logs}");
    let viewer = ok_on(&mut vesper, &id, &e, "operator-logs", &["--which", "viewer"]);
    assert_eq!(viewer["lines"], json!([]), "no screen sharing has run yet");
    let stream_log = tulip1.root.join("state/stream");
    std::fs::create_dir_all(&stream_log).unwrap();
    std::fs::write(stream_log.join("ibara-stream.log"), "[1] Info: starting\n[2] Info: CLIENT CONNECTED\n[3] Info: CLIENT DISCONNECTED\n").unwrap();
    let viewer = ok_on(&mut vesper, &id, &e, "operator-logs", &["--which", "viewer", "--lines", "2"]);
    assert_eq!(viewer["lines"], json!(["[2] Info: CLIENT CONNECTED", "[3] Info: CLIENT DISCONNECTED"]));
    let asked: Vec<String> = machine_log(&world, "tulip1").into_iter().filter(|l| l.starts_with("journalctl")).collect();
    assert_eq!(asked, ["journalctl --user -u agent-computer.service -n 3 --no-pager --output=short-iso"]);

    // Settings for this computer, in its own settings file.
    let sections = ok_on(&mut vesper, &id, &e, "operator-settings", &["get"]);
    let ids: Vec<&str> = sections["sections"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["general", "display", "recovery", "power", "control", "agents"]);
    assert_eq!((setting(&sections, "name")["value"].as_str(), setting(&sections, "name")["default"].as_str()), (Some("Tulip1"), Some("Tulip1")));
    let size = setting(&sections, "virtual_display_size");
    assert_eq!((size["type"].as_str(), size["value"].as_str(), size["scope"].as_str()), (Some("choice"), Some("1920x1080"), Some("computer")));
    assert!(size["choices"].as_array().unwrap().contains(&json!({"value": "2560x1440", "label": "2560 × 1440"})), "{size}");
    for key in ["auto_resume", "self_repair", "wake_on_network", "shared_clipboard"] {
        assert_eq!(setting(&sections, key)["value"], true, "{key} is on by default");
    }
    let set = ok_on(&mut vesper, &id, &e, "operator-settings", &["set", "virtual_display_size", "2560x1440"]);
    assert_eq!((set["key"].as_str(), set["value"].as_str()), (Some("virtual_display_size"), Some("2560x1440")));
    let file = tulip1.home().join(".config/ibara/settings.toml");
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("[display]\nvirtual_display_size = \"2560x1440\""), "{text}");
    let refused = on(&mut vesper, &id, &e, "operator-settings", &["set", "preview_seconds", "99"]);
    assert_eq!(refused["error"]["message"], "Picture interval is a whole number from 1 to 30.", "{refused}");

    // A hand edit applies at once, and a later change keeps its comment.
    fs::write(&file, format!("{text}\n[general]\nname = \"Spud\" # renamed by hand\n\n[power]\nwake_on_network = false\n")).unwrap();
    let sections = ok_on(&mut vesper, &id, &e, "operator-settings", &["get"]);
    assert_eq!((setting(&sections, "name")["value"].as_str(), &setting(&sections, "wake_on_network")["value"]), (Some("Spud"), &json!(false)));
    let health = ok_on(&mut vesper, &id, &e, "operator-health", &[]);
    assert_eq!((health["name"].as_str(), &health["wake"]), (Some("Spud"), &Value::Null), "{health}");
    assert_eq!(directory_row(&mut vesper, &id)["label"], "Spud", "this console follows the name the computer gives itself");

    // Sleep with wake from the network off: no wake-up is turned on.
    let slept = ok_on(&mut vesper, &id, &e, "operator-power", &["--action", "sleep"]);
    assert_eq!((slept["state"].as_str(), slept["wake"].as_str()), (Some("started"), Some("off")), "{slept}");
    assert_eq!(power_log_at(&power, 1), ["systemctl suspend"]);
    let reset = ok_on(&mut vesper, &id, &e, "operator-settings", &["reset", "wake_on_network"]);
    assert_eq!(reset["value"], true);
    assert!(fs::read_to_string(&file).unwrap().contains("name = \"Spud\" # renamed by hand"), "the person's comment stays");
    let slept = ok_on(&mut vesper, &id, &e, "operator-power", &["--action", "sleep"]);
    assert_eq!(slept["wake"], "enabled", "{slept}");
    assert_eq!(power_log_at(&power, 3)[1..], ["iw phy phy0 wowlan enable magic-packet", "systemctl suspend"], "wake-up first, on the adapter's own phy");
    let restarted = ok_on(&mut vesper, &id, &e, "operator-power", &["--action", "restart"]);
    assert_eq!((restarted["state"].as_str(), &restarted["disk_password_warning"]), (Some("started"), &json!(true)));
    ok_on(&mut vesper, &id, &e, "operator-power", &["--action", "shutdown"]);
    assert_eq!(power_log_at(&power, 5)[3..], ["systemctl reboot", "systemctl poweroff"]);
    ok_on(&mut vesper, &id, &e, "operator-power", &["--action", "lock"]);
    let log = machine_log(&world, "tulip1");
    assert!(log.iter().any(|l| l == "omarchy-system-lock "), "{log:?}");
    let reset = ok_on(&mut vesper, &id, &e, "operator-settings", &["reset", "--section", "display"]);
    assert_eq!(setting(&reset, "virtual_display_size")["value"], "1920x1080");
    assert!(!fs::read_to_string(&file).unwrap().contains("[display]"));

    // This console's own settings, in its own file.
    let own = vesper.ok("settings", &["get"]);
    let ids: Vec<&str> = own["sections"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["approvals", "notifications", "updates", "files", "fleet"]);
    assert_eq!(setting(&own, "fleet_preview_seconds")["scope"], "console");
    assert_eq!(vesper.ok("settings", &["set", "fleet_preview_seconds", "10"])["value"], 10);
    let refused = vesper.ask("settings", &["set", "download_folder", "Downloads"]);
    assert!(refused["error"]["message"].as_str().unwrap().contains("full path"), "{refused}");
    let console_file = fs::read_to_string(vesper.home.join(".config/ibara/settings.toml")).unwrap();
    assert!(console_file.contains("[console]\nfleet_preview_seconds = 10"), "{console_file}");

    // Theme: this computer's theme goes to Tulip1, which lacks it, and is applied there once.
    let theme = vesper.home.join(".config/omarchy/themes/nightwire");
    fs::create_dir_all(theme.join("backgrounds")).unwrap();
    fs::write(theme.join("colors.toml"), "accent = \"#7aa2f7\"\n").unwrap();
    fs::write(theme.join("hyprland.lua"), "-- the person's own theme\n").unwrap();
    let picture: Vec<u8> = (0..700_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    fs::write(theme.join("backgrounds/1-city.png"), &picture).unwrap();
    fs::create_dir_all(vesper.home.join(".local/state/omarchy/current")).unwrap();
    fs::write(vesper.home.join(".local/state/omarchy/current/theme.name"), "nightwire\n").unwrap();
    let fleet = vesper.ok("theme-fleet", &[]);
    assert_eq!(fleet["theme"], "nightwire");
    assert_eq!(fleet["results"], json!([{"computer_id": id, "label": "Spud", "state": "applied", "message": ""}]), "{fleet}");
    let arrived = tulip1.home().join(".config/omarchy/themes/nightwire");
    assert_eq!(fs::read(arrived.join("backgrounds/1-city.png")).unwrap(), picture, "the picture arrives unchanged (two pieces)");
    assert_eq!(fs::read_to_string(arrived.join("hyprland.lua")).unwrap(), "-- the person's own theme\n");
    let applied: Vec<String> = machine_log(&world, "tulip1").into_iter().filter(|l| l.starts_with("omarchy-theme-set") || l.starts_with("OMARCHY_PATH")).collect();
    assert_eq!(applied, ["omarchy-theme-set nightwire".to_string(), format!("OMARCHY_PATH={}", world.root.join("omarchy").display())]);
    assert_eq!(vesper.ok("theme-fleet", &[])["results"][0]["state"], "applied");
    let single = vesper.ok("operator-theme", &["apply", "nightwire", "--computer", &id, "--epoch", &e]);
    assert_eq!(single["state"], "applied");
    assert_eq!(machine_log(&world, "tulip1").iter().filter(|l| l.starts_with("omarchy-theme-set")).count(), 1, "a theme already shown is not applied again");

    // Tasks and procedures come over the same route.
    assert_eq!(ok_on(&mut vesper, &id, &e, "operator-tasks", &[])["tasks"], json!([]));
    assert!(ok_on(&mut vesper, &id, &e, "operator-procedures", &[])["items"].is_array());

    // Repairs say what happened; restarting ibara ends the process for its unit to start again.
    let display = ok_on(&mut vesper, &id, &e, "operator-repair", &["reconnect_display"]);
    assert_eq!(display["state"], "still_broken", "no desktop runs in this test: {display}");
    let viewer = ok_on(&mut vesper, &id, &e, "operator-repair", &["restart_viewer"]);
    assert_eq!((viewer["state"].as_str(), viewer["message"].as_str()), (Some("still_broken"), Some("Screen sharing is not set up on this computer.")));
    let restart = ok_on(&mut vesper, &id, &e, "operator-repair", &["restart_ibara"]);
    assert_eq!(restart["state"], "restarting", "{restart}");
    let mut tulip1 = tulip1;
    let deadline = Instant::now() + Duration::from_secs(20);
    let exit = loop {
        if let Some(status) = tulip1.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "ibarad did not restart");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(!exit.success(), "a failed exit, so Restart=on-failure starts it again: {exit:?}");
    world.record(json!({"power_helper_log": power_log(&power), "tulip1_log": machine_log(&world, "tulip1")}));
}

#[test]
fn approvals_and_history_come_from_every_computer() {
    let world = World::new("attention");
    for name in ["tulip1", "vesper", "command"] {
        machine(&world, name);
    }
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut on_tulip1 = Console::start(&world, "tulip1", node("tulip1"), Some(&tulip1));
    let mut riley = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let mut dana = Console::start(&world, "command", node("command"), None);
    let (t1, vx) = (add_own(&mut riley, "tulip1"), add_own(&mut riley, "vesper"));
    let (dana_t1, dana_vx) = (add_accepted(&mut dana, &mut on_tulip1, "tulip1"), add_accepted(&mut dana, &mut riley, "vesper"));

    // Riley lets Dana take control only after asking; Dana asks on both computers.
    for computer in [&t1, &vx] {
        let e = epoch(&mut riley, computer);
        let revision = ok_on(&mut riley, computer, &e, "operator-access", &[])["revision"].clone();
        let body = json!({"subject": "command", "capability": "control", "rule": "ask", "expected_revision": revision}).to_string();
        ok_on(&mut riley, computer, &e, "operator-access-set", &[&body]);
    }
    for computer in [&dana_t1, &dana_vx] {
        let e = epoch(&mut dana, computer);
        let status = ok_on(&mut dana, computer, &e, "operator-status", &[]);
        let (owner, revision) = (status["owner"].as_str().unwrap().to_string(), status["ownership_revision"].as_str().unwrap().to_string());
        let asked = on(&mut dana, computer, &e, "operator-control", &["--op", "take_control", "--owner", &owner, "--revision", &revision]);
        assert_eq!(asked["data"]["result"]["state"], "pending_approval", "{asked}");
    }

    // Riley sees both, from both computers; Dana, who may not answer them, sees none.
    let attention = riley.ok("fleet-attention", &[]);
    assert_eq!(attention["count"], 2, "{attention}");
    let items = attention["items"].as_array().unwrap();
    let mut labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    labels.sort();
    assert_eq!(labels, ["Tulip1", "Vesper"]);
    for item in items {
        assert_eq!((item["kind"].as_str(), item["summary"].as_str()), (Some("approval"), Some("command asks to take control.")), "{item}");
        assert!(item["ref"].as_str().unwrap().starts_with("att_") && item["at"].is_string(), "{item}");
    }
    assert_eq!(dana.ok("fleet-attention", &[]), json!({"items": [], "count": 0, "unreachable": []}));

    // Dana may not restart Tulip1, and hears so plainly.
    let e = epoch(&mut dana, &dana_t1);
    let denied = on(&mut dana, &dana_t1, &e, "operator-power", &["--action", "restart"]);
    assert_eq!(
        (denied["error"]["code"].as_str(), denied["error"]["message"].as_str()),
        (Some("PERMISSION_DENIED"), Some("Tulip1 doesn't let this computer restart, shut down, sleep, lock or update it. Its owner can change that under Access."))
    );

    // Answering one leaves the other.
    let on_t1 = items.iter().find(|i| i["computer_id"] == t1.as_str()).unwrap()["ref"].as_str().unwrap().to_string();
    let e = epoch(&mut riley, &t1);
    let answered = ok_on(&mut riley, &t1, &e, "operator-answer-attention", &[&on_t1, "deny"]);
    assert_eq!(answered["item"]["answer"], "deny");
    let attention = riley.ok("fleet-attention", &[]);
    assert_eq!((attention["count"].as_u64(), attention["items"][0]["computer_id"].as_str()), (Some(1), Some(vx.as_str())), "{attention}");

    // While you were away: both computers' news, once.
    let away = riley.ok("away", &[]);
    assert_eq!(away["since"], 0, "never looked before");
    let news: Vec<&Value> = away["computers"].as_array().unwrap().iter().collect();
    assert_eq!(news.len(), 2, "{away}");
    for computer in &news {
        let kinds: Vec<&str> = computer["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"access") && kinds.contains(&"attention.raised"), "{computer}");
    }
    let seen = riley.ok("away-seen", &[]);
    assert!(seen["since"].as_i64().unwrap() > 0, "{seen}");
    assert_eq!(riley.ok("away", &[])["computers"], json!([]), "nothing new since it was seen");
    let revision = ok_on(&mut riley, &t1, &e, "operator-access", &[])["revision"].clone();
    let body = json!({"subject": "command", "capability": "control", "rule": "deny", "expected_revision": revision}).to_string();
    ok_on(&mut riley, &t1, &e, "operator-access-set", &[&body]);
    let away = riley.ok("away", &[]);
    let news = away["computers"].as_array().unwrap();
    assert_eq!((news.len(), news[0]["label"].as_str(), news[0]["events"][0]["kind"].as_str()), (1, Some("Tulip1"), Some("access")), "{away}");

    // `ibara away` tells the same, in plain text.
    let mut cli = Command::new(IBARA);
    world.machine_env(&mut cli, node("vesper"));
    let out = cli.arg("away").env("HOME", &riley.home).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    world.record(json!({"ibara away": text, "status": out.status.code()}));
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.starts_with("Tulip1 — 1 new since ") && text.contains(news[0]["events"][0]["summary"].as_str().unwrap()), "{text}");

    // An approval names the held action and what it acts on, so Approve is
    // never a blind "change this computer's settings"; one not recognised
    // names the capability it needs in plain words, and its details carry the
    // exact request.
    let e = epoch(&mut riley, &t1);
    let revision = ok_on(&mut riley, &t1, &e, "operator-access", &[])["revision"].clone();
    let body = json!({"subject": "command", "capability": "administer", "rule": "ask", "expected_revision": revision}).to_string();
    ok_on(&mut riley, &t1, &e, "operator-access-set", &[&body]);
    let e = epoch(&mut dana, &dana_t1);
    let revision = ok_on(&mut dana, &dana_t1, &e, "operator-access", &[])["revision"].clone();
    let agents = json!({"subject": "command", "capability": "agents", "rule": "allow", "expected_revision": revision}).to_string();
    let unknown = json!({"subject": "command", "capability": "telepathy", "rule": "allow", "expected_revision": revision}).to_string();
    let restart = ok_on(&mut dana, &dana_t1, &e, "operator-power", &["--action", "restart"]);
    assert_eq!(restart["state"], "pending_approval", "{restart}");
    let restart_ref = restart["attention"].as_str().unwrap().to_string();
    for (command, args) in [
        ("operator-settings", vec!["set", "name", "Spud"]),
        ("operator-access-set", vec![agents.as_str()]),
        ("operator-access-set", vec![unknown.as_str()]),
        ("operator-answer-attention", vec![restart_ref.as_str(), "approve"]),
    ] {
        let held = ok_on(&mut dana, &dana_t1, &e, command, &args);
        assert_eq!(held["state"], "pending_approval", "{command} {args:?}: {held}");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let listed = loop {
        let attention = riley.ok("fleet-attention", &[]);
        let items = attention["items"].as_array().unwrap();
        let mut on_t1: Vec<(String, Value)> =
            items.iter().filter(|i| i["computer_id"] == t1.as_str()).map(|i| (i["summary"].as_str().unwrap().to_string(), i["details"].clone())).collect();
        if on_t1.len() >= 5 || Instant::now() > deadline {
            on_t1.sort_by(|a, b| a.0.cmp(&b.0));
            break on_t1;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let summaries: Vec<&str> = listed.iter().map(|(summary, _)| summary.as_str()).collect();
    assert_eq!(
        summaries,
        [
            "command asks to approve the request “command asks to restart this computer”.",
            "command asks to change the setting “Computer name” to “Spud”.",
            "command asks to let itself run agent tasks.",
            "command asks to manage this computer in a way ibara can't describe; check the details.",
            "command asks to restart this computer.",
        ],
        "{listed:#?}"
    );
    assert_eq!(listed[3].1["request"]["capability"], "telepathy", "{listed:#?}");

    // Vesper stops answering: its approval stays listed, marked, instead of
    // vanishing (which would close its desktop notice for good).
    drop(vesper_target);
    let deadline = Instant::now() + Duration::from_secs(20);
    let attention = loop {
        let attention = riley.ok("fleet-attention", &[]);
        if attention["unreachable"] == json!([vx]) || Instant::now() > deadline {
            break attention;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(attention["unreachable"], json!([vx]), "{attention}");
    let kept: Vec<&Value> = attention["items"].as_array().unwrap().iter().filter(|i| i["computer_id"] == vx.as_str()).collect();
    assert_eq!(kept.len(), 1, "{attention}");
    assert_eq!((kept[0]["summary"].as_str(), kept[0]["unreachable"].as_bool()), (Some("command asks to take control."), Some(true)));
}

/// What the paired computer `subject`'s agents do on `computer` when they
/// send: "ask" or "allow", as that computer's own access table says.
fn sends(console: &mut Console, computer: &str, subject: &str) -> String {
    let e = epoch(console, computer);
    let access = ok_on(console, computer, &e, "operator-access", &[]);
    let row = access["rows"].as_array().unwrap().iter().find(|r| r["subject"] == subject).unwrap_or_else(|| panic!("{access}"));
    row["effects"]["send"].as_str().unwrap().to_string()
}

/// Ask before agents send, spend or delete, from a console's Settings.
/// Failure cases:
/// 1. Off in Settings does not reach every computer this console manages,
///    or reaches a computer added later only if it is changed again.
/// 2. A computer's own choice is overwritten by Settings, or does not beat it.
/// 3. On again leaves computers not asking.
/// 4. It changes, or asks to change, a computer this console may not manage.
#[test]
fn ask_first_off_in_settings_reaches_every_computer_but_one_that_chose_for_itself() {
    let world = World::new("askfirst");
    for name in ["tulip1", "vesper", "hazel", "command"] {
        machine(&world, name);
    }
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let hazel = Target::start(&world, node("hazel"), Some("Hazel"), 300_000);
    let mut on_hazel = Console::start(&world, "hazel", node("hazel"), Some(&hazel));
    let mut riley = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let mut dana = Console::start(&world, "command", node("command"), None);
    let (t1, vx) = (add_own(&mut riley, "tulip1"), add_own(&mut riley, "vesper"));
    add_accepted(&mut dana, &mut on_hazel, "hazel");
    assert_eq!((sends(&mut riley, &t1, "vesper"), sends(&mut riley, &vx, "vesper")), ("ask".into(), "ask".into()));

    // Tulip1 chooses for itself; Settings turns approvals off for the rest.
    let e = epoch(&mut riley, &t1);
    ok_on(&mut riley, &t1, &e, "operator-settings", &["set", "ask_first", "on"]);
    assert_eq!(riley.ok("settings", &["set", "agents_ask_first", "false"])["value"], false);
    riley.ok("fleet-attention", &[]);
    assert_eq!((sends(&mut riley, &t1, "vesper"), sends(&mut riley, &vx, "vesper")), ("ask".into(), "allow".into()));
    let own = ok_on(&mut riley, &t1, &e, "operator-settings", &["get"]);
    let choice = setting(&own, "ask_first");
    assert_eq!((choice["value"].as_str(), &choice["choices"][0]["label"]), (Some("on"), &json!("Same as in Settings (off)")), "{choice}");
    ok_on(&mut riley, &t1, &e, "operator-settings", &["reset", "--section", "agents"]);
    assert_eq!(sends(&mut riley, &t1, "vesper"), "allow", "back to the choice from Settings, which the reset keeps");

    // A computer this console may not manage is neither changed nor asked.
    assert_eq!(dana.ok("settings", &["set", "agents_ask_first", "false"])["value"], false);
    dana.ok("fleet-attention", &[]);
    let hazel_file = hazel.home().join(".config/ibara/settings.toml");
    assert!(!fs::read_to_string(&hazel_file).unwrap_or_default().contains("fleet_ask_first"), "Dana may not manage Hazel");

    // A computer added later gets it too.
    let hz = add_own(&mut riley, "hazel");
    assert_eq!(riley.ok("fleet-attention", &[])["count"], 0, "nobody was asked to let Dana change Hazel");
    assert_eq!(sends(&mut riley, &hz, "vesper"), "allow");

    // On again: every computer asks again, and the file loses the line.
    riley.ok("settings", &["set", "agents_ask_first", "true"]);
    riley.ok("fleet-attention", &[]);
    for computer in [&t1, &vx, &hz] {
        assert_eq!(sends(&mut riley, computer, "vesper"), "ask", "{computer}");
    }
    let file = fs::read_to_string(tulip1.home().join(".config/ibara/settings.toml")).unwrap_or_default();
    assert!(!file.contains("fleet_ask_first"), "{file}");
}

#[test]
fn a_sleeping_computer_is_woken_from_its_own_network() {
    let catcher = UdpSocket::bind("127.0.0.1:0").unwrap();
    catcher.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut world = World::new("wake");
    world.wake_address = Some(catcher.local_addr().unwrap().to_string());
    for name in ["tulip1", "hazel", "vesper"] {
        machine(&world, name);
    }
    network(&world, "tulip1", &[("wlp2s0", TULIP1_MAC, "10.20.30.196", 24, HOME_GATEWAY)]);
    network(&world, "hazel", &[("wlp3s0", HAZEL_MAC, "10.20.30.50", 24, HOME_GATEWAY)]);
    network(&world, "vesper", &[("wlp46s0", "02:00:5e:76:00:01", "192.168.50.20", 24, VESPER_GATEWAY)]);
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    let hazel = Target::start(&world, node("hazel"), Some("Hazel"), 300_000);
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let (t1, hl) = (add_own(&mut vesper, "tulip1"), add_own(&mut vesper, "hazel"));
    // The console takes a computer by its name or `cmp_` id too, as agents see
    // it: the wake information each health read records below is kept for
    // the right computer.
    let cmp = |id: &str| id.replace("computer_", "cmp_");
    for named in ["tulip1".to_string(), cmp(&hl)] {
        let e = epoch(&mut vesper, &named);
        ok_on(&mut vesper, &named, &e, "operator-health", &[]);
    }
    let unknown = vesper.ask("wake", &["walnut"]);
    let message = unknown["error"]["message"].as_str().unwrap_or_default();
    let listed = [format!("\"Tulip1\" ({}, {t1})", cmp(&t1)), format!("\"Hazel\" ({}, {hl})", cmp(&hl))];
    assert!(message.starts_with("\"walnut\" is not one of your computers") && listed.iter().all(|l| message.contains(l.as_str())), "{unknown}");
    let packet = |mac: &str| -> Vec<u8> {
        let bytes: Vec<u8> = mac.split(':').map(|b| u8::from_str_radix(b, 16).unwrap()).collect();
        let mut out = vec![0xff; 6];
        (0..16).for_each(|_| out.extend_from_slice(&bytes));
        out
    };
    let caught = || {
        let mut buffer = [0u8; 256];
        let (n, _) = catcher.recv_from(&mut buffer).expect("a magic packet");
        buffer[..n].to_vec()
    };

    // Tulip1 sleeps; this computer is on another network, Hazel is on Tulip1's.
    drop(tulip1);
    let woke = vesper.ok("wake", &["Tulip1"]);
    assert_eq!(woke, json!({"computer_id": t1, "sent_via": "Hazel", "state": "sent", "sure": true}));
    assert_eq!(caught(), packet(TULIP1_MAC));

    // With Hazel off too, nobody on that network can send it.
    drop(hazel);
    let nobody = vesper.ask("wake", &[&t1]);
    assert_eq!(nobody["error"]["message"], "No computer on Tulip1's network is on to wake it.", "{nobody}");

    // At a café that uses the same range as Tulip1's home, this computer is
    // not on Tulip1's network: it neither sends nor says it did.
    network(&world, "vesper", &[("wlp46s0", "02:00:5e:76:00:01", "10.20.30.77", 24, CAFE_GATEWAY)]);
    let cafe = vesper.ask("wake", &[&t1]);
    assert_eq!(cafe["error"]["message"], "No computer on Tulip1's network is on to wake it.", "{cafe}");
    catcher.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    assert!(catcher.recv_from(&mut [0u8; 256]).is_err(), "nothing was sent from the café");
    catcher.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

    // Once this computer is on Tulip1's network, it sends the packet itself.
    let home = [("wlp46s0", "02:00:5e:76:00:01", "192.168.50.20", 24, VESPER_GATEWAY), ("enp0s31f6", "02:00:5e:76:00:02", "10.20.30.26", 24, HOME_GATEWAY)];
    network(&world, "vesper", &home);
    let woke = vesper.ok("wake", &[&t1]);
    assert_eq!((woke["sent_via"].as_str(), woke["sure"].as_bool()), (Some("this computer"), Some(true)), "{woke}");
    assert_eq!(caught(), packet(TULIP1_MAC));
}

/// Failure cases this must catch:
/// 1. Watch set to Ask First reads as denied on the other computer, so it can
///    never ask and no approval is ever raised.
/// 2. Every picture or read raises its own approval, so a person approves
///    without end, or a waiting console raises one per try.
/// 3. The console does not say that a person there must approve.
/// 4. After Approve, watching is still refused, or the next picture or read
///    asks again.
/// 5. An approval outlives an access change, so a later edit (a Deny) leaves
///    the watcher watching.
/// 6. After Decline, the waiting console raises a new approval at once.
#[test]
fn watching_that_asks_first_asks_once_per_sitting() {
    let world = World::new("watch-ask");
    for name in ["tulip1", "vesper", "command"] {
        machine(&world, name);
    }
    let tulip1 = Target::start(&world, node("tulip1"), Some("Tulip1"), 300_000);
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut on_tulip1 = Console::start(&world, "tulip1", node("tulip1"), Some(&tulip1));
    let mut riley = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let mut dana = Console::start(&world, "command", node("command"), None);
    let t1 = add_own(&mut riley, "tulip1");
    let dana_t1 = add_accepted(&mut dana, &mut on_tulip1, "tulip1");
    let set_watch = |riley: &mut Console, rule: &str| {
        let e = epoch(riley, &t1);
        let revision = ok_on(riley, &t1, &e, "operator-access", &[])["revision"].clone();
        let body = json!({"subject": "command", "capability": "watch", "rule": rule, "expected_revision": revision}).to_string();
        ok_on(riley, &t1, &e, "operator-access-set", &[&body]);
    };
    // Open approvals on Tulip1 from Dana's computer, read on Tulip1 itself.
    let open_asks = |tulip1: &Target| -> Vec<String> {
        let (_, open) = tulip1.admin(&["attention", "open"]);
        let items = open["result"]["items"].as_array().unwrap().iter().filter(|i| i["principal"] == "command");
        items.map(|i| i["att_ref"].as_str().unwrap().to_string()).collect()
    };
    let answer = |riley: &mut Console, att_ref: &str, answer: &str| {
        let e = epoch(riley, &t1);
        ok_on(riley, &t1, &e, "operator-answer-attention", &[att_ref, answer]);
    };
    set_watch(&mut riley, "ask");

    // 1. Dana's console learns it may ask.
    let e = epoch(&mut dana, &dana_t1);
    let status = ok_on(&mut dana, &dana_t1, &e, "operator-status", &[]);
    assert_eq!(status["observation"], "ask_first", "{status}");
    assert!(status["active_task"].is_null(), "nothing is shown before a person approves: {status}");

    // 2, 3. Pictures and reads wait on one approval, and the console says so.
    let picture = |dana: &mut Console| dana.preview(&["--computer", &dana_t1, "--epoch", &e, "--display", "IbaraVirtual", "--quality", "tile"]);
    for _ in 0..3 {
        let waiting = picture(&mut dana);
        assert_eq!(waiting["error"]["code"], "CAPABILITY_UNAVAILABLE", "{waiting}");
        assert!(waiting["error"]["message"].as_str().unwrap().ends_with("Waiting for someone on Tulip1 to approve watching."), "{waiting}");
    }
    assert_eq!(ok_on(&mut dana, &dana_t1, &e, "operator-health", &[])["state"], "pending_approval");
    let asks = open_asks(&tulip1);
    assert_eq!(asks.len(), 1, "{asks:?}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let summary = loop {
        let attention = riley.ok("fleet-attention", &[]);
        let item = attention["items"].as_array().unwrap().iter().find(|i| i["ref"] == asks[0].as_str()).cloned();
        if item.is_some() || Instant::now() > deadline {
            break item.map(|i| i["summary"].clone());
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(summary, Some(json!("command asks to watch this computer: its screen, agent tasks and history.")));

    // 4. Approved: pictures and reads go through, without asking again. This
    // world has no desktop, so a picture gets as far as the missing screen.
    answer(&mut riley, &asks[0], "approve");
    for _ in 0..3 {
        let passed = picture(&mut dana);
        assert!(passed["error"]["message"].as_str().unwrap().contains("Desktop session locked or unavailable."), "{passed}");
    }
    let health = ok_on(&mut dana, &dana_t1, &e, "operator-health", &[]);
    assert!(health["state"].is_null() && health["memory"].is_object(), "{health}");
    let status = ok_on(&mut dana, &dana_t1, &e, "operator-status", &[]);
    assert_eq!(status["observation"], "available_if_desktop_ready", "{status}");
    assert_eq!(open_asks(&tulip1), Vec::<String>::new());

    // 5. Any access change ends the sitting: Dana asks again.
    set_watch(&mut riley, "ask");
    assert!(picture(&mut dana)["error"]["message"].as_str().unwrap().ends_with("to approve watching."));
    let asks = open_asks(&tulip1);
    assert_eq!(asks.len(), 1, "{asks:?}");

    // 6. Declined: refused for the sitting, with no new approval.
    answer(&mut riley, &asks[0], "deny");
    for _ in 0..2 {
        let refused = picture(&mut dana);
        assert!(refused["error"]["message"].as_str().unwrap().contains("A person declined this access request."), "{refused}");
    }
    let status = ok_on(&mut dana, &dana_t1, &e, "operator-status", &[]);
    assert_eq!(status["observation"], "denied", "{status}");
    assert_eq!(open_asks(&tulip1), Vec::<String>::new());
}

/// Hyprland for window management, from `clients` beside it: one window per
/// line, `ADDRESS PID CLASS WORKSPACE_ID WORKSPACE_NAME X Y TITLE…`. The
/// monitor shows workspace 2. `eval` of the close script removes the window
/// it names; of the move script, puts it on the workspace it names.
const WINDOWS_HYPRCTL: &str = r#"#!/bin/sh
here="$(dirname "$0")"
address=$(printf '%s' "$*" | sed -n "s/.*w.address == '\(0x[0-9a-f]*\)'.*/\1/p")
case "$*" in
  *hl.dsp.window.close*)
    grep -v "^$address " "$here/clients" > "$here/clients.new"; mv "$here/clients.new" "$here/clients" ;;
  *hl.dsp.window.move*)
    workspace=$(printf '%s' "$*" | sed -n 's/.*workspace = \([0-9]*\).*/\1/p')
    awk -v a="$address" -v w="$workspace" '$1 == a { $4 = w; $5 = w } { print }' "$here/clients" > "$here/clients.new"
    mv "$here/clients.new" "$here/clients" ;;
  *monitors*) echo '[{"id":0,"name":"IbaraVirtual","width":1280,"height":720,"x":0,"y":0,"scale":1.0,"focused":true,"activeWorkspace":{"id":2,"name":"2"}}]' ;;
  *clients*)
    awk 'BEGIN { printf "[" } {
      title = $8; for (i = 9; i <= NF; i++) title = title " " $i
      printf "%s{\"address\":\"%s\",\"pid\":%s,\"class\":\"%s\",\"workspace\":{\"id\":%s,\"name\":\"%s\"},\"at\":[%s,%s],\"size\":[600,400],\"mapped\":true,\"hidden\":false,\"floating\":false,\"fullscreen\":0,\"title\":\"%s\"}", (NR > 1 ? "," : ""), $1, $2, $3, $4, $5, $6, $7, title
    } END { print "]" }' "$here/clients" ;;
  *) echo '[]' ;;
esac
"#;

/// Each workspace as `[id, name, special, active, [titles]]`.
fn layout(windows: &Value) -> Value {
    let spaces = windows["workspaces"].as_array().unwrap().iter();
    let titles = |w: &Value| w["windows"].as_array().unwrap().iter().map(|x| x["title"].clone()).collect::<Vec<_>>();
    json!(spaces.map(|w| json!([w["id"], w["name"], w["special"], w["active"], titles(w)])).collect::<Vec<_>>())
}

/// A person clears windows an agent left behind, from another computer.
/// Failure cases this must catch:
/// 1. The list does not group windows by workspace, misses the shown (empty)
///    workspace or does not mark it, takes a named workspace (negative id)
///    for a special one, puts the scratchpad among the others, or orders a
///    workspace's windows other than top to bottom.
/// 2. A terminal started with its own app id is not known as a terminal, or
///    another program is.
/// 3. Closing leaves the window, or says it closed when it did not; the
///    history does not say what was closed or moved.
/// 4. A window that is gone, or an address now another process's, is acted
///    on, or not refused as `NOT_FOUND`.
/// 5. Moving leaves the window where it was.
/// 6. A malformed address or workspace reaches the other computer.
/// (The agent's own window is refused in `controller::tests`: a running
/// agent task needs a desktop this world does not have.)
#[test]
fn windows_left_behind_are_closed_and_moved_from_another_computer() {
    let world = World::new("windows");
    let desk = world.root.join("desk-tulip1");
    fs::create_dir_all(&desk).unwrap();
    write_executable(&desk.join("hyprctl"), WINDOWS_HYPRCTL);
    // The processes behind two windows: a terminal (its program is named
    // `foot`) showing btop under Omarchy's own app id, and an editor.
    std::os::unix::fs::symlink("/usr/bin/sleep", desk.join("foot")).unwrap();
    let mut terminal = Command::new(desk.join("foot")).arg("120").spawn().unwrap();
    let mut editor = Command::new("/usr/bin/sleep").arg("120").spawn().unwrap();
    let (term_pid, editor_pid) = (terminal.id().to_string(), editor.id().to_string());
    let clients = format!(
        "0x5578a2 4102 foot 1 1 0 400 htop\n\
         0x5578a1 {term_pid} org.omarchy.btop 1 1 0 0 btop\n\
         0x5578b1 {editor_pid} mousepad 3 3 0 0 Untitled 1 - Mousepad\n\
         0x5578c1 4301 foot -98 special:scratchpad 0 0 scratch\n\
         0x5578d1 4401 foot -1337 hdmi 0 0 notes\n"
    );
    fs::write(desk.join("clients"), clients).unwrap();
    let hyprctl = desk.join("hyprctl");
    let tulip1 = Target::start_with(&world, node("tulip1"), Some("Tulip1"), 300_000, &[("IBARA_TEST_HYPRCTL", hyprctl.as_os_str())]);
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let id = add_own(&mut vesper, "tulip1");
    let e = epoch(&mut vesper, &id);
    let on_desk = || fs::read_to_string(desk.join("clients")).unwrap();

    // 1. Workspaces with windows, and the one shown; the named one after the
    // numbered ones, the scratchpad last.
    let listed = ok_on(&mut vesper, &id, &e, "operator-windows", &[]);
    assert_eq!(
        layout(&listed),
        json!([
            [1, "1", false, false, ["btop", "htop"]],
            [2, "2", false, true, []],
            [3, "3", false, false, ["Untitled 1 - Mousepad"]],
            [-1337, "hdmi", false, false, ["notes"]],
            [-98, "special:scratchpad", true, false, ["scratch"]],
        ]),
        "{listed}"
    );
    // 2. Known as a terminal by its program, not its app id.
    let btop = json!({"address": "0x5578a1", "pid": terminal.id(), "class": "org.omarchy.btop", "title": "btop", "floating": false, "fullscreen": false, "focused": false, "terminal": true, "task": null});
    assert_eq!(listed["workspaces"][0]["windows"][0], btop);
    assert_eq!(listed["workspaces"][2]["windows"][0]["terminal"], false, "{listed}");
    assert_eq!(listed["agent"], Value::Null);

    // 3. Close: the window goes.
    let closed = ok_on(&mut vesper, &id, &e, "operator-window-close", &["--address", "0x5578a1", "--pid", &term_pid]);
    assert_eq!(closed["closed"], true, "{closed}");
    assert!(!on_desk().contains("0x5578a1"), "{}", on_desk());

    // 4. Gone, or the address now another process's: refused, nothing sent.
    let before = on_desk();
    for (command, extra) in [
        ("operator-window-close", vec!["--address", "0x5578a1", "--pid", term_pid.as_str()]),
        ("operator-window-move", vec!["--address", "0x5578a1", "--pid", term_pid.as_str(), "--workspace", "4"]),
        ("operator-window-close", vec!["--address", "0x5578a2", "--pid", "9999"]),
    ] {
        let refused = on(&mut vesper, &id, &e, command, &extra);
        assert_eq!(refused["error"]["code"], "NOT_FOUND", "{command} {extra:?}: {refused}");
        assert_eq!(refused["error"]["message"], "That window is gone.");
    }
    assert_eq!(on_desk(), before);

    // 5. Move: the editor goes to the shown workspace, and its own empties.
    let moved = ok_on(&mut vesper, &id, &e, "operator-window-move", &["--address", "0x5578b1", "--pid", &editor_pid, "--workspace", "2"]);
    assert_eq!((moved["moved"].as_bool(), moved["workspace"].as_i64()), (Some(true), Some(2)), "{moved}");
    let listed = ok_on(&mut vesper, &id, &e, "operator-windows", &[]);
    assert_eq!(
        layout(&listed),
        json!([
            [1, "1", false, false, ["htop"]],
            [2, "2", false, true, ["Untitled 1 - Mousepad"]],
            [-1337, "hdmi", false, false, ["notes"]],
            [-98, "special:scratchpad", true, false, ["scratch"]],
        ]),
        "{listed}"
    );
    let history = vesper.ok("away", &[]);
    let events: Vec<(&str, &str)> = history["computers"][0]["events"].as_array().unwrap().iter().map(|e| (e["kind"].as_str().unwrap(), e["summary"].as_str().unwrap())).collect();
    assert!(events.contains(&("window_closed", "Closed org.omarchy.btop “btop”")), "{history}");
    assert!(events.contains(&("window_moved", "Moved mousepad “Untitled 1 - Mousepad” to workspace 2")), "{history}");
    let _ = (terminal.kill(), editor.kill(), terminal.wait(), editor.wait());

    // 6. Refused here, with Tulip1 off: the other computer is never asked.
    drop(tulip1);
    for (extra, message) in [
        (vec!["--address", "5578a2", "--pid", "4102"], "Expected a window address such as 0x1a2b."),
        (vec!["--address", "0x5578A2", "--pid", "4102"], "Expected a window address such as 0x1a2b."),
        (vec!["--address", "0x5578a2", "--pid", "0"], "Expected the window's process id."),
    ] {
        let refused = on(&mut vesper, &id, &e, "operator-window-close", &extra);
        assert_eq!((refused["error"]["code"].as_str(), refused["error"]["message"].as_str()), (Some("INVALID_ARGUMENT"), Some(message)), "{extra:?}: {refused}");
    }
    let refused = on(&mut vesper, &id, &e, "operator-window-move", &["--address", "0x5578a2", "--pid", "4102", "--workspace", "11"]);
    assert_eq!(refused["error"]["message"], "Choose a workspace from 1 to 10.", "{refused}");
}
