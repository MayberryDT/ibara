//! Focused-window keyboard escape hatch, including an inhibited Hyprland seat.
//!
//! One shared Super+Alt+Escape binding runs `ibara-screen view --toggle-keys`,
//! which toggles whichever viewer has the focus. It is bound and unbound only
//! through Hyprland's chord-string API, as ibara-view does: Hyprland 0.56.2
//! crashes when a Lua keybind handle is unbound after its binding was removed
//! (`keybindRemove`, LuaKeybind.cpp:73), so no handle is ever kept.
use anyhow::{Context, Result, ensure};
use std::{
    cell::Cell,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixDatagram},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const CHORD: &str = "SUPER + ALT + Escape";
const DESCRIPTION: &str = "Ibara Screen: switch keys between computers";

fn directory() -> Result<PathBuf> {
    let dir = PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?).join("ibara-screen");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}
fn socket(pid: u32) -> Result<PathBuf> {
    Ok(directory()?.join(format!("view-{pid}.sock")))
}
pub fn toggle_focused() -> Result<()> {
    let out = std::process::Command::new("hyprctl")
        .args(["-j", "activewindow"])
        .output()?;
    ensure!(out.status.success(), "focused window unavailable");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    if v["class"] != "io.zet.ibara.Screen" {
        return Ok(());
    }
    let pid = v["pid"].as_u64().context("viewer pid unavailable")? as u32;
    UnixDatagram::unbound()?.send_to(b"toggle", socket(pid)?)?;
    Ok(())
}
fn eval(lua: &str) -> Option<std::process::Output> {
    std::process::Command::new("hyprctl").args(["eval", lua]).output().ok()
}
/// Whether Hyprland currently has our binding; `None` when it cannot be read.
fn bound() -> Option<bool> {
    let out = std::process::Command::new("hyprctl").args(["-j", "binds"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(v.as_array()?.iter().any(|b| b["description"] == DESCRIPTION))
}
/// Another viewer process still running (its socket names a live pid).
fn other_viewer_alive(dir: &Path) -> bool {
    let me = std::process::id();
    fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.strip_prefix("view-")?.strip_suffix(".sock")?.parse::<u32>().ok());
        pid.is_some_and(|p| p != me && Path::new(&format!("/proc/{p}")).exists())
    })
}
pub struct Toggle {
    socket: UnixDatagram,
    path: PathBuf,
    install: Option<String>,
    checked: Cell<Instant>,
}
impl Toggle {
    pub fn new() -> Result<Self> {
        let path = socket(std::process::id())?;
        let _ = fs::remove_file(&path);
        let socket = UnixDatagram::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        socket.set_nonblocking(true)?;
        let mut install = None;
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            let exe = std::env::current_exe()?;
            let quoted = format!("'{}'", exe.to_string_lossy().replace('\'', "'\\''"));
            let command = serde_json::to_string(&format!("{quoted} view --toggle-keys"))?;
            let lua = format!(
                "pcall(hl.unbind, '{CHORD}'); hl.bind('{CHORD}', hl.dsp.exec_cmd({command}), {{ dont_inhibit = true, description = '{DESCRIPTION}' }})"
            );
            let out = eval(&lua).context("hyprctl unavailable")?;
            ensure!(
                out.status.success() && !String::from_utf8_lossy(&out.stdout).contains("error"),
                "Hyprland keyboard escape binding failed"
            );
            install = Some(lua);
        }
        Ok(Self { socket, path, install, checked: Cell::new(Instant::now()) })
    }
    pub fn requested(&self) -> bool {
        if self.checked.get().elapsed() >= Duration::from_millis(500) {
            self.checked.set(Instant::now());
            // A config reload drops runtime bindings: put ours back.
            if let Some(lua) = &self.install
                && bound() == Some(false)
            {
                let _ = eval(lua);
            }
        }
        let mut b = [0; 32];
        matches!(self.socket.recv(&mut b), Ok(6)) && &b[..6] == b"toggle"
    }
}
impl Drop for Toggle {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        // Leave the shared binding to any other running viewer, and never
        // remove a binding someone else (ibara-view) installed since.
        if self.install.is_some()
            && !self.path.parent().is_some_and(other_viewer_alive)
            && bound() == Some(true)
        {
            let _ = eval(&format!("pcall(hl.unbind, '{CHORD}')"));
        }
    }
}
