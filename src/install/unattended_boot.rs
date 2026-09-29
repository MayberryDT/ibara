//! `ibara unattended-boot enable|disable|status|lock-at-sign-in on|off`:
//! starting this computer without typing its disk password. Off unless the
//! person turns it on.
//!
//! Omarchy encrypts the root partition with LUKS and unlocks it in the
//! initramfs with the busybox `encrypt` hook (`cryptdevice=` on the kernel
//! line in /etc/default/limine), which always asks for the password. Turning
//! this on (`ibara system unattended-boot enable`, as root):
//!
//! 1. checks the computer is laid out as Omarchy lays it out (a LUKS2 root
//!    opened by `cryptdevice=`, a TPM 2.0, Limine booting UKIs that
//!    limine-mkinitcpio-hook builds) and refuses anything else;
//! 2. backs up the LUKS header, /etc/default/limine, /etc/kernel/cmdline and
//!    limine.conf under /var/backups/ibara/unattended-boot/;
//! 3. adds a boot entry, `ask-disk-password`, on a copy of the image that is
//!    running now, kept until the disk has once unlocked by itself;
//! 4. switches the initramfs to systemd's hooks (a mkinitcpio drop-in after
//!    Omarchy's) and the kernel line to `rd.luks.name=`/`rd.luks.options=`,
//!    and rebuilds the images with limine-mkinitcpio;
//! 5. enrolls a TPM2 key beside the password (which stays, and is asked for
//!    whenever the TPM refuses), sealed to PCR 7 (the Secure Boot state) as
//!    the new initramfs will find it: systemd there extends PCR 7 with
//!    `os-separator` before it opens the disk, which the busybox one never did.
//!
//! A step that fails puts back everything before it. `disable` puts every
//! file back, rebuilds the images and wipes the TPM key. `status` (no root)
//! says whether the next start asks for the password. At each start,
//! `ibara-unattended-boot.service` runs `boot-check`: once the disk has
//! unlocked by itself, the `ask-disk-password` entry goes.
//!
//! What it gives up: PCR 7 records the Secure Boot state, not the kernel line,
//! and Omarchy's Limine lets anyone at the keyboard (or with the unencrypted
//! ESP) change that line, so the key opens the disk for whoever has the whole
//! computer, Secure Boot or not. Only a disk taken out of it stays unreadable.
//! Binding PCR 12 as well needs an embedded `.cmdline` or an enrolled Limine
//! configuration, which Omarchy with Secure Boot and snapper does not have.
//!
//! Omarchy then signs the desktop account in without a password (SDDM's
//! `[Autologin]`), so when that is on, enable refuses until the person also
//! turns on the lock at sign-in: `lock-at-sign-in on` puts a hook in Omarchy's
//! `~/.config/omarchy/hooks/post-boot.d/` that locks the screen right after
//! each sign-in, and `off` removes it.

use super::{BACKUPS, LIB, interactive, is_root, program, root_dir, run, write_root};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "Usage: ibara unattended-boot enable [--dry-run] | disable | status | lock-at-sign-in on|off";
const SYSTEM_USAGE: &str = "Usage: ibara system unattended-boot enable [--dry-run] | disable | boot-check";

/// What ibara turned on, and how to put it back. Root writes it; everyone can read it.
pub const STATE: &str = "/etc/ibara/unattended-boot.json";
/// Runs after Omarchy's drop-ins (`omarchy_hooks.conf`, …): sorted last.
const HOOKS_DROPIN: &str = "/etc/mkinitcpio.conf.d/zz-ibara-unattended-boot.conf";
const HOOKS_TEXT: &str = include_str!("unattended-boot-hooks.conf");
const OMARCHY_HOOKS: &str = "/etc/mkinitcpio.conf.d/omarchy_hooks.conf";
const LIMINE_DEFAULTS: &str = "/etc/default/limine";
const KERNEL_CMDLINE: &str = "/etc/kernel/cmdline";
/// The kernel line of the kept entry: the one it booted with before.
const ENTRY_DROPIN: &str = "/etc/limine-entry-tool.d/zz-ibara-ask-disk-password.conf";
const ENTRY_CONFIGS: [&str; 2] = ["/etc/limine-entry-tool.conf", "/etc/limine-entry-tool.d"];
/// The boot entry on the previous image, until the disk has once unlocked by itself.
pub const FALLBACK: &str = "ask-disk-password";
const UNIT: &str = "ibara-unattended-boot.service";
const SECURE_BOOT: &str = "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";
/// The trade-off, said wherever this is offered. The key is sealed to PCR 7,
/// which does not cover the kernel line, so Secure Boot does not change it.
pub const TRADE_OFF: &str = "The TPM chip unlocks the disk by itself, so anyone who has the whole computer can get at your files, even with Secure Boot on. The encryption still protects the disk if it is taken out.";
/// SDDM's settings in the order it reads them, the last `User=` winning: its
/// defaults, the drop-ins, then its own file.
const SDDM_CONFIGS: [&str; 3] = ["/usr/lib/sddm/sddm.conf.d", "/etc/sddm.conf.d", "/etc/sddm.conf"];
/// Omarchy runs `~/.config/omarchy/hooks/post-boot.d/*` in name order right
/// after each sign-in (`omarchy-hook post-boot`, from Hyprland's start).
pub(super) const SIGN_IN_LOCK: &str = ".config/omarchy/hooks/post-boot.d/00-ibara-lock-at-sign-in";
const SIGN_IN_LOCK_TEXT: &str = include_str!("lock-at-sign-in.sh");
/// Why enable refuses while nothing locks the screen at sign-in.
const SIGNS_IN_BY_ITSELF: &str = "Omarchy signs in automatically after the disk unlocks, so anyone who turned this computer on would get the desktop, and your other computers in ibara. Turn on the lock at sign-in first.";
/// The lock at sign-in, as the console's Settings describe it.
const LOCK_HELP: &str = "Locks the screen right after Omarchy signs you in without a password; your password unlocks it.";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    /// "waiting" until the disk has once unlocked by itself, then "on".
    state: String,
    device: String,
    luks_uuid: String,
    /// The device-mapper name the root is opened as (`root` on Omarchy).
    name: String,
    /// The kernel-line word Omarchy had, and the words that replaced it.
    cryptdevice: String,
    unlock: String,
    kernel_cmdline_file: bool,
    backup: String,
    enabled_at: u64,
}

// ---------------------------------------------------------------------------
// The person's commands.

