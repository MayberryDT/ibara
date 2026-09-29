//! Access enforcement, migration and attention on the existing controller.
use super::{Controller, OperatorGrant, Rule};
use crate::access::{ASKED, Access, CAPABILITIES, Identity, PairRights, Pairing};
use crate::error::{Result, denied, invalid};
use crate::store::{AttentionItem, NewAttention};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// How long a watch answer lasts after the last watch request.
const SITTING_IDLE_MS: i64 = 5 * 60_000;

/// A person's answer to "X asks to watch this computer" when watch is Ask First.
/// Watching is a stream of pictures and reads, so the answer holds for the
/// sitting, not one request: until the computer has made no watch request for
/// [`SITTING_IDLE_MS`], access changes (any edit, a Deny included) or ibara
/// restarts.
#[derive(Debug, Clone)]
pub(crate) struct Sitting {
    revision: u64,
    approved: bool,
    last_ms: i64,
}

/// Where an Ask stands.
enum Asked {
    Pending(Value),
    Approved,
    Declined,
}

impl Controller {
    /// Import once, before accepting connections. Never import revoked legacy
    /// state over an existing model. No credential bytes enter the journal.
    pub fn init_access(&self, paths: &crate::server::Paths, policy: &Value) -> Result<()> {
        if Access::load(&self.journal)?.is_some() {
            return Ok(());
        }
        let fingerprints_path = std::env::var_os("IBARA_ACCESS_FINGERPRINTS")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "/etc/ibara-operator/fingerprints.json".into());
        let keys: Value =
            serde_json::from_slice(&std::fs::read(fingerprints_path)?).map_err(|_| invalid("Invalid fingerprint inventory."))?;
        let raw = (self.operator_grants)();
        let mut access = Access::empty();
        let owner_key = std::fs::read_to_string(&paths.admin_hash)?.trim().to_string();
        access.identities.insert(
            "owner".into(),
            Identity {
                kind: "person".into(),
                computer: "owner".into(),
                key: format!("admin-sha256:{owner_key}"),
            },
        );
        for cap in CAPABILITIES {
            access.import_grant("owner", cap, true, None, BTreeMap::new());
        }
        let principals: Vec<&str> = policy["principals"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let effects = crate::access::CLASSES
            .iter()
            .map(|c| (c.to_string(), self.effect_rules.rule(c)))
            .collect::<BTreeMap<_, _>>();
        // A computer that had enrolled Moonlight could take control before.
        let moonlight = super::control::viewer_clients_from_policy(policy);
        let fingerprints = keys["fingerprints"]
            .as_object()
            .ok_or_else(|| invalid("Missing verified transport fingerprints."))?;
        for (principal, key) in fingerprints {
            if !crate::server::authority::valid_principal(principal) {
                return Err(invalid("Invalid paired computer."));
            }
            let key = key
                .as_str()
                .filter(|k| k.starts_with("SHA256:"))
                .ok_or_else(|| invalid("Invalid paired key."))?
                .to_string();
            let record = raw.get(principal);
            let grant = record.and_then(OperatorGrant::from_value);
            let active = grant.as_ref().is_some_and(|g| g.active(self.now_ms()));
            let agent = principals.contains(&principal.as_str());
            access.identities.insert(
                principal.clone(),
                Identity {
                    kind: "computer".into(),
                    computer: principal.clone(),
                    key: key.clone(),
                },
            );
            access.pairings.insert(
                principal.clone(),
                Pairing {
                    key,
                    endpoint: record.and_then(|r| r["operator_endpoint_id"].as_str()).map(str::to_string),
                    active: active || agent,
                    generation: grant.as_ref().and_then(|g| g.generation.as_u64()).unwrap_or(1),
                },
            );
            let expires = grant.as_ref().and_then(|g| g.expires_at.clone());
            access.import_grant(
                principal,
                "watch",
                active && grant.as_ref().is_some_and(|g| g.observe),
                expires.clone(),
                BTreeMap::new(),
            );
            access.import_grant(
                principal,
                "files",
                active && grant.as_ref().is_some_and(|g| g.files),
                expires.clone(),
                BTreeMap::new(),
            );
            access.import_grant(
                principal,
                "control",
                active && moonlight.contains_key(principal),
                expires.clone(),
                BTreeMap::new(),
            );
            access.import_grant(principal, "agents", agent, None, effects.clone());
            access.import_grant(principal, "administer", false, None, BTreeMap::new());
        }
        if principals.iter().any(|p| !access.pairings.contains_key(*p)) {
            return Err(denied(
                "An agent key is missing from the verified fingerprint inventory; import refused.",
            ));
        }
        access.save(
            &self.journal,
            "owner",
            "Imported existing verified access without increasing rights",
        )
    }
    /// Called only after the server has verified and consumed both pairing proofs,
    /// a person (or, for the same owner, this computer) accepted the pairing, or
    /// the computer brought a valid invite. A computer of this computer's own
    /// owner may also take control, run agent tasks and administer (power,
    /// settings, theme, approvals): all of that person's computers are peers. A
    /// friend's invite gives exactly its level until it ends.
    pub async fn access_pair(&self, principal: &str, binding: &Value, generation: u64, rights: &PairRights) -> Result<()> {
        let Some(mut a) = Access::load(&self.journal)? else { return Ok(()) };
        if !crate::server::authority::valid_principal(principal) {
            return Err(invalid("Invalid paired identity."));
        }
        let key = binding["operator_key_fingerprint"]
            .as_str()
            .ok_or_else(|| invalid("Missing verified key."))?
            .to_string();
        if let Some(p) = a.pairings.get(principal).filter(|p| p.active) {
            if p.key == key && p.generation == generation && p.endpoint.as_deref() == binding["operator_endpoint_id"].as_str() {
                return Ok(());
            }
            return Err(denied("Remove the existing pairing before replacing its key."));
        }
        a.identities.retain(|name, i| name != principal && i.computer != principal);
        a.grants
            .retain(|_, g| g.subject != principal && !g.subject.ends_with(&format!("@{principal}")));
        a.identities.insert(
            principal.into(),
            Identity {
                kind: "computer".into(),
                computer: principal.into(),
                key: key.clone(),
            },
        );
        a.pairings.insert(
            principal.into(),
            Pairing {
                key,
                endpoint: binding["operator_endpoint_id"].as_str().map(str::to_string),
                active: true,
                generation,
            },
        );
        let summary = match rights {
            PairRights::Reviewed { observe, files } => {
                a.import_grant(principal, "watch", *observe, None, BTreeMap::new());
                a.import_grant(principal, "files", *files, None, BTreeMap::new());
                a.import_grant(principal, "control", false, None, BTreeMap::new());
                // Pairing does not grant control, administration or agent tasks. Grant explicitly.
                "Confirmed a mutually verified computer pairing"
            }
            PairRights::OwnComputer => {
                a.import_grant(principal, "watch", true, None, BTreeMap::new());
                a.import_grant(principal, "files", true, None, BTreeMap::new());
                a.import_grant(principal, "control", true, None, BTreeMap::new());
                let effects = crate::access::CLASSES.iter().map(|c| (c.to_string(), self.effect_rules.rule(c))).collect();
                a.import_grant(principal, "agents", true, None, effects);
                a.import_grant(principal, "administer", true, None, BTreeMap::new());
                "Confirmed a mutually verified computer pairing"
            }
            PairRights::Invite { level, expires_at, summary } => {
                a.share_grants(principal, *level, expires_at.clone());
                summary.as_str()
            }
        };
        a.save(&self.journal, "owner", summary)
        // The server saves its consumed proof before the next reconciliation projects transport.
    }
    /// The owner's own computer paired again with a key it already had: a
    /// pairing made before own computers could administer gains that right,
    /// unless someone set administer for it explicitly since. The caller
    /// projects access afterwards, as after any pairing.
    pub fn access_own_computer(&self, principal: &str) -> Result<()> {
        let Some(mut a) = Access::load(&self.journal)? else { return Ok(()) };
        if !a.pairings.get(principal).is_some_and(|p| p.active) || a.grants.contains_key(&format!("{principal}:administer")) {
            return Ok(());
        }
        a.import_grant(principal, "administer", true, None, BTreeMap::new());
        a.save(&self.journal, "owner", "Let the owner's own computer administer this one")
    }
    pub async fn access_unpair(&self, principal: &str) -> Result<()> {
        if let Some(a) = Access::load(&self.journal)?
            && a.pairings.contains_key(principal)
        {
            self.change_access_authorized(
                "owner",
                &json!({"op":"access_unpair","subject":principal,"expected_revision":a.revision}),
            )
            .await?;
        }
        Ok(())
    }
    pub fn access_principal(&self, principal: &str) -> Result<bool> {
        Ok(Access::load(&self.journal)?.is_none_or(|a| a.paired(principal)))
    }
    /// The key fingerprint of `principal`'s active pairing.
    pub fn access_pairing_key(&self, principal: &str) -> Option<String> {
        Access::load(&self.journal).ok().flatten()?.pairings.get(principal).filter(|p| p.active).map(|p| p.key.clone())
    }
    pub(crate) fn access_operator_grant(&self, principal: &str) -> Option<OperatorGrant> {
        match Access::load(&self.journal) {
            Ok(Some(a)) => a.pairings.get(principal).map(|p| OperatorGrant {
                enabled: p.active,
                expires_at: None,
                generation: json!(p.generation),
                observe: match a.rule(principal, "watch", self.now_ms()) {
                    Rule::Allow => true,
                    Rule::Ask => self.sitting(principal, &a) == Some(true),
                    Rule::Deny => false,
                },
                files: a.rule(principal, "files", self.now_ms()) == Rule::Allow,
            }),
            Ok(None) => (self.operator_grants)().get(principal).and_then(OperatorGrant::from_value),
            Err(_) => None,
        }
    }
    pub(crate) fn task_subject(&self, task_ref: &str, principal: &str) -> String {
        self.journal
            .get_task(task_ref)
            .ok()
            .flatten()
            .and_then(|t| t.client_flags["agent"].as_str().map(str::to_string))
            .unwrap_or_else(|| principal.into())
    }
    pub(crate) fn effect_rule(&self, task_ref: &str, principal: &str, class: &str) -> Rule {
        match Access::load(&self.journal) {
            Ok(Some(a)) => a.effect(&self.task_subject(task_ref, principal), class, self.now_ms(), (self.ask_first)()),
            Ok(None) => match self.effect_rules.rule(class) {
                Rule::Ask if ASKED.contains(&class) && !(self.ask_first)() => Rule::Allow,
                rule => rule,
            },
            Err(_) => Rule::Deny,
        }
    }
    /// The access table as `actor` may see it ([`Access::view`]). For someone
    /// who may administer, a computer paired over Tailscale also says whose it
    /// is (`owner`, `computer_name`) when it is someone else's, and the level
    /// of the invite it joined with (`invite`).
    pub(crate) fn access_view(&self, actor: &str) -> Result<Value> {
        let mut view = Access::load(&self.journal)?
            .map(|a| a.view(actor, self.now_ms(), (self.ask_first)()))
            .ok_or_else(|| invalid("Access import has not run."))?;
        if view["can_administer"] != Value::Bool(true) {
            return Ok(view);
        }
        let records = (self.operator_grants)();
        for row in view["rows"].as_array_mut().into_iter().flatten() {
            let Some(record) = row["subject"].as_str().and_then(|s| records.get(s)) else { continue };
            if record.get("own_computer") == Some(&Value::Bool(false)) {
                for (key, pointer) in [("owner", "/tailscale/login"), ("computer_name", "/tailscale/host_name")] {
                    if let Some(value) = record.pointer(pointer).filter(|v| v.is_string()) {
                        row[key] = value.clone();
                    }
                }
            }
            if let Some(level) = record.pointer("/invite/level").filter(|v| v.is_string()) {
                row["invite"] = level.clone();
            }
        }
        Ok(view)
    }
    pub(crate) fn require_access(&self, subject: &str, cap: &str) -> Result<()> {
        if let Some(a) = Access::load(&self.journal)?
            && a.rule(subject, cap, self.now_ms()) == Rule::Deny
        {
            return Err(denied(format!("{subject} is denied {cap} on this computer.")));
        }
        Ok(())
    }
    /// Ask is exact-request, current-control and current-access bound. A result
    /// is pending until a person with administer answers. Consume before dispatch.
    pub(crate) fn access_gate(&self, subject: &str, cap: &str, action: &Value) -> Result<Option<Value>> {
        let Some(a) = Access::load(&self.journal)? else { return Ok(None) };
        self.access_gate_rule(subject, cap, action, a.rule(subject, cap, self.now_ms()))
    }
    pub(crate) fn access_gate_rule(&self, subject: &str, cap: &str, action: &Value, rule: Rule) -> Result<Option<Value>> {
        let Some(a) = Access::load(&self.journal)? else { return Ok(None) };
        match rule {
            Rule::Allow => return Ok(None),
            Rule::Deny => return Err(denied(format!("{subject} is denied {cap} on this computer."))),
            Rule::Ask => {}
        }
        let lease = self.journal.get_active_lease()?;
        let generation = format!(
            "{}:{}:{}:{}",
            self.epoch,
            a.revision,
            self.viewer_revision_name()?,
            lease.as_ref().map(|l| l.generation.as_str()).unwrap_or("idle")
        );
        match self.ask(subject, cap, action, &generation)? {
            Asked::Pending(pending) => Ok(Some(pending)),
            Asked::Approved => Ok(None),
            Asked::Declined => Err(denied("A person declined this access request.")),
        }
    }
    /// The approval item for `action` under `generation`: raised once, and an
    /// answer is consumed by the request that finds it.
    fn ask(&self, subject: &str, cap: &str, action: &Value, generation: &str) -> Result<Asked> {
        let fingerprint = crate::server::policy::sha256_hex(
            json!({"subject":subject,"capability":cap,"action":action,"control":generation})
                .to_string()
                .as_bytes(),
        );
        let operation = format!("access_{fingerprint}");
        let task_ref = action["task_ref"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("access:{subject}"));
        let items = self.journal.list_attention(None, Some(&task_ref), 500)?;
        if let Some(item) = items.into_iter().find(|i| {
            i.kind == "approval"
                && i.operation_ref.as_deref() == Some(&operation)
                && i.generation.as_deref() == Some(generation)
                && i.state != "expired"
        }) {
            if item.state == "answered" {
                self.journal
                    .db()
                    .execute("UPDATE attention_items SET state='expired' WHERE att_ref=?1", [&item.att_ref])?;
                return Ok(if item.answer.as_deref() == Some("approve") { Asked::Approved } else { Asked::Declined });
            }
            return Ok(Asked::Pending(json!({"state":"pending_approval","attention":item.att_ref,"held":item.question})));
        }
        if action.to_string().len() > 3000 {
            return Err(invalid("An ask request is too large to review. Reduce its size."));
        }
        let question = self.access_words(subject, cap, action);
        let details = json!({ "capability": cap, "request": action });
        let item = self.journal.raise_attention(NewAttention {
            task_ref: &task_ref,
            principal: subject,
            kind: "approval",
            operation_ref: Some(&operation),
            generation: Some(generation),
            question: &question,
            details: Some(&details),
            options: &["approve".into(), "deny".into()],
            now_iso: &self.now_iso(),
        })?;
        Ok(Asked::Pending(json!({"state":"pending_approval","attention":item.att_ref,"held":question})))
    }
    /// The gate for a watch request from `subject` (a picture, task following,
    /// or a read such as health or history). With Ask First, one approval
    /// covers the sitting (see [`Sitting`]): the first request raises "X asks
    /// to watch this computer", later ones wait on that same item, and the
    /// answer then holds while `subject` keeps watching.
    pub(crate) fn watch_gate(&self, subject: &str) -> Result<Option<Value>> {
        let Some(a) = Access::load(&self.journal)? else { return Ok(None) };
        let rule = a.rule(subject, "watch", self.now_ms());
        if rule != Rule::Ask {
            return self.access_gate_rule(subject, "watch", &json!({"op": "observe"}), rule);
        }
        let now = self.now_ms();
        let approved = match self.sitting(subject, &a) {
            Some(approved) => {
                if let Some(sitting) = self.watch_sittings.borrow_mut().get_mut(subject) {
                    sitting.last_ms = now;
                }
                approved
            }
            // Bound to this start and these access rules only: an agent task
            // starting while the person reads the request does not replace it.
            None => {
                let generation = format!("{}:{}:watch", self.epoch, a.revision);
                let approved = match self.ask(subject, "watch", &json!({"op": "watch"}), &generation)? {
                    Asked::Pending(pending) => return Ok(Some(pending)),
                    Asked::Approved => true,
                    Asked::Declined => false,
                };
                self.watch_sittings.borrow_mut().insert(subject.to_string(), Sitting { revision: a.revision, approved, last_ms: now });
                approved
            }
        };
        if approved { Ok(None) } else { Err(denied("A person declined this access request.")) }
    }
    /// The answer `subject`'s current watch sitting holds, if one does.
    pub(crate) fn sitting(&self, subject: &str, a: &Access) -> Option<bool> {
        let now = self.now_ms();
        let mut sittings = self.watch_sittings.borrow_mut();
        sittings.retain(|_, s| s.revision == a.revision && now - s.last_ms < SITTING_IDLE_MS);
        sittings.get(subject).map(|s| s.approved)
    }
    pub(crate) async fn change_access(&self, actor: &str, action: &Value) -> Result<Value> {
        if let Some(pending) = self.access_gate(actor, "administer", action)? {
            return Ok(pending);
        }
        self.change_access_authorized(actor, action).await
    }
    pub(crate) async fn change_access_authorized(&self, actor: &str, action: &Value) -> Result<Value> {
        let mut a = Access::load(&self.journal)?.ok_or_else(|| invalid("Access import has not run."))?;
        if action["expected_revision"].as_u64() != Some(a.revision) {
            return Err(denied("Access changed; reload the table before editing."));
        }
        match action["op"].as_str().unwrap_or("") {
            "access_set" => a.put(action)?,
            "access_remove" => {
                if let Some(id) = action["grant_id"].as_str() {
                    let grant = a.grants.get(id).ok_or_else(|| invalid("Unknown grant."))?;
                    if grant.subject == "owner" {
                        return Err(denied("Keep the local owner for recovery."));
                    }
                    a.grants.remove(id);
                } else {
                    let subject = action["subject"].as_str().unwrap_or("");
                    if subject == "owner" || !a.identities.contains_key(subject) {
                        return Err(invalid("Unknown or protected identity."));
                    }
                    a.grants
                        .retain(|_, g| g.subject != subject && !g.subject.ends_with(&format!("@{subject}")));
                }
            }
            "access_unpair" => {
                let subject = action["subject"].as_str().unwrap_or("");
                let p = a.pairings.get_mut(subject).ok_or_else(|| invalid("Unknown pairing."))?;
                p.active = false;
                p.generation += 1;
                a.grants
                    .retain(|_, g| g.subject != subject && !g.subject.ends_with(&format!("@{subject}")));
            }
            _ => return Err(invalid("Unknown access change.")),
        }
        a.save(&self.journal, actor, &access_summary(actor, action))?;
        self.apply_saved_access().await?;
        Ok(a.view(actor, self.now_ms(), (self.ask_first)()))
    }
    /// An agent's request that ibara stop asking a person before it sends,
    /// spends or deletes here. It raises one approval, `subject`'s request to
    /// stop asking, and changes nothing until a person allows it
    /// ([`Controller::answer_approval`]); asking again while it waits raises
    /// nothing new. It asks only for kinds its own rule could change: not
    /// one its computer's own rule asks first for ([`Access::computer_asks`]).
    /// `None` when there is nothing to ask for: each of those kinds of step
    /// already runs without asking, is denied, or asks by its computer's rule.
    pub(crate) fn ask_to_stop_asking(&self, subject: &str) -> Result<Option<String>> {
        let a = Access::load(&self.journal)?.ok_or_else(|| invalid("Access import has not run."))?;
        let now = self.now_ms();
        let kinds: Vec<&str> = ASKED
            .into_iter()
            .filter(|k| a.effect(subject, k, now, (self.ask_first)()) == Rule::Ask && !a.computer_asks(subject, k, now))
            .collect();
        if kinds.is_empty() {
            return Ok(None);
        }
        let task_ref = format!("access:{subject}");
        let waiting = self.journal.list_attention(Some("open"), Some(&task_ref), 100)?;
        if let Some(item) = waiting.into_iter().find(|i| i.details.pointer("/request/op").and_then(Value::as_str) == Some("stop_asking")) {
            return Ok(Some(item.att_ref));
        }
        let question = format!("{subject} asks to stop asking you before it {} on {}.", kinds_words(&kinds, true), self.computer_name());
        let details = json!({ "agent": subject, "request": { "op": "stop_asking", "kinds": kinds } });
        let item = self.journal.raise_attention(NewAttention {
            task_ref: &task_ref,
            principal: subject,
            kind: "approval",
            operation_ref: None,
            generation: None,
            question: &question,
            details: Some(&details),
            options: &["approve".into(), "deny".into()],
            now_iso: &self.now_iso(),
        })?;
        Ok(Some(item.att_ref))
    }
    /// A person's answer, from `actor`, to the approval `att_ref`. Two answers
    /// also change access: `always` (Always Allow) approves an agent's send,
    /// spend or delete step and lets that agent do that kind of step here
    /// without asking from now on; `approve` on an agent's request to stop
    /// asking lets it send, spend and delete here without asking. Each
    /// changes only that agent's own rules on this computer, is saved with
    /// the answer (so an answer that is refused changes nothing), and says
    /// what it changed (`allowed`). Always Allow is refused, and the approval
    /// stays open, where the agent's computer's own rule asks first.
    pub(crate) async fn answer_approval(&self, att_ref: &str, answer: &str, actor: &str) -> Result<(AttentionItem, Option<Value>)> {
        let item = self.journal.get_attention(att_ref)?.ok_or_else(|| invalid("Unknown attention item."))?;
        let approval = item.kind == "approval";
        let stop_asking = approval && item.details.pointer("/request/op").and_then(Value::as_str) == Some("stop_asking");
        let step = item.details["effect"].as_str().and_then(|c| ASKED.into_iter().find(|k| *k == c)).filter(|_| approval && !stop_asking);
        if answer == "always" && step.is_none() {
            return Err(invalid("Always Allow is only for an agent's step that sends, spends or deletes."));
        }
        let kinds: Vec<&'static str> = match (answer, step) {
            _ if item.state != "open" => Vec::new(),
            ("always", Some(class)) => vec![class],
            ("approve", _) if stop_asking => {
                let asked = item.details.pointer("/request/kinds").and_then(Value::as_array).cloned().unwrap_or_default();
                ASKED.into_iter().filter(|k| asked.iter().any(|a| a == k)).collect()
            }
            _ => Vec::new(),
        };
        let recorded = if answer == "always" { "approve" } else { answer };
        let now = self.now_iso();
        if kinds.is_empty() {
            return Ok((self.journal.answer_attention(att_ref, recorded, actor, &now)?, None));
        }
        let agent = item.details["agent"].as_str().filter(|a| crate::access::valid_subject(a)).ok_or_else(|| invalid("This approval does not name its agent."))?;
        let mut a = Access::load(&self.journal)?.ok_or_else(|| invalid("Access import has not run."))?;
        if let Some(class) = step.filter(|c| answer == "always" && a.computer_asks(agent, c, self.now_ms())) {
            let computer = a.computer(agent);
            let name = agent.split('@').next().unwrap_or(agent);
            let verb = kinds_words(&[class], false);
            return Err(invalid(format!(
                "{computer}'s agents ask before they {verb} here, and any agent on {computer} can use the name {name}. Approve this step, or change {computer}'s rule in Access."
            )));
        }
        let changed = a.allow_without_asking(agent, &kinds, self.now_ms())?;
        let allowed = Some(json!({ "agent": agent, "kinds": kinds }));
        if changed.is_empty() {
            return Ok((self.journal.answer_attention(att_ref, recorded, actor, &now)?, allowed));
        }
        let who = if actor == "owner" { "The owner" } else { actor };
        let summary = format!("{who} let {agent} {} on this computer without asking first.", kinds_words(&changed, false));
        let item = a.save_answering(&self.journal, actor, &summary, att_ref, recorded, &now)?;
        // Only a loosened agent rule: nothing to close, and a failed
        // transport cleanup is retried rather than failing the answer.
        self.project_access_background().await;
        Ok((item, allowed))
    }
    /// After saved access changes (edits and `access_sync`): close what it now
    /// denies, then project transport. Both run; a failure of either means the
    /// authority is saved and enforced, and only cleanup needs recovery.
    pub(crate) async fn apply_saved_access(&self) -> Result<()> {
        let settled = self.settle_access().await;
        let projected = self.project_access().await;
        settled.map_err(|e| e.with("access_saved", true))?;
        projected
    }
    /// Deny is applied before projection. It also closes already-open sessions.
    pub(crate) async fn settle_access(&self) -> Result<()> {
        let Some(a) = Access::load(&self.journal)? else { return Ok(()) };
        if let Some(l) = self.journal.get_active_lease()? {
            let subject = self.task_subject(&l.task_ref, &l.principal);
            if a.rule(&subject, "agents", self.now_ms()) == Rule::Deny {
                self.release_lease(Some(l), super::control::Release::AuthorityRevoked, false)
                    .await?;
            }
        }
        let owner = self.viewer_state.borrow().owner.clone();
        if let Some(owner) = owner
            && a.rule(&owner, "control", self.now_ms()) == Rule::Deny
        {
            self.revoke_viewer_operator(&owner).await?;
        }
        Ok(())
    }
    /// Explicit changes, pairing and `access_sync`: always attempt, report failure.
    pub(crate) async fn project_access(&self) -> Result<()> {
        self.project_access_with(true).await
    }
    /// Periodic and per-call reconciliation. Enforcement is in-process, so a
    /// transport cleanup failure must not block agent calls or lease expiry; an
    /// identical failed projection is retried at most once a minute.
    pub(crate) async fn project_access_background(&self) {
        if let Err(e) = self.project_access_with(false).await {
            super::log_event("access_projection_failed", &e.to_string());
        }
    }
    async fn project_access_with(&self, force: bool) -> Result<()> {
        let Some(a) = Access::load(&self.journal)? else { return Ok(()) };
        let mut desired = a.transport(self.now_ms());
        // A pending, epoch-bound pairing needs only the restricted confirmation
        // transport. It grants no desktop/files/agent authority. Restart expires it.
        let grants = (self.operator_grants)();
        for (principal, record) in &grants {
            let p = &record["pending"];
            if p["controller_epoch"].as_str() == Some(&self.epoch)
                && p["expires_at"]
                    .as_str()
                    .and_then(crate::ids::millis_from_iso)
                    .is_some_and(|t| t > self.now_ms())
            {
                desired["peers"][principal] = json!({"operator":true,"agent":false});
            }
        }
        // A computer paired over the tailnet brought its own public key; root
        // enrols it for that computer's restricted account and agent entry. An
        // ended pairing sends no key: the same computer may be paired again
        // under another name with the same key.
        let keys: serde_json::Map<String, Value> = grants
            .iter()
            .filter(|(principal, _)| desired["peers"].get(principal.as_str()).is_some())
            .filter(|(principal, _)| a.pairings.get(principal.as_str()).is_some_and(|p| p.active))
            .filter_map(|(principal, record)| Some((principal.clone(), json!(record.get("operator_public_key")?.as_str()?))))
            .collect();
        if !keys.is_empty() {
            desired["keys"] = Value::Object(keys);
        }
        // A successful identical projection needs no process or socket activity.
        let key = desired.to_string();
        if self.access_projection.borrow().as_deref() == Some(&key) {
            return Ok(());
        }
        if !force
            && self
                .access_projection_failed
                .borrow()
                .as_ref()
                .is_some_and(|(k, at)| *k == key && self.now_ms() - at < 60_000)
        {
            return Ok(());
        }
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let mut socket = tokio::net::UnixStream::connect(&self.access_socket).await?;
            socket.write_all(format!("{desired}\n").as_bytes()).await?;
            let mut line = String::new();
            tokio::io::BufReader::new(socket).read_line(&mut line).await?;
            Ok::<_, std::io::Error>(line)
        })
        .await;
        let value = result
            .ok()
            .and_then(|r| r.ok())
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if value.as_ref().and_then(|v| v["ok"].as_bool()) != Some(true) {
            // The helper applies peers one at a time, so a failure can leave some
            // changed: the last success no longer describes the transport.
            *self.access_projection.borrow_mut() = None;
            *self.access_projection_failed.borrow_mut() = Some((key, self.now_ms()));
            return Err(crate::error::unavailable(
                "Access is saved and enforced, but generated transport cleanup failed. Run access_sync after recovery.",
            )
            .with("access_saved", true));
        }
        *self.access_projection_failed.borrow_mut() = None;
        *self.access_projection.borrow_mut() = Some(key);
        Ok(())
    }
}

