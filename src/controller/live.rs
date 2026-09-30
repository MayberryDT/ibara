//! The live ports: `crate::desktop::Desktop` (plus the Chrome bridge) behind
//! `DesktopPort`, and `ControllerOptions::live` building everything from the
//! configured paths and the startup policy snapshot.

use super::ports::{Button, Capture, Cancel, DesktopPort, Done, Effect, Image, LocalFuture, OutputChange, Preview, PutBack, Rect, Tab, Win, WinKey};
use super::stream::LiveStream;
use super::{ComputerIdentity, ControllerOptions, EffectRules, GrantSource};
use crate::desktop::atspi::ElementPage;
use crate::desktop::chrome::ChromeBridge;
use crate::desktop::watch::DesktopEvent;
use crate::desktop::{self, ClickTarget, Desktop, DesktopConfig, ImageBudget, PreviewFormat, PreviewQuality, SurfaceId, selection};
use crate::error::{Result, unavailable};
use crate::storage::{StorageOptions, StorageService};
use crate::store::JournalOptions;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::broadcast;

/// What `ibarad` knows at start.
pub struct LiveConfig {
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub install_root: PathBuf,
    /// Holds `bin/ibara-wtype`, `bin/ibara-atspi` and `ops/ibara-editor`.
    pub release_root: PathBuf,
    pub procedures_dir: PathBuf,
    /// `policy.json` as read at start (limits already validated by the server).
    pub policy: Value,
    /// `{...policy.operator_grants, ...operator-authority.json}`, re-read per call.
    pub operator_grants: GrantSource,
    /// The Chrome native-host bridge, already listening on `chrome.sock`.
    pub chrome: Option<Arc<ChromeBridge>>,
}

impl ControllerOptions {
    /// Build the desktop, storage and stream ports from the configured paths.
    pub fn live(cfg: LiveConfig) -> Result<ControllerOptions> {
        let desktop = Desktop::new(DesktopConfig::new(&cfg.release_root, &cfg.runtime_dir, &cfg.state_dir));
        let mut storage_options = StorageOptions::from_env(&cfg.policy)?;
        storage_options.root_dir = cfg.data_dir.clone();
        storage_options.state_dir = Some(cfg.state_dir.clone());
        storage_options.candidate_procedures_dir = Some(cfg.data_dir.join("candidates"));
        storage_options.approved_procedures_dir = Some(cfg.procedures_dir.clone());
        let storage = StorageService::open(storage_options)?;
        let port: Rc<dyn DesktopPort> = Rc::new(LiveDesktop {
            desktop: Arc::new(desktop),
            chrome: cfg.chrome,
            watching: RefCell::new(None),
            versions: RefCell::new(HashMap::new()),
            pasting: RefCell::new(None),
        });
        let mut options = ControllerOptions::new(&cfg.state_dir, storage, port);
        let limit = |key: &str| cfg.policy.get(key).and_then(Value::as_u64);
        options.journal = JournalOptions {
            max_metadata_bytes: limit("max_metadata_bytes"),
            metadata_headroom_bytes: limit("metadata_headroom_bytes"),
            metadata_retention_ms: limit("metadata_retention_ms").map(|v| v as i64),
            ..Default::default()
        };
        options.stream = Some(Rc::new(LiveStream::new(&cfg.state_dir)));
        options.operator_grants = cfg.operator_grants;
        options.effect_rules = EffectRules::from_policy(&cfg.policy);
        options.computer = computer_identity();
        let _ = cfg.install_root;
        Ok(options)
    }
}

/// This computer's names: `/etc/ibara/station.json` (`display_label`,
/// `station_id`, `node`, `agent_account`), else the hostname.
fn computer_identity() -> ComputerIdentity {
    let host = std::fs::read_to_string("/etc/hostname").ok().map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
    let file = std::env::var_os("IBARA_STATION_FILE").filter(|f| !f.is_empty()).unwrap_or_else(|| "/etc/ibara/station.json".into());
    let station: Option<Value> = std::fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let field = |key: &str| station.as_ref().and_then(|s| s.get(key)).and_then(Value::as_str).map(str::to_string).filter(|v| !v.is_empty());
    let name = field("display_label").or_else(|| field("station_id")).or_else(|| host.clone()).unwrap_or_else(|| "this computer".into());
    let labels = [field("station_id"), field("node"), host].into_iter().flatten().collect();
    let user = field("agent_account").or_else(|| std::env::var("USER").ok());
    ComputerIdentity { name, labels, user }
}

