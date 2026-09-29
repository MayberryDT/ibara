//! Commands over every paired computer: approvals and problems waiting for a
//! person (`fleet-attention`), what happened while this person was away
//! (`away`, `away-seen`), the theme (`theme-fleet`, `operator-theme`), waking
//! a sleeping computer (`wake`), and this console's own settings (`settings`).
//!
//! Each computer is reached over its operator route; the console learns its
//! controller epoch with a `session` call and keeps it until the computer
//! says it changed. Work over several computers runs a few at a time, each
//! with its own short deadline, so one computer that is off never holds up
//! the rest.

use super::envelope::{Fault, Handled};
use super::everyday::{self, SLOW_DEADLINE};
use super::{Console, Ctx, option, positional, validated_id};
use crate::error::{IbaraError, Result};
use crate::operator::directory::{ListedComputer, OperatorDirectory, operator_state_dir};
use crate::operator::pattern;
use crate::settings::{self, Scope};
use crate::{theme, wake};
use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// A computer's approvals are asked for again after this long.
const ATTENTION_FRESH: Duration = Duration::from_secs(8);
/// How long one computer may take to list its approvals.
const ATTENTION_DEADLINE: Duration = Duration::from_secs(4);
/// How long an item answered here stays hidden from reads that began before the answer.
const ANSWERED_KEEP: Duration = Duration::from_secs(120);
/// Computers asked at once.
const AT_ONCE: usize = 4;
/// Theme pieces sent per request (under the operator route's line bound once in base64).
const THEME_PIECE: usize = 512 * 1024;
const TIMELINE_DEADLINE: Duration = Duration::from_secs(8);

/// What the console keeps between requests.
#[derive(Default)]
pub struct Fleet {
    /// Each computer's controller epoch, as its last `session` said.
    epochs: Mutex<HashMap<String, String>>,
    /// What each computer lists for a person. Never held across a network read.
    attention: Mutex<Attention>,
    /// The timeline revision each computer had when `away` last showed it.
    shown: Mutex<HashMap<String, i64>>,
    /// Which computers have this console's Ask before agents send, spend or
    /// delete; read from its file on first use.
    ask_first: Mutex<Option<AskFirstShared>>,
}

#[derive(Default)]
struct Attention {
    computers: HashMap<String, Listed>,
    /// Items answered from this console, by computer and ref, with when.
    /// A read that began before the answer may still list them; they stay
    /// hidden until a later read leaves them out, or for [`ANSWERED_KEEP`].
    answered: HashMap<(String, String), Instant>,
}

/// One computer's open items as last read.
#[derive(Default)]
struct Listed {
    items: Vec<Value>,
    /// When the last read that worked began; `None` asks again at once.
    read_at: Option<Instant>,
    /// When an answer here last changed what it lists.
    changed_at: Option<Instant>,
    /// A read under way began then (another request uses the items as they
    /// are); one abandoned for over a minute no longer counts.
    reading: Option<Instant>,
    /// The last read failed; `items` are the ones before it.
    unreachable: bool,
}

impl Attention {
    /// Of `computers` (the directory, in order), those to read now; they are
    /// marked as being read. Computers no longer listed are forgotten.
    fn due(&mut self, computers: &[String], now: Instant) -> Vec<String> {
        self.computers.retain(|id, _| computers.contains(id));
        self.answered.retain(|_, at| now.saturating_duration_since(*at) < ANSWERED_KEEP);
        let mut due = Vec::new();
        for id in computers {
            let listed = self.computers.entry(id.clone()).or_default();
            let reading = listed.reading.is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(60));
            if !reading && listed.read_at.is_none_or(|at| now.saturating_duration_since(at) >= ATTENTION_FRESH) {
                listed.reading = Some(now);
                due.push(id.clone());
            }
        }
        due
    }

    /// The outcome of a read of `computer` that began at `began`. A failure
    /// keeps the items from before and leaves the computer due.
    fn record(&mut self, computer: &str, began: Instant, outcome: Option<Vec<Value>>) {
        let listed = self.computers.entry(computer.to_string()).or_default();
        listed.reading = None;
        let Some(items) = outcome else {
            listed.unreachable = true;
            return;
        };
        listed.unreachable = false;
        // An answer here since the read began: what it read may be out of date.
        listed.read_at = if listed.changed_at.is_some_and(|at| at > began) { None } else { Some(began) };
        let lists = |r: &str| items.iter().any(|i| i["ref"] == r);
        self.answered.retain(|(c, r), at| !(c == computer && *at < began && !lists(r)));
        listed.items = items;
    }

    /// `att_ref` on `computer` was answered here at `now`.
    fn answer(&mut self, computer: &str, att_ref: &str, now: Instant) {
        self.answered.insert((computer.to_string(), att_ref.to_string()), now);
        let listed = self.computers.entry(computer.to_string()).or_default();
        listed.changed_at = Some(now);
        listed.read_at = None;
    }

    /// Every open item of `computers` not answered here, the ones kept from
    /// before a failed read marked `unreachable`; and those computers.
    fn items(&self, computers: &[String]) -> (Vec<Value>, Vec<String>) {
        let mut items = Vec::new();
        let mut unreachable = Vec::new();
        for id in computers {
            let Some(listed) = self.computers.get(id) else { continue };
            if listed.unreachable {
                unreachable.push(id.clone());
            }
            for item in &listed.items {
                if item["ref"].as_str().is_some_and(|r| self.answered.contains_key(&(id.clone(), r.to_string()))) {
                    continue;
                }
                let mut item = item.clone();
                if listed.unreachable {
                    item["unreachable"] = json!(true);
                }
                items.push(item);
            }
        }
        items.sort_by(|a, b| a["at"].as_str().unwrap_or("").cmp(b["at"].as_str().unwrap_or("")));
        (items, unreachable)
    }
}

