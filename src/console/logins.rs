//! The sharing computer: this person's browser logins, copied fresh into the
//! browsers of the computers they administer, one site at a time.
//!
//! The settings (`~/.local/state/ibara/login-sharing.json`, 0600) hold the
//! rules, which are authoritative here; each computer holds a projection of
//! them pushed with `login_configure`. A background loop ([`start`]) pushes
//! changed rules, delivers what agents asked for on Allowed sites and keeps
//! what sites did with a shared login. Console commands answer requests and
//! change rules. One login operation runs at a time (`Console::login_gate`).
//! Replies, errors, log lines and the settings file carry metadata and
//! counts only, never a cookie value.

use super::envelope::{Env, Fault, Handled, clip, error_object};
use super::{Console, Ctx, fleet, option, validated_id};
use crate::access::Rule;
use crate::desktop::chrome::ChromeBridge;
use crate::error::{IbaraError, Result, denied, invalid};
use crate::logins::{LoginRules, SiteMemory, check_cookies, check_profile, profile_root, site};
use crate::operator::directory::{ListedComputer, OperatorDirectory, operator_state_dir};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::{JoinHandle, JoinSet};

/// How often the background loop looks at every computer.
const PASS_EVERY: Duration = Duration::from_secs(2);
/// How long one computer may take to answer a login operation.
const CALL_DEADLINE: Duration = Duration::from_secs(10);
/// How long one computer may take to write a site's login.
const DELIVER_DEADLINE: Duration = Duration::from_secs(30);
/// How long the power helper may take to set up or remove the extension.
const SETUP_DEADLINE: Duration = Duration::from_secs(60);
/// Whether this console administers a computer is asked again after this long.
const ADMIN_FRESH: Duration = Duration::from_secs(60);
/// Sites that rejected a shared login, newest last.
const REJECTED_KEEP: usize = 20;
/// Sites with a remembered result.
const MEMORY_LIMIT: usize = 2048;
/// Sites answered at once (an agent asks for at most 20).
const DECISIONS_LIMIT: usize = 20;

const TARGET_ROLE: &str = "This computer also runs agents, so it can't share logins yet. Turn on sharing from the computer you use.";
const ONLY_DEFAULT: &str = "Login sharing needs a browser with only its Default profile.";
const SET_UP_FIRST: &str = "Set up this browser for ibara first.";
const OLDER: &str = "That computer runs an older ibara. Update ibara there first.";
const NOT_WAITING: &str = "This request is no longer waiting.";

/// The browsers login sharing can read: `(browser, name, executable)`.
const BROWSERS: [(&str, &str, &str); 4] = [
    ("chromium", "Chromium", "/usr/lib/chromium/chromium"),
    ("chrome", "Google Chrome", "/opt/google/chrome/chrome"),
    ("brave", "Brave", "/opt/brave-bin/brave"),
    (
        "brave-origin",
        "Brave Origin",
        "/opt/brave-origin-bin/brave",
    ),
];

// ---------------------------------------------------------------------------
// Settings.

/// `login-sharing.json`. Fields added after the prototype default, so its
/// file still reads.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    enabled: bool,
    /// The setup card was answered (Turn On or Not Now).
    #[serde(default)]
    decided: bool,
    browser: String,
    profile: String,
    label: String,
    logins: LoginRules,
    /// Raised on every change to the rules, site memory, `enabled` or
    /// `label`; a computer whose pushed version differs is configured again.
    #[serde(default)]
    version: u64,
    /// By computer, then site.
    #[serde(default)]
    history: BTreeMap<String, BTreeMap<String, Shared>>,
    #[serde(default)]
    rejected: Vec<Rejected>,
    /// Removals a computer has not confirmed yet, by computer.
    #[serde(default)]
    pending_remove: BTreeMap<String, Vec<String>>,
    /// Computers not yet told that sharing is off.
    #[serde(default)]
    pending_off: Vec<String>,
}

/// A site's logins shared with one computer.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Shared {
    #[serde(default)]
    first_goal: Option<String>,
    #[serde(default)]
    first_task_ref: Option<String>,
    #[serde(default)]
    last_shared_ms: i64,
}

/// A site that rejected a shared login.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejected {
    computer: String,
    site: String,
    at_ms: i64,
}

/// Held while the settings file is read, changed and written again.
static SETTINGS: Mutex<()> = Mutex::new(());

fn settings_path() -> PathBuf {
    operator_state_dir().join("login-sharing.json")
}

fn load() -> Result<Settings> {
    match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| denied("Login sharing settings need recovery.")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(e.into()),
    }
}

fn save(settings: &Settings) -> Result<()> {
    let file = settings_path();
    let parent = file
        .parent()
        .ok_or_else(|| invalid("Invalid login settings path."))?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    let temp = file.with_extension("new");
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    out.write_all(&serde_json::to_vec(settings).map_err(|_| invalid("Invalid login settings."))?)?;
    out.sync_all()?;
    std::fs::rename(temp, file)?;
    Ok(())
}

/// Read the settings again, change them and write them, so a change made
/// meanwhile by the loop or another command is kept.
fn update<T>(change: impl FnOnce(&mut Settings) -> Result<T>) -> Result<T> {
    let _held = SETTINGS.lock().unwrap_or_else(|p| p.into_inner());
    let mut settings = load()?;
    let out = change(&mut settings)?;
    save(&settings)?;
    Ok(out)
}

fn drop_removal(settings: &mut Settings, computer: &str, name: &str) {
    let empty = settings
        .pending_remove
        .get_mut(computer)
        .is_some_and(|sites| {
            sites.retain(|s| s != name);
            sites.is_empty()
        });
    if empty {
        settings.pending_remove.remove(computer);
    }
}

// ---------------------------------------------------------------------------
// What this console knows about each computer, between requests.

#[derive(Default)]
pub(super) struct Logins {
    computers: Mutex<HashMap<String, Remote>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// One computer as login sharing last saw it. Forgotten when its controller
/// epoch changes (it restarted, perhaps with a newer ibara).
#[derive(Clone, Default)]
struct Remote {
    epoch: Option<String>,
    /// Its `access` reply's `can_administer`, and when it was read.
    administered: Option<(bool, Instant)>,
    /// The settings version it last took.
    pushed: Option<u64>,
    configured: bool,
    /// The label of the computer its logins come from instead.
    source_elsewhere: Option<String>,
    /// Its ibara has no login sharing.
    older: bool,
    last_error: Option<String>,
    retry_after: Option<Instant>,
}

impl Logins {
    fn with<T>(&self, computer: &str, change: impl FnOnce(&mut Remote) -> T) -> T {
        change(
            self.computers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(computer.to_string())
                .or_default(),
        )
    }

    fn get(&self, computer: &str) -> Remote {
        self.computers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(computer)
            .cloned()
            .unwrap_or_default()
    }

    /// `computer` left the directory or answers as a new computer.
    pub(super) fn forget(&self, computer: &str) {
        self.computers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(computer);
    }