/// `WAYLAND_DISPLAY && XDG_RUNTIME_DIR` (`server.ts:144`).
fn session_env() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    set("WAYLAND_DISPLAY") && set("XDG_RUNTIME_DIR")
}

struct LiveDesktop {
    desktop: Arc<Desktop>,
    chrome: Option<Arc<ChromeBridge>>,
    watching: RefCell<Option<tokio::task::JoinHandle<()>>>,
    versions: RefCell<HashMap<String, Option<String>>>,
    /// The person's clipboard while ibara pastes.
    pasting: RefCell<Option<selection::SetAside>>,
}

fn surface(key: &WinKey) -> SurfaceId {
    SurfaceId { address: key.address.clone(), pid: key.pid, class: key.class.clone() }
}

fn button(b: Button) -> desktop::Button {
    match b {
        Button::Left => desktop::Button::Left,
        Button::Right => desktop::Button::Right,
        Button::Middle => desktop::Button::Middle,
    }
}

fn rect(r: desktop::Rect) -> Rect {
    Rect { x: r.x, y: r.y, width: r.width, height: r.height }
}

fn no_browser() -> crate::error::IbaraError {
    unavailable("The Chrome extension is not connected.").with("execution_not_started", true)
}

impl DesktopPort for LiveDesktop {
    fn session_available(&self) -> bool {
        session_env()
    }

    fn session_ready(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(self.desktop.session_ready())
    }

    fn control_ready(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(self.desktop.control_ready())
    }