fn stale(error: &IbaraError) -> bool {
    error.code == "STALE_TARGET" || error.message.contains("binding changed") || error.message.contains("generation changed")
}

/// A computer's controller epoch: remembered, else from a `session` call.
pub(super) async fn epoch(console: &Console, computer: &str) -> Result<String> {
    if let Some(epoch) = console.fleet.epochs.lock().unwrap_or_else(|p| p.into_inner()).get(computer) {
        return Ok(epoch.clone());
    }
    fresh_epoch(console, computer).await
}

async fn fresh_epoch(console: &Console, computer: &str) -> Result<String> {
    let data = console.sessions.call(computer, None, "session", Value::Null).await?;
    let epoch = data
        .get("controller_epoch")
        .and_then(Value::as_str)
        .filter(|e| pattern::id(e) && data.get("computer_id").and_then(Value::as_str) == Some(computer))
        .ok_or_else(|| IbaraError::new("STALE_TARGET", "The computer's session did not name it.", true))?
        .to_string();
    console.fleet.epochs.lock().unwrap_or_else(|p| p.into_inner()).insert(computer.to_string(), epoch.clone());
    Ok(epoch)
}

/// One operation on one computer with the epoch this console knows, asked
/// again once with a fresh epoch when the computer restarted meanwhile.
pub(super) async fn fleet_call(console: &Console, computer: &str, op: &str, fields: Value, deadline: Duration) -> Result<Value> {
    let work = async {
        let epoch = epoch(console, computer).await?;
        let data = match console.sessions.call(computer, Some(&epoch), op, fields.clone()).await {
            Err(error) if stale(&error) => {
                let epoch = fresh_epoch(console, computer).await?;
                console.sessions.call(computer, Some(&epoch), op, fields).await?
            }
            other => other?,
        };
        if data.get("computer_id").and_then(Value::as_str) != Some(computer) {
            return Err(IbaraError::new("STALE_TARGET", "The reply came from a different computer.", false));
        }
        Ok(data)
    };
    tokio::time::timeout(deadline, work).await.map_err(|_| IbaraError::new("TIMEOUT", "The computer did not answer in time.", true))?
}

/// Every computer this console has added.
fn verified(console: &Console) -> Result<Vec<ListedComputer>> {
    Ok(OperatorDirectory::open(&console.database)?.list_computers()?.into_iter().filter(|r| r.trust_state == "verified").collect())
}

/// Run `work` for each computer, a few at a time; results in directory order.
async fn each<T, F, Fut>(console: &Arc<Console>, rows: Vec<ListedComputer>, work: F) -> Vec<(ListedComputer, T)>
where
    F: Fn(Arc<Console>, ListedComputer) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let permits = Arc::new(Semaphore::new(AT_ONCE));
    let mut set = JoinSet::new();
    for (index, row) in rows.into_iter().enumerate() {
        let job = work(console.clone(), row.clone());
        let permits = permits.clone();
        set.spawn(async move {
            let _turn = permits.acquire_owned().await;
            (index, row, job.await)
        });
    }
    let mut done: Vec<(usize, ListedComputer, T)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(result) = joined {
            done.push(result);
        }
    }
    done.sort_by_key(|(index, ..)| *index);
    done.into_iter().map(|(_, row, value)| (row, value)).collect()
}