/// `ibara unattended-boot …`, run by the person; the root parts go through one `sudo`.
pub fn main(args: &[String]) -> Result<(), String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["status"] => {
            println!("{}", status_text(&status()));
            Ok(())
        }
        ["enable", rest @ ..] => {
            let dry_run = match rest {
                [] => false,
                ["--dry-run"] => true,
                _ => return Err(USAGE.into()),
            };
            sign_in_locked()?;
            if !dry_run {
                println!("Starting without the disk password. {TRADE_OFF}");
                println!("The disk password stays, and is asked for whenever the TPM refuses.\n");
                println!("The next part needs your password once (sudo), then the disk password to add the TPM key beside it.");
            }
            let mut root_args = vec!["unattended-boot", "enable"];
            if dry_run {
                root_args.push("--dry-run");
            }
            as_root(&root_args)
        }
        ["disable"] => {
            println!("Putting back the disk password at every start. The next part needs your password once (sudo).");
            as_root(&["unattended-boot", "disable"])
        }
        ["lock-at-sign-in", on @ ("on" | "off")] => {
            println!("{}", set_sign_in_lock(*on == "on")?);
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

/// The lock at sign-in for the account running this: ibara's hook in
/// Omarchy's post-boot hooks, written or removed. Turning it off is refused
/// while it is all that keeps a computer that starts by itself locked.
fn set_sign_in_lock(on: bool) -> Result<&'static str, String> {
    if is_root() {
        return Err("Run this as the person who signs in, not as root.".into());
    }
    let me = super::Account::current()?;
    let path = own_home(&me).join(SIGN_IN_LOCK);
    if on {
        let dir = path.parent().ok_or("The hooks folder has no parent.")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        let temp = dir.join(format!(".00-ibara-lock-at-sign-in.{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::remove_file(&temp);
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(&temp)?;
            file.write_all(SIGN_IN_LOCK_TEXT.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("Could not write {}: {e}", path.display())
        })?;
        return Ok("The screen now locks right after you are signed in; your password unlocks it.");
    }
    if load_state().is_some() && automatic_sign_in().as_deref() == Some(me.name.as_str()) {
        return Err("This computer starts without its disk password and Omarchy signs you in automatically, so this lock is what keeps anyone who turns it on out of your desktop. Turn off starting without the disk password first: ibara unattended-boot disable.".into());
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok("The screen no longer locks at sign-in."),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("The screen does not lock at sign-in."),
        Err(e) => Err(format!("Could not remove {}: {e}", path.display())),
    }
}

/// The console's `unattended-boot-lock on|off`: the same, then the new status.
pub fn console_lock(args: &[String]) -> Result<Value, String> {
    match args {
        [on] if on == "on" || on == "off" => set_sign_in_lock(on == "on").map(|_| status_json()),
        _ => Err("Usage: unattended-boot-lock on|off".into()),
    }
}

fn as_root(args: &[&str]) -> Result<(), String> {
    if is_root() {
        return system(&args[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>());
    }
    interactive(Command::new("sudo").arg("--").arg(Path::new(LIB).join("bin/ibara")).arg("system").args(args))
}

// ---------------------------------------------------------------------------
// Status, readable without root (the console's `unattended-boot`).

#[derive(Debug)]
pub struct Status {
    /// "not_encrypted", "off", "waiting", "on", "changed" (ibara turned it on
    /// and the boot files changed since) or "elsewhere" (set up outside ibara).
    pub state: &'static str,
    /// Whether the next start asks for the disk password; `None` when unknown.
    pub next_start_asks: Option<bool>,
    pub secure_boot: Option<bool>,
    pub tpm: bool,
    /// The account SDDM signs in without a password, if any.
    pub automatic_sign_in: Option<String>,
    /// Whether that account (else this one) locks the screen right after signing in.
    pub lock_at_sign_in: bool,
    /// Why turning it on would be refused, when that is already clear without root.
    pub unavailable: Option<String>,
}

impl Status {
    /// ibara turned it on (the next start may still ask when the files changed).
    pub fn on(&self) -> bool {
        matches!(self.state, "waiting" | "on" | "changed")
    }

    /// On, with automatic sign-in and nothing locking the screen.
    fn desktop_open(&self) -> bool {
        self.on() && self.automatic_sign_in.is_some() && !self.lock_at_sign_in
    }
}

pub fn status() -> Status {
    let secure_boot = secure_boot();
    let tpm = tpm_present();
    let defaults = std::fs::read_to_string(LIMINE_DEFAULTS).unwrap_or_default();
    let state = load_state();
    let encrypted = crate::power_system::root_encrypted();
    let automatic_sign_in = automatic_sign_in();
    let lock_at_sign_in = sign_in_lock(automatic_sign_in.as_deref());
    let mut out =
        Status { state: "off", next_start_asks: Some(true), secure_boot, tpm, automatic_sign_in, lock_at_sign_in, unavailable: None };
    if let Some(state) = state {
        let intact = defaults.contains(&state.unlock) && Path::new(HOOKS_DROPIN).is_file();
        out.state = if !intact {
            "changed"
        } else if state.state == "on" {
            "on"
        } else {
            "waiting"
        };
        out.next_start_asks = if intact { Some(false) } else { None };
        return out;
    }
    if encrypted == Some(false) {
        out.state = "not_encrypted";
        out.next_start_asks = Some(false);
        return out;
    }
    if !defaults.contains("cryptdevice=") && defaults.contains("rd.luks.") {
        out.state = "elsewhere";
        out.next_start_asks = None;
        return out;
    }
    out.unavailable = if !tpm {
        Some("This computer has no TPM 2.0 chip that Linux can use, so its disk cannot unlock by itself.".into())
    } else if !defaults.contains("cryptdevice=") || !Path::new(OMARCHY_HOOKS).is_file() {
        Some("This computer does not start the way Omarchy sets it up (Limine, and cryptdevice= on the kernel line), so ibara leaves its disk unlocking alone.".into())
    } else if program("limine-mkinitcpio").is_none() || program("systemd-cryptenroll").is_none() {
        Some("limine-mkinitcpio or systemd-cryptenroll is missing, so ibara cannot change how this computer starts.".into())
    } else if out.automatic_sign_in.is_some() && !out.lock_at_sign_in {
        Some(SIGNS_IN_BY_ITSELF.into())
    } else {
        None
    };
    out
}

pub fn status_text(status: &Status) -> String {
    let mut lines = vec![
        match status.state {
            "not_encrypted" => "This computer's disk is not encrypted, so it starts without a password.".to_string(),
            "off" => "Starting this computer asks for the disk password.".to_string(),
            "waiting" | "on" => "Starting this computer does not ask for the disk password: the TPM unlocks the disk. If the TPM refuses (after a firmware or Secure Boot change, for example), it asks for the password as before.".to_string(),
            "changed" => format!("ibara turned on starting without the disk password, but {LIMINE_DEFAULTS} or {HOOKS_DROPIN} changed since, so the next start may ask for it. Run ibara unattended-boot disable to put everything back."),
            _ => "This computer was set up outside ibara to unlock its disk another way; ibara leaves that alone.".to_string(),
        },
    ];
    if status.state == "waiting" {
        lines.push(format!("Until it has started that way once, the boot menu keeps an entry named {FALLBACK} that asks for the password."));
    }
    if status.on() {
        lines.push(TRADE_OFF.into());
        if status.desktop_open() {
            lines.push(format!("{NOTHING_LOCKS} Run: ibara unattended-boot lock-at-sign-in on"));
        } else if status.automatic_sign_in.is_some() {
            lines.push("Omarchy signs in automatically after the disk unlocks, and the screen locks right after; the account password unlocks it.".into());
        }
    }
    if status.state == "off" {
        lines.push(match &status.unavailable {
            Some(why) if why == SIGNS_IN_BY_ITSELF => format!("{why} Run: ibara unattended-boot lock-at-sign-in on"),
            Some(why) => why.clone(),
            None => format!("To change that: ibara unattended-boot enable. {TRADE_OFF}"),
        });
    }
    lines.join("\n")
}

/// Said while it is on, SDDM signs in by itself and nothing locks the screen.
const NOTHING_LOCKS: &str = "Omarchy signs in automatically and nothing locks the screen, so anyone who turns this computer on gets the desktop, and your other computers in ibara. Turn on the lock at sign-in.";

/// The console's `unattended-boot`: what its Settings entries show. `note`:
/// why it cannot be turned on here, that nothing locks the screen at sign-in,
/// or that the boot files changed since. The lock at sign-in is its own
/// switch (`unattended-boot-lock`), shown while SDDM signs in by itself.
pub fn status_json() -> Value {
    let status = status();
    let note = if status.desktop_open() {
        Some(NOTHING_LOCKS.to_string())
    } else if status.state == "changed" {
        Some(format!("{LIMINE_DEFAULTS} or the initramfs changed since this was turned on; turn it off to put everything back."))
    } else {
        status.unavailable.clone()
    };
    json!({
        "state": status.state,
        "on": status.on(),
        "next_start_asks_password": status.next_start_asks,
        "secure_boot": status.secure_boot,
        "tpm": status.tpm,
        "available": status.unavailable.is_none(),
        "note": note,
        "help": TRADE_OFF,
        "automatic_sign_in": status.automatic_sign_in,
        "lock_at_sign_in": status.lock_at_sign_in,
        "lock_help": LOCK_HELP,
        "message": status_text(&status),
    })
}

fn load_state() -> Option<State> {
    serde_json::from_slice(&std::fs::read(STATE).ok()?).ok()
}

fn tpm_present() -> bool {
    std::fs::read_to_string("/sys/class/tpm/tpm0/tpm_version_major").is_ok_and(|v| v.trim() == "2")
}

/// The account SDDM signs in without a password, from its settings files.
fn automatic_sign_in() -> Option<String> {
    let mut texts = Vec::new();
    for place in SDDM_CONFIGS {
        let path = Path::new(place);
        if path.is_dir() {
            let mut files: Vec<PathBuf> =
                std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "conf")).collect();
            files.sort();
            texts.extend(files.iter().filter_map(|f| std::fs::read_to_string(f).ok()));
        } else if let Ok(text) = std::fs::read_to_string(path) {
            texts.push(text);
        }
    }
    autologin_user(&texts)
}

