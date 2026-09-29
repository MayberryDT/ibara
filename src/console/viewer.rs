//! The viewer the console opens for Take Control and Open Viewer: ibara-view,
//! given a one-time ticket on its stdin, or Moonlight for a computer that
//! still shares its screen the old way.
//!
//! This console's viewer identity is one certificate, made once by
//! `ibara-view --create-identity` in `~/.local/state/ibara/viewer` and
//! registered on each computer before taking control (`viewer_register`).

use super::envelope::{Fault, clip};
use super::process::{self, Run, launch, launch_with_input, which};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The Moonlight stream the bridge opened for a held computer.
const MOONLIGHT_STREAM: [&str; 13] = [
    "stream", "--1080", "--fps", "30", "--bitrate", "10000", "--display-mode", "windowed", "--absolute-mouse",
    "--capture-system-keys", "never", "--video-codec", "H.264",
];

/// The keys that switch the keyboard between the two computers.
pub const KEYS_CHORD: &str = "Super+Alt+Escape";

/// `IBARA_VIEW_BIN`, else the first `ibara-view` on `PATH` that is a
/// program (ELF), skipping older scripts of that name.
pub fn ibara_view() -> Option<PathBuf> {
    let elf = |path: &Path| {
        let mut magic = [0u8; 4];
        std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && magic == *b"\x7fELF"
    };
    if let Some(path) = std::env::var_os("IBARA_VIEW_BIN").filter(|p| !p.is_empty()) {
        return which(&path.to_string_lossy()).filter(|p| elf(p));
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("ibara-view"))
        .find(|p| which(&p.to_string_lossy()).is_some() && elf(p))
}

/// `$XDG_STATE_HOME/ibara/viewer`, else `~/.local/state/ibara/viewer`.
pub fn identity_dir() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::operator::home_dir().join(".local/state"));
    state.join("ibara").join("viewer")
}

/// The lowercase hex SHA-256 of the certificate (DER) in a PEM file.
fn cert_sha256(pem: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pem).ok()?;
    let body = text.split("-----BEGIN CERTIFICATE-----").nth(1)?.split("-----END CERTIFICATE-----").next()?;
    let der = base64::engine::general_purpose::STANDARD.decode(body.split_whitespace().collect::<String>()).ok()?;
    Some(crate::store::canonical::hex_encode(&Sha256::digest(der)))
}

/// This console's viewer certificate fingerprint, making the identity the
/// first time.
pub async fn viewer_identity(ibara_view: &Path) -> Result<String, Fault> {
    let dir = identity_dir();
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    if key.is_file()
        && let Some(sha) = cert_sha256(&cert)
    {
        return Ok(sha);
    }
    let dir_text = dir.display().to_string();
    let out = process::run(ibara_view, &["--create-identity", dir_text.as_str()], Run::timeout(Duration::from_secs(20))).await?;
    let printed = out.stdout_text().trim().to_string();
    match cert_sha256(&cert) {
        Some(sha) if out.success() && printed == sha && key.is_file() => Ok(sha),
        _ => Err(Fault::plain("The viewer could not make its identity on this computer.")),
    }
}

/// Start ibara-view with `bundle` (its ticket) on stdin and float its window;
/// the pid is the viewer's.
pub async fn launch_ibara_view(ibara_view: &Path, bundle: &Value) -> Result<u32, Fault> {
    let argv = [ibara_view.display().to_string(), "connect".to_string()];
    let pid = launch_with_input(&argv, bundle.to_string().as_bytes()).await?;
    if pid != 0 {
        place(pid).await;
    }
    Ok(pid)
}

/// Hyprland Lua (`hyprctl eval`, Hyprland 0.56) that floats every window the
/// viewer process `VIEWER_PID` maps, centred, at 1280×720 logical pixels, or the
/// largest 16:9 that fits the output's free area. The person can still tile it
/// or make it full screen. Only this launch is affected: Hyprland's exec rules
/// reach just the first window a process maps (Moonlight's own interface; the
/// stream window follows once connected) and its window rules cannot match a
/// pid, so a `window.open` handler matches the viewer's pid for ten minutes. A
/// Hyprland reload drops the handler; the next launch installs it again.
///
/// Omarchy's `apps/moonlight.lua` rule makes every Moonlight window full screen
/// when it maps, and Hyprland runs `window.open` handlers after that, so the
/// handler first leaves full screen: a floating window stays full screen, and
/// resizing or centring it changes nothing the person sees.
const PLACEMENT: &str = r#"
local now = os.time()
ibara_viewers = ibara_viewers or {}
for pid, at in pairs(ibara_viewers) do
  if now - at > 600 then ibara_viewers[pid] = nil end
end
ibara_viewers[VIEWER_PID] = now
function ibara_place_viewer(w)
  local at = ibara_viewers[w.pid]
  if not at or os.time() - at > 600 then return end
  local width = 1280
  local m = w.monitor
  if m then
    local mw, mh = m.width / m.scale, m.height / m.scale
    if m.transform % 2 == 1 then mw, mh = mh, mw end
    local r = m.reserved
    mw, mh = mw - r.left - r.right, mh - r.top - r.bottom
    width = math.max(160, math.floor(math.min(width, mw, mh * 16 / 9)))
  end
  hl.dispatch(hl.dsp.window.fullscreen_state({ internal = 0, client = 0, window = w }))
  hl.dispatch(hl.dsp.window.float({ action = "enable", window = w }))
  hl.dispatch(hl.dsp.window.resize({ x = width, y = math.floor(width * 9 / 16), window = w }))
  hl.dispatch(hl.dsp.window.center({ window = w }))
end
if not ibara_viewer_hook then
  ibara_viewer_hook = hl.on("window.open", function(w) ibara_place_viewer(w) end)
end
for _, w in ipairs(hl.get_windows()) do
  if w.pid == VIEWER_PID then ibara_place_viewer(w) end
end
"#;

/// Start Moonlight for `host` and float its windows; the pid is Moonlight's.
/// Only for computers from before ibara-view.
pub async fn launch_viewer(moonlight: &Path, host: &str) -> Result<u32, Fault> {
    let mut argv: Vec<String> = vec![moonlight.display().to_string()];
    argv.extend(MOONLIGHT_STREAM.iter().map(|s| s.to_string()));
    argv.push(host.to_string());
    argv.push("Desktop".to_string());
    let pid = launch(&argv)?;
    if pid != 0 {
        place(pid).await;
    }
    Ok(pid)
}

/// Best effort: when Hyprland cannot take the handler, its own rules place the
/// viewer (Omarchy's make it full screen) and the reason goes to the journal.
async fn place(pid: u32) {
    let Some(hyprctl) = which("hyprctl") else { return };
    let lua = PLACEMENT.replace("VIEWER_PID", &pid.to_string());
    let failure = match process::run(hyprctl, &["-i", "0", "eval", lua.as_str()], Run::timeout(Duration::from_secs(2))).await {
        Ok(out) if out.success() && !out.stdout_text().contains("error") => return,
        Ok(out) => format!("{} {}", out.stdout_text(), out.stderr_text()),
        Err(Fault::Coded(_, message) | Fault::Timeout(message) | Fault::Plain(message)) => message,
    };
    eprintln!("ibarad: Hyprland refused the viewer's placement, so its own rules place it: {}", clip(failure.trim(), 300));
}
