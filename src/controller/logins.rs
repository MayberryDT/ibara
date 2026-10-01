//! Login sharing on an agent computer ([agent tools](../../docs/agent-tools.md#logins)).
//!
//! One sharing computer is pinned here (`login_receiver` in the journal's
//! meta). It pushes a projection of its login rules (`login_configure`) and
//! asks every 2 s for what agents here need (`login_pending`) over the
//! existing operator route; this computer never connects to it. An agent asks
//! with `logins` at `computer_begin` or with `browser_act` `sign_in`. Each ask
//! is a login request (`login_requests` in meta); the sites that need the
//! person become one attention item of kind `login`, answered only by the
//! sharing computer (`login_answer`). Allowed sites are delivered by the
//! sharing computer without asking, fresh from the person's browser.
//!
//! Records hold sites, states, times and counts. A cookie value exists here
//! only inside a `login_deliver` action on its way to the browser: it is never
//! saved, logged, put in an error or returned.

use super::Controller;
use super::agent::Reply;
use super::checks::pause;
use super::ports::Cancel;
use super::situation::FrameSpec;
use crate::access::Rule;
use crate::contract::{LoginStanding, SignIn, SignInResult, SiteState, Status};
use crate::error::{IbaraError, Result, denied, invalid};
use crate::ids::id;
use crate::logins::{check_cookies, site};
use crate::store::{Journal, LeaseRecord, NewAttention, TaskRecord};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The sharing computer counts as away when it has not asked for this long.
const STALE_MS: i64 = 15_000;
/// How long a `sign_in` waits for Allowed sites to arrive.
const DELIVERY_WAIT: Duration = Duration::from_secs(10);
/// How long the reloaded page gets to finish loading before it is read.
const PAGE_WAIT: Duration = Duration::from_secs(8);
/// Cookies written per extension call.
const BATCH: usize = 25;
/// Requests kept after their task ended, and results kept for the sharing computer.
const KEEP_REQUESTS: usize = 200;
const KEEP_RESULTS: usize = 100;
const MAX_SITES: usize = 2048;