/// `[Autologin] User=` across SDDM's files in the order it reads them; the
/// last one set wins, and an empty value turns automatic sign-in off.
fn autologin_user(texts: &[String]) -> Option<String> {
    let mut user = None;
    for text in texts {
        let mut section = "";
        for line in text.lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim();
            } else if section == "Autologin"
                && let Some((key, value)) = line.split_once('=')
                && key.trim() == "User"
            {
                user = Some(value.trim().to_string()).filter(|v| !v.is_empty());
            }
        }
    }
    user
}

fn sign_in_lock_on(home: &Path) -> bool {
    std::fs::read(home.join(SIGN_IN_LOCK)).is_ok_and(|body| body == SIGN_IN_LOCK_TEXT.as_bytes())
}

/// The home of the account running this, as its session and Omarchy's hooks see it ($HOME).
fn own_home(me: &super::Account) -> PathBuf {
    std::env::var_os("HOME").filter(|home| !home.is_empty()).map(PathBuf::from).unwrap_or_else(|| me.home.clone())
}

/// Whether the screen locks right after signing in (ibara's hook, as ibara wrote it): for the
/// account SDDM signs in automatically, else for the one running this.
fn sign_in_lock(automatic: Option<&str>) -> bool {
    let me = super::Account::current().ok();
    match (automatic, me) {
        (Some(user), Some(me)) if me.name == user => sign_in_lock_on(&own_home(&me)),
        (Some(user), _) => super::Account::desktop(user).is_ok_and(|account| sign_in_lock_on(&account.home)),
        (None, Some(me)) => sign_in_lock_on(&own_home(&me)),
        (None, None) => false,
    }
}

/// Refused while SDDM signs an account in by itself and nothing locks its screen.
fn sign_in_locked() -> Result<(), String> {
    match automatic_sign_in() {
        Some(user) if !sign_in_lock(Some(&user)) => Err(format!("{SIGNS_IN_BY_ITSELF} Run: ibara unattended-boot lock-at-sign-in on")),
        _ => Ok(()),
    }
}

/// The firmware's SecureBoot variable: four attribute bytes, then 1 when on.
/// UEFI firmware without the variable has no Secure Boot at all.
fn secure_boot() -> Option<bool> {
    match std::fs::read(SECURE_BOOT) {
        Ok(bytes) => bytes.get(4).map(|b| *b == 1),
        Err(_) => Path::new("/sys/firmware/efi").is_dir().then_some(false),
    }
}

// ---------------------------------------------------------------------------
// The root parts.

/// `ibara system unattended-boot …` (root).
pub fn system(args: &[String]) -> Result<(), String> {
    if !is_root() {
        return Err("This part of ibara runs as root. Run ibara unattended-boot instead; it asks for your password.".into());
    }
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["enable"] => enable(false),
        ["enable", "--dry-run"] => enable(true),
        ["disable"] => disable(),
        ["boot-check"] => boot_check(),
        _ => Err(SYSTEM_USAGE.into()),
    }
}

/// `cryptdevice=DEVICE:NAME[:OPTIONS]`, the busybox `encrypt` hook's word.
#[derive(Debug, Clone, PartialEq)]
struct CryptDevice {
    word: String,
    device: String,
    name: String,
    /// crypttab options for the same behavior (`discard`, …).
    options: Vec<&'static str>,
}

fn parse_cryptdevice(cmdline: &str) -> Result<CryptDevice, String> {
    let words: Vec<&str> = cmdline.split_whitespace().collect();
    if let Some(other) = words.iter().find(|w| w.starts_with("rd.luks.") || w.starts_with("luks.") || w.starts_with("cryptkey=")) {
        return Err(format!("The kernel line already has {other}, which is not how Omarchy sets it up, so ibara leaves it alone."));
    }
    let crypt: Vec<&str> = words.iter().copied().filter(|w| w.starts_with("cryptdevice=")).collect();
    let [word] = crypt.as_slice() else {
        return Err(if crypt.is_empty() {
            "The kernel line has no cryptdevice=, so this computer does not unlock its disk the way Omarchy sets it up.".into()
        } else {
            "The kernel line has more than one cryptdevice=, which ibara does not change.".into()
        });
    };
    // Split as the hook splits it: device, name, then the options.
    let mut parts = word["cryptdevice=".len()..].splitn(3, ':');
    let (device, name, options) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));
    if device.is_empty() || !plain(name) {
        return Err(format!("{word} is not a cryptdevice= ibara understands."));
    }
    let mut mapped = Vec::new();
    for option in options.split(',').filter(|o| !o.is_empty()) {
        mapped.push(match option {
            "allow-discards" | "discard" => "discard",
            "no-read-workqueue" | "perf-no_read_workqueue" => "no-read-workqueue",
            "no-write-workqueue" | "perf-no_write_workqueue" => "no-write-workqueue",
            other => return Err(format!("The cryptdevice option {other} has no systemd equivalent ibara knows, so it leaves this computer alone.")),
        });
    }
    Ok(CryptDevice { word: word.to_string(), device: device.to_string(), name: name.to_string(), options: mapped })
}

/// The kernel-line words systemd's initramfs opens the same disk with, the TPM first.
fn unlock_words(uuid: &str, crypt: &CryptDevice) -> String {
    let mut options = vec!["tpm2-device=auto"];
    options.extend(&crypt.options);
    format!("rd.luks.name={uuid}={} rd.luks.options={uuid}={}", crypt.name, options.join(","))
}

/// `text` with the one `from` replaced by `to`; refused when `from` is not there exactly once.
fn switch_once(text: &str, from: &str, to: &str, file: &str) -> Result<String, String> {
    match text.matches(from).count() {
        1 => Ok(text.replacen(from, to, 1)),
        0 => Err(format!("{file} does not have {from}, so ibara stopped rather than guess.")),
        _ => Err(format!("{file} has {from} more than once, so ibara stopped rather than guess.")),
    }
}

/// A `KEY=value` from limine-entry-tool's configuration files, the last one read winning.
fn entry_setting(key: &str) -> Option<String> {
    let mut files = vec![PathBuf::from(ENTRY_CONFIGS[0])];
    if let Ok(dir) = std::fs::read_dir(ENTRY_CONFIGS[1]) {
        let mut more: Vec<PathBuf> = dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "conf")).collect();
        more.sort();
        files.extend(more);
    }
    files.push(PathBuf::from(LIMINE_DEFAULTS));
    let mut found = None;
    for file in files {
        for line in std::fs::read_to_string(&file).unwrap_or_default().lines() {
            if let Some(value) = line.trim().strip_prefix(key).and_then(|rest| rest.strip_prefix('=')) {
                found = Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    found
}

fn esp() -> PathBuf {
    PathBuf::from(entry_setting("ESP_PATH").unwrap_or_else(|| "/boot".into()))
}

/// One boot entry of limine.conf: `//name` (depth 2 is a kernel of this OS).
#[derive(Debug, Default, PartialEq)]
struct Entry {
    depth: usize,
    name: String,
    path: Option<String>,
    cmdline: Option<String>,
}

fn limine_entries(text: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let depth = line.chars().take_while(|c| *c == '/').count();
        if depth > 0 {
            let name = line[depth..].trim_start_matches('+').trim().to_string();
            entries.push(Entry { depth, name, ..Entry::default() });
        } else if let Some(last) = entries.last_mut() {
            if let Some(path) = line.strip_prefix("path:") {
                last.path = Some(path.trim().to_string());
            } else if let Some(cmdline) = line.strip_prefix("cmdline:") {
                last.cmdline = Some(cmdline.trim().to_string());
            }
        }
    }
    entries
}

/// This OS's kernel entries that limine-mkinitcpio writes (not the kept one).
fn kernel_entries(entries: &[Entry]) -> impl Iterator<Item = &Entry> {
    entries.iter().filter(|e| e.depth == 2 && e.cmdline.is_some() && e.name != FALLBACK)
}

/// `boot():/EFI/Linux/x.efi#hash` → `/boot/EFI/Linux/x.efi`.
fn entry_file(esp: &Path, path: &str) -> Option<PathBuf> {
    let inside = path.strip_prefix("boot():")?.split('#').next()?;
    Some(esp.join(inside.trim_start_matches('/')))
}

fn limine_conf() -> Result<(PathBuf, String), String> {
    let path = esp().join("limine.conf");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}. Omarchy's Limine keeps its menu there.", path.display()))?;
    Ok((path, text))
}