/// The fleet's items from one computer's `attention` reply: approvals and
/// questions it lists to this computer, and a problem needing a person. An
/// approval's `summary` is what a person reads; its `details` (the structured
/// request, null from older computers) are for a Details view. A question's
/// `options` are the answers its agent offered (empty: any short answer).
fn items_of(row: &ListedComputer, result: &Value) -> Vec<Value> {
    let mut items: Vec<Value> = result["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["state"] == "open")
        .map(|item| {
            json!({
                "computer_id": row.computer_id, "label": row.label, "ref": item["att_ref"], "kind": item["kind"],
                "summary": item.get("summary").cloned().unwrap_or_else(|| item["question"].clone()),
                "details": item.get("details").cloned().unwrap_or(Value::Null),
                "options": item.get("options").filter(|o| o.is_array()).cloned().unwrap_or_else(|| json!([])),
                "at": item["created_at"], "task_ref": item["task_ref"],
            })
        })
        .collect();
    let needs = &result["repair"]["needs_person"];
    if needs.is_object() {
        items.push(json!({
            "computer_id": row.computer_id, "label": row.label, "ref": needs["fix"], "kind": "repair",
            "summary": needs["message"], "at": needs["at"],
        }));
    }
    items
}

/// `fleet-attention`: everything waiting for a person, on every computer.
/// Each computer is asked at most every 8 s (at once when named by `--fresh
/// ID`, which the console sends for computers working or waiting), a few at a time, 4 s each. One
/// that does not answer keeps the items it listed before, marked
/// `unreachable` (and is named in `unreachable`), and is asked again next
/// time. Items answered here stay out of the list even when a read that
/// began before the answer still names them.
pub async fn fleet_attention(ctx: &Ctx) -> Handled {
    let rows = verified(&ctx.console)?;
    let ids: Vec<String> = rows.iter().map(|r| r.computer_id.clone()).collect();
    let lock = || ctx.console.fleet.attention.lock().unwrap_or_else(|p| p.into_inner());
    // `--fresh ID`: a computer the console sees working or waiting is asked
    // now, not after the others' cache age, so a new approval shows at once.
    let fresh: Vec<&str> = ctx.args.windows(2).filter(|w| w[0] == "--fresh").map(|w| w[1].as_str()).collect();
    let due = {
        let mut state = lock();
        for id in fresh {
            if let Some(listed) = state.computers.get_mut(id) {
                listed.read_at = None;
            }
        }
        state.due(&ids, Instant::now())
    };
    let due: Vec<ListedComputer> = rows.into_iter().filter(|r| due.contains(&r.computer_id)).collect();
    let began = Instant::now();
    let fetched = each(&ctx.console, due, |console, row| async move {
        let data = fleet_call(&console, &row.computer_id, "attention", Value::Null, ATTENTION_DEADLINE).await?;
        // Only a computer this console may manage takes its Settings' choice.
        if data["result"]["can_answer"] == json!(true) {
            share_ask_first(&console, &row.computer_id).await;
        }
        Ok::<_, IbaraError>(items_of(&row, &data["result"]))
    })
    .await;
    let mut state = lock();
    for (row, outcome) in fetched {
        state.record(&row.computer_id, began, outcome.ok());
    }
    let (items, unreachable) = state.items(&ids);
    drop(state);
    let count = items.len();
    Ok(ctx.ready(json!({"items": items, "count": count, "unreachable": unreachable})))
}

/// An answer here settled `att_ref` on `computer`: leave it out from now on
/// and ask the computer again next time.
pub(super) fn forget_attention(console: &Console, computer: &str, att_ref: &str) {
    console.fleet.attention.lock().unwrap_or_else(|p| p.into_inner()).answer(computer, att_ref, Instant::now());
}

// ---------------------------------------------------------------------------
// While you were away.

/// `~/.local/state/ibara/seen.json`: when this person last looked, and each
/// computer's timeline revision at that moment.
#[derive(Debug, Default)]
struct Seen {
    at_ms: i64,
    revisions: Map<String, Value>,
}

fn seen_path() -> std::path::PathBuf {
    operator_state_dir().join("seen.json")
}

impl Seen {
    fn load() -> Seen {
        let value: Value = std::fs::read_to_string(seen_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
        Seen { at_ms: value["seen_at"].as_i64().unwrap_or(0), revisions: value["computers"].as_object().cloned().unwrap_or_default() }
    }

    fn save(&self) -> Result<()> {
        let dir = operator_state_dir();
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        }
        let text = json!({"seen_at": self.at_ms, "computers": self.revisions}).to_string();
        crate::operator::replace_file(&seen_path(), text.as_bytes(), 0o600)?;
        Ok(())
    }
}

/// Each computer's events since this person last looked (the newest 20 of a
/// computer never looked at); computers with nothing new, or not answering,
/// are left out. Returns the list and each answering computer's revision.
async fn history(console: &Arc<Console>) -> Result<(Vec<Value>, Vec<(String, i64)>)> {
    let revisions = Seen::load().revisions;
    let rows = verified(console)?;
    let fetched = each(console, rows, move |console, row| {
        let after = revisions.get(&row.computer_id).and_then(Value::as_i64);
        let fields = json!({"after": after.unwrap_or(0), "limit": if after.is_some() { 100 } else { 20 }});
        async move { fleet_call(&console, &row.computer_id, "timeline", fields, TIMELINE_DEADLINE).await }
    })
    .await;
    let mut computers = Vec::new();
    let mut revisions = Vec::new();
    for (row, outcome) in fetched {
        let Ok(data) = outcome else { continue };
        let result = &data["result"];
        if let Some(revision) = result["revision"].as_i64() {
            revisions.push((row.computer_id.clone(), revision));
        }
        let events: Vec<Value> = result["events"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| json!({"at": e["at"], "kind": e["kind"], "actor": e["actor"], "summary": e["summary"]}))
            .collect();
        if !events.is_empty() {
            computers.push(json!({"computer_id": row.computer_id, "label": row.label, "events": events}));
        }
    }
    Ok((computers, revisions))
}

/// `away`: what happened on each computer since this person last marked it seen.
pub async fn away(ctx: &Ctx) -> Handled {
    let (computers, revisions) = history(&ctx.console).await?;
    ctx.console.fleet.shown.lock().unwrap_or_else(|p| p.into_inner()).extend(revisions);
    Ok(ctx.ready(json!({"since": Seen::load().at_ms, "computers": computers})))
}

/// `away-seen`: everything shown (or, for a computer not shown, everything
/// until now) is seen.
pub async fn away_seen(ctx: &Ctx) -> Handled {
    Ok(ctx.ready(mark_seen(&ctx.console).await?))
}

/// Mark everything seen; the new `{since}`.
pub async fn mark_seen(console: &Arc<Console>) -> Result<Value> {
    let shown: HashMap<String, i64> = std::mem::take(&mut *console.fleet.shown.lock().unwrap_or_else(|p| p.into_inner()));
    let rows: Vec<ListedComputer> = verified(console)?.into_iter().filter(|r| !shown.contains_key(&r.computer_id)).collect();
    let fetched = each(console, rows, |console, row| async move {
        fleet_call(&console, &row.computer_id, "timeline", json!({"after": i64::MAX, "limit": 1}), TIMELINE_DEADLINE).await
    })
    .await;
    let mut seen = Seen::load();
    for (id, revision) in shown {
        seen.revisions.insert(id, json!(revision));
    }
    for (row, outcome) in fetched {
        if let Some(revision) = outcome.ok().and_then(|d| d["result"]["revision"].as_i64()) {
            seen.revisions.insert(row.computer_id, json!(revision));
        }
    }
    seen.at_ms = crate::ids::now_millis();
    seen.save()?;
    Ok(json!({"since": seen.at_ms}))
}

// ---------------------------------------------------------------------------
// Theme.

/// The theme bundle, packed once per request.
type Packed = Arc<(Vec<u8>, String)>;
type Bundle = Arc<tokio::sync::OnceCell<std::result::Result<Packed, String>>>;

async fn bundle_for(cell: &Bundle, name: &str) -> std::result::Result<Packed, String> {
    cell.get_or_init(|| async {
        let dir = theme::source(name).ok_or_else(|| format!("The {name} theme's files are not on this computer."))?;
        let bytes = theme::pack(&dir).map_err(|e| e.message)?;
        let sha: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        Ok(Arc::new((bytes, sha)))
    })
    .await
    .clone()
}

/// Apply `name` on one computer, sending the theme first when it lacks it.
/// `(state, message)`: `applied`, `offline` or `failed`.
async fn apply_theme(console: &Console, computer: &str, label: &str, name: &str, bundle: &Bundle) -> (&'static str, String) {
    let offline = |e: &IbaraError| matches!(e.code, "TIMEOUT" | "SESSION_UNAVAILABLE");
    let failed = |e: &IbaraError| {
        let text = e.message.strip_prefix(&format!("{}: ", e.code)).unwrap_or(&e.message).to_string();
        if text.contains(" is denied ") { format!("{label} doesn't let this computer change its theme.") } else { text }
    };
    let apply = || fleet_call(console, computer, "theme_apply", json!({"name": name}), SLOW_DEADLINE);
    let first = match apply().await {
        Err(e) if offline(&e) => return ("offline", format!("{label} isn't answering.")),
        Err(e) => return ("failed", failed(&e)),
        Ok(data) => data,
    };
    match first["result"]["state"].as_str() {
        Some("applied") => return ("applied", String::new()),
        Some("pending_approval") => return ("failed", format!("Waiting for someone on {label} to approve.")),
        Some("missing") => {}
        _ => return ("failed", format!("{label} gave an unexpected answer.")),
    }
    let packed = match bundle_for(bundle, name).await {
        Ok(packed) => packed,
        Err(message) => return ("failed", message),
    };
    let (bytes, sha) = (&packed.0, &packed.1);
    let upload = uuid::Uuid::new_v4().simple().to_string();
    for (index, piece) in bytes.chunks(THEME_PIECE).enumerate() {
        let fields = json!({
            "name": name, "upload_id": upload, "offset": index * THEME_PIECE, "total": bytes.len(), "sha256": sha,
            "data": base64::engine::general_purpose::STANDARD.encode(piece),
        });
        if let Err(e) = fleet_call(console, computer, "theme_upload", fields, Duration::from_secs(30)).await {
            return if offline(&e) { ("offline", format!("{label} stopped answering.")) } else { ("failed", failed(&e)) };
        }
    }
    match apply().await {
        Ok(data) if data["result"]["state"] == "applied" => ("applied", String::new()),
        Ok(_) => ("failed", format!("{label} did not take the theme.")),
        Err(e) if offline(&e) => ("offline", format!("{label} isn't answering.")),
        Err(e) => ("failed", failed(&e)),
    }
}

/// `theme-fleet`: this computer's Omarchy theme on every computer.
pub async fn theme_fleet(ctx: &Ctx) -> Handled {
    let Some(name) = theme::current_name() else {
        return Ok(ctx.failure("CAPABILITY_UNAVAILABLE", "This computer's Omarchy theme could not be read.", "failed", true));
    };
    let bundle: Bundle = Arc::default();
    let rows = verified(&ctx.console)?;
    let theme_name = name.clone();
    let results = each(&ctx.console, rows, move |console, row| {
        let (name, bundle) = (theme_name.clone(), bundle.clone());
        async move { apply_theme(&console, &row.computer_id, &row.label, &name, &bundle).await }
    })
    .await;
    let results: Vec<Value> = results
        .into_iter()
        .map(|(row, (state, message))| json!({"computer_id": row.computer_id, "label": row.label, "state": state, "message": message}))
        .collect();
    Ok(ctx.ready(json!({"theme": name, "results": results})))
}

/// `operator-theme apply NAME --computer ID --epoch E`.
pub async fn operator_theme(ctx: &Ctx) -> Handled {
    let (computer, epoch) = everyday::target(ctx)?;
    let pos = positional(&ctx.args);
    let name = match pos.as_slice() {
        ["apply", name] if theme::valid_name(name) => name.to_string(),
        _ => return Err(Fault::plain("Use: apply NAME")),
    };
    ctx.console.fleet.epochs.lock().unwrap_or_else(|p| p.into_inner()).insert(computer.clone(), epoch);
    let label = everyday::label(&ctx.console, &computer);
    match apply_theme(&ctx.console, &computer, &label, &name, &Arc::default()).await {
        ("applied", _) => Ok(ctx.ready(json!({"computer_id": computer, "theme": name, "state": "applied"}))),
        ("offline", message) => Ok(ctx.failure("OFFLINE", &message, "offline", true)),
        (_, message) => Ok(ctx.failure("THEME_FAILED", &message, "failed", true)),
    }
}

// ---------------------------------------------------------------------------
// Wake.

/// `wake NAME`: a magic packet from this computer when it is on that
/// computer's network, else from another computer on it that is on. The
/// network is its subnet and, when recorded, its gateway's MAC, so a
/// computer on another network with the same private range does not count.
/// `sure` is false when the packet went only from this computer and whether
/// this is the same network could not be told.
pub async fn wake(ctx: &Ctx) -> Handled {
    let computer = validated_id(positional(&ctx.args).first().copied(), "computer_id")?;
    let rows = verified(&ctx.console)?;
    let Some(row) = rows.iter().find(|r| r.computer_id == computer) else {
        return Ok(ctx.failure("UNKNOWN_COMPUTER", "That computer is not in this console.", "failed", false));
    };
    let (mac, subnet) = match (row.wake["mac"].as_str(), row.wake["subnet"].as_str()) {
        (Some(mac), Some(subnet)) => (mac.to_string(), subnet.to_string()),
        _ => {
            let message = format!(
                "{} has not said how to wake it. Turn it on once with Wake from the network on in its settings, and ibara will remember.",
                row.label
            );
            return Ok(ctx.failure("CAPABILITY_UNAVAILABLE", &message, "failed", false));
        }
    };
    let (Some(mac_bytes), Some(network)) = (wake::parse_mac(&mac), wake::parse_subnet(&subnet)) else {
        return Ok(ctx.failure("CAPABILITY_UNAVAILABLE", &format!("The way to wake {} is not valid.", row.label), "failed", false));
    };
    let gateway = row.wake["gateway_mac"].as_str();
    let sent = |via: &str, sure: bool| json!({"computer_id": computer, "sent_via": via, "state": "sent", "sure": sure});
    let here = wake::here(&subnet, gateway).await;
    if here != wake::Here::Elsewhere {
        wake::send(mac_bytes, network)?;
        if here == wake::Here::Same {
            return Ok(ctx.ready(sent("this computer", true)));
        }
    }
    // Another computer on that network: the same subnet, and the same gateway when both are known.
    let helpers = rows.iter().filter(|r| {
        let same_gateway = match (r.wake["gateway_mac"].as_str(), gateway) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => true,
        };
        r.computer_id != computer && r.wake["subnet"].as_str() == Some(subnet.as_str()) && same_gateway
    });
    for helper in helpers {
        let mut fields = json!({"mac": mac, "subnet": subnet});
        if let Some(gateway) = gateway {
            fields["gateway_mac"] = json!(gateway);
        }
        if let Ok(data) = fleet_call(&ctx.console, &helper.computer_id, "send_wake", fields, Duration::from_secs(8)).await
            && data["result"]["state"] == "sent"
        {
            return Ok(ctx.ready(sent(&helper.label, data["result"]["sure"].as_bool().unwrap_or(false))));
        }
    }
    if here == wake::Here::Unsure {
        return Ok(ctx.ready(sent("this computer", false)));
    }
    Ok(ctx.failure("OFFLINE", &format!("No computer on {}'s network is on to wake it.", row.label), "offline", true))
}