    fn locked(&self) -> LocalFuture<'_, bool> {
        Box::pin(self.desktop.locked())
    }

    fn windows(&self) -> LocalFuture<'_, Result<Vec<Win>>> {
        Box::pin(async move {
            let (windows, active) = tokio::join!(self.desktop.windows(), self.desktop.focused());
            let active = active?.map(|a| a.address);
            Ok(windows?
                .into_iter()
                .filter(|w| w.mapped && !w.hidden)
                .map(|w| {
                    let g = w.geometry();
                    Win {
                        focused: active.as_deref() == Some(w.address.as_str()),
                        address: w.address,
                        pid: w.pid,
                        class: w.class,
                        title: w.title,
                        floating: w.floating,
                        workspace: w.workspace.name,
                        rect: rect(g),
                    }
                })
                .collect())
        })
    }

    fn elements<'a>(&'a self, key: &'a WinKey, query: Option<&'a str>, limit: u32, cursor: Option<u32>) -> LocalFuture<'a, Result<ElementPage>> {
        Box::pin(async move { self.desktop.elements(&surface(key), query, limit, cursor).await })
    }

    fn act<'a>(&'a self, effect: &'a Effect, cancel: &'a Cancel) -> LocalFuture<'a, Result<Done>> {
        Box::pin(async move {
            match effect {
                Effect::Launch { app_id } => self.desktop.launch(app_id, None, Some(cancel)).await.map(|l| Done { pid: Some(l.pid), ..Done::default() }),
                Effect::Focus(key) => self.desktop.focus(&surface(key), Some(cancel)).await.map(|_| Done::default()),
                Effect::Close(key) => self.desktop.close(&surface(key), Some(cancel)).await.map(|_| Done::default()),
                Effect::ClickElement { surface: key, element, button: b, double } => {
                    let target = ClickTarget::Element(element.target(surface(key)));
                    self.desktop.click(&target, button(*b), *double, Some(cancel)).await.map(|point| Done { point, ..Done::default() })
                }
                Effect::ClickPoint { x, y, surface: key, button: b, double } => {
                    let target = ClickTarget::Point { x: *x, y: *y, surface: key.as_ref().map(surface) };
                    self.desktop.click(&target, button(*b), *double, Some(cancel)).await.map(|point| Done { point, ..Done::default() })
                }
                Effect::Type { surface: key, text, cursor } => self.desktop.type_text(&surface(key), text, *cursor, Some(cancel)).await.map(|_| Done::default()),
                Effect::Key { surface: key, combo } => self.desktop.key(&surface(key), combo, Some(cancel)).await.map(|_| Done::default()),
                Effect::Scroll { at, dx, dy } => {
                    let at = at.map(|(x, y)| desktop::Point { x, y });
                    self.desktop.scroll(at, *dx, *dy, Some(cancel)).await.map(|_| Done::default())
                }
            }
        })
    }

    fn capture<'a>(&'a self, target: &'a Capture, max_bytes: usize) -> LocalFuture<'a, Result<Image>> {
        Box::pin(async move {
            let budget = ImageBudget { max_bytes, ..ImageBudget::default() };
            let img = match target {
                Capture::Surface(key) => self.desktop.capture_surface(&surface(key), &budget).await?,
                Capture::Screen => self.desktop.capture_screen(&budget).await?,
                Capture::Replay(key) => {
                    let small = ImageBudget { max_bytes, max_width: 640, max_height: 640 };
                    self.desktop.capture_surface(&surface(key), &small).await?
                }
            };
            Ok(Image { mime: img.mime_type().to_string(), width: img.width, height: img.height, region: rect(img.region), bytes: img.bytes })
        })
    }

    fn browser_connected(&self) -> bool {
        self.chrome.as_ref().is_some_and(|c| c.connected())
    }

    fn browser_reader_installed(&self) -> bool {
        crate::entry::browser_setup::reader_installed(std::path::Path::new("/"))
    }

    fn tabs(&self) -> LocalFuture<'_, Result<Vec<Tab>>> {
        Box::pin(async move {
            let Some(chrome) = &self.chrome else { return Err(no_browser()) };
            Ok(chrome.tabs().await?.into_iter().map(|t| Tab { id: t.id, title: t.title, url: t.url, focused: t.focused }).collect())
        })
    }

    fn browser_call<'a>(&'a self, op: &'a str, args: Value, effect: bool) -> LocalFuture<'a, Result<Value>> {
        Box::pin(async move {
            let Some(chrome) = &self.chrome else { return Err(no_browser()) };
            chrome.call(op, args, effect).await
        })
    }

    fn browser_cancel(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(async move {
            match &self.chrome {
                Some(chrome) => chrome.cancel().await,
                None => Ok(()),
            }
        })
    }

    /// The extension reconnects 2 s after its port closes.
    fn browser_reconnect(&self) -> LocalFuture<'_, bool> {
        Box::pin(async move {
            match &self.chrome {
                Some(chrome) => chrome.reconnect(std::time::Duration::from_secs(5)).await,
                None => false,
            }
        })
    }

    fn outputs(&self) -> LocalFuture<'_, Result<Vec<super::ports::DisplayInfo>>> {
        Box::pin(async move {
            Ok(self
                .desktop
                .outputs()
                .await?
                .into_iter()
                .map(|o| super::ports::DisplayInfo { display_id: o.display_id, label: o.label, display_revision: o.display_revision })
                .collect())
        })
    }

    fn preview<'a>(&'a self, display_id: &'a str, quality: &'a str, format: &'a str) -> LocalFuture<'a, Result<Preview>> {
        Box::pin(async move {
            let frame = self.desktop.preview(display_id, PreviewQuality::parse(quality)?, PreviewFormat::parse(format)).await?;
            Ok(Preview {
                picture: frame.picture.clone(),
                width: frame.width,
                height: frame.height,
                source_width: frame.source_width,
                source_height: frame.source_height,
                display_revision: frame.display_revision.clone(),
            })
        })
    }

    fn video_capability(&self) -> super::ports::VideoCapability {
        self.desktop.video_capability()
    }

    fn observe_video<'a>(&'a self, viewer: &'a str, access: &'a str, display: Option<&'a str>, width: u32, height: u32, cursor: Option<u64>) -> LocalFuture<'a, Result<super::ports::VideoChunk>> {
        Box::pin(self.desktop.observe_video(viewer, access, display, width, height, cursor))
    }

    fn stop_video(&self) {
        self.desktop.stop_video();
    }

    fn memory_pressure(&self) -> Option<String> {
        self.desktop.memory_pressure()
    }

    fn subscribe(&self) -> Option<broadcast::Receiver<DesktopEvent>> {
        Some(self.desktop.subscribe())
    }

    fn start_watch(&self) {
        let mut watching = self.watching.borrow_mut();
        if watching.is_none() && session_env() {
            *watching = Some(self.desktop.start_watch());
        }
    }

    fn release_input(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(self.desktop.release_input())
    }

    fn reset_input(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(self.desktop.reset_input())
    }

    fn has_password_field<'a>(&'a self, key: &'a WinKey) -> LocalFuture<'a, Result<bool>> {
        Box::pin(async move { self.desktop.has_password_field(&surface(key)).await })
    }

    fn set_agent(&self, label: Option<String>) {
        self.desktop.set_agent(label);
    }

    fn set_idle_inhibited(&self, on: bool) -> LocalFuture<'_, Result<()>> {
        Box::pin(async move {
            if !session_env() {
                return Ok(());
            }
            self.desktop.set_idle_inhibited(on).await
        })
    }

    fn keep_awake(&self) -> LocalFuture<'_, Result<bool>> {
        Box::pin(async move {
            if !session_env() {
                return Ok(false);
            }
            self.desktop.keep_awake().await
        })
    }

    fn output_change(&self) -> LocalFuture<'_, Result<Option<OutputChange>>> {
        Box::pin(self.desktop.output_change())
    }

    fn apply_output_change(&self, change: OutputChange) -> LocalFuture<'_, Result<()>> {
        Box::pin(self.desktop.apply_output_change(change))
    }

    fn resize_virtual(&self) -> LocalFuture<'_, Result<bool>> {
        Box::pin(async move {
            if !session_env() {
                return Ok(false);
            }
            self.desktop.resize_virtual().await
        })
    }

    fn capabilities(&self) -> LocalFuture<'_, Vec<Value>> {
        Box::pin(async move { self.desktop.capabilities().await.into_iter().filter_map(|c| serde_json::to_value(c).ok()).collect() })
    }

    /// `PPid:` from `/proc/<pid>/status`.
    fn parent_pid(&self, pid: i64) -> Option<i64> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status.lines().find_map(|l| l.strip_prefix("PPid:")).and_then(|v| v.trim().parse().ok())
    }

    /// The pacman package version of the program behind `pid`, when its
    /// binary's name is also its package's (`/usr/bin/mousepad` → `mousepad 0.6.3`).
    fn app_version(&self, pid: i64) -> Option<String> {
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        let name = exe.file_name()?.to_string_lossy().into_owned();
        if let Some(v) = self.versions.borrow().get(&name) {
            return v.clone();
        }
        let version = pacman_version(&name);
        let mut cache = self.versions.borrow_mut();
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(name, version.clone());
        version
    }

    fn clipboard_read(&self, limit: usize) -> LocalFuture<'_, Result<Option<super::ports::Clip>>> {
        Box::pin(desktop::clipboard::read(limit))
    }

    fn clipboard_write<'a>(&'a self, clip: &'a super::ports::Clip) -> LocalFuture<'a, Result<()>> {
        Box::pin(desktop::clipboard::write(clip))
    }

    fn clipboard_watch(&self) -> Result<tokio::sync::mpsc::Receiver<()>> {
        desktop::clipboard::watch()
    }

    fn clipboard_set_aside(&self) -> LocalFuture<'_, Result<()>> {
        Box::pin(async move {
            let display = selection::display()?;
            let set_aside = blocking(move || selection::set_aside(&display)).await?;
            self.pasting.replace(Some(set_aside));
            Ok(())
        })
    }

    fn clipboard_paste_text<'a>(&'a self, text: &'a str) -> LocalFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(mut set_aside) = self.pasting.take() else {
                return Err(crate::error::internal("The clipboard was not set aside before pasting."));
            };
            let text = text.to_string();
            let (set_aside, offered) = blocking(move || {
                let offered = set_aside.offer_text(&text);
                Ok((set_aside, offered))
            })
            .await?;
            self.pasting.replace(Some(set_aside));
            offered
        })
    }

    fn clipboard_put_back(&self) -> LocalFuture<'_, Result<PutBack>> {
        Box::pin(async move {
            match self.pasting.take() {
                Some(set_aside) => blocking(move || set_aside.put_back()).await,
                None => Ok(PutBack::Unchanged),
            }
        })
    }
}

/// The Wayland clipboard client blocks; it runs off the controller's thread.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(|e| crate::error::internal(format!("clipboard: {e}")))?
}

/// `<name>-<pkgver>-<pkgrel>` in pacman's local database, as `pkgver`.
fn pacman_version(name: &str) -> Option<String> {
    let prefix = format!("{name}-");
    std::fs::read_dir("/var/lib/pacman/local").ok()?.flatten().find_map(|entry| {
        let dir = entry.file_name().to_string_lossy().into_owned();
        let rest = dir.strip_prefix(&prefix)?;
        let (version, release) = rest.rsplit_once('-')?;
        let plausible = version.starts_with(|c: char| c.is_ascii_digit()) && release.bytes().all(|b| b.is_ascii_digit() || b == b'.');
        plausible.then(|| version.to_string())
    })
}

impl Drop for LiveDesktop {
    fn drop(&mut self) {
        if let Some(handle) = self.watching.borrow_mut().take() {
            handle.abort();
        }
    }
}
