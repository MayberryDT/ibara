//! The authoritative access records, kept atomically in the existing journal.
//! Transport files are projections; after import they never grant authority.
use crate::error::{Result, denied, invalid};
use crate::ids::millis_from_iso;

fn expiry(s: &str) -> Option<i64> {
    // A small, unambiguous UTC format; validate before the shared date parser.
    if !s.is_ascii() || !(20..=24).contains(&s.len()) || !s.ends_with('Z') {
        return None;
    }
    let normalized = if s.len() == 20 {
        format!("{}.000Z", &s[..19])
    } else {
        if s.as_bytes()[19] != b'.' || s.len() < 22 {
            return None;
        }
        let f = &s[20..s.len() - 1];
        if !f.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        format!("{}.{f:0<3}Z", &s[..19])
    };
    let t = millis_from_iso(&normalized)?;
    (crate::ids::iso_from_millis(t) == normalized).then_some(t)
}

use crate::store::{AttentionItem, Journal};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const CAPABILITIES: [&str; 5] = ["watch", "files", "control", "agents", "administer"];
/// This computer's local owner: the person holding the admin key, whom
/// `computerctl` acts as. No paired computer or agent principal may use it.
pub const OWNER: &str = "owner";
pub const CLASSES: [&str; 6] = ["observe", "change", "send", "spend", "destructive", "access"];
/// The kinds of agent step "Ask before agents send, spend or delete" covers.
pub const ASKED: [&str; 3] = ["send", "spend", "destructive"];
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    Allow,
    Ask,
    Deny,
}
impl Rule {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            _ => Err(invalid("Rule must be allow, ask or deny.")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}
pub fn default_effect(class: &str) -> Rule {
    if matches!(class, "observe" | "change") {
        Rule::Allow
    } else {
        Rule::Ask
    }
}
/// What a friend's invite lets their computer do here. Never administer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareLevel {
    /// Watch only.
    Watch,
    /// Watch; files, take control and agent tasks each ask first.
    UseWithApproval,
    /// Watch, files and take control; agent tasks ask first.
    TakeControl,
}
impl ShareLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "watch" => Some(Self::Watch),
            "use_with_approval" => Some(Self::UseWithApproval),
            "take_control" => Some(Self::TakeControl),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Watch => "watch",
            Self::UseWithApproval => "use_with_approval",
            Self::TakeControl => "take_control",
        }
    }
    /// As a person reads it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Watch => "Watch",
            Self::UseWithApproval => "Use with Approval",
            Self::TakeControl => "Take Control",
        }
    }
    /// The rule for each capability; administer is always denied.
    pub fn rule(self, capability: &str) -> Rule {
        match (self, capability) {
            (_, "watch") => Rule::Allow,
            (Self::TakeControl, "files" | "control") => Rule::Allow,
            (Self::UseWithApproval, "files" | "control" | "agents") | (Self::TakeControl, "agents") => Rule::Ask,
            _ => Rule::Deny,
        }
    }
}
/// What a newly paired computer may do here.
#[derive(Debug, Clone)]
pub enum PairRights {
    /// Watch and files as given; someone else's computer a person accepted gets both.
    Reviewed { observe: bool, files: bool },
    /// The same person's computer: everything, administer included.
    OwnComputer,
    /// A friend's invite: exactly its level, every grant ending at `expires_at`
    /// (UTC, as grants store it), and the sentence the timeline shows.
    Invite { level: ShareLevel, expires_at: Option<String>, summary: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub kind: String,
    pub computer: String,
    pub key: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    pub key: String,
    pub endpoint: Option<String>,
    pub active: bool,
    pub generation: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub subject: String,
    pub capability: String,
    pub rule: Rule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effects: BTreeMap<String, Rule>,
}
impl Grant {
    fn active(&self, now: i64) -> bool {
        self.expires_at.as_deref().is_none_or(|v| expiry(v).is_some_and(|t| t > now))
    }
    /// The rule this grant itself sets for agent steps of `class`, if any. A
    /// computer's map that names every kind was written whole, by pairing,
    /// import or an older console that copied the rules shown: in it, only a
    /// rule that differs from the default was chosen, and the others keep
    /// their defaults (send, spend and delete follow Ask before agents send,
    /// spend or delete). An agent's own map was only ever written for that
    /// agent, so every rule in it counts, an Ask First an older console
    /// wrote whole included. A map naming some kinds sets exactly those.
    fn explicit(&self, class: &str) -> Option<Rule> {
        let rule = self.effects.get(class).copied()?;
        let whole = self.effects.len() == CLASSES.len() && !self.subject.contains('@');
        if whole && rule == default_effect(class) { None } else { Some(rule) }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub version: u32,
    pub revision: u64,
    pub identities: BTreeMap<String, Identity>,
    pub pairings: BTreeMap<String, Pairing>,
    pub grants: BTreeMap<String, Grant>,
}
impl Access {
    pub fn empty() -> Self {
        Self {
            version: 1,
            revision: 1,
            identities: BTreeMap::new(),
            pairings: BTreeMap::new(),
            grants: BTreeMap::new(),
        }
    }
    pub fn load(journal: &Journal) -> Result<Option<Self>> {
        Self::read(journal.db())
    }
    pub fn read(db: &rusqlite::Connection) -> Result<Option<Self>> {
        let raw: Option<String> = db
            .query_row("SELECT value FROM meta WHERE key='access_model'", [], |r| r.get(0))
            .optional()?;
        let Some(raw) = raw else { return Ok(None) };
        let model: Self =
            serde_json::from_str(&raw).map_err(|_| denied("Access records are unreadable; administrator recovery is required."))?;
        if model.version != 1 || model.identities.len() > 512 || model.grants.len() > 4096 {
            return Err(denied("Unsupported access records."));
        }
        Ok(Some(model))
    }
    pub fn save(&mut self, journal: &Journal, actor: &str, summary: &str) -> Result<()> {
        self.revision += 1;
        self.write(journal, actor, summary, true)
    }
    /// [`Access::save`] for a change a person made by answering the approval
    /// `att_ref` with `answer`: the answer and the change are saved together,
    /// or neither is, so an answer that is refused changes nothing. Every
    /// other approval ends as after any change; `att_ref` stays, so the step
    /// it approved still runs once.
    pub fn save_answering(&mut self, journal: &Journal, actor: &str, summary: &str, att_ref: &str, answer: &str, now_iso: &str) -> Result<AttentionItem> {
        self.revision += 1;
        journal.answer_attention_with(att_ref, answer, actor, now_iso, |tx| self.write_in(tx, actor, summary, true, Some(att_ref)))
    }
    /// Record a computer-vouched agent name on first use. The name already
    /// inherits exactly its computer's rights, so this is not a policy change:
    /// it keeps the revision (open edits stay valid) and pending approvals.
    pub fn save_identity(&self, journal: &Journal, actor: &str, summary: &str) -> Result<()> {
        self.write(journal, actor, summary, false)
    }
    fn write(&self, journal: &Journal, actor: &str, summary: &str, policy_change: bool) -> Result<()> {
        let tx = journal.db().unchecked_transaction()?;
        self.write_in(&tx, actor, summary, policy_change, None)?;
        tx.commit()?;
        Ok(())
    }
    fn write_in(&self, tx: &rusqlite::Transaction<'_>, actor: &str, summary: &str, policy_change: bool, keep: Option<&str>) -> Result<()> {
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('access_model',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_value(self)
                .map_err(|_| invalid("Invalid access records."))?
                .to_string()],
        )?;
        tx.execute(
            "INSERT INTO timeline_events(at,kind,task_ref,actor,summary,data) VALUES(?1,'access',NULL,?2,?3,?4)",
            rusqlite::params![crate::ids::now_iso(), actor, summary, json!({"revision":self.revision}).to_string()],
        )?;
        // Old Node and pre-access Rust builds must refuse this journal, rather
        // than silently ignoring its revocations during a rollback.
        tx.execute("UPDATE meta SET value='3' WHERE key='schema_version'", [])?;
        tx.execute("UPDATE meta SET value=?1 WHERE key='core_schema_version'", [crate::store::CORE_SCHEMA_VERSION.to_string()])?;
        if policy_change {
            // Any policy change invalidates unanswered and answered-but-unused approvals.
            tx.execute(
                "UPDATE attention_items SET state='expired' WHERE state IN ('open','answered') AND kind IN ('approval','access') AND att_ref IS NOT ?1",
                [keep],
            )?;
        }
        Ok(())
    }
    pub fn paired(&self, subject: &str) -> bool {
        if subject == "owner" {
            return self.identities.contains_key(subject);
        }
        let computer = self
            .identities
            .get(subject)
            .map(|i| i.computer.as_str())
            .unwrap_or_else(|| subject.rsplit('@').next().unwrap_or(subject));
        self.pairings.get(computer).is_some_and(|p| p.active)
    }
    pub fn computer<'a>(&'a self, subject: &'a str) -> &'a str {
        self.identities
            .get(subject)
            .map(|i| i.computer.as_str())
            .unwrap_or_else(|| subject.rsplit('@').next().unwrap_or(subject))
    }
    /// The active grants of `capability` that apply to `subject`: its own and
    /// its computer's; none when it is not paired. An agent never has more
    /// than its computer: its own grants count only while its computer holds
    /// an active one, so they end with that computer's share, time limit or
    /// grant.
    fn matching(&self, subject: &str, capability: &str, now: i64) -> Vec<&Grant> {
        if !self.paired(subject) {
            return Vec::new();
        }
        let computer = self.computer(subject);
        let grants: Vec<&Grant> = self
            .grants
            .values()
            .filter(|g| g.capability == capability && (g.subject == subject || g.subject == computer) && g.active(now))
            .collect();
        if subject != computer && !grants.iter().any(|g| g.subject == computer) {
            return Vec::new();
        }
        grants
    }
    pub fn rule(&self, subject: &str, capability: &str, now: i64) -> Rule {
        self.matching(subject, capability, now).into_iter().map(|g| g.rule).max().unwrap_or(Rule::Deny)
    }
    /// The rule for `subject`'s agent steps of `class`. Denied anywhere wins.
    /// For send, spend and delete, the strictest rule the agent or its
    /// computer sets comes next: an agent's own Allowed does not outrank its
    /// computer's Ask First ([`Access::computer_asks`]). A kind neither sets
    /// follows `ask_first` (this computer's Ask before agents send, spend or
    /// delete) for agents from a computer that may administer this one, the
    /// owner's own; agents from any other computer, a friend's, ask first.
    /// Other kinds take the strictest rule of the agent and its computer.
    pub fn effect(&self, subject: &str, class: &str, now: i64, ask_first: bool) -> Rule {
        let grants = self.matching(subject, "agents", now);
        if grants.is_empty() || grants.iter().any(|g| g.rule == Rule::Deny || g.explicit(class) == Some(Rule::Deny)) {
            return Rule::Deny;
        }
        if !ASKED.contains(&class) {
            return grants.iter().map(|g| g.explicit(class).unwrap_or_else(|| default_effect(class))).max().unwrap_or(Rule::Deny);
        }
        let own_computer = self.rule(self.computer(subject), "administer", now) == Rule::Allow;
        let unset = if ask_first || !own_computer { Rule::Ask } else { Rule::Allow };
        grants.iter().filter_map(|g| g.explicit(class)).max().unwrap_or(unset)
    }
    /// Whether `subject`'s computer's own rule asks first before its agents'
    /// steps of `class`. Any agent on that computer can use another's name,
    /// so an agent's own Allowed does not outrank it.
    pub fn computer_asks(&self, subject: &str, class: &str, now: i64) -> bool {
        let computer = self.computer(subject);
        subject != computer
            && self.matching(subject, "agents", now).iter().any(|g| g.subject == computer && g.explicit(class) == Some(Rule::Ask))
    }
    /// What `subject` may do here. `effects` are the rules its agent steps
    /// follow; `own_effects` the ones its own active grant sets, which the
    /// Access tab changes one at a time.
    pub fn own_row(&self, subject: &str, now: i64, ask_first: bool) -> Value {
        let caps: BTreeMap<_, _> = CAPABILITIES.iter().map(|c| (*c, self.rule(subject, c, now))).collect();
        let effects: BTreeMap<_, _> = CLASSES.iter().map(|c| (*c, self.effect(subject, c, now, ask_first))).collect();
        let own: BTreeMap<_, _> = CLASSES
            .iter()
            .filter_map(|c| {
                let own = self.grants.values().filter(|g| g.subject == subject && g.capability == "agents" && g.active(now));
                own.filter_map(|g| g.explicit(c)).max().map(|r| (*c, r))
            })
            .collect();
        json!({"subject":subject,"capabilities":caps,"effects":effects,"own_effects":own,"paired":self.paired(subject)})
    }
    /// The access table as `actor` may see it: all of it for someone who may
    /// administer this computer; otherwise only `actor`'s own computer and its
    /// agents, never who else this computer is shared with. `ask_first` is
    /// this computer's Ask before agents send, spend or delete.
    pub fn view(&self, actor: &str, now: i64, ask_first: bool) -> Value {
        let admin = self.rule(actor, "administer", now) == Rule::Allow;
        let computer = self.computer(actor);
        let shown = |subject: &str| admin || (subject != OWNER && self.computer(subject) == computer);
        let rows: Vec<_> = self
            .identities
            .keys()
            .filter(|s| shown(s))
            .map(|s| {
                let mut r = self.own_row(s, now, ask_first);
                r["kind"] = json!(self.identities[s].kind);
                r
            })
            .collect();
        let identities: BTreeMap<_, _> = self
            .identities
            .iter()
            .filter(|(s, _)| shown(s))
            .map(|(s, i)| {
                (
                    s,
                    json!({"kind":i.kind,"computer":i.computer,"key":if s=="owner" {"local administrator credential"}else{&i.key}}),
                )
            })
            .collect();
        let pairings: BTreeMap<_, _> = self.pairings.iter().filter(|(c, _)| admin || c.as_str() == computer).collect();
        let grants: BTreeMap<_, _> = self.grants.iter().filter(|(_, g)| shown(&g.subject)).collect();
        json!({"revision":self.revision,"identities":identities,"pairings":pairings,"grants":grants,"rows":rows,
            "can_administer":admin, "ask_first":ask_first,
            "boundary":"ibara asks first only for steps it can recognize or that an agent declares. An agent that can run commands or use the desktop can do anything the person signed in there can."})
    }
    /// Let `subject`'s agent steps of `kinds` run without asking a person, as
    /// its own rule; its other rules stay. A kind it is denied, or one its
    /// computer's own rule asks first for ([`Access::computer_asks`]), is left
    /// as it is. The rule ends with the first grant it rests on (a friend's
    /// share, a timed Agent Tasks grant), and like any agent's own rule it
    /// counts only while its computer may run agent tasks here. The kinds it
    /// changed, none when each already ran without asking.
    pub fn allow_without_asking(&mut self, subject: &str, kinds: &[&'static str], now: i64) -> Result<Vec<&'static str>> {
        if self.rule(subject, "agents", now) == Rule::Deny {
            return Err(denied(format!("{subject} may not run agent tasks on this computer.")));
        }
        let id = format!("{subject}:agents");
        let own = self.grants.get(&id).filter(|g| g.active(now));
        let mut effects: BTreeMap<String, Rule> =
            CLASSES.iter().filter_map(|c| own.and_then(|g| g.explicit(c)).map(|r| (c.to_string(), r))).collect();
        let changed: Vec<&'static str> = kinds
            .iter()
            .copied()
            .filter(|k| {
                ASKED.contains(k)
                    && self.effect(subject, k, now, true) != Rule::Deny
                    && !self.computer_asks(subject, k, now)
                    && effects.get(*k) != Some(&Rule::Allow)
            })
            .collect();
        if changed.is_empty() {
            return Ok(changed);
        }
        for kind in &changed {
            effects.insert(kind.to_string(), Rule::Allow);
        }
        let rule = own.map(|g| g.rule).unwrap_or_else(|| self.rule(subject, "agents", now));
        let expires_at = self.matching(subject, "agents", now).into_iter().filter_map(|g| g.expires_at.clone()).min_by_key(|t| expiry(t));
        self.put(&json!({"subject": subject, "capability": "agents", "rule": rule.name(), "expires_at": expires_at, "effects": effects}))?;
        Ok(changed)
    }
    pub fn put(&mut self, action: &Value) -> Result<()> {
        let subject = action["subject"].as_str().unwrap_or("");
        if !valid_subject(subject) || !self.paired(subject) {
            return Err(denied("Pair this identity before granting access."));
        }
        if !self.identities.contains_key(subject) {
            let computer = subject.rsplit_once('@').ok_or_else(|| invalid("Unknown identity."))?.1;
            let key = self
                .pairings
                .get(computer)
                .ok_or_else(|| denied("Computer is not paired."))?
                .key
                .clone();
            self.identities.insert(
                subject.into(),
                Identity {
                    kind: "agent".into(),
                    computer: computer.into(),
                    key,
                },
            );
        }
        let capability = action["capability"].as_str().unwrap_or("");
        if !CAPABILITIES.contains(&capability) {
            return Err(invalid("Unknown capability."));
        }
        let rule = Rule::parse(action["rule"].as_str().unwrap_or(""))?;
        let expires_at = action
            .get("expires_at")
            .filter(|v| !v.is_null())
            .map(|v| v.as_str().unwrap_or("").to_string());
        if expires_at.as_deref().is_some_and(|t| expiry(t).is_none()) {
            return Err(invalid("expires_at must be UTC: YYYY-MM-DDTHH:MM:SS[.sss]Z."));
        }
        let effects: BTreeMap<String, Rule> =
            serde_json::from_value(action.get("effects").cloned().unwrap_or(json!({}))).map_err(|_| invalid("Invalid effect rules."))?;
        if effects.keys().any(|c| !CLASSES.contains(&c.as_str())) || !effects.is_empty() && capability != "agents" {
            return Err(invalid("Effect rules belong to run agent tasks."));
        }
        let id = action["grant_id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{subject}:{capability}"));
        if id.len() > 150 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || b"@_.:-".contains(&c)) {
            return Err(invalid("Invalid grant id."));
        }
        if self.grants.len() >= 4096 && !self.grants.contains_key(&id) {
            return Err(invalid("Too many grants."));
        }
        self.grants.insert(
            id,
            Grant {
                subject: subject.into(),
                capability: capability.into(),
                rule,
                expires_at,
                effects,
            },
        );
        // Grants only stop matching over time, so the owner stays able to recover
        // if administer is allowed now and still allowed once every grant expired.
        let now = crate::ids::now_millis();
        if [now, i64::MAX].into_iter().any(|t| self.rule("owner", "administer", t) != Rule::Allow) {
            return Err(denied("Keep the local owner's permanent administer grant for recovery."));
        }
        Ok(())
    }
    pub fn import_grant(&mut self, subject: &str, capability: &str, allow: bool, expires: Option<String>, effects: BTreeMap<String, Rule>) {
        self.grants.insert(
            format!("{subject}:{capability}"),
            Grant {
                subject: subject.into(),
                capability: capability.into(),
                rule: if allow { Rule::Allow } else { Rule::Deny },
                expires_at: expires,
                effects,
            },
        );
    }
    /// A friend's invite: exactly `level` on every capability, each grant ending at `expires`.
    pub fn share_grants(&mut self, subject: &str, level: ShareLevel, expires: Option<String>) {
        for capability in CAPABILITIES {
            self.grants.insert(
                format!("{subject}:{capability}"),
                Grant {
                    subject: subject.into(),
                    capability: capability.into(),
                    rule: level.rule(capability),
                    expires_at: expires.clone(),
                    effects: BTreeMap::new(),
                },
            );
        }
    }
    pub fn transport(&self, now: i64) -> Value {
        let peers: BTreeMap<_, _> = self
            .pairings
            .keys()
            .map(|s| {
                let agents = self
                    .identities
                    .keys()
                    .filter(|i| self.computer(i) == s)
                    .any(|i| self.rule(i, "agents", now) != Rule::Deny);
                let operator = ["watch", "files", "control", "administer"]
                    .iter()
                    .any(|c| self.rule(s, c, now) != Rule::Deny);
                (s.clone(), json!({"agent":agents,"operator":operator}))
            })
            .collect();
        json!({"peers":peers})
    }
}
pub fn valid_subject(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.split('@').count() <= 2
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@_.-".contains(&b))
}

/// `codex@vesper`: the agent's name at the principal's computer. The name is
/// its MCP client's, kept to plain characters, and short for the agents
/// people know by a product name ([`short_name`]).
pub(crate) fn agent_label(client_name: &str, principal: &str) -> String {
    let name: String = client_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(32)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("{}@{principal}", if name.is_empty() { "agent" } else { short_name(&name) })
}

/// The product name for a well-known MCP client's name: Codex sends
/// `codex-mcp-client`, Claude Code `claude-code`, Claude Desktop `claude-ai`
/// and Gemini CLI `gemini-cli-mcp-client`. Any other name stays as it is.
fn short_name(name: &str) -> &str {
    match name {
        "codex-mcp-client" => "codex",
        "claude-code" | "claude-ai" => "claude",
        "gemini-cli-mcp-client" => "gemini",
        other => other,
    }
}

/// An agent's subject under its short name (`codex@vesper` for
/// `codex-mcp-client@vesper`); none when it already has it.
fn renamed(subject: &str) -> Option<String> {
    let (name, computer) = subject.split_once('@')?;
    let short = short_name(name);
    (short != name).then(|| format!("{short}@{computer}"))
}

/// Move what is kept under an agent's long name (`codex-mcp-client@vesper`,
/// from before agents had short names, or written by an older ibara after
/// going back to it) to its short name: its identity, its own permissions and
/// the tasks it owns. Runs whenever the journal opens; with nothing under a
/// long name it changes nothing. Where the short name already has a
/// permission of the same id, the moved one keeps its own id: an agent's
/// permissions all apply, the strictest winning, so moving never widens what
/// it may do. As after any access change, the revision moves on and
/// approvals not yet used end.
pub(crate) fn rename_agents(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    // Unreadable records stay as they are, refused where access is read.
    if let Ok(Some(mut a)) = Access::read(tx) {
        let old: Vec<String> = a.identities.keys().filter(|s| renamed(s).is_some()).cloned().collect();
        let mut changed = !old.is_empty();
        for subject in old {
            if let (Some(identity), Some(short)) = (a.identities.remove(&subject), renamed(&subject)) {
                a.identities.entry(short).or_insert(identity);
            }
        }
        let ids: Vec<String> = a.grants.iter().filter(|(_, g)| renamed(&g.subject).is_some()).map(|(id, _)| id.clone()).collect();
        changed |= !ids.is_empty();
        for id in ids {
            let Some(mut grant) = a.grants.remove(&id) else { continue };
            let Some(short) = renamed(&grant.subject) else { continue };
            let moved = id
                .strip_prefix(grant.subject.as_str())
                .and_then(|rest| rest.strip_prefix(':'))
                .map(|capability| format!("{short}:{capability}"))
                .filter(|moved| !a.grants.contains_key(moved))
                .unwrap_or(id);
            grant.subject = short;
            a.grants.insert(moved, grant);
        }
        if changed {
            a.revision += 1;
            a.write_in(tx, "ibarad", "Well-known agents now go by short names, such as codex; their permissions carried over", true, None)?;
        }
    }
    let owned: Vec<(String, String)> = tx
        .prepare("SELECT task_ref, client_flags FROM tasks WHERE client_flags LIKE '%@%'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (task_ref, flags) in owned {
        let Ok(mut flags) = serde_json::from_str::<Value>(&flags) else { continue };
        let Some(short) = flags["agent"].as_str().and_then(renamed) else { continue };
        flags["agent"] = json!(short);
        tx.execute("UPDATE tasks SET client_flags = ?1 WHERE task_ref = ?2", rusqlite::params![flags.to_string(), task_ref])?;
    }
    Ok(())
}