/// The hooks mkinitcpio builds with: its configuration, then its drop-ins in order.
fn effective_hooks() -> Result<Vec<String>, String> {
    let script = r#"set +u; HOOKS=(); . /etc/mkinitcpio.conf; for f in /etc/mkinitcpio.conf.d/*.conf; do [[ -f $f ]] && . "$f"; done; printf '%s\n' "${HOOKS[@]}""#;
    Ok(run("bash", &["-c", script])?.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
}

fn has(hooks: &[String], name: &str) -> bool {
    hooks.iter().any(|h| h == name)
}

/// Whether `systemd-cryptenroll DEVICE` lists a key slot of `kind` (`tpm2`, `password`).
fn has_slot(device: &str, kind: &str) -> Result<bool, String> {
    let text = run("systemd-cryptenroll", &[device])?;
    let mut rows = text.lines().skip_while(|l| !l.split_whitespace().eq(["SLOT", "TYPE"]));
    rows.next();
    Ok(rows.any(|row| row.split_whitespace().nth(1) == Some(kind)))
}

/// The kernel package that is running (`linux-omarchy`), which names its boot entry.
fn running_kernel() -> Result<String, String> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").map_err(|e| format!("The running kernel: {e}"))?;
    let builtin = format!("/usr/lib/modules/{}/modules.builtin", release.trim());
    let name = run("pacman", &["-Qqo", &builtin]).map_err(|_| format!("No installed kernel package owns {builtin}; restart after the last kernel update, then try again."))?;
    Ok(name.trim().to_string())
}

fn resolve_device(spec: &str) -> Result<PathBuf, String> {
    let by = |dir: &str, id: &str| PathBuf::from(format!("/dev/disk/{dir}/{id}"));
    let path = if let Some(id) = spec.strip_prefix("PARTUUID=") {
        by("by-partuuid", &id.to_lowercase())
    } else if let Some(id) = spec.strip_prefix("UUID=") {
        by("by-uuid", id)
    } else if let Some(id) = spec.strip_prefix("PARTLABEL=") {
        by("by-partlabel", id)
    } else if let Some(id) = spec.strip_prefix("LABEL=") {
        by("by-label", id)
    } else {
        PathBuf::from(spec)
    };
    std::fs::canonicalize(&path).map_err(|e| format!("The disk {spec} ({}): {e}", path.display()))
}

/// Everything enable changes, read and checked before anything changes.
#[derive(Debug)]
struct Plan {
    device: String,
    luks_uuid: String,
    crypt: CryptDevice,
    unlock: String,
    defaults: String,
    kernel_cmdline: Option<String>,
    kernel: String,
    uki: PathBuf,
    old_cmdline: String,
    hooks: Vec<String>,
    secure_boot: Option<bool>,
}

fn check() -> Result<Plan, String> {
    if Path::new(STATE).exists() {
        return Err("Starting without the disk password is already on. ibara unattended-boot status says more; ibara unattended-boot disable turns it off.".into());
    }
    for name in ["cryptsetup", "systemd-cryptenroll", "limine-mkinitcpio", "limine-entry-tool", "lsinitcpio", "bash"] {
        program(name).ok_or_else(|| format!("{name} is missing, so ibara cannot change how this computer starts."))?;
    }
    let cmdline = std::fs::read_to_string("/proc/cmdline").map_err(|e| format!("/proc/cmdline: {e}"))?;
    let crypt = parse_cryptdevice(&cmdline)?;
    let device = resolve_device(&crypt.device)?.display().to_string();
    let mapped = run("cryptsetup", &["status", &crypt.name]).map_err(|_| format!("The disk is not open as {}, which its kernel line names.", crypt.name))?;
    let field = |key: &str| mapped.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_string()));
    if field("type:").as_deref() != Some("LUKS2") {
        return Err("The disk is not LUKS2, and only LUKS2 can hold a TPM key.".into());
    }
    if field("device:").as_deref() != Some(device.as_str()) {
        return Err(format!("{} is open from another device than {device}, which the kernel line names.", crypt.name));
    }
    let luks_uuid = run("cryptsetup", &["luksUUID", &device])?.trim().to_string();
    if !has_slot(&device, "password")? {
        return Err("The disk has no password key slot, so there would be nothing to fall back on.".into());
    }
    if has_slot(&device, "tpm2")? {
        return Err(format!("The disk already has a TPM key that ibara did not add. Remove it first (systemd-cryptenroll {device} --wipe-slot=tpm2) or leave things as they are."));
    }
    if !tpm_present() || !run("systemd-cryptenroll", &["--tpm2-device=list"]).is_ok_and(|t| t.lines().any(|l| l.starts_with("/dev/tpmrm"))) {
        return Err("This computer has no TPM 2.0 chip that Linux can use, so its disk cannot unlock by itself.".into());
    }
    for file in ["/usr/lib/initcpio/install/systemd", "/usr/lib/initcpio/install/sd-encrypt", "/usr/lib/initcpio/install/sd-vconsole", "/usr/lib/cryptsetup/libcryptsetup-token-systemd-tpm2.so"] {
        if !Path::new(file).is_file() {
            return Err(format!("{file} is missing, so systemd could not unlock the disk at start."));
        }
    }
    if entry_setting("ENABLE_UKI").as_deref() != Some("yes") {
        return Err("This computer's boot images are not the unified kernel images Omarchy builds (ENABLE_UKI=yes), so ibara leaves them alone.".into());
    }
    let defaults = std::fs::read_to_string(LIMINE_DEFAULTS).map_err(|e| format!("{LIMINE_DEFAULTS}: {e}"))?;
    switch_once(&defaults, &crypt.word, "", LIMINE_DEFAULTS)?;
    let kernel_cmdline = std::fs::read_to_string(KERNEL_CMDLINE).ok().filter(|text| text.contains(&crypt.word));
    if !Path::new(OMARCHY_HOOKS).is_file() {
        return Err(format!("{OMARCHY_HOOKS} is missing, so this is not the initramfs Omarchy sets up."));
    }
    if Path::new(HOOKS_DROPIN).exists() || Path::new(ENTRY_DROPIN).exists() {
        return Err(format!("{HOOKS_DROPIN} or {ENTRY_DROPIN} is left from an earlier try; run ibara unattended-boot disable first."));
    }
    let hooks = effective_hooks()?;
    if !has(&hooks, "udev") || !has(&hooks, "encrypt") || has(&hooks, "systemd") || has(&hooks, "sd-encrypt") {
        return Err(format!("mkinitcpio builds with the hooks {}, not Omarchy's udev and encrypt, so ibara leaves them alone.", hooks.join(" ")));
    }
    if has(&hooks, "btrfs-overlayfs") && !Path::new("/usr/lib/initcpio/install/sd-btrfs-overlayfs").is_file() {
        return Err("The initramfs uses btrfs-overlayfs, and its systemd version (sd-btrfs-overlayfs) is missing.".into());
    }
    let kernel = running_kernel()?;
    let (conf, text) = limine_conf()?;
    let entries = limine_entries(&text);
    if entries.iter().any(|e| e.name == FALLBACK) {
        return Err(format!("{} already has an entry named {FALLBACK}; remove it first (limine-entry-tool --remove-uki {FALLBACK}).", conf.display()));
    }
    let running = kernel_entries(&entries)
        .find(|e| e.name == kernel)
        .ok_or_else(|| format!("{} has no entry for the running kernel {kernel}.", conf.display()))?;
    let old_cmdline = running.cmdline.clone().unwrap_or_default();
    if !old_cmdline.split_whitespace().any(|w| w == crypt.word) || old_cmdline.contains('"') {
        return Err(format!("The {kernel} entry in {} does not start the way this computer started now.", conf.display()));
    }
    let uki = running
        .path
        .as_deref()
        .and_then(|p| entry_file(&esp(), p))
        .filter(|p| p.is_file())
        .ok_or_else(|| format!("The image of the {kernel} entry in {} is missing.", conf.display()))?;
    let unlock = unlock_words(&luks_uuid, &crypt);
    // Last: whether it is worth doing comes after whether it can be done.
    sign_in_locked()?;
    Ok(Plan { device, luks_uuid, crypt, unlock, defaults, kernel_cmdline, kernel, uki, old_cmdline, hooks, secure_boot: secure_boot() })
}