// ---------------------------------------------------------------------------
// This console's settings.

/// A settings command's words: `get`, `set KEY VALUE`, `reset KEY` or
/// `reset --section ID` (the other options, such as `--computer`, left out).
pub(super) fn settings_args(args: &[String]) -> Vec<&str> {
    let words = positional(args);
    match option(args, "--section") {
        Some(section) if words == ["reset"] => vec!["reset", "--section", section],
        _ => words,
    }
}

/// `settings get|set KEY VALUE|reset KEY|reset --section ID` (scope `console`).
/// A change to Ask before agents send, spend or delete reaches every
/// computer this console may manage with the next `fleet-attention`, which
/// then reads them all at once.
pub fn console_settings(ctx: &Ctx) -> Handled {
    let args = settings_args(&ctx.args);
    match settings::command(Scope::Console, &args, &settings::Defaults::default()) {
        Ok(result) => {
            if matches!(args.as_slice(), ["set" | "reset", "agents_ask_first", ..] | ["reset", "--section", "approvals"]) {
                let mut attention = ctx.console.fleet.attention.lock().unwrap_or_else(|p| p.into_inner());
                attention.computers.values_mut().for_each(|listed| listed.read_at = None);
            }
            Ok(ctx.ready(result))
        }
        Err(error) => Ok(ctx.failure(error.code, &error.message, "failed", error.retry_safe)),
    }
}