/// One plain sentence for the timeline about an access change.
fn access_summary(actor: &str, action: &Value) -> String {
    let who = if actor == "owner" { "The owner".to_string() } else { actor.to_string() };
    let subject = action["subject"].as_str().unwrap_or("a computer");
    let doing = match action["capability"].as_str().unwrap_or("") {
        "watch" => "watch this computer",
        "files" => "send and get files",
        "control" => "take control",
        "agents" => "run agents here",
        "administer" => "manage this computer",
        _ => "use this computer",
    };
    match (action["op"].as_str().unwrap_or(""), action["rule"].as_str().unwrap_or("")) {
        ("access_set", "allow") => format!("{who} let {subject} {doing}."),
        ("access_set", "ask") => format!("{who} made {subject} ask first to {doing}."),
        ("access_set", _) => format!("{who} stopped {subject} from being able to {doing}."),
        ("access_remove", _) => format!("{who} removed a permission for {subject}."),
        ("access_unpair", _) => format!("{who} removed {subject} from this computer."),
        _ => format!("{who} changed who may use this computer."),
    }
}

/// Kinds of agent step as a person says them: "send, spend and delete", or
/// with `third` "sends, spends or deletes".
fn kinds_words(kinds: &[&str], third: bool) -> String {
    let words: Vec<&str> = kinds
        .iter()
        .map(|k| match (*k, third) {
            ("send", false) => "send",
            ("send", true) => "sends",
            ("spend", false) => "spend",
            ("spend", true) => "spends",
            (_, false) => "delete",
            (_, true) => "deletes",
        })
        .collect();
    match words.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} {} {last}", rest.join(", "), if third { "or" } else { "and" }),
    }
}