fn print_plan(plan: &Plan, backup: &Path) {
    println!("  Disk: {} (LUKS2 {}), opened as {}", plan.device, plan.luks_uuid, plan.crypt.name);
    println!(
        "  Secure Boot: {} (a later change to it makes the next start ask for the password)",
        match plan.secure_boot {
            Some(true) => "on",
            Some(false) => "off",
            None => "unknown",
        }
    );
    if let Some(user) = automatic_sign_in() {
        println!("  Sign-in: Omarchy signs {user} in automatically; the screen locks right after (lock at sign-in is on)");
    }
    println!("  Back up the LUKS header, {LIMINE_DEFAULTS}, {KERNEL_CMDLINE} and limine.conf to {}", backup.display());
    println!("  Keep a boot entry {FALLBACK} on a copy of {} (the {} image running now)", plan.uki.display(), plan.kernel);
    println!("    with the kernel line it has now: {}", plan.old_cmdline);
    println!("  Write {HOOKS_DROPIN}; mkinitcpio then builds with:");
    println!("    {}", switched_hooks(&plan.hooks).join(" "));
    println!("  In {LIMINE_DEFAULTS}{}, replace", if plan.kernel_cmdline.is_some() { format!(" and {KERNEL_CMDLINE}") } else { String::new() });
    println!("    {}", plan.crypt.word);
    println!("  with");
    println!("    {}", plan.unlock);
    println!("  Rebuild the boot images: limine-mkinitcpio");
    println!("  Add a TPM2 key to {} beside the password, sealed to PCR 7 (the Secure Boot state) as the new", plan.device);
    println!("    initramfs will find it: systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs=7:sha256=…");
    println!("  {TRADE_OFF}");
}

/// What the drop-in makes of `hooks`, the same mapping as its shell (checked after writing it).
fn switched_hooks(hooks: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for hook in hooks {
        let next = match hook.as_str() {
            "udev" => "systemd",
            "keymap" | "consolefont" if out.iter().any(|h| h == "sd-vconsole") => continue,
            "keymap" | "consolefont" => "sd-vconsole",
            "encrypt" => "sd-encrypt",
            "btrfs-overlayfs" => "sd-btrfs-overlayfs",
            "resume" => continue,
            other => other,
        };
        out.push(next.to_string());
    }
    out
}

/// What enable has done so far, so a failure can put it back.
#[derive(Default)]
struct Done {
    /// The boot images were being rebuilt, so putting back rebuilds them too.
    images: bool,
}

fn enable(dry_run: bool) -> Result<(), String> {
    println!("Checking this computer:");
    let plan = check()?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let backup = Path::new(BACKUPS).join("unattended-boot").join(stamp.to_string());
    if dry_run {
        println!("Everything is as Omarchy sets it up. Turning this on would:");
        print_plan(&plan, &backup);
        println!("Nothing was changed (--dry-run).");
        return Ok(());
    }
    print_plan(&plan, &backup);
    back_up(&plan, &backup)?;
    let state = State {
        state: "waiting".into(),
        device: plan.device.clone(),
        luks_uuid: plan.luks_uuid.clone(),
        name: plan.crypt.name.clone(),
        cryptdevice: plan.crypt.word.clone(),
        unlock: plan.unlock.clone(),
        kernel_cmdline_file: plan.kernel_cmdline.is_some(),
        backup: backup.display().to_string(),
        enabled_at: stamp,
    };
    let mut done = Done::default();
    match switch_on(&plan, &state, &backup, &mut done) {
        Ok(()) => {
            println!("\nDone. The next start unlocks the disk with the TPM, without asking for the password.");
            println!("Until it has once, the boot menu keeps {FALLBACK}, which asks for the password on the previous image.");
            println!("Backups: {}", backup.display());
            Ok(())
        }
        Err(error) => {
            println!("\nThat did not work, so ibara is putting everything back.");
            let undone = put_back(&state, done.images);
            match undone {
                Ok(()) => Err(format!("{error}\nEverything was put back; starting asks for the disk password as before.")),
                Err(also) => Err(format!("{error}\nPutting it back also failed: {also}\nThe backups are in {}.", backup.display())),
            }
        }
    }
}

fn back_up(plan: &Plan, dir: &Path) -> Result<(), String> {
    root_dir(dir, 0o700)?;
    run("cryptsetup", &["luksHeaderBackup", &plan.device, "--header-backup-file", &dir.join("luks-header.img").display().to_string()])?;
    let (conf, text) = limine_conf()?;
    write_root(&dir.join("limine"), plan.defaults.as_bytes(), 0o600)?;
    write_root(&dir.join("limine.conf"), text.as_bytes(), 0o600)?;
    if let Ok(cmdline) = std::fs::read(KERNEL_CMDLINE) {
        write_root(&dir.join("kernel-cmdline"), &cmdline, 0o600)?;
    }
    println!("Backed up the LUKS header, {LIMINE_DEFAULTS} and {} to {}", conf.display(), dir.display());
    Ok(())
}