// ---------------------------------------------------------------------------
// Ask before agents send, spend or delete, for every computer.

/// Which computers already have this console's choice for Ask before agents
/// send, spend or delete (`~/.local/state/ibara/ask-first.json`). A change of
/// the choice, here or by hand in the settings file, starts the list over.
/// Each computer this console may manage takes the choice the next time its
/// approvals are read (`fleet_attention`), so a computer that was off, or is
/// added later, gets it then; one that has it is not asked again, so another
/// console's later choice stands until this one changes its own.
#[derive(Debug, Default, PartialEq)]
struct AskFirstShared {
    ask_first: bool,
    computers: Vec<String>,
    /// Computers whose ibara said it has no such setting, with the controller
    /// epoch they said it under. Kept only in memory: each is asked again
    /// once it restarts, as it does when it is updated.
    older: HashMap<String, String>,
}

fn ask_first_path() -> std::path::PathBuf {
    operator_state_dir().join("ask-first.json")
}

impl AskFirstShared {
    fn load() -> AskFirstShared {
        let value: Value = std::fs::read_to_string(ask_first_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
        let computers = value["computers"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        AskFirstShared { ask_first: value["ask_first"].as_bool().unwrap_or(true), computers, older: HashMap::new() }
    }

    fn save(&self) -> Result<()> {
        let dir = operator_state_dir();
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        }
        let text = json!({"ask_first": self.ask_first, "computers": self.computers}).to_string();
        crate::operator::replace_file(&ask_first_path(), text.as_bytes(), 0o600)?;
        Ok(())
    }

    /// Whether `computer`, whose controller epoch is `epoch`, is to be given
    /// the choice `ask_first` now.
    fn due(&mut self, computer: &str, ask_first: bool, epoch: Option<&str>) -> bool {
        if self.ask_first != ask_first {
            *self = AskFirstShared { ask_first, ..AskFirstShared::default() };
        }
        !self.computers.iter().any(|c| c == computer) && (epoch.is_none() || self.older.get(computer).map(String::as_str) != epoch)
    }

    /// `computer`'s reply to being given `ask_first`, with its controller
    /// epoch after it; true when it now has the choice, so the list is to be
    /// saved. Only a reply that names the setting counts: an older ibara's
    /// "There is no setting called fleet_ask_first." waits for its restart,
    /// and any other refusal is asked again next time.
    fn record(&mut self, computer: &str, ask_first: bool, epoch: Option<&str>, reply: &Result<Value>) -> bool {
        if self.ask_first != ask_first || self.computers.iter().any(|c| c == computer) {
            return false;
        }
        match reply {
            Ok(data) if data["result"]["key"] == "fleet_ask_first" => {
                self.older.remove(computer);
                self.computers.push(computer.to_string());
                true
            }
            Err(error) if error.code == "INVALID_ARGUMENT" && error.message == "There is no setting called fleet_ask_first." => {
                if let Some(epoch) = epoch {
                    self.older.insert(computer.to_string(), epoch.to_string());
                }
                false
            }
            _ => false,
        }
    }
}

/// Give `computer` this console's choice for Ask before agents send, spend
/// or delete, unless it has it already. On: the computer's own `fleet_ask_first`
/// goes back to its default; off: it is set off. A computer that is not
/// answering, or refuses for any other reason, is asked again next time; one
/// whose ibara has no such setting yet is asked again once it restarts.
async fn share_ask_first(console: &Console, computer: &str) {
    let ask_first = settings::current().bool("agents_ask_first");
    let epoch = || console.fleet.epochs.lock().unwrap_or_else(|p| p.into_inner()).get(computer).cloned();
    let due = {
        let before = epoch();
        let mut shared = console.fleet.ask_first.lock().unwrap_or_else(|p| p.into_inner());
        shared.get_or_insert_with(AskFirstShared::load).due(computer, ask_first, before.as_deref())
    };
    if !due {
        return;
    }
    let args = if ask_first { json!(["reset", "fleet_ask_first"]) } else { json!(["set", "fleet_ask_first", "false"]) };
    let reply = fleet_call(console, computer, "settings", json!({"args": args}), ATTENTION_DEADLINE).await;
    let after = epoch();
    let mut shared = console.fleet.ask_first.lock().unwrap_or_else(|p| p.into_inner());
    let Some(shared) = shared.as_mut() else { return };
    if shared.record(computer, ask_first, after.as_deref(), &reply)
        && let Err(error) = shared.save()
    {
        eprintln!("{}", json!({"event": "ask_first_unsaved", "detail": error.message}));
    }
}

// ---------------------------------------------------------------------------
// `ibara away`.

/// `ibara away [--seen]`: what happened on each computer since this person
/// last looked, as plain text (for people and agents); `--seen` then marks
/// it seen, as the console's `away-seen` does. Exit 0, 1 on failure, 64 on
/// bad usage.
pub fn away_main(args: Vec<std::ffi::OsString>) -> i32 {
    let mark = match args.as_slice() {
        [] => false,
        [flag] if flag == "--seen" => true,
        _ => {
            eprintln!("Usage: ibara away [--seen]");
            return 64;
        }
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return 1 };
    runtime.block_on(async {
        let database = crate::operator::directory::directory_path(None);
        let console = Arc::new(Console::new(database, std::env::temp_dir()));
        let outcome = history(&console).await;
        let code = match outcome {
            Err(error) => {
                eprintln!("{}", error.message);
                1
            }
            Ok((computers, revisions)) => {
                print!("{}", away_text(&computers, Seen::load().at_ms));
                if mark {
                    console.fleet.shown.lock().unwrap_or_else(|p| p.into_inner()).extend(revisions);
                    match mark_seen(&console).await {
                        Ok(_) => println!("Marked as seen."),
                        Err(error) => {
                            eprintln!("{}", error.message);
                            return 1;
                        }
                    }
                }
                0
            }
        };
        console.sessions.close_all().await;
        code
    })
}

/// One block per computer: its name, then each event's time and summary.
fn away_text(computers: &[Value], since_ms: i64) -> String {
    let since = if since_ms > 0 { format!("since {}", short_time(&crate::ids::iso_from_millis(since_ms))) } else { "recently".to_string() };
    if computers.is_empty() {
        return format!("Nothing new on your computers {since}.\n");
    }
    let mut out = String::new();
    for computer in computers {
        let events = computer["events"].as_array().map(Vec::as_slice).unwrap_or_default();
        out.push_str(&format!("{} — {} new {since}\n", computer["label"].as_str().unwrap_or("A computer"), events.len()));
        for event in events {
            out.push_str(&format!("  {}  {}\n", short_time(event["at"].as_str().unwrap_or("")), event["summary"].as_str().unwrap_or("")));
        }
    }
    out
}

/// `2026-09-26T21:05:07.123Z` → `2026-09-26 21:05 UTC`.
fn short_time(iso: &str) -> String {
    match (iso.get(..10), iso.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time} UTC"),
        _ => iso.to_string(),
    }
}

