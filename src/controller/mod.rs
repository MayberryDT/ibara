//! The controller engine: a port of `controller/src/core.ts` with the agent
//! path speaking contract 4 (docs/agent-tools.md).
//!
//! - `agent`: the eleven tools.
//! - `situation`: frames, choices, the situation line and `since` events.
//! - `checks`: typed checks and expectations.
//! - `control`: leases, settlement, pause and the viewer ownership machine.
//! - `operator`, `admin`: the operator and administrator operations.
//! - `stream`: `ibara-stream`, the stream Take Control opens, as a child.
//! - `clipboard`: the shared clipboard while a person holds control.
//! - `watchdog`: repairs known stuck states and resumes a computer only the
//!   system paused.
//! - `budget`: keeps a response inside the client's byte limit.
//! - `reader`: the read-only journal view storage consults.
//! - `live`: the ports over `crate::desktop` and the configured paths.
//!
//! One `Controller` lives on one current-thread runtime (inside a `LocalSet`),
//! shared as `Rc<Controller>`. State that changes lives in `Cell`/`RefCell`;
//! no borrow is held across an `.await`.

mod admin;
mod clipboard;
mod agent;
mod access;
mod approval;
mod budget;
mod checks;
mod control;
mod everyday;
mod live;
mod operator;
pub mod ports;
mod reader;
mod replay;
mod situation;
pub mod stream;
mod watchdog;

#[cfg(test)]
mod tests;

pub use everyday::{EVERYDAY_OPS, SLOW_OPS};
pub use live::LiveConfig;
pub use operator::SLOW_FILE_OPS;
pub use ports::*;

use crate::error::{IbaraError, Result};
use crate::ids::{iso_from_millis, now_millis};
use crate::storage::StorageService;
use crate::store::{Clock, Journal, JournalOptions};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, watch};

/// Lease idle expiry since the last heartbeat (`core.ts:183`).
pub const IDLE_EXPIRY_MS: i64 = 300_000;
/// Grace after a `disconnect` before the lease is revoked (`core.ts:184`).
pub const DISCONNECT_GRACE_MS: i64 = 30_000;
const QUEUE_LIMIT: usize = 32;
const RATE_WINDOW_MS: i64 = 10_000;
const RATE_LIMIT: usize = 80;
const MAX_RATE_KEYS: usize = 256;

/// This computer as agents see it.
#[derive(Debug, Clone, Default)]
pub struct ComputerIdentity {
    /// The display name, e.g. `Tulip1`.
    pub name: String,
    /// Other names `computer_begin` accepts (station id, node, hostname).
    pub labels: Vec<String>,
    /// The desktop user.
    pub user: Option<String>,
}

pub use crate::access::Rule;

/// Per-class rules from `policy.json` `effect_rules`, else the settled defaults:
/// allow observe and change; ask for send, spend, destructive and access.
#[derive(Debug, Clone)]
pub struct EffectRules(BTreeMap<String, Rule>);

impl Default for EffectRules {
    fn default() -> Self {
        let mut rules = BTreeMap::new();
        for class in ["observe", "change"] {
            rules.insert(class.to_string(), Rule::Allow);
        }
        for class in ["send", "spend", "destructive", "access"] {
            rules.insert(class.to_string(), Rule::Ask);
        }
        EffectRules(rules)
    }
}

impl EffectRules {
    /// Read `effect_rules` (`{class: "allow"|"ask"|"deny"}`); unknown classes and
    /// values are ignored, so a malformed entry keeps the default.
    pub fn from_policy(policy: &Value) -> Self {
        let mut rules = EffectRules::default();
        if let Some(map) = policy.get("effect_rules").and_then(Value::as_object) {
            for (class, rule) in map {
                if !crate::store::EFFECT_CLASSES.contains(&class.as_str()) {
                    continue;
                }
                let rule = match rule.as_str() {
                    Some("allow") => Rule::Allow,
                    Some("ask") => Rule::Ask,
                    Some("deny") => Rule::Deny,
                    _ => continue,
                };
                rules.0.insert(class.clone(), rule);
            }
        }
        rules
    }

    pub fn rule(&self, class: &str) -> Rule {
        self.0.get(class).copied().unwrap_or(Rule::Ask)
    }
}

