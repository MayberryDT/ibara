//! `ibara power-system`, end to end through the real binary.
//!
//! The helper runs with stdin and stdout piped, as systemd hands it the
//! accepted socket. Fake `systemctl`, `iw` and `systemd-cryptenroll` in a temp
//! bin dir append their argv to a log; fixture sysfs and proc trees describe the
//! computer. The fake `systemctl` waits up to five seconds for the test to
//! receive the reply and logs whether it had, so a helper that runs the power
//! command before its answer is flushed is caught.
//!
//! Failure cases this must catch:
//! 1. The power command runs before the reply reaches the caller (or the reply is never flushed).
//! 2. restart, shutdown or sleep run the wrong `systemctl` verb, or `disk` runs one at all.
//! 3. Wi-Fi wake uses a phy other than the adapter's own, or the request can name a phy.
//! 4. A tunnel (tailscale0), unknown adapter, `lo`, `..`, a path or an over-long name is accepted,
//!    or anything (iw, systemctl) runs for it.
//! 5. `wake: null` touches iw, or does not sleep.
//! 6. A Wi-Fi card without wake support is not reported as not_supported; another iw failure,
//!    or a wired adapter the kernel cannot configure, stops the sleep or loses its message.
//! 7. The Tulip1 layout (btrfs on /dev/mapper/root, cryptdevice= on the command line,
//!    a password-only slot) is not encrypted true, tpm_unlock false.
//! 8. A tpm2 slot with sd-encrypt is not tpm_unlock true; a tpm2 slot with the `encrypt` hook is.
//! 9. Root on LVM on LUKS is missed, or one sealed disk without a tpm2 slot still reports tpm_unlock.
//! 10. An unencrypted root reports encrypted, or runs systemd-cryptenroll.
//! 11. Bad JSON, an unknown op, extra fields or an oversized request run anything.

use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const IBARA: &str = env!("CARGO_BIN_EXE_ibara");

const TULIP1_MOUNTS: &str = "\
22 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:13 - proc proc rw
32 2 0:29 /@ / rw,relatime shared:1 - btrfs /dev/mapper/root rw,compress=zstd:3,ssd,space_cache=v2,subvolid=256,subvol=/@
45 32 179:1 / /boot rw,relatime shared:30 - vfat /dev/mmcblk2p1 rw
";
const PASSWORD_ONLY: &str = "SLOT TYPE    \n   0 password\n";
const WITH_TPM: &str = "SLOT TYPE    \n   0 password\n   1 tpm2\n";

struct Computer {
    root: PathBuf,
}