#[cfg(test)]
mod tests {
    //! Failure cases the merge of attention reads must catch:
    //! 1. A read that began before an answer here brings the answered item back.
    //! 2. A failed read empties a computer's items (and so closes their notices).
    //! 3. A failed read counts as fresh, so the computer is not asked again.
    //! 4. An answered item stays hidden after a later read shows it is gone,
    //!    so its hold is never cleared.
    use super::*;

    fn item(r: &str) -> Value {
        json!({"computer_id": "cmp_1", "ref": r, "at": format!("2026-09-27T10:00:0{}Z", r.len())})
    }

    fn refs(items: &[Value]) -> Vec<&str> {
        items.iter().map(|i| i["ref"].as_str().unwrap()).collect()
    }

    #[test]
    fn answers_here_stay_answered_and_a_failed_read_keeps_what_was_there() {
        let ids = vec!["cmp_1".to_string()];
        let mut state = Attention::default();
        let t0 = Instant::now();
        assert_eq!(state.due(&ids, t0), ids);
        state.record("cmp_1", t0, Some(vec![item("att_a"), item("att_bb")]));
        assert!(state.due(&ids, t0 + Duration::from_secs(1)).is_empty(), "fresh for a while");

        // A poll begins, the person answers att_a here, then the poll returns
        // what it read before the answer (case 1).
        let poll = t0 + Duration::from_secs(9);
        assert_eq!(state.due(&ids, poll), ids);
        state.answer("cmp_1", "att_a", poll + Duration::from_millis(500));
        state.record("cmp_1", poll, Some(vec![item("att_a"), item("att_bb")]));
        assert_eq!(refs(&state.items(&ids).0), ["att_bb"]);
        assert_eq!(state.due(&ids, poll + Duration::from_secs(1)), ids, "read again at once after an answer");

        // The computer stops listing it: the hold is settled (case 4) …
        let later = poll + Duration::from_secs(2);
        state.record("cmp_1", later, Some(vec![item("att_bb")]));
        assert!(state.answered.is_empty());

        // … and a failed read keeps att_bb, marked, and is retried (cases 2 and 3).
        let failing = later + Duration::from_secs(9);
        assert_eq!(state.due(&ids, failing), ids);
        state.record("cmp_1", failing, None);
        let (items, unreachable) = state.items(&ids);
        assert_eq!((refs(&items), unreachable), (vec!["att_bb"], ids.clone()));
        assert_eq!(items[0]["unreachable"], true);
        assert_eq!(state.due(&ids, failing + Duration::from_secs(1)), ids);
        state.record("cmp_1", failing + Duration::from_secs(1), Some(vec![]));
        assert_eq!(state.items(&ids), (vec![], vec![]));
    }