/// Settlement timing (`drainTimeoutMs`).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long settlement waits for queued effects.
    pub drain: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { drain: Duration::from_millis(5000) }
    }
}

/// `operatorGrants()` entries: `{...policy.operator_grants, ...operator-authority.json}`.
pub type GrantSource = Rc<dyn Fn() -> Map<String, Value>>;

/// Whether agents here ask a person before they send, spend or delete: the
/// setting of that name (`settings::Values::ask_first`), read on each use.
pub type AskFirst = Rc<dyn Fn() -> bool>;

pub struct ControllerOptions {
    pub state_dir: PathBuf,
    pub journal: JournalOptions,
    pub storage: StorageService,
    pub desktop: Rc<dyn DesktopPort>,
    /// `ibara-stream` for Take Control; `None` where nobody may take control.
    pub stream: Option<Rc<dyn StreamPort>>,
    pub operator_grants: GrantSource,
    pub computer: ComputerIdentity,
    pub effect_rules: EffectRules,
    pub ask_first: AskFirst,
    pub idle_expiry_ms: i64,
    pub disconnect_grace_ms: i64,
    pub timing: Timing,
    /// The desktop person's home folder: where `~/` points, and where agents
    /// may name absolute paths outside ibara's own folders.
    pub home_dir: PathBuf,
    /// The root access helper's socket (`IBARA_ACCESS_SOCKET`).
    pub access_socket: PathBuf,
}

impl ControllerOptions {
    /// Defaults for everything but the state directory, storage and desktop.
    pub fn new(state_dir: impl Into<PathBuf>, storage: StorageService, desktop: Rc<dyn DesktopPort>) -> Self {
        ControllerOptions {
            state_dir: state_dir.into(),
            journal: JournalOptions::default(),
            storage,
            desktop,
            stream: None,
            operator_grants: Rc::new(Map::new),
            computer: ComputerIdentity { name: "this computer".into(), ..Default::default() },
            effect_rules: EffectRules::default(),
            ask_first: Rc::new(|| crate::settings::current().ask_first()),
            idle_expiry_ms: IDLE_EXPIRY_MS,
            disconnect_grace_ms: DISCONNECT_GRACE_MS,
            timing: Timing::default(),
            home_dir: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")),
            access_socket: std::env::var_os("IBARA_ACCESS_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/run/ibara-access.sock")),
        }
    }
}

/// One operator grant (`OperatorGrant`, `core.ts:22`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OperatorGrant {
    pub enabled: bool,
    pub expires_at: Option<String>,
    pub generation: Value,
    pub observe: bool,
    pub files: bool,
}

impl OperatorGrant {
    fn from_value(value: &Value) -> Option<OperatorGrant> {
        let obj = value.as_object()?;
        Some(OperatorGrant {
            enabled: obj.get("enabled") == Some(&Value::Bool(true)),
            expires_at: obj.get("expires_at").and_then(Value::as_str).map(str::to_string),
            generation: obj.get("generation").cloned().unwrap_or(Value::Null),
            observe: obj.get("observe") == Some(&Value::Bool(true)),
            files: obj.get("files") == Some(&Value::Bool(true)),
        })
    }

    /// `operatorGrantActive`: enabled and not past `expires_at` (an unparsable
    /// expiry never expires, as `Date.parse` → NaN did).
    pub fn active(&self, now_ms: i64) -> bool {
        self.enabled
            && !self.expires_at.as_deref().and_then(crate::ids::millis_from_iso).is_some_and(|at| at <= now_ms)
    }

    /// `action.expected_authorization_generation === grant.generation`.
    pub fn generation_matches(&self, expected: Option<&Value>) -> bool {
        match (expected, &self.generation) {
            (Some(Value::Number(a)), Value::Number(b)) => a.as_f64() == b.as_f64(),
            _ => false,
        }
    }
}

/// Memory-only viewer ownership (`viewerOwner`, `viewerRevision`, …).
#[derive(Debug, Default)]
pub(crate) struct ViewerState {
    pub owner: Option<String>,
    /// The stream generation the holder's tickets are for.
    pub generation: Option<u64>,
    pub revision: u64,
    pub fault: bool,
    /// Who had paused the computer before this take-control (`None`: not paused).
    pub pause_before_take: Option<crate::store::PauseOrigin>,
    /// True when a failed `initialize_viewer`, not other work, set `unsettled`.
    pub startup_unsettled: bool,
}