fn meta_get(journal: &Journal, key: &str) -> Result<Option<String>> {
    Ok(journal
        .db()
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

fn meta_set(journal: &Journal, key: &str, value: &str) -> Result<()> {
    journal.db().execute("INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key, value])?;
    Ok(())
}

/// The pinned sharing computer and its rules for this computer.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receiver {
    pub source: Option<String>,
    pub label: String,
    pub enabled: bool,
    /// Written by the first prototype; the heartbeat is kept in memory now.
    #[serde(default)]
    pub seen_ms: i64,
    /// The sharing computer's rules for this computer: `{site: {rule, last_result?}}`.
    pub sites: BTreeMap<String, Value>,
    /// When each site's login was last written here.
    #[serde(default)]
    pub shared: BTreeMap<String, i64>,
    /// Sites to remove from this computer's browser once it is connected.
    #[serde(default)]
    pub remove_pending: Vec<String>,
}

impl Receiver {
    pub fn load(journal: &Journal) -> Result<Self> {
        match meta_get(journal, "login_receiver")? {
            None => Ok(Self::default()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|_| denied("Login sharing records need administrator recovery.")),
        }
    }

    pub fn save(&self, journal: &Journal) -> Result<()> {
        meta_set(
            journal,
            "login_receiver",
            &serde_json::to_string(self).map_err(|_| invalid("Invalid login records."))?,
        )
    }

    fn is_source(&self, operator: &str) -> bool {
        self.source.as_deref() == Some(operator)
    }

    fn check_source(&self, operator: &str) -> Result<()> {
        if !self.is_source(operator) {
            return Err(denied("Only this computer's sharing computer may do this.")
                .with("source_elsewhere", self.source.is_some())
                .with("label", self.label.clone()));
        }
        Ok(())
    }

    fn sharing(&self) -> bool {
        self.source.is_some() && self.enabled
    }

    /// The rule for `site`; a site with no rule is Ask First.
    fn rule(&self, site: &str) -> Rule {
        self.sites
            .get(site)
            .and_then(|r| r["rule"].as_str())
            .and_then(|r| Rule::parse(r).ok())
            .unwrap_or(Rule::Ask)
    }

    fn last_result(&self, site: &str) -> Option<String> {
        self.sites
            .get(site)
            .and_then(|r| r["last_result"]["result"].as_str())
            .map(str::to_string)
    }
}

/// One site of a login request.
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    site: String,
    /// The site this one signs in through (`irs.gov` through `id.me`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    /// `asking` (for the person), `deliver` (Allowed, on its way), `shared`
    /// (written here), `worked` (the page left its sign-in), `rejected` (the
    /// page still asked to sign in), `unknown` (written only in part),
    /// `declined` or `denied`.
    state: String,
    /// Why the sharing computer could not deliver yet:
    /// `waiting_for_browser`, `signed_out_there` or `sharing_off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl Entry {
    fn open(&self) -> bool {
        matches!(self.state.as_str(), "asking" | "deliver")
    }
}

/// An agent's request for logins: the sites of one begin or one `sign_in`.
#[derive(Clone, Serialize, Deserialize)]
struct Request {
    request_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    att_ref: Option<String>,
    task_ref: String,
    goal: String,
    agent: String,
    principal: String,
    /// The agent comes from one of the person's own computers.
    own: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<String>,
    created_ms: i64,
    sites: Vec<Entry>,
}

#[derive(Default, Serialize, Deserialize)]
struct Book {
    requests: Vec<Request>,
    /// `{site, result, task_ref, at_ms}` for the sharing computer's site memory, sent once.
    results: Vec<Value>,
}

impl Book {
    fn load(journal: &Journal) -> Result<Self> {
        match meta_get(journal, "login_requests")? {
            None => Ok(Self::default()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|_| denied("Login request records need administrator recovery.")),
        }
    }

    fn save(&mut self, journal: &Journal) -> Result<()> {
        if self.requests.len() > KEEP_REQUESTS {
            let extra = self.requests.len() - KEEP_REQUESTS;
            self.requests.drain(..extra);
        }
        if self.results.len() > KEEP_RESULTS {
            let extra = self.results.len() - KEEP_RESULTS;
            self.results.drain(..extra);
        }
        meta_set(
            journal,
            "login_requests",
            &serde_json::to_string(self).map_err(|_| invalid("Invalid login request records."))?,
        )
    }

    fn find(&self, request_ref: &str) -> Option<usize> {
        self.requests
            .iter()
            .position(|r| r.request_ref == request_ref)
    }
}

/// What the agent is told for a site: `shared`, or a reason code.
#[derive(Clone, PartialEq)]
enum Seen {
    /// Allowed, and the sharing computer is here: it arrives in a moment.
    Delivering,
    Shared,
    Code(&'static str),
}

/// What `next` says for each reason code.
fn next_for(code: &str, site: &str, task_ref: &str, att: Option<&str>) -> String {
    let again = "then send this same browser_act request again (same request_id)";
    match code {
        "waiting_for_person" => format!(
            "waiting_for_person: your person is asked on their sharing computer ({}). Call computer_wait({{task_ref: \"{task_ref}\", for: {{attention: \"{}\"}}, deadline_ms: 50000}}), {again}. Don't end your turn or ask your user for a password.",
            att.unwrap_or("att_"),
            att.unwrap_or("att_")
        ),
        "waiting_for_browser" => format!("waiting_for_browser: your person's browser on their sharing computer isn't running. Keep waiting: {again} in a minute; it completes when the browser opens."),
        "waiting_for_sharing_computer" => format!("waiting_for_sharing_computer: the sharing computer is off or asleep. Do other work or wait, {again}; it completes when that computer is back."),
        "signed_out_there" => format!("signed_out_there: your person isn't signed in to {site} in their browser and was asked to sign in there and choose Retry. Wait, {again}."),
        "sharing_off" => format!("sharing_off: no computer shares logins with this one yet; your person was offered Turn On Login Sharing. Wait, {again}."),
        "declined" => format!("declined: your person chose Don't Share for {site} this time. Don't ask again in this task; take another route or ask your person with computer_checkpoint."),
        "denied" => format!("denied: {site} is not shared with this computer. Don't ask for it."),
        "site_rejected" => format!("site_rejected: {site} still shows its sign-in page after the login was copied. Ask your person to sign in with Take Control (computer_checkpoint)."),
        "unknown" => "unknown: ibara couldn't confirm the whole login was written; send sign_in again to retry, or ask your person to sign in with Take Control.".into(),
        "not_available" => format!("not_available: logins aren't shared with this computer; sign in to {site} another way or ask your person with computer_checkpoint."),
        _ => format!("{code}: {again}."),
    }
}

/// Codes an agent waits on (the request completes by itself), in the order `next` names them.
const WAITING: [&str; 5] = [
    "waiting_for_person",
    "signed_out_there",
    "waiting_for_browser",
    "waiting_for_sharing_computer",
    "sharing_off",
];

/// `https://a.b.example/x?y` → `a.b.example`.
fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    Some(host.split(':').next().unwrap_or(host))
}

/// A page's address without its query or fragment.
fn page_of(url: &str) -> String {
    crate::controller::clip(url.split(['?', '#']).next().unwrap_or(url), 200)
}

impl Controller {
    /// Whether `principal` is one of this computer's owner's own computers:
    /// one that may administer this computer. Without an access model, only
    /// the owner's computers are paired.
    fn own_principal(&self, principal: &str) -> Result<bool> {
        Ok(crate::access::Access::load(&self.journal)?
            .is_none_or(|a| a.rule(principal, "administer", self.now_ms()) == Rule::Allow))
    }

    fn sharing_computer_here(&self) -> bool {
        self.now_ms() - self.login_seen.get() <= STALE_MS
    }

    /// A site's first standing for an agent: the entry state it starts in
    /// (`None` when it gets no entry) and what the agent is told.
    fn first_standing(
        &self,
        receiver: &Receiver,
        name: &str,
        own: bool,
        available: bool,
    ) -> (Option<&'static str>, &'static str) {
        if !available {
            return (None, "not_available");
        }
        if !receiver.sharing() {
            return (Some("asking"), "sharing_off");
        }
        match receiver.rule(name) {
            Rule::Deny => (None, "denied"),
            Rule::Allow if own => (Some("deliver"), "allowed"),
            _ => (Some("asking"), "asking"),
        }
    }

    /// Record `request` and ask the person once for its sites that need them.
    fn open_request(
        &self,
        mut request: Request,
        lease: &LeaseRecord,
        op_ref: Option<&str>,
    ) -> Result<Request> {
        let asked: Vec<&Entry> = request
            .sites
            .iter()
            .filter(|e| e.state == "asking")
            .collect();
        if !asked.is_empty() {
            let receiver = Receiver::load(&self.journal)?;
            let computer = self.computer_name();
            let what = match asked.as_slice() {
                [one] => format!("your login for {}", one.site),
                many => format!("your logins for {} sites", many.len()),
            };
            let question = format!(
                "{}, working on “{}” on {computer}, wants {what}",
                request.agent,
                crate::controller::squash(&request.goal, 80)
            );
            let details = json!({
                "agent": request.agent,
                "principal": request.principal,
                "task": { "task_ref": request.task_ref, "goal": request.goal },
                "sites": asked.iter().map(|e| json!({ "site": e.site, "via": e.via })).collect::<Vec<_>>(),
                "page": request.page,
                "own": request.own,
                "source_label": if receiver.source.is_some() { Some(receiver.label.clone()) } else { None },
                "request_ref": request.request_ref,
            });
            let now = self.now_iso();
            let item = self.journal.raise_attention(NewAttention {
                task_ref: &request.task_ref,
                principal: &request.principal,
                kind: "login",
                operation_ref: op_ref,
                generation: Some(&lease.generation),
                question: &question,
                details: Some(&details),
                options: &[],
                now_iso: &now,
            })?;
            let names: Vec<&str> = asked.iter().map(|e| e.site.as_str()).collect();
            self.push_event(
                Some(&request.task_ref),
                &format!(
                    "{} asks a person for logins: {}",
                    item.att_ref,
                    names.join(", ")
                ),
            );
            request.att_ref = Some(item.att_ref);
        }
        for entry in request.sites.iter().filter(|e| e.state == "denied") {
            self.refused(&request.task_ref, &request.agent, &entry.site, "denied");
        }
        let mut book = Book::load(&self.journal)?;
        book.requests.push(request.clone());
        book.save(&self.journal)?;
        Ok(request)
    }

    /// `login for X refused`, on the timeline and in `since`.
    fn refused(&self, task_ref: &str, agent: &str, name: &str, why: &str) {
        let text = format!("login for {name} refused");
        self.timeline(
            "login_refused",
            Some(task_ref),
            agent,
            &text,
            json!({ "site": name, "computer": self.computer_name(), "reason": why }),
        );
        self.push_event(Some(task_ref), &text);
    }

    fn new_request(
        &self,
        task: &TaskRecord,
        own: bool,
        page: Option<String>,
        sites: Vec<Entry>,
    ) -> Request {
        Request {
            request_ref: id("login"),
            att_ref: None,
            task_ref: task.task_ref.clone(),
            goal: task.goal.clone(),
            agent: self.task_subject(&task.task_ref, &task.principal),
            principal: task.principal.clone(),
            own,
            page,
            created_ms: self.now_ms(),
            sites,
        }
    }

    /// `computer_begin`'s `logins`: each site's standing, one request for
    /// them all and one attention item for the ones the person answers. It
    /// never waits.
    pub(super) fn begin_logins(
        &self,
        task: &TaskRecord,
        lease: &LeaseRecord,
        names: &[String],
    ) -> Result<Vec<LoginStanding>> {
        let receiver = Receiver::load(&self.journal)?;
        let own = self.own_principal(&task.principal)?;
        let available = receiver.source.is_some() || own;
        let mut standings = Vec::new();
        let mut entries = Vec::new();
        for raw in names {
            let name = site(raw)?;
            if standings.iter().any(|s: &LoginStanding| s.site == name) {
                continue;
            }
            let (entry, mut state) = self.first_standing(&receiver, &name, own, available);
            let last_result = receiver.last_result(&name);
            if last_result.as_deref() == Some("site_rejected")
                && matches!(state, "allowed" | "asking")
            {
                state = "rejected_before";
            }
            if state == "denied" {
                entries.push(Entry {
                    site: name.clone(),
                    via: None,
                    state: "denied".into(),
                    reason: None,
                });
            } else if let Some(entry) = entry {
                entries.push(Entry {
                    site: name.clone(),
                    via: None,
                    state: entry.into(),
                    reason: None,
                });
            }
            standings.push(LoginStanding {
                site: name,
                state: state.into(),
                attention: None,
                last_result,
            });
        }
        if entries.is_empty() {
            return Ok(standings);
        }
        let request = self.open_request(self.new_request(task, own, None, entries), lease, None)?;
        for standing in &mut standings {
            if request
                .sites
                .iter()
                .any(|e| e.site == standing.site && e.state == "asking")
            {
                standing.attention = request.att_ref.clone();
            }
        }
        Ok(standings)
    }

    /// Plan a `sign_in`: which sites, and the request each one is answered
    /// in. Sites still asked or on their way in an earlier request of the
    /// task join it; declined and rejected ones stay so for the task; the
    /// rest start a new request. Returns `[{site, request_ref}]`.
    pub(super) async fn sign_in_plan(
        &self,
        task: &TaskRecord,
        lease: &LeaseRecord,
        op_ref: &str,
        sites: &[String],
    ) -> Result<Value> {
        let tab = if self.desktop.browser_connected() {
            self.desktop
                .tabs()
                .await
                .ok()
                .and_then(|tabs| tabs.into_iter().find(|t| t.focused))
        } else {
            None
        };
        let page = tab.as_ref().map(|t| page_of(&t.url));
        let mut names: Vec<(String, Option<String>)> = Vec::new();
        if sites.is_empty() {
            let tab = tab.ok_or_else(|| {
                IbaraError::new("STALE_TARGET", "No browser tab is focused. Focus the tab that shows the sign-in page, then call sign_in again.", true)
                    .with("execution_not_started", true)
            })?;
            let context = self
                .desktop
                .browser_call("login_context", json!({ "tabId": tab.id }), false)
                .await?;
            let current = context["url"].as_str().and_then(url_host).and_then(|h| site(h).ok()).ok_or_else(|| {
                IbaraError::new("STALE_TARGET", "The focused tab is not a web page. Open the sign-in page, then call sign_in again.", true)
                    .with("execution_not_started", true)
            })?;
            let previous = context["previous"]
                .as_str()
                .and_then(url_host)
                .and_then(|h| site(h).ok())
                .filter(|p| *p != current);
            names.push((current.clone(), None));
            if let Some(previous) = previous {
                names.push((previous, Some(current)));
            }
        } else {
            for raw in sites {
                let name = site(raw)?;
                if !names.iter().any(|(n, _)| *n == name) {
                    names.push((name, None));
                }
            }
        }
        let receiver = Receiver::load(&self.journal)?;
        let own = self.own_principal(&task.principal)?;
        let available = receiver.source.is_some() || own;
        let book = Book::load(&self.journal)?;
        let mut plan = Vec::new();
        let mut fresh = Vec::new();
        for (name, via) in names {
            let earlier = book
                .requests
                .iter()
                .rev()
                .filter(|r| r.task_ref == task.task_ref)
                .find_map(|r| r.sites.iter().find(|e| e.site == name).map(|e| (r, e)));
            match earlier {
                Some((r, e)) if e.open() || matches!(e.state.as_str(), "declined" | "rejected") => {
                    plan.push(json!({ "site": name, "request_ref": r.request_ref }));
                }
                _ => {
                    let (entry, state) = self.first_standing(&receiver, &name, own, available);
                    let state = match (entry, state) {
                        (_, "denied") => "denied",
                        (Some(entry), _) => entry,
                        (None, _) => {
                            plan.push(json!({ "site": name, "state": state }));
                            continue;
                        }
                    };
                    fresh.push(Entry {
                        site: name.clone(),
                        via,
                        state: state.into(),
                        reason: None,
                    });
                    plan.push(json!({ "site": name }));
                }
            }
        }
        if !fresh.is_empty() {
            let request = self.open_request(
                self.new_request(task, own, page, fresh),
                lease,
                Some(op_ref),
            )?;
            for step in plan
                .iter_mut()
                .filter(|s| s.get("request_ref").is_none() && s.get("state").is_none())
            {
                step["request_ref"] = json!(request.request_ref);
            }
        }
        Ok(json!(plan))
    }

    /// What the agent is told for one planned site now.
    fn seen(&self, receiver: &Receiver, book: &Book, step: &Value) -> (Seen, Option<String>) {
        if let Some(code) = step["state"].as_str() {
            return (
                Seen::Code(if code == "not_available" {
                    "not_available"
                } else {
                    "denied"
                }),
                None,
            );
        }
        let name = step["site"].as_str().unwrap_or("");
        let Some(request) = step["request_ref"]
            .as_str()
            .and_then(|r| book.find(r))
            .map(|i| &book.requests[i])
        else {
            return (Seen::Code("unknown"), None);
        };
        let Some(entry) = request.sites.iter().find(|e| e.site == name) else {
            return (Seen::Code("unknown"), None);
        };
        let att = request.att_ref.clone();
        let seen = match entry.state.as_str() {
            "shared" | "worked" => Seen::Shared,
            "rejected" => Seen::Code("site_rejected"),
            "declined" => Seen::Code("declined"),
            "denied" => Seen::Code("denied"),
            "unknown" => Seen::Code("unknown"),
            _ if !receiver.sharing() => Seen::Code("sharing_off"),
            _ if receiver.rule(name) == Rule::Deny => Seen::Code("denied"),
            _ if !self.sharing_computer_here() => Seen::Code("waiting_for_sharing_computer"),
            _ => match entry.reason.as_deref() {
                Some("waiting_for_browser") => Seen::Code("waiting_for_browser"),
                Some("signed_out_there") => Seen::Code("signed_out_there"),
                Some("sharing_off") => Seen::Code("sharing_off"),
                _ if receiver.rule(name) == Rule::Allow
                    && (entry.state == "deliver" || request.own) =>
                {
                    Seen::Delivering
                }
                _ => {
                    let open = att
                        .as_deref()
                        .and_then(|a| self.journal.get_attention(a).ok().flatten())
                        .is_some_and(|a| a.state == "open");
                    if open {
                        Seen::Code("waiting_for_person")
                    } else {
                        Seen::Code("declined")
                    }
                }
            },
        };
        (seen, att)
    }

    /// A `sign_in` as planned: wait up to 10 s for Allowed sites, then, once
    /// every site is settled and one was written, reload the tab and read
    /// whether it left its sign-in page.
    pub(super) async fn sign_in_outcome(
        &self,
        task: &TaskRecord,
        lease: &LeaseRecord,
        plan: &Value,
        gone: &Cancel,
    ) -> Result<Reply> {
        let steps = plan.as_array().cloned().unwrap_or_default();
        let started = Instant::now();
        let mut seen: Vec<(Seen, Option<String>)>;
        loop {
            let receiver = Receiver::load(&self.journal)?;
            let book = Book::load(&self.journal)?;
            seen = steps
                .iter()
                .map(|s| self.seen(&receiver, &book, s))
                .collect();
            let delivering = seen.iter().any(|(s, _)| *s == Seen::Delivering);
            if !delivering
                || started.elapsed() >= DELIVERY_WAIT
                || pause(Duration::from_millis(200), gone).await
            {
                break;
            }
            self.assert_authority(lease)?;
        }
        let codes: Vec<(String, String, Option<String>)> = steps
            .iter()
            .zip(&seen)
            .map(|(step, (s, att))| {
                let code = match s {
                    Seen::Delivering => "waiting_for_sharing_computer",
                    Seen::Shared => "shared",
                    Seen::Code(c) => c,
                };
                (
                    step["site"].as_str().unwrap_or("").to_string(),
                    code.to_string(),
                    att.clone(),
                )
            })
            .collect();
        let waiting = WAITING
            .iter()
            .find_map(|w| codes.iter().find(|(_, c, _)| c == w));
        if let Some((name, code, att)) = waiting {
            let attention = if code == "waiting_for_person" {
                att.clone()
            } else {
                None
            };
            let result = SignInResult {
                sign_in: SignIn {
                    sites: codes
                        .iter()
                        .map(|(s, c, _)| SiteState {
                            site: s.clone(),
                            state: c.clone(),
                        })
                        .collect(),
                    page: "unknown".into(),
                },
                frame: None,
                next: Some(next_for(code, name, &task.task_ref, attention.as_deref())),
                attention,
            };
            return Ok(Reply {
                status: Status::Pending,
                result: serde_json::to_value(result).unwrap_or(Value::Null),
                images: Vec::new(),
                task_ref: Some(task.task_ref.clone()),
            });
        }
        let mut states: Vec<(String, String)> = codes
            .iter()
            .map(|(s, c, _)| (s.clone(), c.clone()))
            .collect();
        let fresh: Vec<(String, String)> = steps
            .iter()
            .zip(&states)
            .filter(|(step, (_, c))| {
                c == "shared" && self.entry_state(step).as_deref() == Some("shared")
            })
            .map(|(step, (s, _))| {
                (
                    s.clone(),
                    step["request_ref"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let mut page = "unknown".to_string();
        let mut frame = None;
        if !states.iter().any(|(_, c)| c == "shared") {
            // Nothing was written: nothing to reload.
        } else if let Some(tab) = self
            .desktop
            .tabs()
            .await
            .ok()
            .and_then(|tabs| tabs.into_iter().find(|t| t.focused))
        {
            self.assert_authority(lease)?;
            let after_ms = self.now_ms();
            let reloaded = self
                .desktop
                .browser_call("login_reload", json!({ "tabId": tab.id }), true)
                .await;
            if reloaded.is_ok_and(|v| v["reloaded"] == true) {
                let until = Instant::now() + PAGE_WAIT;
                loop {
                    let read = self
                        .desktop
                        .browser_call(
                            "login_page",
                            json!({ "tabId": tab.id, "after_ms": after_ms }),
                            false,
                        )
                        .await;
                    if let Some(p) = read
                        .ok()
                        .filter(|v| {
                            v["document_ms"]
                                .as_f64()
                                .is_some_and(|ms| ms >= after_ms as f64)
                        })
                        .and_then(|v| v["page"].as_str().map(str::to_string))
                    {
                        page = p;
                    }
                    if page != "unknown"
                        || Instant::now() >= until
                        || pause(Duration::from_millis(300), gone).await
                    {
                        break;
                    }
                }
            }
            let rejected_site = if page == "still_sign_in" {
                let here = url_host(&tab.url).and_then(|h| site(h).ok());
                match here.filter(|h| fresh.iter().any(|(s, _)| s == h)) {
                    Some(h) => vec![h],
                    None => fresh.iter().map(|(s, _)| s.clone()).collect(),
                }
            } else {
                Vec::new()
            };
            if page != "unknown" && !fresh.is_empty() {
                self.settle_pages(task, &fresh, &rejected_site)?;
                for (name, state) in &mut states {
                    if rejected_site.contains(name) {
                        *state = "site_rejected".into();
                    }
                }
            }
            if let Ok((built, _)) = self.build_frame(task, lease, &FrameSpec::default()).await {
                frame = Some(built.frame.clone());
            }
        }
        let next = states
            .iter()
            .find(|(_, c)| c != "shared")
            .map(|(s, c)| next_for(c, s, &task.task_ref, None))
            .or_else(|| (page == "still_sign_in").then(|| "The page still asks to sign in; ask your person with computer_checkpoint to sign in with Take Control.".to_string()))
            .or_else(|| (page == "unknown").then(|| "ibara couldn't tell whether the page is signed in; observe the tab before relying on it.".to_string()));
        let result = SignInResult {
            sign_in: SignIn {
                sites: states
                    .into_iter()
                    .map(|(site, state)| SiteState { site, state })
                    .collect(),
                page,
            },
            frame,
            attention: None,
            next,
        };
        Ok(Reply {
            status: Status::Ok,
            result: serde_json::to_value(result).unwrap_or(Value::Null),
            images: Vec::new(),
            task_ref: Some(task.task_ref.clone()),
        })
    }

    fn entry_state(&self, step: &Value) -> Option<String> {
        let book = Book::load(&self.journal).ok()?;
        let request = &book.requests[book.find(step["request_ref"].as_str()?)?];
        request
            .sites
            .iter()
            .find(|e| Some(e.site.as_str()) == step["site"].as_str())
            .map(|e| e.state.clone())
    }

    /// The page after a reload: the written sites worked, or `rejected`
    /// ones still showed a sign-in page. Each result goes to the sharing
    /// computer once, for its site memory.
    fn settle_pages(
        &self,
        task: &TaskRecord,
        fresh: &[(String, String)],
        rejected: &[String],
    ) -> Result<()> {
        let mut book = Book::load(&self.journal)?;
        let now = self.now_ms();
        for (name, request_ref) in fresh {
            let Some(i) = book.find(request_ref) else {
                continue;
            };
            let rejected_here = rejected.contains(name);
            if let Some(entry) = book.requests[i]
                .sites
                .iter_mut()
                .find(|e| e.site == *name && e.state == "shared")
            {
                entry.state = if rejected_here { "rejected" } else { "worked" }.into();
            }
            let result = if rejected_here {
                "site_rejected"
            } else {
                "worked"
            };
            book.results.push(
                json!({ "site": name, "result": result, "task_ref": task.task_ref, "at_ms": now }),
            );
            if rejected_here {
                self.push_event(
                    Some(&task.task_ref),
                    &format!("{name} still shows its sign-in page after its login was shared"),
                );
            }
        }
        book.save(&self.journal)
    }

    // ---- the sharing computer's operations ------------------------------------------

    pub(super) async fn login_operator(&self, operator: &str, action: &Value) -> Result<Value> {
        if action["op"] == "login_configure" {
            let records = (self.operator_grants)();
            if let Some(node) = records
                .get(operator)
                .and_then(|r| r.pointer("/tailscale/node"))
                .and_then(Value::as_str)
            {
                let status = crate::tailnet::status()
                    .await
                    .map_err(|_| denied("Could not verify the sharing computer's identity."))?;
                if status.own.as_ref().is_some_and(|own| own.node == node) {
                    return Err(denied(
                        "This computer also runs agents, so it can't share logins yet. Turn on sharing from the computer you use.",
                    ));
                }
            }
        }
        match action["op"].as_str().unwrap_or("") {
            "login_configure" => self.login_configure(operator, action),
            "login_pending" => self.login_pending(operator, action).await,
            "login_deliver" => self.login_deliver(operator, action).await,
            "login_report" => self.login_report(operator, action),
            "login_answer" => self.login_answer(operator, action),
            "login_remove" => self.login_remove(operator, action).await,
            "login_probe" => self.login_probe(operator, action).await,
            _ => Err(invalid("Unknown operator operation.")),
        }
    }

    fn login_configure(&self, operator: &str, action: &Value) -> Result<Value> {
        let mut receiver = Receiver::load(&self.journal)?;
        let elsewhere = receiver.source.as_deref().is_some_and(|s| s != operator);
        if action["check"] == true || (elsewhere && action["replace"] != true) {
            let mut reply = json!({ "configured": false, "source_elsewhere": elsewhere });
            if elsewhere {
                reply["label"] = json!(receiver.label);
            }
            return Ok(reply);
        }
        let label = action["label"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.chars().count() <= 128)
            .ok_or_else(|| invalid("Name the sharing computer."))?;
        let sites = action["sites"]
            .as_object()
            .filter(|m| m.len() <= MAX_SITES)
            .ok_or_else(|| invalid("Invalid login rules."))?;
        for (name, row) in sites {
            if site(name)? != *name {
                return Err(invalid("Login rules need registrable domains."));
            }
            Rule::parse(row["rule"].as_str().unwrap_or(""))?;
            if row
                .as_object()
                .is_none_or(|m| m.keys().any(|k| k != "rule" && k != "last_result"))
            {
                return Err(invalid("Invalid login rules."));
            }
        }
        receiver.source = Some(operator.to_string());
        receiver.label = label.trim().to_string();
        receiver.enabled = action["enabled"] == true;
        receiver.sites = sites.clone().into_iter().collect();
        receiver.save(&self.journal)?;
        Ok(json!({ "configured": true, "enabled": receiver.enabled }))
    }

    /// Open requests and site results, for the sharing computer; also its heartbeat.
    async fn login_pending(&self, operator: &str, action: &Value) -> Result<Value> {
        let receiver = Receiver::load(&self.journal)?;
        receiver.check_source(operator)?;
        self.login_seen.set(self.now_ms());
        self.login_deferred_removals().await;
        let mut book = Book::load(&self.journal)?;
        let mut requests = Vec::new();
        let answering = action["att_ref"].as_str();
        for request in &book.requests {
            if answering.is_some_and(|att| request.att_ref.as_deref() != Some(att)) {
                continue;
            }
            if answering.is_none() && !request.sites.iter().any(Entry::open) {
                continue;
            }
            let active = self.journal.get_task(&request.task_ref)?.is_some_and(|t| {
                matches!(t.state.as_str(), "active" | "created" | "waiting_for_human")
            });
            if !active {
                continue;
            }
            let att_open = request
                .att_ref
                .as_deref()
                .and_then(|a| self.journal.get_attention(a).ok().flatten())
                .is_some_and(|a| a.state == "open");
            let sites: Vec<Value> = request
                .sites
                .iter()
                .filter(|e| answering.is_some() || e.open())
                .map(|e| {
                    let rule = receiver.rule(&e.site);
                    let need = if !e.open() || !receiver.enabled || rule == Rule::Deny {
                        "none"
                    } else if rule == Rule::Allow && (e.state == "deliver" || request.own) {
                        "deliver"
                    } else if e.state == "asking" && att_open {
                        "answer"
                    } else {
                        "none"
                    };
                    let mut row = json!({ "site": e.site, "need": need });
                    if let Some(via) = &e.via {
                        row["via"] = json!(via);
                    }
                    if let Some(reason) = &e.reason {
                        row["reason"] = json!(reason);
                    }
                    row
                })
                .collect();
            let mut row = json!({
                "request_ref": request.request_ref, "task_ref": request.task_ref, "goal": request.goal,
                "agent": request.agent, "own": request.own, "sites": sites,
            });
            if let Some(att) = &request.att_ref {
                row["att_ref"] = json!(att);
            }
            requests.push(row);
        }
        let results = std::mem::take(&mut book.results);
        if !results.is_empty() {
            book.save(&self.journal)?;
        }
        Ok(json!({ "requests": requests, "results": results }))
    }

    /// Write one site's login into this computer's browser, a batch at a time.
    async fn login_deliver(&self, operator: &str, action: &Value) -> Result<Value> {
        let receiver = Receiver::load(&self.journal)?;
        receiver.check_source(operator)?;
        if !receiver.enabled {
            return Err(denied("Login sharing is off for this computer."));
        }
        let name = site(action["site"].as_str().unwrap_or(""))?;
        if receiver.rule(&name) != Rule::Allow {
            return Err(denied(
                "This site has not been allowed by the sharing computer.",
            ));
        }
        let request_ref = action["request_ref"].as_str();
        let request = match request_ref {
            None => None,
            Some(r) => {
                let book = Book::load(&self.journal)?;
                let request = book
                    .find(r)
                    .map(|i| book.requests[i].clone())
                    .ok_or_else(|| invalid("Unknown login request."))?;
                let active = self.journal.get_task(&request.task_ref)?.is_some_and(|t| {
                    matches!(t.state.as_str(), "active" | "created" | "waiting_for_human")
                });
                if !active {
                    return Err(denied("The task that asked for this login has ended."));
                }
                if !request.sites.iter().any(|e| e.site == name) {
                    return Err(invalid("That site is not part of the login request."));
                }
                Some(request)
            }
        };
        let cookies = action["cookies"]
            .as_array()
            .ok_or_else(|| invalid("Invalid login cookie bundle."))?;
        let count = check_cookies(&name, &action["cookies"])?;
        if count == 0 {
            return Err(invalid("There is no login to deliver."));
        }
        if !self.desktop.browser_connected() {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "This computer's browser isn't running, so the login waits for the next time it's needed.", true)
                .with("execution_not_started", true));
        }
        // The record precedes the write and holds no values.
        let refresh = receiver.shared.contains_key(&name);
        let (kind, verb) = if refresh {
            ("login_refreshed", "refreshed")
        } else {
            ("login_shared", "shared")
        };
        let task_ref = request.as_ref().map(|r| r.task_ref.as_str());
        let text = format!("login for {name} {verb}");
        self.timeline(
            "login_delivery_started",
            task_ref,
            operator,
            &format!("copying login for {name}"),
            json!({
                "site": name, "computer": self.computer_name(), "cookies": count,
                "agent": request.as_ref().map(|r| r.agent.clone()), "person": request.as_ref().map(|r| r.principal.clone()),
            }),
        );
        let mut written = 0u64;
        let mut failed: Vec<Value> = Vec::new();
        for (batch, chunk) in cookies.chunks(BATCH).enumerate() {
            let offset = batch * BATCH;
            match self
                .desktop
                .browser_call(
                    "cookies_write",
                    json!({ "site": name, "cookies": chunk }),
                    true,
                )
                .await
            {
                Ok(result) => {
                    written += result["written"].as_u64().unwrap_or(0);
                    for f in result["failed"].as_array().into_iter().flatten() {
                        let index = f["index"].as_u64().unwrap_or(0) as usize + offset;
                        let field = f["field"]
                            .as_str()
                            .filter(|s| {
                                s.len() <= 32
                                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                            })
                            .unwrap_or("unknown");
                        failed.push(json!({ "index": index, "field": field }));
                    }
                }
                Err(e)
                    if batch == 0
                        && e.details.get("execution_not_started") == Some(&Value::Bool(true)) =>
                {
                    return Err(e);
                }
                Err(_) => {
                    failed.extend(
                        (offset..cookies.len())
                            .map(|index| json!({ "index": index, "field": "unknown" })),
                    );
                    break;
                }
            }
        }
        // Browser work yields: merge only our field into the current rules.
        let mut current = Receiver::load(&self.journal)?;
        if failed.is_empty() {
            if current.source.as_deref() == Some(operator) {
                current.shared.insert(name.clone(), self.now_ms());
                current.save(&self.journal)?;
            }
            self.timeline(
                kind,
                task_ref,
                operator,
                &text,
                json!({"site": name, "written": written, "failed": 0}),
            );
            self.push_event(task_ref, &text);
        } else {
            let text = format!("login for {name} may be incomplete");
            self.timeline(
                "login_delivery_incomplete",
                task_ref,
                operator,
                &text,
                json!({"site": name, "written": written, "failed": failed.len()}),
            );
            self.push_event(task_ref, &text);
        }
        if let Some(request) = request {
            let mut book = Book::load(&self.journal)?;
            if let Some(i) = book.find(&request.request_ref) {
                if let Some(entry) = book.requests[i].sites.iter_mut().find(|e| e.site == name) {
                    entry.state = if failed.is_empty() {
                        "shared"
                    } else {
                        "unknown"
                    }
                    .into();
                    entry.reason = None;
                }
                self.close_if_settled(&mut book, i, operator)?;
            }
            book.save(&self.journal)?;
        }
        Ok(json!({ "site": name, "written": written, "failed": failed }))
    }

    /// Close a request's attention item once none of its sites waits for the person.
    fn close_if_settled(&self, book: &mut Book, index: usize, operator: &str) -> Result<bool> {
        let request = &book.requests[index];
        if request.sites.iter().any(|e| e.state == "asking") {
            return Ok(false);
        }
        let Some(att) = request.att_ref.clone() else {
            return Ok(true);
        };
        if self
            .journal
            .get_attention(&att)?
            .is_some_and(|a| a.state == "open")
        {
            let shared: Vec<&str> = request
                .sites
                .iter()
                .filter(|e| matches!(e.state.as_str(), "shared" | "worked"))
                .map(|e| e.site.as_str())
                .collect();
            let refused: Vec<&str> = request
                .sites
                .iter()
                .filter(|e| matches!(e.state.as_str(), "declined" | "denied"))
                .map(|e| e.site.as_str())
                .collect();
            let mut words = Vec::new();
            let incomplete: Vec<&str> = request
                .sites
                .iter()
                .filter(|e| e.state == "unknown")
                .map(|e| e.site.as_str())
                .collect();
            if !incomplete.is_empty() {
                words.push(format!("Could not confirm {}", incomplete.join(", ")));
            }
            if !shared.is_empty() {
                words.push(format!("Shared {}", shared.join(", ")));
            }
            if !refused.is_empty() {
                words.push(format!("Not shared {}", refused.join(", ")));
            }
            let answer = if words.is_empty() {
                "Answered".to_string()
            } else {
                words.join("; ")
            };
            self.journal
                .answer_attention(&att, &answer, operator, &self.now_iso())?;
            self.push_event(
                Some(&request.task_ref),
                &format!("{att} answered: {answer}"),
            );
        }
        Ok(true)
    }

    fn login_report(&self, operator: &str, action: &Value) -> Result<Value> {
        Receiver::load(&self.journal)?.check_source(operator)?;
        let name = site(action["site"].as_str().unwrap_or(""))?;
        let reason = action["reason"]
            .as_str()
            .filter(|r| {
                matches!(
                    *r,
                    "waiting_for_browser" | "signed_out_there" | "sharing_off" | "unknown"
                )
            })
            .ok_or_else(|| {
                invalid(
                    "The reason is waiting_for_browser, signed_out_there, sharing_off or unknown.",
                )
            })?;
        let mut book = Book::load(&self.journal)?;
        let i = book
            .find(action["request_ref"].as_str().unwrap_or(""))
            .ok_or_else(|| invalid("Unknown login request."))?;
        let entry = book.requests[i]
            .sites
            .iter_mut()
            .find(|e| e.site == name && e.open())
            .ok_or_else(|| invalid("That site is not waiting in the login request."))?;
        if reason == "unknown" {
            entry.state = "unknown".into();
            entry.reason = None;
            self.close_if_settled(&mut book, i, operator)?;
            book.save(&self.journal)?;
        } else if entry.reason.as_deref() != Some(reason) {
            entry.reason = Some(reason.to_string());
            book.save(&self.journal)?;
        }
        Ok(json!({ "ok": true }))
    }

    /// The person's answer, from the sharing computer: sites it shared
    /// (already delivered), declined this time, or denied. Declined and
    /// denied hold for the task. The item closes once no site waits.
    fn login_answer(&self, operator: &str, action: &Value) -> Result<Value> {
        let receiver = Receiver::load(&self.journal)?;
        let decline_unpinned = receiver.source.is_none()
            && action["decisions"]
                .as_object()
                .is_some_and(|d| !d.is_empty() && d.values().all(|v| v == "declined"));
        if !decline_unpinned {
            receiver.check_source(operator)?;
        }
        let att = action["att_ref"].as_str().unwrap_or("");
        let item = self
            .journal
            .get_attention(att)?
            .filter(|a| a.kind == "login")
            .ok_or_else(|| invalid("Unknown login request."))?;
        let decisions = action["decisions"]
            .as_object()
            .ok_or_else(|| invalid("Give a decision for each site."))?;
        let mut book = Book::load(&self.journal)?;
        let i = book
            .requests
            .iter()
            .position(|r| r.att_ref.as_deref() == Some(att))
            .ok_or_else(|| invalid("Unknown login request."))?;
        let (task_ref, agent) = (
            book.requests[i].task_ref.clone(),
            book.requests[i].agent.clone(),
        );
        for (name, decision) in decisions {
            let decision = decision
                .as_str()
                .filter(|d| matches!(*d, "shared" | "declined" | "denied"))
                .ok_or_else(|| invalid("A decision is shared, declined or denied."))?;
            let entry = book.requests[i]
                .sites
                .iter_mut()
                .find(|e| e.site == *name)
                .ok_or_else(|| invalid("That site is not part of the login request."))?;
            if decision == "shared" || !entry.open() {
                continue;
            }
            entry.state = decision.into();
            entry.reason = None;
            if item.state == "open" {
                self.refused(&task_ref, &agent, name, decision);
            }
        }
        let closed = self.close_if_settled(&mut book, i, operator)?;
        let sites: Vec<Value> = book.requests[i]
            .sites
            .iter()
            .map(|e| json!({ "site": e.site, "state": e.state }))
            .collect();
        book.save(&self.journal)?;
        Ok(json!({ "closed": closed, "sites": sites }))
    }

    /// Remove a site's login from this computer's browser; with the browser
    /// closed, once it next connects. The site leaves the list at once.
    async fn login_remove(&self, operator: &str, action: &Value) -> Result<Value> {
        let mut receiver = Receiver::load(&self.journal)?;
        receiver.check_source(operator)?;
        let name = site(action["site"].as_str().unwrap_or(""))?;
        receiver.sites.remove(&name);
        receiver.shared.remove(&name);
        if !receiver.remove_pending.contains(&name) {
            receiver.remove_pending.push(name.clone());
        }
        receiver.save(&self.journal)?;
        let result = if self.desktop.browser_connected() {
            self.desktop
                .browser_call("cookies_remove", json!({"site": name}), true)
                .await
                .ok()
        } else {
            None
        };
        let done = result
            .as_ref()
            .is_some_and(|r| r["failed"].as_array().is_some_and(Vec::is_empty));
        if done {
            let mut current = Receiver::load(&self.journal)?;
            current.remove_pending.retain(|s| s != &name);
            current.save(&self.journal)?;
        }
        let text = if done {
            format!("login for {name} removed")
        } else {
            format!("removing login for {name} is pending")
        };
        self.timeline(if done { "login_removed" } else { "login_removal_pending" }, None, operator, &text,
            json!({"site": name, "computer": self.computer_name(), "removed": result.as_ref().and_then(|r| r["removed"].as_u64()), "deferred": !done}));
        self.push_event(None, &text);
        let mut reply =
            json!({"site": name, "removed": result.as_ref().and_then(|r| r["removed"].as_u64())});
        if !done {
            reply["deferred"] = json!(true);
        }
        Ok(reply)
    }

    /// Retry removals without retaining a stale rules snapshot across browser work.
    pub(super) async fn login_deferred_removals(&self) {
        let Ok(receiver) = Receiver::load(&self.journal) else {
            return;
        };
        if !self.desktop.browser_connected() {
            return;
        }
        for name in receiver.remove_pending {
            let result = self
                .desktop
                .browser_call("cookies_remove", json!({"site": name}), true)
                .await;
            if result.is_ok_and(|r| r["failed"].as_array().is_some_and(Vec::is_empty)) {
                let saved = Receiver::load(&self.journal).and_then(|mut current| {
                    current.remove_pending.retain(|s| s != &name);
                    current.save(&self.journal)
                });
                if let Err(e) = saved {
                    super::log_event("login_remove_failed", &e.to_string());
                }
            }
        }
    }

    async fn login_probe(&self, operator: &str, action: &Value) -> Result<Value> {
        Receiver::load(&self.journal)?.check_source(operator)?;
        let name = site(action["site"].as_str().unwrap_or(""))?;
        let counted = self
            .desktop
            .browser_call("cookies_count", json!({ "site": name }), false)
            .await?;
        let mut result = json!({ "site": name, "cookies": counted["count"].as_u64().unwrap_or(0), "page": "unknown" });
        let tab = self
            .desktop
            .tabs()
            .await
            .ok()
            .and_then(|tabs| tabs.into_iter().find(|t| t.focused));
        if let Some(tab) = tab {
            let args = json!({ "tabId": tab.id });
            if let Ok(context) = self
                .desktop
                .browser_call("login_context", args.clone(), false)
                .await
            {
                for (key, out) in [("url", "current_site"), ("previous", "previous_site")] {
                    if let Some(host) = context[key]
                        .as_str()
                        .and_then(url_host)
                        .and_then(|h| site(h).ok())
                    {
                        result[out] = json!(host);
                    }
                }
            }
            if let Ok(page) = self.desktop.browser_call("login_page", args, false).await {
                result["page"] = page["page"].clone();
            }
        }
        Ok(result)
    }

    // ---- computer_status ------------------------------------------------------------

    /// The fleet row's login phrase: `logins from Laptop · 23 sites allowed` or `logins off`.
    pub(super) fn login_phrase(&self) -> String {
        let receiver = Receiver::load(&self.journal).unwrap_or_default();
        if !receiver.sharing() {
            return "logins off".into();
        }
        let allowed = receiver
            .sites
            .keys()
            .filter(|s| receiver.rule(s) == Rule::Allow)
            .count();
        format!(
            "logins from {} · {allowed} site{} allowed",
            receiver.label,
            if allowed == 1 { "" } else { "s" }
        )
    }

    /// For `computer_status({ref: cmp_…})`, after the capability line: the
    /// Allowed site names and how many are Denied (never their names).
    pub(super) fn login_sites(&self) -> String {
        let receiver = Receiver::load(&self.journal).unwrap_or_default();
        if !receiver.sharing() {
            return String::new();
        }
        let allowed: Vec<&str> = receiver
            .sites
            .keys()
            .filter(|s| receiver.rule(s) == Rule::Allow)
            .map(String::as_str)
            .collect();
        let denied = receiver
            .sites
            .keys()
            .filter(|s| receiver.rule(s) == Rule::Deny)
            .count();
        let names = if allowed.is_empty() {
            "none".to_string()
        } else {
            crate::controller::clip(&allowed.join(", "), 600)
        };
        format!(" · logins allowed: {names}; {denied} denied")
    }
}

#[cfg(test)]
mod tests {
    use super::url_host;

    #[test]
    fn a_page_address_gives_its_host() {
        assert_eq!(
            url_host("https://login.irs.gov:8443/a?b#c"),
            Some("login.irs.gov")
        );
        assert_eq!(url_host("http://user@id.me/x"), Some("id.me"));
        assert_eq!(url_host("chrome://settings"), None);
    }
}