fn switch_on(plan: &Plan, state: &State, backup: &Path, done: &mut Done) -> Result<(), String> {
    // First, so `disable` can put back whatever a crash in the middle leaves.
    write_state(state)?;

    // The kept entry: the running image, with the kernel line it booted with.
    println!("\nKeeping {FALLBACK} on the image running now:");
    let copy = backup.join(format!("{FALLBACK}.efi"));
    std::fs::copy(&plan.uki, &copy).map_err(|e| format!("Could not copy {}: {e}", plan.uki.display()))?;
    write_root(Path::new(ENTRY_DROPIN), entry_dropin(&plan.old_cmdline).as_bytes(), 0o644)?;
    let comment = "Asks for the disk password. ibara keeps this until the disk has unlocked by itself once.";
    interactive(Command::new("limine-entry-tool").args(["--add-uki", FALLBACK]).arg(&copy).args(["--comment", comment]))?;
    let (_, text) = limine_conf()?;
    let entries = limine_entries(&text);
    let kept = entries.iter().find(|e| e.name == FALLBACK).ok_or_else(|| format!("limine-entry-tool did not add {FALLBACK}."))?;
    if kept.cmdline.as_deref() != Some(plan.old_cmdline.as_str()) {
        return Err(format!("{FALLBACK} did not get the kernel line it needs ({}).", kept.cmdline.as_deref().unwrap_or("none")));
    }

    println!("\nSwitching the initramfs and the kernel line to systemd:");
    write_root(Path::new(HOOKS_DROPIN), HOOKS_TEXT.as_bytes(), 0o644)?;
    let hooks = effective_hooks()?;
    if hooks != switched_hooks(&plan.hooks) {
        return Err(format!("mkinitcpio would build with {} instead of {}.", hooks.join(" "), switched_hooks(&plan.hooks).join(" ")));
    }
    write_root(Path::new(LIMINE_DEFAULTS), switch_once(&plan.defaults, &plan.crypt.word, &plan.unlock, LIMINE_DEFAULTS)?.as_bytes(), 0o644)?;
    if let Some(text) = &plan.kernel_cmdline {
        write_root(Path::new(KERNEL_CMDLINE), switch_once(text, &plan.crypt.word, &plan.unlock, KERNEL_CMDLINE)?.as_bytes(), 0o644)?;
    }

    println!("\nRebuilding the boot images (limine-mkinitcpio):");
    done.images = true;
    let separator = rebuild(&plan.unlock, &plan.crypt.word)?;

    // The key is sealed to PCR 7 as the new initramfs will find it when it opens the disk.
    let now = pcr7()?;
    let at_start = pcr7_at_start(&now, separator_measured(&std::fs::read_to_string(MEASURE_LOG).unwrap_or_default()), separator)?;
    println!("\nAdding the TPM key; systemd-cryptenroll asks for the disk password:");
    interactive(Command::new("systemd-cryptenroll").arg("--tpm2-device=auto").arg(format!("--tpm2-pcrs=7:sha256={at_start}")).arg(&plan.device))?;
    if !has_slot(&plan.device, "tpm2")? {
        return Err("systemd-cryptenroll finished, but the disk lists no TPM key.".into());
    }
    if at_start == now {
        // The same Secure Boot state opens it at the next start.
        run("cryptsetup", &["open", "--test-passphrase", "--token-only", &plan.device]).map_err(|e| format!("The TPM did not open the disk just now ({e})."))?;
        println!("The TPM opened the disk just now, as it will at the next start.");
    }
    run("systemctl", &["enable", UNIT])?;
    Ok(())
}

/// PCR 7 (Secure Boot state, SHA-256 bank) as the kernel reads it now.
fn pcr7() -> Result<String, String> {
    let path = "/sys/class/tpm/tpm0/pcr-sha256/7";
    let value = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{path} does not hold a SHA-256 value."));
    }
    Ok(value)
}

/// Where systemd keeps what it measured into the TPM this boot (JSON records, each after an RS).
const MEASURE_LOG: &str = "/run/log/systemd/tpm2-measure.log";

/// Whether this boot's systemd already extended PCR 7 with `os-separator`.
fn separator_measured(log: &str) -> bool {
    log.split('\u{1e}').filter_map(|record| serde_json::from_str::<Value>(record.trim()).ok()).any(|record| {
        record["pcr"] == json!(7) && record["content"]["eventType"] == json!("os-separator")
    })
}

/// PCR 7 when systemd's initramfs opens the disk. Its systemd-pcrosseparator
/// extends PCR 7 with SHA-256("os-separator") first (on a measured UKI boot),
/// which Omarchy's busybox initramfs never does, so a key sealed to PCR 7 as
/// it is now would not open at the next start.
fn pcr7_at_start(now: &str, measured_now: bool, measured_next: bool) -> Result<String, String> {
    use sha2::Digest;
    match (measured_now, measured_next) {
        (false, true) => {
            let mut bytes = Vec::with_capacity(64);
            for i in (0..now.len()).step_by(2) {
                bytes.push(u8::from_str_radix(&now[i..i + 2], 16).map_err(|_| "PCR 7 is not hex.".to_string())?);
            }
            bytes.extend_from_slice(&sha2::Sha256::digest(b"os-separator"));
            Ok(crate::store::canonical::hex_encode(&sha2::Sha256::digest(&bytes)))
        }
        (true, false) => Err("This start measured more into PCR 7 than the new boot image will, so ibara cannot tell what the TPM will see.".into()),
        _ => Ok(now.to_string()),
    }
}

fn entry_dropin(cmdline: &str) -> String {
    format!(
        "# Written by `ibara unattended-boot enable`: the {FALLBACK} entry boots the image from before\n# with the kernel line it had. ibara removes this once the disk has unlocked by itself.\nKERNEL_CMDLINE[{FALLBACK}]=\"{cmdline}\"\n"
    )
}

fn write_state(state: &State) -> Result<(), String> {
    root_dir(Path::new(STATE).parent().unwrap_or(Path::new("/etc")), 0o755)?;
    let mut body = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    body.push(b'\n');
    write_root(Path::new(STATE), &body, 0o644)
}

/// limine-mkinitcpio, then every kernel entry checked: it goes on to the next
/// kernel when one fails, so its exit status alone says too little. `gone`:
/// the words no kernel line may keep. True when the images extend PCR 7 with
/// `os-separator` before opening the disk.
fn rebuild(want: &str, gone: &str) -> Result<bool, String> {
    interactive(&mut Command::new("limine-mkinitcpio"))?;
    let (conf, text) = limine_conf()?;
    let entries = limine_entries(&text);
    let mut separators = Vec::new();
    for entry in kernel_entries(&entries) {
        let cmdline = entry.cmdline.as_deref().unwrap_or("");
        if !cmdline.contains(want) || cmdline.split_whitespace().any(|w| gone.split_whitespace().any(|g| g == w)) {
            return Err(format!("The {} entry in {} did not get the new kernel line.", entry.name, conf.display()));
        }
        let image = entry.path.as_deref().and_then(|p| entry_file(&esp(), p)).ok_or_else(|| format!("The {} entry has no image.", entry.name))?;
        let listing = run("lsinitcpio", &[&image.display().to_string()])?;
        let systemd = listing.lines().any(|l| l.ends_with("usr/lib/systemd/systemd-cryptsetup"));
        if systemd != want.contains("rd.luks.") {
            return Err(format!("The image of {} ({}) was not rebuilt with the right hooks.", entry.name, image.display()));
        }
        separators.push(systemd && listing.lines().any(|l| l.ends_with("systemd-pcrosseparator.service")));
    }
    let Some(first) = separators.first().copied() else {
        return Err(format!("{} lists no kernel entries after the rebuild.", conf.display()));
    };
    if separators.iter().any(|s| *s != first) {
        return Err("The rebuilt images differ in what they measure into the TPM, so one key cannot open the disk from all of them.".into());
    }
    // systemd-pcrosseparator runs only on a boot the kernel's stub measured.
    Ok(first && Path::new(MEASURED_UKI).exists())
}

/// Set by systemd-stub when it measured the UKI (systemd's `measured-os`).
const MEASURED_UKI: &str = "/sys/firmware/efi/efivars/StubPcrKernelImage-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

fn disable() -> Result<(), String> {
    let Some(state) = load_state() else {
        return Err(format!("ibara did not turn on starting without the disk password here ({STATE} is missing), so there is nothing for it to put back."));
    };
    println!("Putting back the disk password at every start:");
    put_back(&state, true)?;
    println!("\nDone. The next start asks for the disk password. Backups stay in {}.", state.backup);
    Ok(())
}