/// Cancellation for in-flight effects of the current lease (`this.abort`).
struct Abort {
    cancel: Cancel,
    signal: watch::Sender<bool>,
}

impl Abort {
    fn new() -> Self {
        Abort { cancel: Cancel::new(), signal: watch::Sender::new(false) }
    }
}

pub struct Controller {
    journal: Journal,
    storage: StorageService,
    desktop: Rc<dyn DesktopPort>,
    stream: Option<Rc<dyn StreamPort>>,
    /// The highest stream generation used, so every new one is above it.
    stream_generation: Cell<u64>,
    clipboard: RefCell<clipboard::Clipboard>,
    operator_grants: GrantSource,
    reader: Arc<reader::JournalReader>,
    computer: ComputerIdentity,
    computer_id: String,
    effect_rules: EffectRules,
    ask_first: AskFirst,
    access_projection: RefCell<Option<String>>,
    /// The last failed projection and when it failed; periodic retries back off.
    access_projection_failed: RefCell<Option<(String, i64)>>,
    access_socket: PathBuf,
    replay: replay::Replay,
    epoch: String,
    endpoint_id: String,
    clock: Clock,
    idle_expiry_ms: i64,
    disconnect_grace_ms: i64,
    timing: Timing,
    home: checks::HomeRule,
    closed: Cell<bool>,
    me: RefCell<Weak<Controller>>,
    // effects
    effect_lock: AsyncMutex<()>,
    queue_depth: Cell<usize>,
    abort: RefCell<Abort>,
    settling: Cell<bool>,
    settled: watch::Sender<u64>,
    display_maintenance: Cell<bool>,
    /// A display reconcile loop is running.
    display_loop: Cell<bool>,
    /// The task whose effect is running, and when the last effect ended.
    effect_task: RefCell<Option<String>>,
    last_effect_end_ms: Cell<i64>,
    /// Journalled calls queued or running: (principal, task, request id) → arguments.
    in_flight: RefCell<HashMap<(String, String, String), Value>>,
    // per-connection
    rate: RefCell<HashMap<String, VecDeque<i64>>>,
    grace_deadlines: RefCell<HashMap<String, i64>>,
    /// Connections whose client stopped answering (a dropped network the
    /// computer has not noticed yet): the same agent may carry on elsewhere.
    silent: RefCell<HashSet<String>>,
    capabilities: RefCell<Vec<Value>>,
    // agent situation
    sessions: RefCell<situation::Sessions>,
    events: RefCell<situation::EventLog>,
    frames: RefCell<situation::Frames>,
    revision: Cell<u64>,
    // operators and viewer
    follow_pins: RefCell<HashMap<String, operator::FollowPin>>,
    /// Answers to "X asks to watch this computer", per computer (see `access::Sitting`).
    watch_sittings: RefCell<HashMap<String, access::Sitting>>,
    preview_sequence: Cell<u64>,
    viewer_state: RefCell<ViewerState>,
    /// The latest step of a running task and its click, for the console.
    last_step: RefCell<Option<operator::StepSeen>>,
    viewer_lock: AsyncMutex<()>,
    watchdog: RefCell<watchdog::Watchdog>,
    /// One watchdog pass at a time.
    watchdog_turn: AsyncMutex<()>,
    state_dir: PathBuf,
    /// A person asked for ibarad to restart (`repair restart_ibara`).
    restart: tokio::sync::Notify,
    /// How this computer can be woken, and when that was read.
    wake_cache: RefCell<Option<(i64, Value)>>,
    /// Whether a restart stops at the disk password, and when that was read.
    disk_cache: RefCell<Option<(i64, Value)>>,
    /// The virtual screen size last given to `IbaraVirtual`.
    virtual_size_applied: RefCell<Option<String>>,
    /// The last preview per display and quality, for the picture interval setting.
    previews: RefCell<HashMap<(String, String), (i64, Value)>>,
}