    /// Failure cases for giving each computer this console's Ask before
    /// agents send, spend or delete:
    /// 1. A refusal that is not about the setting (any other
    ///    INVALID_ARGUMENT, the console's own included) counts as the
    ///    computer having it, so it is never given the choice.
    /// 2. A computer whose ibara has no such setting yet is never given it
    ///    once it restarts on a newer ibara.
    /// 3. While it is still on the older ibara, it is asked on every read.
    #[test]
    fn a_computer_has_the_choice_only_once_it_took_it() {
        let mut shared = AskFirstShared { ask_first: true, computers: vec!["cmp_2".into()], ..Default::default() };
        let refused = |message: &str| -> Result<Value> { Err(IbaraError::new("INVALID_ARGUMENT", message, true)) };
        assert!(shared.due("cmp_1", false, Some("e1")));
        assert!(shared.due("cmp_2", false, Some("e1")), "a change of choice starts the list over");

        assert!(!shared.record("cmp_1", false, Some("e1"), &refused("The computer's reply could not be read.")));
        assert!(shared.due("cmp_1", false, Some("e1")), "asked again next time (case 1)");
        assert!(!shared.record("cmp_1", false, Some("e1"), &refused("There is no setting called fleet_ask_first.")));
        assert!(!shared.due("cmp_1", false, Some("e1")), "not while it runs that ibara (case 3)");
        assert!(shared.due("cmp_1", false, Some("e2")), "once it restarted, as after an update (case 2)");
        assert!(shared.record("cmp_1", false, Some("e2"), &Ok(json!({"result": {"key": "fleet_ask_first"}}))));
        assert!(!shared.due("cmp_1", false, Some("e3")));
        assert_eq!(shared.computers, ["cmp_1"]);
    }
}