/// Every change enable made, undone; safe to repeat. `images`: rebuild them.
fn put_back(state: &State, images: bool) -> Result<(), String> {
    for (file, needed) in [(LIMINE_DEFAULTS, true), (KERNEL_CMDLINE, state.kernel_cmdline_file)] {
        if !needed {
            continue;
        }
        let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        if text.contains(&state.unlock) {
            write_root(Path::new(file), switch_once(&text, &state.unlock, &state.cryptdevice, file)?.as_bytes(), 0o644)?;
            println!("  {file}: {} is back", state.cryptdevice);
        } else if !text.contains(&state.cryptdevice) {
            return Err(format!("{file} has neither {} nor {}; put it back from {} by hand.", state.unlock, state.cryptdevice, state.backup));
        }
    }
    if Path::new(HOOKS_DROPIN).exists() {
        std::fs::remove_file(HOOKS_DROPIN).map_err(|e| format!("{HOOKS_DROPIN}: {e}"))?;
        println!("  Removed {HOOKS_DROPIN}");
    }
    if images {
        println!("\nRebuilding the boot images (limine-mkinitcpio):");
        rebuild(&state.cryptdevice, &state.unlock)?;
    }
    forget_fallback()?;
    if has_slot(&state.device, "tpm2")? {
        run("systemd-cryptenroll", &[&state.device, "--wipe-slot=tpm2"])?;
        if has_slot(&state.device, "tpm2")? {
            return Err(format!("The TPM key is still on {} after wiping it.", state.device));
        }
        println!("  Removed the TPM key from {}", state.device);
    }
    let _ = run("systemctl", &["disable", UNIT]);
    if Path::new(STATE).exists() {
        std::fs::remove_file(STATE).map_err(|e| format!("{STATE}: {e}"))?;
    }
    Ok(())
}

/// The `ask-disk-password` entry, its kernel line and its image, gone.
fn forget_fallback() -> Result<(), String> {
    let (_, text) = limine_conf()?;
    if limine_entries(&text).iter().any(|e| e.name == FALLBACK) {
        run("limine-entry-tool", &["--remove-uki", FALLBACK, "--quiet"])?;
        println!("  Removed the {FALLBACK} boot entry");
    }
    if Path::new(ENTRY_DROPIN).exists() {
        std::fs::remove_file(ENTRY_DROPIN).map_err(|e| format!("{ENTRY_DROPIN}: {e}"))?;
    }
    Ok(())
}

/// The agents that put a password prompt on the screen; one runs whenever systemd-cryptsetup asks.
const ASK_PASSWORD: [&str; 3] =
    ["systemd-ask-password-plymouth.service", "systemd-ask-password-console.service", "systemd-ask-password-wall.service"];

/// Whether this boot's disk opened with the TPM key, from the journal: its
/// cryptsetup unit finished, said nothing about the TPM (it only speaks of it
/// when the key fails), and no password prompt ran. The TPM path itself is silent.
fn unlocked_by_tpm(cryptsetup_log: &str, ask_password_log: &str) -> bool {
    cryptsetup_log.lines().any(|l| l.starts_with("Finished "))
        && !cryptsetup_log.contains("TPM")
        && !cryptsetup_log.contains("falling back")
        && ask_password_log.trim().is_empty()
}

/// At each start (`ibara-unattended-boot.service`): once the disk has
/// unlocked by itself, the kept entry is no longer needed.
fn boot_check() -> Result<(), String> {
    let Some(mut state) = load_state() else { return Ok(()) };
    if state.state != "waiting" {
        let _ = run("systemctl", &["disable", UNIT]);
        return Ok(());
    }
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let words: Vec<&str> = cmdline.split_whitespace().collect();
    if !state.unlock.split_whitespace().all(|w| words.contains(&w)) {
        println!("This start used another entry; {FALLBACK} stays.");
        return Ok(());
    }
    let unit = format!("systemd-cryptsetup@{}.service", state.name);
    let log = run("journalctl", &["-q", "-b", "-o", "cat", "-u", &unit]).unwrap_or_default();
    let agents = ASK_PASSWORD.iter().flat_map(|agent| ["-u", agent]);
    let asked = run("journalctl", &["-q", "-b", "-o", "cat"].into_iter().chain(agents).collect::<Vec<_>>()).unwrap_or_default();
    if !unlocked_by_tpm(&log, &asked) {
        println!("The disk did not unlock by itself this time; {FALLBACK} stays.");
        return Ok(());
    }
    forget_fallback()?;
    state.state = "on".into();
    write_state(&state)?;
    let _ = run("systemctl", &["disable", UNIT]);
    println!("The disk unlocked by itself; the {FALLBACK} entry is no longer needed.");
    Ok(())
}


/// Failure cases (the switch itself is proven in a virtual machine):
/// 1. `cryptdevice=PARTUUID=…:root` split at the wrong colon, so the wrong disk or name is used.
/// 2. The hook's discard option dropped (TRIM silently off), or an unknown option dropped silently.
/// 3. A kernel line already on rd.luks, or with two cryptdevice words, changed anyway.
/// 4. /etc/default/limine rewritten when the word is there zero or two times.
/// 5. The drop-in, sourced by bash after Omarchy's hooks, keeps udev, encrypt, resume or
///    btrfs-overlayfs, repeats sd-vconsole, drops a hook Omarchy added, or differs from what
///    enable checks it against.
/// 6. Snapshot entries or the OS heading taken for kernel entries; an image path read with its hash.
/// 7. The key sealed to PCR 7 as Omarchy's busybox boot reads it, so the first start through
///    systemd's initramfs (which extends PCR 7 with `os-separator` first) asks for the password;
///    or the separator counted twice when this boot already measured it.
/// 8. The kept entry removed after a start where the TPM refused and the password was typed
///    (systemd-cryptsetup is silent when the TPM opens the disk, and says little when it doesn't).
/// 9. Automatic sign-in missed (set in a drop-in, spaced around `=`), taken from another section,
///    or still seen as on after a file SDDM reads later empties it.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_sign_in_is_read_as_sddm_reads_it() {
        let defaults = "[Autologin]\nRelogin=false\nSession=\nUser=\n\n[General]\nHaltCommand=/usr/bin/systemctl poweroff\n".to_string();
        let omarchy = "[Autologin]\nUser=riley\nSession=omarchy.desktop\n".to_string();
        assert_eq!(autologin_user(&[defaults.clone(), omarchy.clone()]), Some("riley".into()));
        assert_eq!(autologin_user(&["[Autologin]\n  User = riley \n".into()]), Some("riley".into()));
        assert_eq!(autologin_user(&[omarchy, "[Autologin]\nUser=\n".into()]), None);
        assert_eq!(autologin_user(&[defaults, "[Users]\nRememberLastUser=true\nUser=riley\n".into()]), None);
        assert_eq!(autologin_user(&[]), None);
    }

    const OMARCHY: &str = "cryptdevice=PARTUUID=3c1e5a2b-7d40-4f86-9a1b-2e6c0d8f4b17:root root=/dev/mapper/root zswap.enabled=0 rootflags=subvol=@ rw rootfstype=btrfs  resume=/dev/mapper/root resume_offset=1899451 quiet splash";

    #[test]
    fn cryptdevice_is_split_as_the_encrypt_hook_splits_it() {
        let crypt = parse_cryptdevice(OMARCHY).unwrap();
        assert_eq!(crypt.device, "PARTUUID=3c1e5a2b-7d40-4f86-9a1b-2e6c0d8f4b17");
        assert_eq!(crypt.name, "root");
        assert_eq!(crypt.word, "cryptdevice=PARTUUID=3c1e5a2b-7d40-4f86-9a1b-2e6c0d8f4b17:root");
        assert_eq!(
            unlock_words("5e2d9c41", &crypt),
            "rd.luks.name=5e2d9c41=root rd.luks.options=5e2d9c41=tpm2-device=auto"
        );
        let discard = parse_cryptdevice("cryptdevice=/dev/nvme0n1p2:cryptroot:allow-discards,perf-no_read_workqueue root=/dev/mapper/cryptroot").unwrap();
        assert_eq!((discard.device.as_str(), discard.name.as_str()), ("/dev/nvme0n1p2", "cryptroot"));
        assert_eq!(
            unlock_words("u", &discard),
            "rd.luks.name=u=cryptroot rd.luks.options=u=tpm2-device=auto,discard,no-read-workqueue"
        );
        assert!(parse_cryptdevice("cryptdevice=/dev/sda2:root:sector-size=4096").is_err());
    }

    #[test]
    fn kernel_lines_not_laid_out_as_omarchy_does_are_refused() {
        assert!(parse_cryptdevice("root=/dev/sda2 rw quiet").is_err());
        assert!(parse_cryptdevice("rd.luks.name=u=root root=/dev/mapper/root").is_err());
        assert!(parse_cryptdevice(&format!("{OMARCHY} rd.luks.options=tpm2-device=auto")).is_err());
        assert!(parse_cryptdevice(&format!("{OMARCHY} cryptdevice=/dev/sdb1:data")).is_err());
        assert!(parse_cryptdevice(&format!("{OMARCHY} cryptkey=rootfs:/key")).is_err());
        assert!(parse_cryptdevice("cryptdevice=/dev/sda2 root=/dev/mapper/root").is_err());
    }

    #[test]
    fn the_kernel_line_file_changes_only_when_the_word_is_there_once() {
        let word = "cryptdevice=PARTUUID=f9:root";
        let file = format!("ESP_PATH=\"/boot\"\nKERNEL_CMDLINE[default]+=\"{word} root=/dev/mapper/root rw\"\n");
        let switched = switch_once(&file, word, "rd.luks.name=u=root", "limine").unwrap();
        assert_eq!(switched, "ESP_PATH=\"/boot\"\nKERNEL_CMDLINE[default]+=\"rd.luks.name=u=root root=/dev/mapper/root rw\"\n");
        assert_eq!(switch_once(&switched, "rd.luks.name=u=root", word, "limine").unwrap(), file);
        assert!(switch_once("KERNEL_CMDLINE[default]+=\"quiet\"", word, "x", "limine").is_err());
        assert!(switch_once(&format!("{file}KERNEL_CMDLINE[linux]+=\"{word}\"\n"), word, "x", "limine").is_err());
    }

    /// Omarchy's HOOKS line (omarchy_hooks.conf) plus omarchy_resume.conf, then the drop-in, in bash.
    fn sourced(hooks: &str) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("ibara-hooks-{}-{}", std::process::id(), hooks.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let dropin = dir.join("zz-ibara-unattended-boot.conf");
        std::fs::write(&dropin, HOOKS_TEXT).unwrap();
        let script = format!("set -u; HOOKS=({hooks}); HOOKS+=(resume); . '{}'; printf '%s\\n' \"${{HOOKS[@]}}\"", dropin.display());
        let out = Command::new("bash").args(["-c", &script]).output().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().lines().map(str::to_string).collect()
    }

    #[test]
    fn the_drop_in_switches_omarchys_hooks_to_systemd_and_keeps_the_rest() {
        let omarchy = "base udev plymouth keyboard autodetect microcode modconf kms keymap consolefont block encrypt filesystems fsck btrfs-overlayfs";
        let expected: Vec<String> =
            "base systemd plymouth keyboard autodetect microcode modconf kms sd-vconsole block sd-encrypt filesystems fsck sd-btrfs-overlayfs"
                .split(' ')
                .map(str::to_string)
                .collect();
        let got = sourced(omarchy);
        assert_eq!(got, expected);
        let mut before: Vec<String> = omarchy.split(' ').map(str::to_string).collect();
        before.push("resume".into());
        assert_eq!(switched_hooks(&before), got);
        // Omarchy drops kms on NVIDIA-only computers; the drop-in keeps that.
        let nvidia = omarchy.replace(" kms", "");
        assert_eq!(sourced(&nvidia), expected.iter().filter(|h| *h != "kms").cloned().collect::<Vec<_>>());
    }

    const LIMINE_CONF: &str = "default_entry: 2