    /// End the background loop.
    pub(super) fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// This computer and its browser.

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| denied("The selected browser profile is unavailable."))
}

/// The profiles a browser lists in its `Local State`.
fn profiles(root: &Path) -> Vec<String> {
    let state: Option<Value> = std::fs::read(root.join("Local State"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    state
        .as_ref()
        .and_then(|s| s["profile"]["info_cache"].as_object())
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

/// The supported browsers installed here, with their profiles.
fn browsers() -> Vec<Value> {
    let Ok(home) = home() else { return Vec::new() };
    BROWSERS
        .iter()
        .filter(|(_, _, executable)| Path::new(executable).exists())
        .filter_map(|(browser, name, _)| {
            let profiles = profiles(&profile_root(browser, &home).ok()?);
            let supported = profiles == ["Default"];
            let mut row = json!({"browser": browser, "name": name, "profiles": profiles, "supported": supported});
            if !supported {
                row["reason"] = json!(ONLY_DEFAULT);
            }
            Some(row)
        })
        .collect()
}

fn install_root() -> PathBuf {
    std::env::var_os("IBARA_INSTALL_ROOT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| "/opt/agent-computer".into())
}

/// ibara was set up for this browser and profile
/// (`browser/selection.json`), and the browser has only that profile.
fn check_selection(browser: &str, profile: &str) -> Result<()> {
    let root = profile_root(browser, &home()?)?;
    check_profile(&root)?;
    let selected: Value = std::fs::read(install_root().join("browser/selection.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| denied(SET_UP_FIRST))?;
    if selected["browser"] != browser
        || selected["root"].as_str() != root.to_str()
        || selected["profile"] != profile
    {
        return Err(denied(SET_UP_FIRST));
    }
    Ok(())
}

fn bridge(console: &Console) -> Option<Arc<ChromeBridge>> {
    console
        .chrome
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn connected(console: &Console) -> bool {
    bridge(console).is_some_and(|c| c.connected())
}

/// The selected browser, when it is the one set up for ibara and it is open.
fn chrome(console: &Console, settings: &Settings) -> Result<Arc<ChromeBridge>> {
    check_selection(&settings.browser, &settings.profile)?;
    bridge(console)
        .filter(|c| c.connected())
        .ok_or_else(|| denied("Open the selected browser to share logins."))
}

/// This computer also runs agents: its controller is listening. The
/// browser bridge socket is never tried (it serves one peer only).
fn target_role() -> bool {
    let runtime = std::env::var_os("IBARA_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| "/run/agent-computer".into());
    std::os::unix::net::UnixStream::connect(runtime.join("controller.sock")).is_ok()
}

/// This computer's name for the others.
fn own_name() -> String {
    crate::settings::current()
        .text("name")
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "This computer".into())
}

/// Counts of a bundle's kinds of cookies (never a value).
fn kinds(cookies: &Value) -> Value {
    let rows = cookies.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let count = |test: fn(&Value) -> bool| rows.iter().filter(|c| test(c)).count();
    json!({
        "http_only": count(|c| c["httpOnly"] == true),
        "secure": count(|c| c["secure"] == true),
        "host_only": count(|c| c["hostOnly"] == true),
        "partitioned": count(|c| c.get("partitionKey").is_some()),
    })
}

// ---------------------------------------------------------------------------
// Computers.

fn late() -> IbaraError {
    IbaraError::new("TIMEOUT", "The computer did not answer in time.", true)
}

/// A login operation on `computer`; its reply's `result`.
async fn call(
    console: &Console,
    computer: &str,
    op: &str,
    fields: Value,
    deadline: Duration,
) -> Result<Value> {
    let mut data = fleet::fleet_call(console, computer, op, fields, deadline).await?;
    Ok(data
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// The computer's ibara has no such operation.
fn older(error: &IbaraError) -> bool {
    error.message.ends_with("Unknown operator operation.")
}

/// Whether this console administers `computer`: its `access` reply, read at
/// most every minute, and again at once when asked to `recheck` or when the
/// computer restarted.
async fn administers(console: &Console, computer: &str, recheck: bool) -> Result<bool> {
    let epoch = tokio::time::timeout(CALL_DEADLINE, fleet::epoch(console, computer))
        .await
        .map_err(|_| late())??;
    let now = Instant::now();
    let known = console.logins.with(computer, |remote| {
        if remote.epoch.as_deref() != Some(epoch.as_str()) {
            *remote = Remote {
                epoch: Some(epoch.clone()),
                ..Remote::default()
            };
        }
        remote
            .administered
            .filter(|(_, at)| !recheck && now.duration_since(*at) < ADMIN_FRESH)
            .map(|(can, _)| can)
    });
    if let Some(can) = known {
        return Ok(can);
    }
    let rights = call(console, computer, "access", Value::Null, CALL_DEADLINE).await?;
    let can = rights["can_administer"] == true;
    console.logins.with(computer, |remote| {
        remote.administered = Some((can, Instant::now()))
    });
    Ok(can)
}

fn known_administered(console: &Console, computer: &str) -> bool {
    console
        .logins
        .get(computer)
        .administered
        .is_some_and(|(can, _)| can)
}

#[derive(Clone, Copy, PartialEq)]
enum Standing {
    /// Answers, and this console administers it.
    Yes,
    /// Answers, and this console does not administer it.
    No,
    /// Not answering; administered when last asked.
    Offline,
    /// Not answering, never asked.
    Unknown,
}

async fn standing_of(console: &Console, computer: &str) -> Standing {
    match administers(console, computer, false).await {
        Ok(true) => Standing::Yes,
        Ok(false) => Standing::No,
        Err(_) if known_administered(console, computer) => Standing::Offline,
        Err(_) => Standing::Unknown,
    }
}

/// Every added computer and whether this console administers it, all asked
/// at once; in directory order.
async fn standing(console: &Arc<Console>) -> Result<Vec<(ListedComputer, Standing)>> {
    let rows = fleet::verified(console)?;
    let mut set = JoinSet::new();
    for (index, row) in rows.iter().enumerate() {
        let console = console.clone();
        let computer = row.computer_id.clone();
        set.spawn(async move { (index, standing_of(&console, &computer).await) });
    }
    let mut found = vec![Standing::Unknown; rows.len()];
    while let Some(joined) = set.join_next().await {
        if let Ok((index, standing)) = joined {
            found[index] = standing;
        }
    }
    Ok(rows.into_iter().zip(found).collect())
}

/// An added computer, named any way a person names one.
fn resolve(console: &Console, name: &str) -> Result<ListedComputer> {
    let directory = OperatorDirectory::open(&console.database)?;
    let row = directory.resolve_computer(name);
    directory.close();
    let row = row?;
    if row.trust_state != "verified" {
        return Err(invalid("That computer is not added yet."));
    }
    Ok(row)
}

/// The `computer_` id of a computer this console administers. One not
/// answering counts when it was administered when last asked.
async fn administered_computer(console: &Console, name: &str) -> Result<String> {
    let row = resolve(console, name)?;
    match administers(console, &row.computer_id, false).await {
        Ok(true) => Ok(row.computer_id),
        Ok(false) => Err(denied(format!(
            "You don't administer {}, so it can't use your logins.",
            row.label
        ))),
        Err(_) if known_administered(console, &row.computer_id) => Ok(row.computer_id),
        Err(error) => Err(error),
    }
}

fn configure_fields(settings: &Settings, computer: &str, replace: bool) -> Value {
    json!({
        "enabled": settings.enabled,
        "label": settings.label,
        "sites": settings.logins.projection(computer),
        "replace": replace,
    })
}

fn elsewhere_label(result: &Value) -> String {
    result["label"]
        .as_str()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or("another computer")
        .to_string()
}

enum Pushed {
    Configured,
    /// Its logins come from another computer, with this label.
    Elsewhere(String),
    Older,
}

/// Give `computer` the rules as `settings` has them.
async fn push(
    console: &Console,
    computer: &str,
    settings: &Settings,
    replace: bool,
) -> Result<Pushed> {
    let fields = configure_fields(settings, computer, replace);
    match call(console, computer, "login_configure", fields, CALL_DEADLINE).await {
        Err(error) if older(&error) => {
            console.logins.with(computer, |remote| remote.older = true);
            Ok(Pushed::Older)
        }
        Err(error) => Err(error),
        Ok(result) if result["configured"] != true && result["source_elsewhere"] == true => {
            let label = elsewhere_label(&result);
            console.logins.with(computer, |remote| {
                remote.configured = false;
                remote.source_elsewhere = Some(label.clone());
            });
            Ok(Pushed::Elsewhere(label))
        }
        Ok(result) if result["configured"] != true => {
            Err(denied("The computer did not take the login rules."))
        }
        Ok(_) => {
            console.logins.with(computer, |remote| {
                remote.configured = true;
                remote.source_elsewhere = None;
                remote.pushed = Some(settings.version);
            });
            Ok(Pushed::Configured)
        }
    }
}

/// A request an agent made, as `login_pending` lists it.
struct Asked {
    request_ref: String,
    task_ref: Option<String>,
    goal: Option<String>,
    own: bool,
}

impl Asked {
    fn read(request: &Value) -> Option<Asked> {
        let text = |key: &str| {
            request[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Some(Asked {
            request_ref: text("request_ref")?,
            task_ref: text("task_ref"),
            goal: text("goal").map(|g| clip(&g, 200)),
            own: request["own"] == true,
        })
    }
}

/// The sites a request lists.
fn request_sites(request: &Value) -> impl Iterator<Item = (String, &Value)> {
    request["sites"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| Some((site(entry["site"].as_str()?).ok()?, entry)))
}

enum Outcome {
    Off,
    /// The site is not Allowed for the computer.
    Denied,
    /// The selected browser is not open, or not the one set up for ibara.
    NoBrowser,
    SignedOut,
    /// Delivered; `failed` counts cookies the computer could not write.
    Sent {
        written: Value,
        failed: usize,
        failed_fields: BTreeMap<String, usize>,
        kinds: Value,
    },
}

/// Read `name`'s login fresh from the browser and deliver it to `computer`,
/// after checking the rules again. The caller holds the login gate.
async fn share_fresh(
    console: &Console,
    computer: &str,
    name: &str,
    asked: Option<&Asked>,
) -> Result<Outcome> {
    let settings = load()?;
    if !settings.enabled {
        return Ok(Outcome::Off);
    }
    if settings.logins.rule(name, computer) != Rule::Allow {
        return Ok(Outcome::Denied);
    }
    let Ok(chrome) = chrome(console, &settings) else {
        return Ok(Outcome::NoBrowser);
    };
    let Ok(mut read) = chrome
        .call("cookies_read", json!({"site": name}), false)
        .await
    else {
        return Ok(Outcome::NoBrowser);
    };
    let cookies = read
        .get_mut("cookies")
        .map(Value::take)
        .unwrap_or(Value::Null);
    if check_cookies(name, &cookies)? == 0 {
        return Ok(Outcome::SignedOut);
    }
    let kinds = kinds(&cookies);
    let refresh = settings
        .history
        .get(computer)
        .and_then(|sites| sites.get(name))
        .is_some_and(|s| s.last_shared_ms > 0);
    let mut fields = json!({"site": name, "cookies": cookies, "refresh": refresh});
    if let Some(asked) = asked {
        fields["request_ref"] = json!(asked.request_ref);
    }
    let result = call(console, computer, "login_deliver", fields, DELIVER_DEADLINE).await?;
    let failed = result["failed"].as_array().map_or(0, Vec::len);
    let mut failed_fields = BTreeMap::new();
    for failure in result["failed"].as_array().into_iter().flatten() {
        let field = failure["field"]
            .as_str()
            .filter(|field| {
                matches!(
                    *field,
                    "deadline"
                        | "set"
                        | "value"
                        | "domain"
                        | "hostOnly"
                        | "path"
                        | "secure"
                        | "httpOnly"
                        | "sameSite"
                        | "session"
                        | "expirationDate"
                        | "partitionKey"
                )
            })
            .unwrap_or("unknown");
        *failed_fields.entry(field.to_string()).or_insert(0) += 1;
    }
    if failed == 0
        && let Err(error) = record_shared(computer, name, asked)
    {
        eprintln!(
            "{}",
            json!({"event": "login_history_unsaved", "site": name, "detail": error.message})
        );
    }
    Ok(Outcome::Sent {
        written: result["written"].clone(),
        failed,
        failed_fields,
        kinds,
    })
}

fn record_shared(computer: &str, name: &str, asked: Option<&Asked>) -> Result<()> {
    update(|settings| {
        let shared = settings
            .history
            .entry(computer.to_string())
            .or_default()
            .entry(name.to_string())
            .or_default();
        if let Some(asked) = asked
            && shared.first_goal.is_none()
            && shared.first_task_ref.is_none()
        {
            shared.first_goal = asked.goal.clone();
            shared.first_task_ref = asked.task_ref.clone();
        }
        shared.last_shared_ms = crate::ids::now_millis();
        Ok(())
    })
}

/// Tell `computer` why a site of a request was not delivered.
async fn report(
    console: &Console,
    computer: &str,
    request_ref: &str,
    name: &str,
    reason: &str,
) -> Result<()> {
    let fields = json!({"request_ref": request_ref, "site": name, "reason": reason});
    call(console, computer, "login_report", fields, CALL_DEADLINE)
        .await
        .map(drop)
}

/// Keep what sites did with shared logins (`login_pending`'s `results`,
/// returned once): a rejection is remembered, a later success clears it.
fn absorb(computer: &str, results: &Value) -> Result<()> {
    let rows: Vec<(String, &'static str, i64)> = results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let name = site(row["site"].as_str()?).ok()?;
            let result = match row["result"].as_str()? {
                "worked" => "worked",
                "site_rejected" => "site_rejected",
                _ => return None,
            };
            Some((
                name,
                result,
                row["at_ms"].as_i64().unwrap_or_else(crate::ids::now_millis),
            ))
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    update(|settings| {
        let mut changed = false;
        for (name, result, at_ms) in rows {
            let memory = &mut settings.logins.memory;
            if memory.len() < MEMORY_LIMIT || memory.contains_key(&name) {
                memory.insert(
                    name.clone(),
                    SiteMemory {
                        result: result.to_string(),
                        at_ms,
                    },
                );
                changed = true;
            }
            if result == "worked" {
                settings.rejected.retain(|r| r.site != name);
                continue;
            }
            settings.rejected.push(Rejected {
                computer: computer.to_string(),
                site: name,
                at_ms,
            });
            let over = settings.rejected.len().saturating_sub(REJECTED_KEEP);
            settings.rejected.drain(..over);
        }
        if changed {
            settings.version += 1;
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// The background loop.

/// Start the loop; the console's own requests never wait on it.
pub(super) fn start(console: &Arc<Console>) {
    let task = tokio::spawn(run(console.clone()));
    if let Some(earlier) = console
        .logins
        .task
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .replace(task)
    {
        earlier.abort();
    }
}

async fn run(console: Arc<Console>) {
    let mut every = tokio::time::interval(PASS_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_failure: Option<String> = None;
    loop {
        every.tick().await;
        let failure = pass(&console).await.err().map(|e| e.message);
        if let Some(detail) = failure
            .as_ref()
            .filter(|d| last_failure.as_ref() != Some(*d))
        {
            eprintln!(
                "{}",
                json!({"event": "login_pass_failed", "detail": detail})
            );
        }
        last_failure = failure;
    }
}

/// One look at every added computer, all at once; each holds the login gate
/// only for its own work, and one failing never stops the rest.
async fn pass(console: &Arc<Console>) -> Result<()> {
    let settings = load()?;
    if !settings.enabled && settings.pending_off.is_empty() && settings.pending_remove.is_empty() {
        return Ok(());
    }
    let rows = fleet::verified(console)?;
    let listed = |c: &String| rows.iter().any(|r| &r.computer_id == c);
    if !settings.pending_off.iter().all(listed) || !settings.pending_remove.keys().all(listed) {
        update(|s| {
            s.pending_off.retain(listed);
            s.pending_remove.retain(|c, _| listed(c));
            Ok(())
        })?;
    }
    let mut set = JoinSet::new();
    for row in rows.iter() {
        let console = console.clone();
        let computer = row.computer_id.clone();
        set.spawn(async move {
            let outcome = tend(&console, &computer).await;
            console.logins.with(&computer, |remote| {
                remote.last_error = outcome.err().map(|e| e.message);
                remote.retry_after = remote
                    .last_error
                    .as_ref()
                    .map(|_| Instant::now() + Duration::from_secs(30));
            });
        });
    }
    while set.join_next().await.is_some() {}
    Ok(())
}

/// One computer's turn: push changed rules, deliver what its agents asked
/// for, keep what sites did, and retry turning sharing off and removals.
async fn tend(console: &Console, computer: &str) -> Result<()> {
    let settings = load()?;
    let off = settings.pending_off.iter().any(|c| c == computer);
    let removals = settings.pending_remove.contains_key(computer);
    if !settings.enabled && !off && !removals {
        return Ok(());
    }
    let remote = console.logins.get(computer);
    if remote.source_elsewhere.is_some() || remote.retry_after.is_some_and(|at| Instant::now() < at)
    {
        return Ok(());
    }
    let failed_before = remote.last_error.is_some();
    if !administers(console, computer, failed_before).await? || console.logins.get(computer).older {
        if off || removals {
            update(|s| {
                s.pending_off.retain(|c| c != computer);
                s.pending_remove.remove(computer);
                Ok(())
            })?;
        }
        return Ok(());
    }
    let _gate = console.login_gate.lock().await;
    let settings = load()?;
    if settings.enabled {
        if console.logins.get(computer).pushed != Some(settings.version) {
            match push(console, computer, &settings, false).await? {
                Pushed::Configured => {}
                Pushed::Elsewhere(_) | Pushed::Older => return Ok(()),
            }
        }
        let pending = match call(console, computer, "login_pending", json!({}), CALL_DEADLINE).await
        {
            Ok(pending) => pending,
            Err(error) => {
                // An unchanged version does not mean we remain the source.
                // Probe the pin once, then skip a computer another source owns.
                if matches!(
                    push(console, computer, &settings, false).await,
                    Ok(Pushed::Elsewhere(_))
                ) {
                    return Ok(());
                }
                return Err(error);
            }
        };
        absorb(computer, &pending["results"])?;
        deliver_pending(console, computer, &pending["requests"]).await?;
    } else if settings.pending_off.iter().any(|c| c == computer) {
        push(console, computer, &settings, false).await?;
        update(|s| {
            s.pending_off.retain(|c| c != computer);
            Ok(())
        })?;
    }
    let removals = load()?
        .pending_remove
        .get(computer)
        .cloned()
        .unwrap_or_default();
    for name in removals {
        match call(
            console,
            computer,
            "login_remove",
            json!({"site": name}),
            CALL_DEADLINE,
        )
        .await
        {
            Ok(result) if result["deferred"] == true => continue,
            Ok(_) => {}
            Err(error) if older(&error) => {}
            Err(error) => return Err(error),
        }
        update(|s| {
            drop_removal(s, computer, &name);
            Ok(())
        })?;
    }
    Ok(())
}

/// Deliver each site an agent from this person's own computers is waiting
/// for (`need: "deliver"`), fresh; say why when it can't be.
async fn deliver_pending(console: &Console, computer: &str, requests: &Value) -> Result<()> {
    let mut failure = None;
    for request in requests.as_array().into_iter().flatten() {
        let Some(asked) = Asked::read(request).filter(|a| a.own) else {
            continue;
        };
        for (name, entry) in request_sites(request) {
            if entry["need"] != "deliver" {
                continue;
            }
            let reason = match share_fresh(console, computer, &name, Some(&asked)).await {
                Ok(Outcome::NoBrowser) => "waiting_for_browser",
                Ok(Outcome::SignedOut) => "signed_out_there",
                Ok(Outcome::Off) => "sharing_off",
                Ok(Outcome::Sent { failed, .. }) if failed > 0 => continue, // target already marked unknown
                Ok(Outcome::Denied) => "unknown",
                Ok(_) => continue,
                Err(error) => {
                    failure = Some(error);
                    "unknown"
                }
            };
            report(console, computer, &asked.request_ref, &name, reason).await?;
        }
    }
    failure.map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------
// Commands.

pub(super) async fn command(ctx: &Ctx) -> Handled {
    match ctx.head.command.as_str() {
        "login-settings" => {
            standing(&ctx.console).await?;
            Ok(ctx.ready(settings_reply(&ctx.console)?))
        }
        "login-browser-status" => Ok(ctx.ready(json!({"connected": connected(&ctx.console)}))),
        "login-on" => login_on(ctx).await,
        "login-not-now" => {
            update(|s| {
                s.decided = true;
                Ok(())
            })?;
            Ok(ctx.ready(json!({"decided": true})))
        }
        "login-off" => login_off(ctx).await,
        "login-rule" => login_rule(ctx).await,
        "login-rows" => login_rows(ctx),
        "login-answer" => login_answer(ctx).await,
        "login-share-with" => share_with(ctx).await,
        "login-sync" => sync(ctx).await,
        "login-remove" => remove(ctx).await,
        "login-probe" => probe(ctx).await,
        "login-share" => share(ctx).await,
        "login-test-seed" if std::env::var("IBARA_LOGIN_TESTS").as_deref() == Ok("1") => {
            test_seed(ctx).await
        }
        _ => Err(Fault::plain("Unknown login command.")),
    }
}

/// `login-settings`.
fn settings_reply(console: &Console) -> Result<Value> {
    let settings = load()?;
    let all_rules: Map<String, Value> = settings
        .logins
        .rules
        .iter()
        .filter_map(|(name, layers)| Some((name.clone(), json!(layers.get("all")?))))
        .collect();
    let mut computers = Map::new();
    for row in fleet::verified(console)? {
        let remote = console.logins.get(&row.computer_id);
        if !remote.administered.is_some_and(|(can, _)| can) {
            continue;
        }
        let allowed = settings
            .logins
            .rules
            .keys()
            .filter(|name| settings.logins.rule(name, &row.computer_id) == Rule::Allow)
            .count();
        let mut entry = json!({"configured": remote.configured, "sites_allowed": allowed});
        if let Some(label) = remote.source_elsewhere {
            entry["source_elsewhere"] = json!(label);
        }
        if remote.older {
            entry["older"] = json!(true);
        }
        computers.insert(row.computer_id, entry);
    }
    // Presentation-only rule summary, limited to computers this console administers.
    let mut rule_rows = Vec::new();
    for (site, layers) in &settings.logins.rules {
        for (id, rule) in layers {
            if id == "all" || computers.contains_key(id) {
                let effective = if id == "all" { *rule } else { settings.logins.rule(site, id) };
                rule_rows.push(json!({"site": site, "computer": id, "rule": effective}));
            }
        }
    }
    let site_count = rule_rows.iter().filter_map(|row| row["site"].as_str())
        .collect::<std::collections::BTreeSet<_>>().len();
    Ok(json!({
        "enabled": settings.enabled,
        "decided": settings.decided,
        "browser": settings.browser,
        "profile": settings.profile,
        "label": settings.label,
        "connected": connected(console),
        "installable": crate::controller::power_socket_exists(),
        "browsers": browsers(),
        "all_rules": all_rules,
        "rule_rows": rule_rows,
        "site_count": site_count,
        "computers": computers,
        "target_role": target_role(),
        "rejected": settings.rejected,
    }))
}

/// `login-on --browser B --profile P [--label L] [--replace]`.
async fn login_on(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    if target_role() {
        return Err(Fault::plain(TARGET_ROLE));
    }
    let browser = option(&ctx.args, "--browser").unwrap_or("");
    if !BROWSERS.iter().any(|(b, ..)| *b == browser) {
        return Err(Fault::plain(
            "Choose Chromium, Google Chrome, Brave or Brave Origin.",
        ));
    }
    let profile = option(&ctx.args, "--profile").unwrap_or("Default");
    if profile != "Default" || profiles(&profile_root(browser, &home()?)?) != ["Default"] {
        return Err(Fault::plain(ONLY_DEFAULT));
    }
    let label = match option(&ctx.args, "--label")
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        Some(label) if label.chars().count() > 80 || label.chars().any(char::is_control) => {
            return Err(Fault::plain("Give a name of at most 80 characters."));
        }
        Some(label) => label.to_string(),
        None => own_name(),
    };
    let replace = ctx.args.iter().any(|a| a == "--replace");
    let reachable: Vec<String> = standing(console)
        .await?
        .into_iter()
        .filter(|(_, s)| *s == Standing::Yes)
        .map(|(row, _)| row.computer_id)
        .collect();
    if !replace {
        let mut probe = load()?;
        probe.enabled = true;
        probe.label = label.clone();
        for computer in &reachable {
            let mut fields = configure_fields(&probe, computer, false);
            fields["check"] = json!(true);
            let answer = {
                let _gate = console.login_gate.lock().await;
                call(console, computer, "login_configure", fields, CALL_DEADLINE).await
            };
            let Ok(result) = answer else { continue };
            if result["source_elsewhere"] == true {
                let other = elsewhere_label(&result);
                let message = format!(
                    "Logins for your computers come from {other}. Share from this computer instead?"
                );
                let mut env = Env::new("failed", json!({"label": other}));
                env.error =
                    error_object("ANOTHER_SOURCE", &message, true, &[("label", json!(other))]);
                return Ok(ctx.envelope(env));
            }
        }
    }
    if crate::controller::power_socket_exists() {
        let request = json!({"op": "browser_setup", "browser": browser, "profile": profile});
        crate::controller::power_request(request, SETUP_DEADLINE).await?;
    } else if check_selection(browser, profile).is_err() {
        return Err(Fault::plain(SET_UP_FIRST));
    }
    let settings = update(|s| {
        s.enabled = true;
        s.decided = true;
        s.browser = browser.to_string();
        s.profile = profile.to_string();
        s.label = label;
        s.version += 1;
        s.pending_off.clear();
        Ok(s.clone())
    })?;
    for computer in &reachable {
        let _gate = console.login_gate.lock().await;
        // A computer not taking it now is configured by the loop.
        let _ = push(console, computer, &settings, replace).await;
    }
    Ok(ctx.ready(json!({"enabled": true})))
}

/// `login-off`: tell every computer now; those not answering are told by
/// the loop. Logins already copied stay.
async fn login_off(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    let settings = update(|s| {
        s.enabled = false;
        s.version += 1;
        Ok(s.clone())
    })?;
    let mut pending = Vec::new();
    for (row, standing) in standing(console).await? {
        match standing {
            Standing::No => continue,
            Standing::Yes => {
                let _gate = console.login_gate.lock().await;
                if push(console, &row.computer_id, &settings, false)
                    .await
                    .is_ok()
                {
                    continue;
                }
            }
            Standing::Offline | Standing::Unknown => {}
        }
        pending.push(row.computer_id);
    }
    update(|s| {
        s.pending_off = pending.clone();
        Ok(())
    })?;
    let mut reply = json!({"enabled": false, "extension_removed": false});
    if crate::controller::power_socket_exists() && !settings.browser.is_empty() {
        let request = json!({"op": "browser_remove", "browser": settings.browser});
        match crate::controller::power_request(request, SETUP_DEADLINE).await {
            Ok(_) => reply["extension_removed"] = json!(true),
            Err(error) => reply["extension_error"] = json!(error.message),
        }
    }
    Ok(ctx.ready(reply))
}

/// `login-rule --computer ID|all --site S --rule allow|ask|deny`.
async fn login_rule(ctx: &Ctx) -> Handled {
    let name = site(option(&ctx.args, "--site").unwrap_or(""))?;
    let rule = Rule::parse(option(&ctx.args, "--rule").unwrap_or(""))?;
    let scope = match option(&ctx.args, "--computer").unwrap_or("") {
        "all" => "all".to_string(),
        computer => administered_computer(&ctx.console, computer).await?,
    };
    update(|s| {
        s.logins.set(&name, &scope, rule)?;
        s.version += 1;
        Ok(())
    })?;
    Ok(ctx.ready(json!({"site": name, "computer": scope, "rule": rule})))
}

/// `login-rows --computer ID`: each site with a rule for the computer or
/// for all computers.
fn login_rows(ctx: &Ctx) -> Handled {
    let computer =
        resolve(&ctx.console, option(&ctx.args, "--computer").unwrap_or(""))?.computer_id;
    let settings = load()?;
    let history = settings.history.get(&computer);
    let mut rows = Vec::new();
    let mut denied_all = Vec::new();
    for (name, layers) in &settings.logins.rules {
        let own_rule = layers.get(&computer).copied();
        let all_rule = layers.get("all").copied();
        if all_rule == Some(Rule::Deny) {
            denied_all.push(name.clone());
        }
        if own_rule.is_none() && all_rule.is_none() {
            continue;
        }
        let shared = history.and_then(|h| h.get(name));
        rows.push(json!({
            "site": name,
            "rule": settings.logins.rule(name, &computer),
            "own_rule": own_rule,
            "all_rule": all_rule,
            "first_goal": shared.and_then(|s| s.first_goal.as_deref()),
            "last_shared_ms": shared.map(|s| s.last_shared_ms).filter(|ms| *ms > 0),
            "last_result": settings.logins.memory.get(name).map(|m| m.result.as_str()),
            "last_result_at_ms": settings.logins.memory.get(name).map(|m| m.at_ms),
        }));
    }
    Ok(ctx.ready(json!({"rows": rows, "denied_all": denied_all})))
}

#[derive(Clone, Copy, PartialEq)]
enum Decision {
    Share,
    ShareAll,
    Decline,
    Never,
}

/// `--decisions`: a JSON object of site to `share`, `share_all`, `decline` or `never`.
fn decisions(raw: &str) -> Result<Vec<(String, Decision)>> {
    let bad = || {
        invalid(
            "Give the decisions as a JSON object of site to share, share_all, decline or never.",
        )
    };
    let map: Map<String, Value> = serde_json::from_str(raw).map_err(|_| bad())?;
    if map.is_empty() || map.len() > DECISIONS_LIMIT {
        return Err(bad());
    }
    let mut out: Vec<(String, Decision)> = Vec::new();
    for (name, value) in map {
        let decision = match value.as_str() {
            Some("share") => Decision::Share,
            Some("share_all") => Decision::ShareAll,
            Some("decline") => Decision::Decline,
            Some("never") => Decision::Never,
            _ => return Err(bad()),
        };
        let name = site(&name)?;
        if out.iter().any(|(n, _)| *n == name) {
            return Err(invalid("Give each site once."));
        }
        out.push((name, decision));
    }
    Ok(out)
}

/// `login-answer --computer ID --att ATT --decisions JSON [--retry]`: set
/// the rules, push them, deliver the shared sites, then answer. A site
/// waiting for the browser or signed out here is left out of the answer,
/// so the request stays open; `--retry` runs the same decisions again.
async fn login_answer(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    let att = validated_id(option(&ctx.args, "--att"), "attention reference")?;
    let decisions = decisions(option(&ctx.args, "--decisions").unwrap_or(""))?;
    let computer =
        administered_computer(console, option(&ctx.args, "--computer").unwrap_or("")).await?;
    let _gate = console.login_gate.lock().await;
    // Declining carries no values or rule changes. The target validates the
    // item and allows it from an administrator even before a source is pinned.
    if decisions.iter().all(|(_, d)| *d == Decision::Decline) {
        let answers: Map<String, Value> = decisions
            .iter()
            .map(|(n, _)| (n.clone(), json!("declined")))
            .collect();
        let answered = call(
            console,
            &computer,
            "login_answer",
            json!({"att_ref": att, "decisions": answers}),
            CALL_DEADLINE,
        )
        .await?;
        if answered["closed"] == true {
            fleet::forget_attention(console, &computer, &att);
        }
        return Ok(ctx.ready(json!({"sites": decisions.iter().map(|(n, _)| json!({"site": n, "outcome": "declined"})).collect::<Vec<_>>()})));
    }
    let pending = call(
        console,
        &computer,
        "login_pending",
        json!({"att_ref": att}),
        CALL_DEADLINE,
    )
    .await?;
    absorb(&computer, &pending["results"])?;
    let request = pending["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["att_ref"] == att.as_str())
        .ok_or_else(|| invalid(NOT_WAITING))?;
    let asked = Asked::read(request).ok_or_else(|| invalid(NOT_WAITING))?;
    let listed: Vec<String> = request_sites(request).map(|(name, _)| name).collect();
    if let Some((name, _)) = decisions.iter().find(|(name, _)| !listed.contains(name)) {
        return Err(Fault::Plain(format!("{name} is not part of this request.")));
    }
    update(|s| {
        let mut changed = false;
        for (name, decision) in &decisions {
            let layers = match decision {
                Decision::Share => vec![(computer.as_str(), Rule::Allow)],
                Decision::ShareAll => vec![("all", Rule::Allow), (computer.as_str(), Rule::Allow)],
                Decision::Never => vec![("all", Rule::Deny)],
                Decision::Decline => continue,
            };
            // Ask First on either layer must not defeat an explicit Share.
            // Preserve an All Computers Deny unless Share All replaces it.
            if *decision == Decision::Share
                && s.logins.rules.get(name).and_then(|l| l.get("all")) == Some(&Rule::Ask)
            {
                s.logins.rules.get_mut(name).unwrap().remove("all");
                changed = true;
            }
            for (layer, rule) in layers {
                if s.logins.rules.get(name).and_then(|l| l.get(layer)) != Some(&rule) {
                    s.logins.set(name, layer, rule)?;
                    changed = true;
                }
            }
        }
        if changed {
            s.version += 1;
        }
        Ok(())
    })?;
    match push(console, &computer, &load()?, false).await? {
        Pushed::Configured => {}
        Pushed::Older => return Err(Fault::plain(OLDER)),
        Pushed::Elsewhere(label) => {
            return Err(Fault::Plain(format!(
                "Logins for that computer come from {label}."
            )));
        }
    }
    let mut outcomes = Vec::new();
    let mut answers = Map::new();
    for (name, decision) in &decisions {
        let outcome = match decision {
            Decision::Decline => "declined",
            Decision::Never => "denied",
            Decision::Share | Decision::ShareAll => {
                match share_fresh(console, &computer, name, Some(&asked)).await? {
                    Outcome::Off => "sharing_off",
                    Outcome::Denied if load()?.logins.rule(name, &computer) == Rule::Deny => {
                        "denied"
                    }
                    Outcome::Denied => "unknown",
                    Outcome::NoBrowser => "waiting_for_browser",
                    Outcome::SignedOut => "signed_out_there",
                    Outcome::Sent { failed, .. } if failed > 0 => "unknown",
                    Outcome::Sent { .. } => "shared",
                }
            }
        };
        match outcome {
            "shared" | "declined" | "denied" => {
                answers.insert(name.clone(), json!(outcome));
            }
            "sharing_off" | "waiting_for_browser" | "signed_out_there" => {
                report(console, &computer, &asked.request_ref, name, outcome).await?;
            }
            _ => {}
        }
        outcomes.push(json!({"site": name, "outcome": outcome}));
    }
    if !answers.is_empty() {
        let fields = json!({"att_ref": att, "decisions": answers});
        let answered = call(console, &computer, "login_answer", fields, CALL_DEADLINE).await?;
        if answered["closed"] == true {
            fleet::forget_attention(console, &computer, &att);
        }
    }
    Ok(ctx.ready(json!({"sites": outcomes})))
}

/// Where a fresh copy went.
#[derive(Default)]
struct Spread {
    delivered: Vec<String>,
    deferred: Vec<String>,
    unknown: Vec<String>,
    older: Vec<String>,
}

impl Spread {
    /// Push the rules to `computer` and deliver `sites` to it fresh.
    async fn add(
        &mut self,
        console: &Console,
        computer: String,
        standing: Standing,
        sites: &[String],
    ) {
        match standing {
            Standing::No | Standing::Unknown => return,
            Standing::Offline => {
                self.deferred.push(computer);
                return;
            }
            Standing::Yes => {}
        }
        let _gate = console.login_gate.lock().await;
        let pushed = match load() {
            Ok(settings) => push(console, &computer, &settings, false).await,
            Err(error) => Err(error),
        };
        match pushed {
            Ok(Pushed::Configured) => {}
            Ok(Pushed::Older) => return self.older.push(computer),
            Ok(Pushed::Elsewhere(_)) | Err(_) => return self.deferred.push(computer),
        }
        let (mut all, mut sent, mut unknown) = (true, 0, false);
        for name in sites {
            match share_fresh(console, &computer, name, None).await {
                Ok(Outcome::Sent { failed: 0, .. }) => sent += 1,
                Ok(Outcome::Sent { .. }) => unknown = true,
                // Allowed for All Computers but Ask First or Denied on this
                // one (Remove, Never Share): not this computer's to receive.
                Ok(Outcome::Denied) => {}
                Ok(_) => all = false,
                Err(error) => {
                    all = false;
                    unknown |= matches!(error.code, "TIMEOUT" | "OUTCOME_UNKNOWN");
                    if error.code == "TIMEOUT" {
                        break;
                    }
                }
            }
        }
        if unknown {
            self.unknown.push(computer)
        } else if !all {
            self.deferred.push(computer)
        } else if sent > 0 {
            self.delivered.push(computer)
        }
    }
}

/// `login-share-with --site S --to ID[,ID…]|all`: Allow the site for those
/// computers (or all of them) and give each a fresh copy now; one that is
/// off, or whose browser (or this one) is not open, gets it on first need.
async fn share_with(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    let name = site(option(&ctx.args, "--site").unwrap_or(""))?;
    let to = option(&ctx.args, "--to").unwrap_or("");
    let everyone = to == "all";
    let mut chosen: Vec<String> = Vec::new();
    if !everyone {
        for part in to.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let computer = administered_computer(console, part).await?;
            if !chosen.contains(&computer) {
                chosen.push(computer);
            }
        }
        if chosen.is_empty() {
            return Err(Fault::plain(
                "Choose the computers to share this login with.",
            ));
        }
    }
    update(|s| {
        if everyone {
            s.logins.set(&name, "all", Rule::Allow)?;
        }
        for computer in &chosen {
            s.logins.set(&name, computer, Rule::Allow)?;
        }
        s.version += 1;
        Ok(())
    })?;
    let targets: Vec<(String, Standing)> = if everyone {
        standing(console)
            .await?
            .into_iter()
            .map(|(row, standing)| (row.computer_id, standing))
            .collect()
    } else {
        let mut targets = Vec::new();
        for computer in chosen {
            let standing = match standing_of(console, &computer).await {
                Standing::Unknown => Standing::Offline,
                standing => standing,
            };
            targets.push((computer, standing));
        }
        targets
    };
    let sites = [name.clone()];
    let mut spread = Spread::default();
    for (computer, standing) in targets {
        spread.add(console, computer, standing, &sites).await;
    }
    Ok(ctx.ready(json!({"site": name, "delivered": spread.delivered, "deferred": spread.deferred, "unknown": spread.unknown, "older": spread.older})))
}

/// Of `sites`, those with no cookies in this computer's browser; `false`
/// when the browser can't be read (then none are named).
async fn signed_out_here(
    console: &Console,
    settings: &Settings,
    sites: &[String],
) -> (bool, Vec<String>) {
    let Ok(chrome) = chrome(console, settings) else {
        return (false, Vec::new());
    };
    let mut out = Vec::new();
    for name in sites {
        let _gate = console.login_gate.lock().await;
        let Ok(mut read) = chrome
            .call("cookies_read", json!({"site": name}), false)
            .await
        else {
            return (false, Vec::new());
        };
        let cookies = read
            .get_mut("cookies")
            .map(Value::take)
            .unwrap_or(Value::Null);
        if check_cookies(name, &cookies).is_ok_and(|count| count == 0) {
            out.push(name.clone());
        }
    }
    (true, out)
}

/// `login-sync [--dry-run]`: every site Allowed on any computer becomes
/// Allowed for all computers, and each gets a fresh copy now.
async fn sync(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    let dry_run = ctx.args.iter().any(|a| a == "--dry-run");
    let settings = load()?;
    let sites: Vec<String> = settings
        .logins
        .rules
        .iter()
        .filter(|(name, layers)| {
            layers
                .keys()
                .any(|key| settings.logins.rule(name, key) == Rule::Allow)
        })
        .map(|(name, _)| name.clone())
        .collect();
    let rows = standing(console).await?;
    let computers = rows
        .iter()
        .filter(|(_, s)| matches!(s, Standing::Yes | Standing::Offline))
        .count();
    let (browser_connected, signed_out) = signed_out_here(console, &settings, &sites).await;
    let mut reply = json!({
        "sites": sites.len(),
        "computers": computers,
        "signed_out": signed_out,
        "browser_connected": browser_connected,
    });
    if dry_run {
        return Ok(ctx.ready(reply));
    }
    update(|s| {
        for name in &sites {
            s.logins.set(name, "all", Rule::Allow)?;
        }
        s.version += 1;
        Ok(())
    })?;
    let mut spread = Spread::default();
    for (row, standing) in rows {
        spread.add(console, row.computer_id, standing, &sites).await;
    }
    reply["delivered"] = json!(spread.delivered);
    reply["deferred"] = json!(spread.deferred);
    reply["unknown"] = json!(spread.unknown);
    reply["older"] = json!(spread.older);
    Ok(ctx.ready(reply))
}

/// `login-remove --computer ID --site S`: the site is Ask First on that
/// computer again and its cookies leave that computer's browser, now or
/// when it next answers. Works with sharing off.
async fn remove(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    let name = site(option(&ctx.args, "--site").unwrap_or(""))?;
    let computer =
        administered_computer(console, option(&ctx.args, "--computer").unwrap_or("")).await?;
    let settings = update(|s| {
        s.logins.set(&name, &computer, Rule::Ask)?;
        s.version += 1;
        let empty = s.history.get_mut(&computer).is_some_and(|sites| {
            sites.remove(&name);
            sites.is_empty()
        });
        if empty {
            s.history.remove(&computer);
        }
        drop_removal(s, &computer, &name);
        Ok(s.clone())
    })?;
    let _gate = console.login_gate.lock().await;
    let reached = match push(console, &computer, &settings, false).await {
        Ok(Pushed::Older) => return Err(Fault::plain(OLDER)),
        Ok(_) => {
            call(
                console,
                &computer,
                "login_remove",
                json!({"site": name}),
                CALL_DEADLINE,
            )
            .await
        }
        Err(error) => Err(error),
    };
    let reply = match reached {
        Ok(result) => {
            if result["deferred"] == true {
                update(|s| {
                    let sites = s.pending_remove.entry(computer.clone()).or_default();
                    if !sites.contains(&name) {
                        sites.push(name.clone());
                    }
                    Ok(())
                })?;
            }
            result
        }
        Err(error) if older(&error) => return Err(Fault::plain(OLDER)),
        Err(_) => {
            update(|s| {
                let sites = s.pending_remove.entry(computer.clone()).or_default();
                if !sites.contains(&name) {
                    sites.push(name.clone());
                }
                Ok(())
            })?;
            json!({"site": name, "removed": null, "deferred": true})
        }
    };
    Ok(ctx.ready(reply))
}

/// `login-probe --computer ID --site S`: the site's cookie count there.
async fn probe(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    let name = site(option(&ctx.args, "--site").unwrap_or(""))?;
    let computer =
        administered_computer(console, option(&ctx.args, "--computer").unwrap_or("")).await?;
    let _gate = console.login_gate.lock().await;
    Ok(ctx.ready(
        call(
            console,
            &computer,
            "login_probe",
            json!({"site": name}),
            CALL_DEADLINE,
        )
        .await?,
    ))
}

/// `login-share --computer ID --site S`: a fresh copy of an Allowed site now.
async fn share(ctx: &Ctx) -> Handled {
    let console = &ctx.console;
    let name = site(option(&ctx.args, "--site").unwrap_or(""))?;
    let computer =
        administered_computer(console, option(&ctx.args, "--computer").unwrap_or("")).await?;
    let _gate = console.login_gate.lock().await;
    let settings = load()?;
    if !settings.enabled {
        return Ok(ctx.ready(json!({"site": name, "reason": "sharing_off"})));
    }
    if settings.logins.rule(&name, &computer) != Rule::Allow {
        return Err(Fault::plain("Allow this site before sharing its login."));
    }
    match push(console, &computer, &settings, false).await? {
        Pushed::Configured => {}
        Pushed::Older => return Err(Fault::plain(OLDER)),
        Pushed::Elsewhere(label) => {
            return Err(Fault::Plain(format!(
                "Logins for that computer come from {label}."
            )));
        }
    }
    let reply = match share_fresh(console, &computer, &name, None).await? {
        Outcome::Off => json!({"site": name, "reason": "sharing_off"}),
        Outcome::Denied => return Err(Fault::plain("Allow this site before sharing its login.")),
        Outcome::NoBrowser => json!({"site": name, "reason": "waiting_for_browser"}),
        Outcome::SignedOut => json!({"site": name, "reason": "signed_out_there"}),
        Outcome::Sent {
            written,
            failed,
            failed_fields,
            kinds,
        } => {
            json!({"site": name, "written": written, "failed": failed, "failed_fields": failed_fields, "kinds": kinds})
        }
    };
    Ok(ctx.ready(reply))
}

/// `login-test-seed --site S [--host H] [--value V] [--clear]` (only with
/// `IBARA_LOGIN_TESTS=1`, only `.test` sites): write disposable cookies of
/// every kind into this computer's browser, or remove the site's cookies.
/// The value is never echoed.
async fn test_seed(ctx: &Ctx) -> Handled {
    let name = site(option(&ctx.args, "--site").unwrap_or(""))?;
    if !name.ends_with(".test") {
        return Err(Fault::plain("Disposable .test sites only."));
    }
    let chrome = chrome(&ctx.console, &load()?)?;
    let _gate = ctx.console.login_gate.lock().await;
    if ctx.args.iter().any(|a| a == "--clear") {
        let result = chrome
            .call("cookies_remove", json!({"site": name}), true)
            .await?;
        return Ok(ctx.ready(json!({"removed": result["removed"]})));
    }
    let host = option(&ctx.args, "--host")
        .unwrap_or(&name)
        .to_ascii_lowercase();
    if site(&host)? != name {
        return Err(Fault::plain("The host must belong to the disposable site."));
    }
    let value = match option(&ctx.args, "--value") {
        Some(value)
            if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) =>
        {
            return Err(Fault::plain(
                "Give a test value of at most 4096 characters, without control characters.",
            ));
        }
        Some(value) => value.to_string(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let kinds = [
        ("ibara_login", false, true, "lax", false),
        ("__Host-ibara_secure", true, true, "strict", false),
        ("ibara_none", true, false, "no_restriction", false),
        ("ibara_domain", false, false, "unspecified", false),
        ("ibara_partitioned", true, true, "no_restriction", true),
    ];
    let cookies: Vec<Value> = kinds
        .into_iter()
        .map(|(cookie, secure, http_only, same_site, partitioned)| {
            let domain = if cookie == "ibara_domain" { format!(".{name}") } else { host.clone() };
            let mut row = json!({
                "domain": domain,
                "hostOnly": cookie != "ibara_domain",
                "name": cookie,
                "value": value,
                "path": "/",
                "secure": secure,
                "httpOnly": http_only,
                "sameSite": same_site,
                "session": true,
            });
            if partitioned {
                row["partitionKey"] = json!({"topLevelSite": format!("https://{name}"), "hasCrossSiteAncestor": false});
            }
            row
        })
        .collect();
    let result = chrome
        .call(
            "cookies_write",
            json!({"site": name, "cookies": cookies}),
            true,
        )
        .await?;
    Ok(ctx.ready(json!({"written": result["written"]})))
}
