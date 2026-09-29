//! Typed checks (evaluated at begin, status and finish) and expectations
//! (awaited after each step until met or deadline).

use super::ports::{Cancel, Win, WinKey};
use super::situation::FrameState;
use super::{Controller, clip, squash};
use crate::contract::{Check, CheckState, Destination, Expectation};
use crate::error::{IbaraError, Result, invalid, unavailable};
use crate::store::TaskRecord;
use serde_json::Value;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// The computer an agent works from, as a send names it.
struct OwnComputer {
    computer_id: String,
    endpoint_id: String,
    label: String,
    host: String,
}

impl crate::operator::directory::Named for OwnComputer {
    fn names(&self) -> crate::operator::directory::ComputerNames<'_> {
        crate::operator::directory::ComputerNames { computer_id: &self.computer_id, endpoint_id: &self.endpoint_id, label: &self.label, host: &self.host }
    }
}

/// `path` as one shell word: as it is when that is safe, else quoted.
fn shell_word(path: &str) -> std::borrow::Cow<'_, str> {
    if !path.is_empty() && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+,:@%=~".contains(&b)) && !path.starts_with('~') {
        path.into()
    } else {
        crate::operator::control::posix_quote(path).into()
    }
}

/// Refuse a destination that is not an absolute path in its plainest form,
/// before anyone is asked about a send to it.
pub(crate) fn destination_path(field: &str, path: &str) -> Result<()> {
    if path.starts_with('/') && path.chars().map(char::len_utf16).sum::<usize>() <= 4096 && crate::storage::jsv::posix_normalize(path) == path {
        return Ok(());
    }
    Err(invalid(format!("{field}: '{}' is not an absolute path without . or .. parts or doubled slashes; give the full path the file must reach", clip(path, 80)))
        .with("execution_not_started", true))
}

const MAX_FILE_READ: u64 = 4 * 1024 * 1024;
const MAX_WITHIN_MS: u64 = 120_000;

/// The default deadline for each kind of expectation (docs/agent-tools.md).
pub(crate) fn default_within(expect: &Expectation) -> Duration {
    let ms = match expect {
        Expectation::Window(_) => 5000,
        Expectation::Dialog(_) => 3000,
        Expectation::Focus(_) => 2000,
        Expectation::Text(_) => 3000,
        Expectation::Element(_) => 3000,
        Expectation::Url(_) => 5000,
        Expectation::File(_) => 3000,
        Expectation::Settled(s) => s.quiet_ms.saturating_add(3000),
    };
    Duration::from_millis(expect.within_ms().unwrap_or(ms).min(MAX_WITHIN_MS))
}

/// How an expectation ended.
#[derive(Debug, Clone)]
pub(crate) struct Awaited {
    pub met: bool,
    pub detail: String,
    pub waited_ms: u64,
}

/// A check's evaluation.
#[derive(Debug, Clone)]
pub(crate) struct Evaluated {
    pub state: CheckState,
    pub detail: Option<String>,
}