impl Controller {
    /// `new Controller(…)` (`core.ts:171-198`): open the journal, hand storage
    /// its journal view, and rotate the epoch (which revokes any lease, makes
    /// dispatched `running` receipts `unknown` and expires older observations).
    pub fn new(options: ControllerOptions) -> Result<Controller> {
        let clock: Clock = options.journal.now.clone().unwrap_or_else(|| Arc::new(now_millis));
        let mut journal_options = options.journal.clone();
        journal_options.now = Some(clock.clone());
        let journal = Journal::open(&options.state_dir, journal_options)?;
        let reader = Arc::new(reader::JournalReader::open(&options.state_dir, clock.clone())?);
        options.storage.set_authority_policy(reader.clone());
        let epoch = journal.rotate_epoch(&iso_from_millis(clock()))?;
        let endpoint_id = journal.endpoint_identity()?;
        let computer_id = computer_id_for(&endpoint_id);
        let watchdog = watchdog::Watchdog::new(journal.last_event(watchdog::REPAIR)?);
        let mut computer = options.computer;
        if computer.name.is_empty() {
            computer.name = "this computer".into();
        }
        let home = {
            let storage = options.storage.options();
            let ibara = [storage.root_dir.as_path(), storage.state_dir.as_path(), options.state_dir.as_path()];
            checks::HomeRule::new(&options.home_dir, &ibara, &std::fs::read_to_string("/etc/passwd").unwrap_or_default())
        };
        Ok(Controller {
            journal,
            storage: options.storage,
            desktop: options.desktop,
            stream: options.stream,
            stream_generation: Cell::new(0),
            clipboard: RefCell::new(clipboard::Clipboard::default()),
            operator_grants: options.operator_grants,
            reader,
            computer,
            computer_id,
            effect_rules: options.effect_rules,
            ask_first: options.ask_first,
            access_projection: RefCell::new(None),
            access_projection_failed: RefCell::new(None),
            access_socket: options.access_socket,
            replay: replay::Replay::new(&options.state_dir),
            epoch,
            endpoint_id,
            clock,
            idle_expiry_ms: options.idle_expiry_ms,
            disconnect_grace_ms: options.disconnect_grace_ms,
            timing: options.timing,
            home,
            closed: Cell::new(false),
            me: RefCell::new(Weak::new()),
            effect_lock: AsyncMutex::new(()),
            queue_depth: Cell::new(0),
            abort: RefCell::new(Abort::new()),
            settling: Cell::new(false),
            settled: watch::Sender::new(0),
            display_maintenance: Cell::new(false),
            display_loop: Cell::new(false),
            effect_task: RefCell::new(None),
            last_effect_end_ms: Cell::new(0),
            in_flight: RefCell::new(HashMap::new()),
            rate: RefCell::new(HashMap::new()),
            grace_deadlines: RefCell::new(HashMap::new()),
            silent: RefCell::new(HashSet::new()),
            capabilities: RefCell::new(Vec::new()),
            sessions: RefCell::new(situation::Sessions::default()),
            events: RefCell::new(situation::EventLog::default()),
            frames: RefCell::new(situation::Frames::default()),
            revision: Cell::new(0),
            follow_pins: RefCell::new(HashMap::new()),
            watch_sittings: RefCell::new(HashMap::new()),
            preview_sequence: Cell::new(0),
            viewer_state: RefCell::new(ViewerState::default()),
            last_step: RefCell::new(None),
            viewer_lock: AsyncMutex::new(()),
            watchdog: RefCell::new(watchdog),
            watchdog_turn: AsyncMutex::new(()),
            state_dir: options.state_dir.clone(),
            restart: tokio::sync::Notify::new(),
            wake_cache: RefCell::new(None),
            disk_cache: RefCell::new(None),
            virtual_size_applied: RefCell::new(None),
            previews: RefCell::new(HashMap::new()),
        })
    }