/+Omarchy
comment: machine-id=a3f0 order-priority=50
  //linux-omarchy
  comment: kernel-id=linux-omarchy
  protocol: efi
  path: boot():/EFI/Linux/omarchy_linux-omarchy.efi#d40c6b77
  cmdline: cryptdevice=PARTUUID=f9:root root=/dev/mapper/root rw

  //ask-disk-password
  path: boot():/EFI/Linux/omarchy_ask-disk-password.efi#0a1b
  cmdline: cryptdevice=PARTUUID=f9:root root=/dev/mapper/root rw

     //Snapshots
     ///1 │ 2026-09-23 20:04:05
     ////linux
     path: boot():/a3f0/limine_history/omarchy_linux.efi_sha256_8d2b#081c
     cmdline: cryptdevice=PARTUUID=f9:root rootflags=subvol=/@/.snapshots/1/snapshot
";

    #[test]
    fn only_this_systems_kernel_entries_count() {
        let entries = limine_entries(LIMINE_CONF);
        let kernels: Vec<&str> = kernel_entries(&entries).map(|e| e.name.as_str()).collect();
        assert_eq!(kernels, ["linux-omarchy"]);
        assert!(entries.iter().any(|e| e.name == FALLBACK && e.depth == 2));
        let path = entries[1].path.as_deref().unwrap();
        assert_eq!(entry_file(Path::new("/boot"), path).unwrap(), PathBuf::from("/boot/EFI/Linux/omarchy_linux-omarchy.efi"));
    }

    /// PCR 7 read in the VM (OVMF, swtpm) during the Omarchy boot, and in the same VM once
    /// systemd's initramfs had run, with the record systemd logged for it.
    const PCR7_BUSYBOX: &str = "b5710bf57d25623e4019027da116821fa99f5c81e9e38b87671cc574f9281439";
    const PCR7_SYSTEMD: &str = "03f359c6155efb9c42dd82c470c66f3579ea5721690787e6b1d3b0ac199ff501";
    const SEPARATOR_RECORD: &str = r#"{"pcr":7,"digests":[{"hashAlg":"sha256","digest":"ff5b9d73dad709633ae76adf444012b57e913a12ed7403c3931145862f35f841"}],"content_type":"systemd","content":{"string":"os-separator","bootId":"189c18dd7ba245f38cf49d9ac154b525","timestamp":1515761,"eventType":"os-separator"}}"#;

    #[test]
    fn the_key_is_sealed_to_pcr7_as_systemds_initramfs_finds_it() {
        assert_eq!(pcr7_at_start(PCR7_BUSYBOX, false, true).unwrap(), PCR7_SYSTEMD);
        assert_eq!(pcr7_at_start(PCR7_SYSTEMD, true, true).unwrap(), PCR7_SYSTEMD);
        assert_eq!(pcr7_at_start(PCR7_BUSYBOX, false, false).unwrap(), PCR7_BUSYBOX);
        assert!(pcr7_at_start(PCR7_SYSTEMD, true, false).is_err());
        let other_pcr = SEPARATOR_RECORD.replace(r#""pcr":7"#, r#""pcr":0"#);
        assert!(separator_measured(&format!("\u{1e}{other_pcr}\n\u{1e}{SEPARATOR_RECORD}\n")));
        assert!(!separator_measured(&format!("\u{1e}{other_pcr}\n")));
        assert!(!separator_measured(""));
    }

    #[test]
    fn only_a_start_the_tpm_opened_counts_as_unlocked_by_itself() {
        // Journal lines from the VM's three kinds of start.
        let opened = "Starting Cryptography Setup for root...\nFinished Cryptography Setup for root.\n";
        let cleared = "Starting Cryptography Setup for root...\nFailed to unseal secret using TPM2: State not recoverable\nTPM2 operation failed, falling back to traditional unlocking: State not recoverable\nSet cipher aes, mode xts-plain64, key size 512 bits for device /dev/disk/by-uuid/6f38.\nFinished Cryptography Setup for root.\n";
        let mismatch = "Starting Cryptography Setup for root...\nTPM policy does not match current system state. Either system has been tampered with or policy out-of-date: Operation not permitted\nFinished Cryptography Setup for root.\n";
        let prompted = "Started Forward Password Requests to Plymouth.\n";
        assert!(unlocked_by_tpm(opened, ""));
        assert!(!unlocked_by_tpm(cleared, prompted));
        assert!(!unlocked_by_tpm(mismatch, prompted));
        // A prompt that ran (for this disk or another) is never an unattended start.
        assert!(!unlocked_by_tpm(opened, prompted));
        // Still waiting, or never started: not yet.
        assert!(!unlocked_by_tpm("Starting Cryptography Setup for root...\n", ""));
    }
}