impl Evaluated {
    fn met(detail: impl Into<String>) -> Self {
        Evaluated { state: CheckState::Met, detail: Some(detail.into()) }
    }
    fn unmet(detail: impl Into<String>) -> Self {
        Evaluated { state: CheckState::Unmet, detail: Some(detail.into()) }
    }
    fn unknown(detail: impl Into<String>) -> Self {
        Evaluated { state: CheckState::Unknown, detail: Some(detail.into()) }
    }
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

fn has_state(states: &[String], wanted: &str) -> bool {
    let wanted = wanted.to_lowercase();
    let synonyms: &[&str] = match wanted.as_str() {
        "enabled" => &["enabled", "sensitive"],
        "visible" => &["visible", "showing"],
        "disabled" => return !states.iter().any(|s| s == "enabled" || s == "sensitive"),
        "unchecked" => return !states.iter().any(|s| s == "checked"),
        "hidden" => return !states.iter().any(|s| s == "showing"),
        _ => &[],
    };
    states.iter().any(|s| *s == wanted || synonyms.contains(&s.as_str()))
}

/// `app` is any name a launch takes for an approved app (`browser`,
/// `chromium`, `chrome` and `google chrome` all match Chromium and Google
/// Chrome) or part of the window class.
fn window_matches(w: &Win, app: Option<&str>, title: Option<&str>) -> bool {
    use super::situation::{app_class, app_id_for};
    let app = app.map(|a| app_id_for(a).and_then(app_class).unwrap_or(a));
    app.is_none_or(|a| contains_ci(&w.class, a)) && title.is_none_or(|t| contains_ci(&w.title, t))
}

fn read_bounded(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_READ).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Why ibara refuses a place in the home folder.
const OWN: &str = "is in ibara's own folders, not the task's files";
const ACCOUNT: &str = "is in another account's home folder";
const OUTSIDE: &str = "is outside the task workspace and the home folder";

/// Where agents may name absolute paths outside the task workspace: the
/// desktop person's home folder, except every place ibara keeps or trusts
/// there ([`crate::install::home_paths`] and the controller's own folders)
/// and other accounts' homes inside it. A home folder of `/` is none.
///
/// Refused places are kept relative to the home folder, both as named and
/// with symbolic links resolved, so a home folder or an ibara folder reached
/// through a link is still matched.
pub(crate) struct HomeRule {
    dir: Option<PathBuf>,
    real: PathBuf,
    refused: Vec<(PathBuf, &'static str)>,
}

/// Where a path lies for [`HomeRule`].
#[derive(Debug)]
pub(crate) enum Place<'a> {
    /// In the home folder, at this relative path.
    Home(&'a Path),
    Refused(&'static str),
    Outside,
}

impl HomeRule {
    /// `ibara` are the controller's own folders; `passwd` names the other accounts.
    pub(crate) fn new(home: &Path, ibara: &[&Path], passwd: &str) -> HomeRule {
        if home.parent().is_none() {
            return HomeRule { dir: None, real: PathBuf::new(), refused: Vec::new() };
        }
        let real = real_path(home);
        let mut refused = Vec::new();
        let mut refuse = |path: &Path, why: &'static str| {
            for (root, path) in [(home, path.to_path_buf()), (real.as_path(), real_path(path))] {
                if let Ok(rel) = path.strip_prefix(root) {
                    refused.push((rel.to_path_buf(), why));
                }
            }
        };
        for path in crate::install::home_paths(home).iter().map(PathBuf::as_path).chain(ibara.iter().copied()) {
            refuse(path, OWN);
        }
        for other in passwd.lines().filter_map(|line| line.split(':').nth(5)).map(Path::new) {
            if other.is_absolute() && other.parent().is_some() && other != home && real_path(other) != real {
                refuse(other, ACCOUNT);
            }
        }
        HomeRule { dir: Some(home.to_path_buf()), real, refused }
    }

    pub(crate) fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Where `path` (absolute, without `..`) lies, by its name or, when that
    /// differs, with the home folder's links resolved.
    pub(crate) fn place<'a>(&self, path: &'a Path) -> Place<'a> {
        let Some(dir) = &self.dir else { return Place::Outside };
        let Some(rel) = path.strip_prefix(dir).or_else(|_| path.strip_prefix(&self.real)).ok() else { return Place::Outside };
        match self.refused.iter().find(|(place, _)| rel.starts_with(place)) {
            Some((_, why)) => Place::Refused(why),
            None => Place::Home(rel),
        }
    }
}