    /// Start-up fencing before any socket opens (`server.ts:165-178`): start the
    /// event watcher, reset input, pause for the system (revoking historical
    /// authority and settling), then fence the viewer, and run the periodic
    /// sweeps (lease expiry, disconnect grace, viewer grant expiry) and the
    /// watchdog, which retries the viewer, repairs stuck states and resumes
    /// once the computer is healthy unless a person paused it.
    pub async fn start(self: &Rc<Self>) -> Result<()> {
        *self.me.borrow_mut() = Rc::downgrade(self);
        self.desktop.start_watch();
        if let Some(rx) = self.desktop.subscribe() {
            tokio::task::spawn_local(situation::pump_events(Rc::downgrade(self), rx));
        }
        if let Err(e) = self.desktop.reset_input().await {
            log_event("reset_input_failed", &e.to_string());
        }
        // This computer takes agent work: its screensaver and idle lock must
        // never shut agents (or a person taking control) out.
        match self.desktop.keep_awake().await {
            Ok(true) => log_event("stay_awake_on", "Turned on Omarchy's Stay Awake so this computer never locks agents out."),
            Ok(false) => {}
            Err(e) => log_event("stay_awake_failed", &e.to_string()),
        }
        self.system_pause().await?;
        if self.stream.is_some() {
            if let Err(e) = self.initialize_viewer().await {
                log_event("viewer_initialize_failed", &e.to_string());
            }
            tokio::task::spawn_local(control::sweep_viewer(Rc::downgrade(self)));
            tokio::task::spawn_local(control::stream_watch(Rc::downgrade(self)));
        }
        tokio::task::spawn_local(control::lease_ticks(Rc::downgrade(self)));
        tokio::task::spawn_local(watchdog::watchdog_loop(Rc::downgrade(self)));
        Ok(())
    }

    /// The system's pause (start, shutdown). It never replaces a person's
    /// pause, and the watchdog ends it once the computer is healthy.
    pub async fn system_pause(&self) -> Result<Value> {
        self.ensure_open()?;
        self.reconcile().await?;
        self.admin_pause(crate::store::PauseOrigin::System).await
    }

    /// Stop serving: close the controller, cancel jobs and browser work, release input.
    pub async fn shutdown(&self) {
        self.closed.set(true);
        self.abort_effects();
        if let Err(e) = self.storage.cancel_jobs(None).await {
            log_event("cancel_jobs_failed", &e.to_string());
        }
        let _ = self.desktop.browser_cancel().await;
        // A viewer's held keys are released before this process ends.
        if let Some(stream) = self.stream.clone() {
            let _ = stream.revoke().await;
            let _ = stream.stop().await;
        }
        self.clipboard_stop();
        // The person's pointer again before this process (and Cua's cursor with it) ends.
        self.desktop.set_agent(None);
        if let Err(e) = self.desktop.release_input().await {
            log_event("release_input_failed", &e.to_string());
        }
        let _ = self.desktop.set_idle_inhibited(false).await;
        self.storage.close();
    }

    /// Resolves once a person asked for ibarad to restart; the server then
    /// shuts down and exits with a failure so its unit starts it again.
    pub async fn restart_requested(&self) {
        self.restart.notified().await
    }

    /// The controller epoch, rotated at every start.
    pub fn epoch(&self) -> String {
        self.epoch.clone()
    }

    /// `meta.endpoint_identity`, stable across restarts.
    pub fn endpoint_id(&self) -> String {
        self.endpoint_id.clone()
    }

    /// The gateway `transfer` kind (`storage.transfer`).
    pub async fn transfer(&self, principal: &str, connection_id: &str, request: Value) -> Result<Value> {
        self.ensure_open()?;
        self.assert_identity(principal, connection_id)?;
        self.storage.transfer(principal, &request, Some(connection_id))
    }

    // ---- shared helpers -----------------------------------------------------

    pub(crate) fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    pub(crate) fn now_iso(&self) -> String {
        iso_from_millis(self.now_ms())
    }

    pub(crate) fn ensure_open(&self) -> Result<()> {
        if self.closed.get() {
            return Err(IbaraError::new("SESSION_UNAVAILABLE", "ibara restarted during this call.", false)
                .requires_reconciliation()
                .with("next", "Try again in a few seconds. For an agent's step, first check what it did with computer_status."));
        }
        Ok(())
    }

    /// `assertIdentity` (`core.ts:701-708`).
    pub(crate) fn assert_identity(&self, principal: &str, connection_id: &str) -> Result<()> {
        if principal.is_empty() || principal.len() > 128 {
            return Err(crate::error::denied("Authenticated principal is required."));
        }
        if connection_id.is_empty() || connection_id.len() > 128 {
            return Err(crate::error::denied("Connection identity is required."));
        }
        Ok(())
    }