impl Computer {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ibara-power-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["bin", "sys/class/net", "sys/block", "sys/dev/block", "sys/class/block", "proc/self", "cryptenroll"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let r = root.display();
        let scripts = [
            (
                "systemctl",
                format!(
                    "i=0\nwhile [ ! -e {r}/replied ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done\n\
                     if [ -e {r}/replied ]; then when=after-reply; else when=before-reply; fi\n\
                     echo \"systemctl $when $*\" >> {r}/log"
                ),
            ),
            (
                "iw",
                format!(
                    "echo \"iw $*\" >> {r}/log\n\
                     case \"$(cat {r}/iw-mode 2>/dev/null)\" in\n\
                     unsupported) echo 'command failed: Operation not supported (-95)' >&2; exit 161;;\n\
                     busy) echo 'command failed: Device or resource busy (-16)' >&2; exit 240;;\n\
                     esac"
                ),
            ),
            (
                "systemd-cryptenroll",
                format!("echo \"systemd-cryptenroll $*\" >> {r}/log\nexec cat \"{r}/cryptenroll/$(basename \"$1\")\""),
            ),
        ];
        for (name, body) in scripts {
            let path = root.join("bin").join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let computer = Computer { root };
        computer.net("lo", false, None);
        computer.net("tailscale0", false, None);
        computer.net("wlp2s0", true, Some("phy0"));
        computer.net("wlp3s0", true, Some("phy1"));
        computer
    }

    fn net(&self, name: &str, device: bool, phy: Option<&str>) {
        let dir = self.root.join("sys/class/net").join(name);
        fs::create_dir_all(&dir).unwrap();
        if device {
            fs::create_dir_all(dir.join("device")).unwrap();
        }
        if let Some(phy) = phy {
            fs::create_dir_all(dir.join("phy80211")).unwrap();
            fs::write(dir.join("phy80211/name"), format!("{phy}\n")).unwrap();
        }
    }

    /// A block device, linked from dev/block and class/block as sysfs does.
    fn block(&self, name: &str, numbers: &str, dm: Option<(&str, &str)>, slaves: &[&str]) {
        let dir = self.root.join("sys/block").join(name);
        fs::create_dir_all(dir.join("slaves")).unwrap();
        fs::write(dir.join("dev"), format!("{numbers}\n")).unwrap();
        if let Some((dm_name, uuid)) = dm {
            fs::create_dir_all(dir.join("dm")).unwrap();
            fs::write(dir.join("dm/name"), format!("{dm_name}\n")).unwrap();
            fs::write(dir.join("dm/uuid"), format!("{uuid}\n")).unwrap();
        }
        for slave in slaves {
            symlink(self.root.join("sys/block").join(slave), dir.join("slaves").join(slave)).unwrap();
        }
        symlink(&dir, self.root.join("sys/dev/block").join(numbers)).unwrap();
        symlink(&dir, self.root.join("sys/class/block").join(name)).unwrap();
    }

    fn boot(&self, mountinfo: &str, cmdline: &str) {
        fs::write(self.root.join("proc/self/mountinfo"), mountinfo).unwrap();
        fs::write(self.root.join("proc/cmdline"), format!("{cmdline}\n")).unwrap();
    }

    fn slots(&self, device: &str, table: &str) {
        fs::write(self.root.join("cryptenroll").join(device), table).unwrap();
    }

    /// Tulip1 as inspected: LUKS2 on mmcblk2p2 opened as `root` by the `encrypt` hook.
    fn tulip1(&self) {
        self.block("mmcblk2p2", "179:2", None, &[]);
        self.block("dm-0", "253:0", Some(("root", "CRYPT-LUKS2-5e2d9c413a7b4c1d9e8f0a1b2c3d4e5f-root")), &["mmcblk2p2"]);
        self.boot(TULIP1_MOUNTS, "initrd=\\initramfs-linux.img cryptdevice=PARTUUID=0c4b1f3e-02:root root=/dev/mapper/root zswap.enabled=0 rootflags=subvol=@ rw");
        self.slots("mmcblk2p2", PASSWORD_ONLY);
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("log")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Sends one request; returns the reply line and the exit status.
    fn ask(&self, request: &[u8]) -> (Value, i32) {
        let _ = fs::remove_file(self.root.join("replied"));
        let mut child = Command::new(IBARA)
            .arg("power-system")
            .env("IBARA_POWER_BIN_DIR", self.root.join("bin"))
            .env("IBARA_POWER_SYSFS", self.root.join("sys"))
            .env("IBARA_POWER_PROC", self.root.join("proc"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let request = request.to_vec();
        // The helper may stop reading early (oversized requests).
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&request);
        });
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        fs::write(self.root.join("replied"), "").unwrap();
        writer.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "power-system did not exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        (serde_json::from_str(&line).unwrap_or_else(|_| panic!("reply is not JSON: {line:?}")), status.code().unwrap_or(-1))
    }

    fn request(&self, request: Value) -> Value {
        self.ask(format!("{request}\n").as_bytes()).0
    }
}

impl Drop for Computer {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn refused(reply: &Value) -> bool {
    reply["ok"] == false && reply["error"]["code"] == "INVALID_ARGUMENT" && reply["error"]["message"].as_str().is_some_and(|m| !m.is_empty())
}

#[test]
fn restart_and_shutdown_answer_before_the_computer_goes_down() {
    let computer = Computer::new("power");
    let (reply, status) = computer.ask(b"{\"op\":\"restart\"}\n");
    assert_eq!(reply, json!({"ok": true, "action": "restart", "state": "started"}));
    assert_eq!(status, 0);
    assert_eq!(computer.log(), ["systemctl after-reply reboot"]);

    assert_eq!(computer.request(json!({"op": "shutdown"})), json!({"ok": true, "action": "shutdown", "state": "started"}));
    assert_eq!(computer.log(), ["systemctl after-reply reboot", "systemctl after-reply poweroff"]);
}

#[test]
fn sleep_turns_on_wake_for_the_adapters_own_wifi_card() {
    let computer = Computer::new("wifi");
    let reply = computer.request(json!({"op": "sleep", "wake": {"ifname": "wlp3s0"}}));
    assert_eq!(reply, json!({"ok": true, "action": "sleep", "state": "started", "wake": "enabled"}));
    assert_eq!(computer.log(), ["iw phy phy1 wowlan enable magic-packet", "systemctl after-reply suspend"]);

    // The caller cannot pick the phy.
    assert!(refused(&computer.request(json!({"op": "sleep", "wake": {"ifname": "wlp3s0", "phy": "phy0"}}))));
    assert_eq!(computer.log().len(), 2);
}

#[test]
fn sleep_without_wake_leaves_the_network_alone() {
    let computer = Computer::new("nowake");
    assert_eq!(computer.request(json!({"op": "sleep", "wake": null})), json!({"ok": true, "action": "sleep", "state": "started", "wake": "off"}));
    assert_eq!(computer.log(), ["systemctl after-reply suspend"]);
}

#[test]
fn sleep_goes_ahead_when_wake_cannot_be_turned_on() {
    let computer = Computer::new("wakefail");
    fs::write(computer.root.join("iw-mode"), "unsupported").unwrap();
    let reply = computer.request(json!({"op": "sleep", "wake": {"ifname": "wlp2s0"}}));
    assert_eq!(reply, json!({"ok": true, "action": "sleep", "state": "started", "wake": "not_supported"}));

    fs::write(computer.root.join("iw-mode"), "busy").unwrap();
    let reply = computer.request(json!({"op": "sleep", "wake": {"ifname": "wlp2s0"}}));
    assert_eq!(reply["wake"], "failed");
    assert!(reply["message"].as_str().unwrap().contains("Device or resource busy"), "{reply}");
    assert_eq!(
        computer.log(),
        [
            "iw phy phy0 wowlan enable magic-packet",
            "systemctl after-reply suspend",
            "iw phy phy0 wowlan enable magic-packet",
            "systemctl after-reply suspend",
        ]
    );

    // A wired adapter the kernel does not have: the ethtool ioctl fails, the
    // computer still sleeps, and iw is not involved.
    computer.net("ibfx9", true, None);
    let reply = computer.request(json!({"op": "sleep", "wake": {"ifname": "ibfx9"}}));
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["wake"], "failed");
    assert!(reply["message"].as_str().unwrap().contains("ibfx9"), "{reply}");
    assert_eq!(computer.log()[4..], ["systemctl after-reply suspend"]);
}

#[test]
fn sleep_refuses_adapters_that_cannot_wake_the_computer_and_does_nothing() {
    let computer = Computer::new("badwake");
    fs::create_dir_all(computer.root.join("sys/class/net/../outside/device")).unwrap();
    for ifname in ["tailscale0", "lo", "eth7", "..", "../outside", "wlp2s0/..", "", "abcdefghijklmnop", "wl p2s0"] {
        let reply = computer.request(json!({"op": "sleep", "wake": {"ifname": ifname}}));
        assert!(refused(&reply), "{ifname}: {reply}");
    }
    assert!(refused(&computer.request(json!({"op": "sleep", "wake": {"ifname": 7}}))));
    assert!(refused(&computer.request(json!({"op": "sleep", "wake": "wlp2s0"}))));
    assert_eq!(computer.log(), Vec::<String>::new());
}

#[test]
fn bad_requests_are_refused_and_nothing_runs() {
    let computer = Computer::new("bad");
    computer.tulip1();
    for request in [
        b"restart\n".to_vec(),
        b"{\"op\":\"restart\"".to_vec(),
        b"[\"restart\"]\n".to_vec(),
        b"{}\n".to_vec(),
        b"{\"op\":\"hibernate\"}\n".to_vec(),
        b"{\"op\":\"restart\",\"delay\":5}\n".to_vec(),
        b"{\"op\":\"disk\",\"device\":\"/dev/sda\"}\n".to_vec(),
        b"\n".to_vec(),
    ] {
        let (reply, _) = computer.ask(&request);
        assert!(refused(&reply), "{}: {reply}", String::from_utf8_lossy(&request));
    }
    let mut huge = b"{\"op\":\"restart\",\"pad\":\"".to_vec();
    huge.resize(70 * 1024, b'a');
    huge.extend_from_slice(b"\"}\n");
    assert!(refused(&computer.ask(&huge).0));
    assert_eq!(computer.log(), Vec::<String>::new());
}

#[test]
fn tulip1_is_encrypted_and_needs_its_passphrase() {
    let computer = Computer::new("tulip1");
    computer.tulip1();
    assert_eq!(computer.request(json!({"op": "disk"})), json!({"ok": true, "encrypted": true, "tpm_unlock": false}));
    // Nothing powers off.
    assert!(!computer.log().iter().any(|line| line.starts_with("systemctl")));
}

#[test]
fn a_tpm_slot_unlocks_only_with_sd_encrypt() {
    let computer = Computer::new("tpm");
    computer.tulip1();
    computer.slots("mmcblk2p2", WITH_TPM);
    // The `encrypt` hook still asks for the passphrase.
    assert_eq!(computer.request(json!({"op": "disk"}))["tpm_unlock"], false);

    computer.boot(TULIP1_MOUNTS, "rd.luks.name=5e2d9c41-3a7b-4c1d-9e8f-0a1b2c3d4e5f=root root=/dev/mapper/root rw");
    assert_eq!(computer.request(json!({"op": "disk"})), json!({"ok": true, "encrypted": true, "tpm_unlock": true}));

    computer.slots("mmcblk2p2", "SLOT TYPE\n   0 password\n   1 recovery\n");
    assert_eq!(computer.request(json!({"op": "disk"}))["tpm_unlock"], false);
}

#[test]
fn root_on_lvm_on_luks_needs_every_sealed_disk_to_have_a_tpm_slot() {
    let computer = Computer::new("lvm");
    computer.block("sda2", "8:2", None, &[]);
    computer.block("sdb1", "8:17", None, &[]);
    computer.block("dm-0", "254:0", Some(("cryptfast", "CRYPT-LUKS2-1111-cryptfast")), &["sda2"]);
    computer.block("dm-2", "254:2", Some(("cryptbig", "CRYPT-LUKS2-2222-cryptbig")), &["sdb1"]);
    computer.block("dm-1", "254:1", Some(("vg-root", "LVM-abcdef")), &["dm-0", "dm-2"]);
    computer.boot("30 1 254:1 / / rw,relatime shared:1 - ext4 /dev/mapper/vg-root rw\n", "root=/dev/mapper/vg-root rw");
    computer.slots("sda2", WITH_TPM);
    computer.slots("sdb1", PASSWORD_ONLY);
    assert_eq!(computer.request(json!({"op": "disk"})), json!({"ok": true, "encrypted": true, "tpm_unlock": false}));

    computer.slots("sdb1", WITH_TPM);
    assert_eq!(computer.request(json!({"op": "disk"})), json!({"ok": true, "encrypted": true, "tpm_unlock": true}));
    assert!(computer.log().contains(&"systemd-cryptenroll /dev/sdb1".to_string()));
}

#[test]
fn an_unencrypted_root_is_reported_without_asking_cryptsetup() {
    let computer = Computer::new("plain");
    computer.block("vdz2", "252:2", None, &[]);
    computer.boot("32 2 0:29 /@ / rw,relatime shared:1 - btrfs /dev/vdz2 rw,subvol=/@\n", "root=/dev/vdz2 rw");
    assert_eq!(computer.request(json!({"op": "disk"})), json!({"ok": true, "encrypted": false, "tpm_unlock": false}));
    assert_eq!(computer.log(), Vec::<String>::new());
}