/// `path` with the links in its longest existing part resolved.
fn real_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut at = path;
    loop {
        if let Ok(found) = std::fs::canonicalize(at) {
            return missing.iter().rev().fold(found, |dir, name| dir.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

impl Controller {
    /// Resolve a check or expectation path: relative paths are in the task's
    /// workspace, `~/` is the desktop user's home, absolute paths must be in
    /// one of those two, outside ibara's own folders ([`HomeRule`]). `..` is
    /// refused. A check reads through links, so the path must also stay
    /// there with its links resolved.
    pub(crate) fn resolve_path(&self, task_ref: &str, path: &str) -> Result<PathBuf> {
        if path.is_empty() || path.contains('\0') {
            return Err(invalid("path: give a file path"));
        }
        let raw = Path::new(path);
        if raw.components().any(|c| c == Component::ParentDir) {
            return Err(invalid(format!("path: '{}' uses '..'; name the file directly", clip(path, 80))));
        }
        let workspace = self.storage.workspace(task_ref, false)?;
        let resolved = match (path.strip_prefix("~/"), self.home.dir()) {
            (Some(rest), Some(home)) => home.join(rest),
            (Some(_), None) => return Err(invalid(format!("path: '{}' {OUTSIDE}; ibara cannot check it here", clip(path, 80)))),
            _ if raw.is_absolute() => raw.to_path_buf(),
            _ => workspace.join(raw),
        };
        let allowed = |path: &Path, workspace: &Path| match self.home.place(path) {
            _ if path.starts_with(workspace) => Ok(()),
            Place::Home(_) => Ok(()),
            Place::Refused(why) => Err(why),
            Place::Outside => Err(OUTSIDE),
        };
        match allowed(&resolved, &workspace).and_then(|()| allowed(&real_path(&resolved), &real_path(&workspace))) {
            Ok(()) => Ok(resolved),
            Err(why) => Err(invalid(format!("path: '{}' {why}; ibara cannot check it here", clip(path, 80)))),
        }
    }

    /// Where a `computer_files` path or `computer_exec` cwd (`field`) lies: a
    /// relative path is in the task's workspace, as is an absolute one inside
    /// it; `~/` and other absolute paths inside the desktop user's home folder
    /// start there, except ibara's own folders and other accounts' homes
    /// ([`HomeRule`]). `..` is refused. Storage then descends from that folder
    /// without following symlinks.
    pub(crate) fn task_path(&self, task_ref: &str, field: &str, path: &str) -> Result<TaskPath> {
        let raw = Path::new(path);
        if raw.components().any(|c| c == Component::ParentDir) {
            return Err(invalid(format!("{field}: '{}' uses '..'; name the file directly", clip(path, 80))));
        }
        let absolute = match (path.strip_prefix('~'), self.home.dir()) {
            (Some(""), Some(home)) => home.to_path_buf(),
            (Some(rest), Some(home)) if rest.starts_with('/') => home.join(rest.trim_start_matches('/')),
            // No home folder: refused below as outside.
            (Some(rest), None) if rest.is_empty() || rest.starts_with('/') => PathBuf::from("~"),
            _ if raw.is_absolute() => raw.components().collect(),
            _ => return Ok(TaskPath { base: None, rel: path.to_string() }),
        };
        let workspace = self.storage.workspace(task_ref, false)?;
        let text = |rel: &Path| if rel.as_os_str().is_empty() { ".".to_string() } else { rel.to_string_lossy().into_owned() };
        if let Ok(rel) = absolute.strip_prefix(&workspace) {
            return Ok(TaskPath { base: None, rel: text(rel) });
        }
        let why = match (self.home.place(&absolute), self.home.dir()) {
            (Place::Home(rel), Some(home)) => return Ok(TaskPath { base: Some(home.to_path_buf()), rel: text(rel) }),
            (Place::Refused(why), _) => why,
            _ => OUTSIDE,
        };
        let home = self.home.dir().map(|home| format!(" or the home folder {} (~/ works too)", home.display())).unwrap_or_default();
        Err(invalid(format!(
            "{field}: '{}' {why}. Use a path relative to the task workspace, or an absolute path inside the workspace {}{home}",
            clip(path, 80),
            workspace.display(),
        )))
    }

    /// Refuse a typed check that can never pass on this computer (at begin).
    pub(crate) fn refuse_impossible_check(&self, task_ref: &str, task_principal: &str, index: usize, check: &Check, capabilities: &[Value]) -> Result<()> {
        let at = |field: &str| format!("checks[{index}].check.{field}");
        let reframe = |e: IbaraError, field: &str| {
            let message = e.message.split_once(": ").map_or(e.message.as_str(), |(_, m)| m).to_string();
            IbaraError::new(e.code, format!("{}: {message}", at(field)), e.retry_safe)
        };
        let accessibility_missing = capabilities.iter().any(|c| {
            let name = c.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase();
            (name.contains("atspi") || name.contains("accessib")) && c.get("status").and_then(Value::as_str) == Some("unavailable")
        });
        match check {
            Check::FileExists(c) => self.resolve_path(task_ref, &c.path).map(|_| ()).map_err(|e| reframe(e, "path")),
            Check::FileContent(c) => self.resolve_path(task_ref, &c.path).map(|_| ()).map_err(|e| reframe(e, "path")),
            Check::Artifact(c) => {
                if c.sha256.len() == 64 && c.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                    Ok(())
                } else {
                    Err(invalid(format!("{}: expected 64 lowercase hex characters", at("sha256"))))
                }
            }
            // A browser task starts with the browser closed: its page reader
            // connects when it opens, and the check is read at finish.
            Check::Url(_) if !self.desktop.browser_connected() && !self.desktop.browser_reader_installed() => Err(unavailable(format!(
                "checks[{index}].check: url checks need ibara's page reader in Chromium or Google Chrome, and it is not installed on this computer; use another check or your assessment"
            ))),
            Check::Element(_) | Check::TextPresent(_) if accessibility_missing => Err(unavailable(format!(
                "checks[{index}].check: this computer has no accessibility tree to check against; use a file check or your assessment"
            ))),
            Check::Delivered(d) => destination_path(&at("path"), &d.path).and_then(|_| self.delivery_host(task_principal, &at("host"), &d.host).map(|_| ())),
            _ => Ok(()),
        }
    }

    /// Evaluate a typed check against current state.
    pub(crate) async fn evaluate_check(&self, task: &TaskRecord, check: &Check) -> Evaluated {
        match check {
            Check::FileExists(c) => match self.resolve_path(&task.task_ref, &c.path) {
                Ok(path) if path.is_file() => Evaluated::met(format!("{} exists", c.path)),
                Ok(_) => Evaluated::unmet(format!("{} does not exist", c.path)),
                Err(e) => Evaluated::unknown(e.message),
            },
            Check::FileContent(c) => match self.resolve_path(&task.task_ref, &c.path).map(|p| read_bounded(&p)) {
                Ok(Ok(text)) => {
                    let ok = match (&c.equals, &c.contains) {
                        (Some(equals), _) => text == *equals || text.trim_end_matches('\n') == equals.trim_end_matches('\n'),
                        (_, Some(contains)) => text.contains(contains.as_str()),
                        _ => false,
                    };
                    if ok { Evaluated::met(format!("{} has the expected content", c.path)) } else { Evaluated::unmet(format!("{} does not have the expected content", c.path)) }
                }
                Ok(Err(_)) => Evaluated::unmet(format!("{} cannot be read", c.path)),
                Err(e) => Evaluated::unknown(e.message),
            },
            Check::Artifact(c) => match self.storage.artifacts(&task.task_ref, &task.principal) {
                Ok(items) => {
                    let found = items.iter().any(|a| {
                        a.get("sha256").and_then(Value::as_str) == Some(c.sha256.as_str())
                            && a.get("delivery").and_then(Value::as_str) != Some("unavailable")
                    });
                    if found { Evaluated::met("a published file has that digest") } else { Evaluated::unmet("no published file has that digest") }
                }
                Err(e) => Evaluated::unknown(e.message),
            },
            Check::Url(c) if !self.desktop.browser_connected() => {
                Evaluated::unmet(format!("no browser with ibara's page reader was open, so no tab's address contains {}", c.contains))
            }
            Check::Url(c) => match self.url_state(&c.contains).await {
                Ok(true) => Evaluated::met(format!("a tab's address contains {}", c.contains)),
                Ok(false) => Evaluated::unmet(format!("no tab's address contains {}", c.contains)),
                Err(e) => Evaluated::unknown(e.message),
            },
            Check::Element(c) => match self.element_state(&c.query, c.state.as_deref(), c.value.as_deref(), None).await {
                Ok(true) => Evaluated::met(format!("element '{}' matches", squash(&c.query, 40))),
                Ok(false) => Evaluated::unmet(format!("no element '{}' matches", squash(&c.query, 40))),
                Err(e) => Evaluated::unknown(e.message),
            },
            Check::TextPresent(c) => match self.text_state(&c.text, None, None).await {
                Ok(true) => Evaluated::met("the text is on screen"),
                Ok(false) => Evaluated::unmet("the text is not in the focused window"),
                Err(e) => Evaluated::unknown(e.message),
            },
            Check::Delivered(d) => self.delivered_state(task, d),
        }
    }

    fn delivered_state(&self, task: &TaskRecord, to: &Destination) -> Evaluated {
        let host = self.delivery_host(&task.principal, "host", &to.host).unwrap_or_else(|_| to.host.clone());
        let matching: Vec<&Value> = task
            .deliveries
            .iter()
            .filter(|d| {
                d.get("host_id").and_then(Value::as_str) == Some(host.as_str()) && d.get("destination_path").and_then(Value::as_str) == Some(to.path.as_str())
            })
            .collect();
        if matching.is_empty() {
            return Evaluated::unmet(format!("nothing has been sent to {host}:{}", to.path));
        }
        if matching.iter().any(|d| self.storage.delivery_verified(&task.task_ref, d)) {
            return Evaluated::met(format!("delivery to {host} verified"));
        }
        match matching.iter().find_map(|d| d.get("artifact_ref").and_then(Value::as_str)) {
            Some(artifact) => Evaluated::unmet(format!("delivery to {host} is not verified yet: {}", self.fetch_hint(artifact, &host, &to.path))),
            None => Evaluated::unmet(format!("nothing has been sent to {host}:{}", to.path)),
        }
    }

    /// The id a delivery to `name` is recorded under: `principal`'s own
    /// computer, the one computer its files can reach, because its collector
    /// runs there. It answers to the names and ids every command takes
    /// ([`crate::operator::directory::pick_computer`]): its name and host in any
    /// case, its `cmp_` or `computer_` id, its endpoint id, and its collector
    /// identity. Anything else is refused before a person is asked, naming it.
    pub(crate) fn delivery_host(&self, principal: &str, field: &str, name: &str) -> Result<String> {
        let canonical = self.storage.collector_host(principal);
        let record = (self.operator_grants)().remove(principal).unwrap_or(Value::Null);
        let paired = crate::access::Access::load(&self.journal).ok().flatten().and_then(|a| a.pairings.get(principal).and_then(|p| p.endpoint.clone()));
        let endpoint_id = paired.or_else(|| record["operator_endpoint_id"].as_str().map(str::to_string)).unwrap_or_default();
        let computer_id = if endpoint_id.is_empty() { String::new() } else { super::computer_id_for(&endpoint_id).replacen("cmp_", "computer_", 1) };
        let own = OwnComputer { computer_id, endpoint_id, label: principal.to_string(), host: record.pointer("/tailscale/node").and_then(Value::as_str).unwrap_or(principal).to_string() };
        let host_name = record.pointer("/tailscale/host_name").and_then(Value::as_str).unwrap_or("");
        if name == canonical || (!host_name.is_empty() && host_name.to_lowercase() == name.to_lowercase()) || crate::operator::directory::pick_computer(std::slice::from_ref(&own), name).is_ok() {
            return Ok(canonical);
        }
        Err(invalid(format!(
            "{field}: \"{}\" is not a computer ibara can send to. A send goes only to the computer you work from, where your collector fetches it: {}.",
            clip(name, 60),
            crate::operator::directory::computer_choices([&own])
        ))
        .with("execution_not_started", true))
    }

    /// How a sent file's bytes move to `host:path`, and when its delivery counts.
    pub(crate) fn fetch_hint(&self, artifact_ref: &str, host: &str, path: &str) -> String {
        format!(
            "On {host}, run `ibara client --computer {} fetch {artifact_ref} {}` to fetch it, or a person saves it there from the ibara console; the delivery is verified only then.",
            self.computer_id,
            shell_word(path)
        )
    }

    async fn url_state(&self, contains: &str) -> Result<bool> {
        if !self.desktop.browser_connected() {
            return Err(unavailable("The Chrome extension is not connected."));
        }
        Ok(self.desktop.tabs().await?.iter().any(|t| t.url.contains(contains)))
    }

    async fn focused_or(&self, surface: Option<&WinKey>) -> Result<Option<WinKey>> {
        if let Some(key) = surface {
            return Ok(Some(key.clone()));
        }
        Ok(self.desktop.windows().await?.into_iter().find(|w| w.focused).map(|w| w.key()))
    }

    async fn element_state(&self, query: &str, state: Option<&str>, value: Option<&str>, surface: Option<&WinKey>) -> Result<bool> {
        let Some(key) = self.focused_or(surface).await? else {
            return Ok(false);
        };
        let page = self.desktop.elements(&key, Some(query), 20, None).await?;
        if !page.available {
            return Err(unavailable("The focused window exposes no accessibility tree."));
        }
        Ok(page.elements.iter().any(|e| {
            let named = contains_ci(&e.name, query) || contains_ci(&e.role, query) || e.value.as_deref().is_some_and(|v| contains_ci(v, query));
            named
                && state.is_none_or(|s| has_state(&e.states, s))
                && value.is_none_or(|v| e.value.as_deref() == Some(v) || (e.value.is_none() && e.name == v))
        }))
    }

    async fn text_state(&self, text: &str, surface: Option<&WinKey>, frame: Option<&FrameState>) -> Result<bool> {
        let _ = frame;
        let Some(key) = self.focused_or(surface).await? else {
            return Ok(false);
        };
        let page = self.desktop.elements(&key, None, 60, None).await?;
        if !page.available {
            return Err(unavailable("The window exposes no accessibility tree; ibara does no OCR."));
        }
        Ok(page.text.contains(text)
            || page.elements.iter().any(|e| e.name.contains(text) || e.value.as_deref().is_some_and(|v| v.contains(text))))
    }

    /// One evaluation of an expectation. `Ok(Some(detail))` when met.
    async fn expectation_now(&self, task_ref: &str, expect: &Expectation, before: &[Win], frame: Option<&FrameState>) -> Result<Option<String>> {
        match expect {
            // In an act, `before` is the desktop before the step, and a window
            // counts only when it appeared or came to match after it. A wait
            // passes no `before`, so any window in the wanted state counts.
            Expectation::Window(e) => {
                let windows = self.desktop.windows().await?;
                let matches = |w: &Win| window_matches(w, e.app.as_deref(), e.title.as_deref());
                let seen_before = |w: &Win| before.iter().any(|b| b.address == w.address && matches(b));
                let found = match (e.gone, e.app.is_some() || e.title.is_some()) {
                    (false, _) => windows.iter().find(|w| matches(w) && !seen_before(w)),
                    (true, true) => windows.iter().find(|w| matches(w)),
                    // Gone without a filter: the window that had focus before the step.
                    (true, false) => before.iter().find(|b| b.focused).and_then(|f| windows.iter().find(|w| w.address == f.address)),
                };
                Ok(match (e.gone, found) {
                    (false, Some(w)) => Some(format!("window \"{}\" is open", squash(&w.title, 48))),
                    (true, None) => Some("the window is gone".into()),
                    _ => None,
                })
            }
            Expectation::Dialog(e) => {
                let windows = self.desktop.windows().await?;
                let titled = |w: &Win, t: &str| contains_ci(&w.title, t) && (w.floating || w.title.trim().eq_ignore_ascii_case(t.trim()));
                let dialog = match (&e.title, e.gone) {
                    // Open with a title: a dialog that appeared, or came to
                    // match, after the step, as for `window`. One left open
                    // before it does not count.
                    (Some(t), false) => windows.iter().find(|w| titled(w, t) && !before.iter().any(|b| b.address == w.address && titled(b, t))),
                    // Gone with a title: the matching dialog in front before
                    // the step, else any matching dialog (none before it, or a wait).
                    (Some(t), true) => match before.iter().find(|b| b.focused && titled(b, t)).or_else(|| before.iter().find(|b| titled(b, t))) {
                        Some(front) => windows.iter().find(|w| w.address == front.address),
                        None => windows.iter().find(|w| titled(w, t)),
                    },
                    (None, false) => windows.iter().find(|w| w.floating && !before.iter().any(|b| b.address == w.address)),
                    // Gone without a title: the dialog in front before the step,
                    // or with no `before` (a wait) any dialog.
                    (None, true) => match before.iter().find(|b| b.floating && b.focused).or_else(|| before.iter().find(|b| b.floating)) {
                        Some(front) => windows.iter().find(|w| w.address == front.address),
                        None if before.is_empty() => windows.iter().find(|w| w.floating),
                        None => None,
                    },
                };
                Ok(match (e.gone, dialog) {
                    (false, Some(w)) => Some(format!("dialog \"{}\" is open", squash(&w.title, 48))),
                    (true, None) => Some("the dialog closed".into()),
                    _ => None,
                })
            }
            Expectation::Focus(e) => {
                let windows = self.desktop.windows().await?;
                Ok(windows
                    .iter()
                    .find(|w| w.focused && window_matches(w, e.app.as_deref(), e.title.as_deref()))
                    .map(|w| format!("\"{}\" has focus", squash(&w.title, 48))))
            }
            Expectation::Text(e) => {
                let surface = match &e.surface {
                    Some(name) => {
                        let windows = self.desktop.windows().await?;
                        Some(self.resolve_surface(frame, &windows, name)?.key())
                    }
                    None => None,
                };
                Ok(self.text_state(&e.text, surface.as_ref(), frame).await?.then(|| "the text is present".to_string()))
            }
            Expectation::Element(e) => Ok(self
                .element_state(&e.query, e.state.as_deref(), e.value.as_deref(), None)
                .await?
                .then(|| format!("element '{}' matches", squash(&e.query, 40)))),
            Expectation::Url(e) => Ok(self.url_state(&e.contains).await?.then(|| format!("the address contains {}", e.contains))),
            Expectation::File(e) => {
                let path = self.resolve_path(task_ref, &e.path)?;
                let exists = path.is_file();
                let wanted = e.exists.unwrap_or(true);
                if exists != wanted {
                    return Ok(None);
                }
                if let (true, Some(contains)) = (exists, &e.contains) {
                    let text = read_bounded(&path).unwrap_or_default();
                    return Ok(text.contains(contains.as_str()).then(|| format!("{} has the text", e.path)));
                }
                Ok(Some(if exists { format!("{} exists", e.path) } else { format!("{} is gone", e.path) }))
            }
            Expectation::Settled(_) => Ok(None),
        }
    }

    /// Wait for an expectation until met, its deadline, or the caller going
    /// away (`gone`). Never repeats an effect.
    pub(crate) async fn await_expectation(
        &self,
        task_ref: &str,
        expect: &Expectation,
        before: &[Win],
        frame: Option<&FrameState>,
        within: Option<Duration>,
        gone: &Cancel,
    ) -> Awaited {
        let started = Instant::now();
        let within = within.unwrap_or_else(|| default_within(expect));
        let waited = |s: Instant| s.elapsed().as_millis() as u64;
        if let Expectation::Settled(s) = expect {
            return self.await_settled(Duration::from_millis(s.quiet_ms), within, started, gone).await;
        }
        let slow = matches!(expect, Expectation::Text(_) | Expectation::Element(_));
        let interval = Duration::from_millis(if slow { 300 } else { 100 });
        let mut last_error: Option<String>;
        loop {
            match self.expectation_now(task_ref, expect, before, frame).await {
                Ok(Some(detail)) => return Awaited { met: true, detail, waited_ms: waited(started) },
                Ok(None) => last_error = None,
                Err(e) if matches!(e.code, "INVALID_ARGUMENT" | "AMBIGUOUS_TARGET") => {
                    return Awaited { met: false, detail: e.message, waited_ms: waited(started) };
                }
                Err(e) => last_error = Some(e.message),
            }
            if started.elapsed() >= within {
                let detail = match last_error {
                    Some(reason) => format!("not seen within {} ms ({})", within.as_millis(), squash(&reason, 120)),
                    None => format!("not seen within {} ms", within.as_millis()),
                };
                return Awaited { met: false, detail, waited_ms: waited(started) };
            }
            if pause(interval.min(within.saturating_sub(started.elapsed()).max(Duration::from_millis(10))), gone).await {
                return Awaited { met: false, detail: GONE.into(), waited_ms: waited(started) };
            }
        }
    }

    /// Quiet: no desktop events and an unchanged window list for `quiet`.
    async fn await_settled(&self, quiet: Duration, within: Duration, started: Instant, gone: &Cancel) -> Awaited {
        let snapshot = |windows: &[Win]| windows.iter().map(|w| format!("{}|{}|{}", w.address, w.title, w.focused)).collect::<Vec<_>>();
        let mut last = self.desktop.windows().await.map(|w| snapshot(&w)).unwrap_or_default();
        let mut quiet_from = Instant::now();
        let mut last_event = self.events.borrow().last_desktop_ms;
        loop {
            if quiet_from.elapsed() >= quiet {
                return Awaited { met: true, detail: format!("quiet for {} ms", quiet.as_millis()), waited_ms: started.elapsed().as_millis() as u64 };
            }
            if started.elapsed() >= within {
                return Awaited { met: false, detail: format!("not quiet within {} ms", within.as_millis()), waited_ms: started.elapsed().as_millis() as u64 };
            }
            if pause(Duration::from_millis(100), gone).await {
                return Awaited { met: false, detail: GONE.into(), waited_ms: started.elapsed().as_millis() as u64 };
            }
            let now = self.desktop.windows().await.map(|w| snapshot(&w)).unwrap_or_default();
            let event = self.events.borrow().last_desktop_ms;
            if now != last || event != last_event {
                quiet_from = Instant::now();
                last = now;
                last_event = event;
            }
        }
    }
}

/// An agent's path, resolved by [`Controller::task_path`]: `rel` from the
/// task's workspace (`base` none) or from the home folder.
#[derive(Debug, Clone)]
pub(crate) struct TaskPath {
    pub base: Option<PathBuf>,
    pub rel: String,
}

/// Why a wait ended early.
pub(crate) const GONE: &str = "stopped waiting: the caller went away";

/// Sleep for `d`; `true` when the caller went away first.
pub(crate) async fn pause(d: Duration, gone: &Cancel) -> bool {
    if gone.is_cancelled() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = gone.cancelled() => true,
    }
}