    /// `touchRate` (`core.ts:710-717`): at most 80 calls per 10 s per connection.
    pub(crate) fn touch_rate(&self, connection_id: &str) -> Result<()> {
        let now = self.now_ms();
        let mut rate = self.rate.borrow_mut();
        if rate.len() > MAX_RATE_KEYS {
            rate.retain(|_, hits| hits.back().is_some_and(|t| now - t < RATE_WINDOW_MS));
        }
        let hits = rate.entry(connection_id.to_string()).or_default();
        while hits.front().is_some_and(|t| now - t >= RATE_WINDOW_MS) {
            hits.pop_front();
        }
        hits.push_back(now);
        if hits.len() > RATE_LIMIT {
            return Err(IbaraError::new("BUDGET_EXCEEDED", "Per-connection request rate exceeded.", true));
        }
        Ok(())
    }

    /// The current lease's cancellation token and storage abort signal.
    pub(crate) fn abort_handles(&self) -> (Cancel, watch::Receiver<bool>) {
        let abort = self.abort.borrow();
        (abort.cancel.clone(), abort.signal.subscribe())
    }

    /// `this.abort.abort(); this.abort = new AbortController()`.
    pub(crate) fn abort_effects(&self) {
        let old = std::mem::replace(&mut *self.abort.borrow_mut(), Abort::new());
        old.cancel.cancel();
        old.signal.send_replace(true);
    }

    /// Run `effect` in the single serial effect queue (`enqueue`, `core.ts:1018`).
    pub(crate) async fn enqueue<T>(&self, effect: impl Future<Output = Result<T>>) -> Result<T> {
        if self.queue_depth.get() >= QUEUE_LIMIT {
            return Err(IbaraError::new("BUSY", "Effect queue is full.", true));
        }
        self.queue_depth.set(self.queue_depth.get() + 1);
        let _depth = DepthGuard(&self.queue_depth);
        let _turn = self.effect_lock.lock().await;
        effect.await
    }

    /// `drainEffects` (`core.ts:1027-1041`). Inside an effect, waiting for the
    /// queue would deadlock, so only the depth is checked.
    pub(crate) async fn drain_effects(&self, inside_effect: bool) -> bool {
        if inside_effect {
            return self.queue_depth.get() <= 1;
        }
        if self.queue_depth.get() == 0 {
            return true;
        }
        match tokio::time::timeout(self.timing.drain, self.effect_lock.lock()).await {
            Ok(_turn) => self.queue_depth.get() == 0,
            Err(_) => false,
        }
    }

    /// Record that an effect for `task_ref` starts or ends (window ownership
    /// and "a person touched it" both key off this).
    pub(crate) fn mark_effect(&self, task_ref: Option<&str>) {
        *self.effect_task.borrow_mut() = task_ref.map(str::to_string);
        if task_ref.is_none() {
            self.last_effect_end_ms.set(self.now_ms());
        }
    }

    pub(crate) fn operator_grant(&self, operator_id: &str) -> Option<OperatorGrant> {
        self.access_operator_grant(operator_id)
    }

    pub(crate) fn next_revision(&self) -> u64 {
        self.revision.set(self.revision.get() + 1);
        self.revision.get()
    }
}

struct DepthGuard<'a>(&'a Cell<usize>);

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// `cmp_` plus the first 24 hex of `sha256(endpoint_id)`, the same digest the
/// operator directory uses for its `computer_` ids.
pub fn computer_id_for(endpoint_id: &str) -> String {
    let digest = Sha256::digest(endpoint_id.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("cmp_{}", &hex[..24])
}

/// `^[A-Za-z0-9_.:-]{1,128}$`
pub(crate) fn is_ref(s: &str) -> bool {
    (1..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// `^[a-z][a-z0-9_-]{0,63}$`
pub(crate) fn is_principal(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && s.len() <= 64
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// `^[A-Za-z0-9_.:-]{8,256}$`
pub(crate) fn is_stable_identity(s: &str) -> bool {
    (8..=256).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// Clip to at most `max` characters.
pub(crate) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => text[..i].to_string(),
        None => text.to_string(),
    }
}

/// Collapse whitespace and clip with an ellipsis.
pub(crate) fn squash(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max {
        joined
    } else {
        format!("{}…", clip(&joined, max.saturating_sub(1)))
    }
}

/// One structured line on stderr for the journal.
pub(crate) fn log_event(event: &str, detail: &str) {
    eprintln!("{}", serde_json::json!({ "event": event, "detail": clip(detail, 400), "time": crate::ids::now_iso() }));
}
