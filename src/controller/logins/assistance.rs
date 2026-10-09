//! Durable, non-secret assistance. Human intent outlives a desktop lease;
//! old input approvals do not. Credentials never enter this record.
use super::*;
use crate::contract::{LoginAssistanceInput, SignupProposal};
use rusqlite::{Transaction, TransactionBehavior, params};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Signup {
    proposal: SignupProposal,
    state: String,
    #[serde(default)]
    claim_ref: Option<String>,
    #[serde(default)]
    evidence_ref: Option<String>,
    #[serde(default)]
    created: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SignupAttempt {
    principal: String,
    agent: String,
    signup: Signup,
}

impl Book {
    pub(super) fn remember_attempts(&mut self) {
        for request in &self.requests {
            if let Some(signup)=request.signup.as_ref().filter(|s|s.claim_ref.is_some()) {
                if let Some(old)=self.signup_attempts.iter_mut().find(|a|a.signup.claim_ref==signup.claim_ref) {
                    old.signup=signup.clone();
                } else {
                    self.signup_attempts.push(SignupAttempt{principal:request.principal.clone(),agent:request.agent.clone(),signup:signup.clone()});
                }
            }
        }
    }
}

impl Entry {
    pub(super) fn assistance_state(&self) -> Option<&'static str> {
        match self.state.as_str() {
            "deferred" => Some("deferred"),
            "no_account" => Some("no_account"),
            "without_account" => Some("without_account"),
            "person_sign_in" => Some("person_sign_in"),
            "verify_session" => Some("verify_session"),
            "signup_proposed" => Some("signup_proposed"),
            "signup_approved" => Some("signup_approved"),
            "signup_in_progress" => Some("signup_in_progress"),
            "needs_verification" => Some("needs_verification"),
            "needs_reconciliation" => Some("needs_reconciliation"),
            _ => None,
        }
    }
}

impl Controller {
    fn assistance_request(&self, book: &Book, att: &str) -> Result<usize> {
        let item = self.journal.get_attention(att)?.filter(|a| a.kind == "login" && a.state == "open"
            && a.details.pointer("/assistance/version").and_then(Value::as_u64) == Some(1))
            .ok_or_else(|| invalid("This login request is no longer open or does not support assistance. Refresh it."))?;
        let i = book.requests.iter().position(|r| r.att_ref.as_deref() == Some(att))
            .ok_or_else(|| invalid("Unknown login assistance request."))?;
        if self.journal.get_task(&book.requests[i].task_ref)?.is_none_or(|t| matches!(t.state.as_str(), "completed" | "cancelled")) {
            return Err(invalid("The task was completed or cancelled; this request cannot resume it."));
        }
        if item.principal != book.requests[i].principal { return Err(denied("Login request identity changed.")); }
        Ok(i)
    }

    pub(in crate::controller) fn login_assistance_status(&self, att: &str) -> Result<Option<Value>> {
        let book=Book::load(&self.journal)?;
        book.requests.iter().find(|r|r.att_ref.as_deref()==Some(att))
            .map(|r|self.assistance_view(r)).transpose()
    }

    fn assistance_view(&self, request: &Request) -> Result<Value> {
        let mut history=Book::load(&self.journal)?;
        history.remember_attempts();
        let active = self.journal.get_active_lease()?.is_some_and(|l| l.task_ref == request.task_ref);
        let needs_person = request.sites.iter().any(|e| matches!(e.state.as_str(), "asking" | "deferred" | "person_sign_in" | "signup_proposed"));
        let notify = request.sites.iter().any(|e| matches!(e.state.as_str(), "asking" | "signup_proposed"));
        Ok(json!({
            "version":1, "revision":request.revision, "attention":request.att_ref,
            "request_ref":request.request_ref, "task_ref":request.task_ref,
            "notification": if notify {"attention"} else {"quiet"},
            "continuation":if active {"in_task"} else if needs_person {"waiting_for_you"} else {"ready_to_resume"},
            "sites":request.sites.iter().map(|e| json!({"site":e.site,"state":e.state,"via":e.via,
                "next":next_for(&e.state,&e.site,&request.task_ref,request.att_ref.as_deref()),
                "person_response":e.person_response,"responded_at":e.responded_at,
                "account_availability":if e.person_response.as_deref()==Some("no_account") {"reported_none"} else {"unknown"},
                "method":if crate::logins::independent_site(&e.site) {"independent_session"} else {"legacy_copy_available"}
            })).collect::<Vec<_>>(),
            "signup":request.signup,
            "prior_setups":history.signup_attempts.iter().filter(|a|a.principal==request.principal && a.agent==request.agent
                && request.sites.iter().any(|e|e.site==a.signup.proposal.site)).map(|a|&a.signup).collect::<Vec<_>>(),
            "account_creation":if request.signup.as_ref().is_some_and(|s|s.state=="approved") {"approved_proposal_only"} else {"not_authorized"}
        }))
    }

    /// Commit the semantic response and its visible projection together.
    fn save_assistance(&self, book: &mut Book, i: usize, actor: &str) -> Result<Value> {
        let request = &book.requests[i];
        let att = request.att_ref.clone().ok_or_else(|| invalid("This login request has no attention item."))?;
        let item = self.journal.get_attention(&att)?.ok_or_else(|| invalid("Unknown login request."))?;
        let view = self.assistance_view(request)?;
        let mut details = item.details;
        details["assistance"] = view.clone();
        details["sites"] = view["sites"].clone();
        details["task"] = json!({"task_ref":request.task_ref,"goal":request.goal});
        let terminal = request.sites.iter().all(|e| matches!(e.state.as_str(),"resolved"|"declined"|"denied"|"cancelled"));
        let tx = Transaction::new_unchecked(self.journal.db(), TransactionBehavior::Immediate)?;
        self.journal.db().execute("UPDATE attention_items SET details=?1, task_ref=?2, state=?3, answer=?4, answered_by=?5, answered_at=?6 WHERE att_ref=?7 AND state='open'",
            params![details.to_string(),request.task_ref,if terminal {"answered"} else {"open"},
                if terminal {Some("Login assistance settled")} else {None}, if terminal {Some(actor)} else {None},
                if terminal {Some(self.now_iso())} else {None},att])?;
        book.save(&self.journal)?;
        tx.commit()?;
        self.push_event(Some(&book.requests[i].task_ref), &format!("{att} login assistance changed; read computer_status for its current response"));
        self.timeline("login_assistance",Some(&book.requests[i].task_ref),actor,"Login assistance updated",
            json!({"attention":att,"revision":view["revision"],"sites":view["sites"].as_array().into_iter().flatten().map(|e|json!({"site":e["site"],"state":e["state"]})).collect::<Vec<_>>() }));
        Ok(view)
    }

    pub(super) fn login_authorize_once(&self, operator: &str, action: &Value) -> Result<Value> {
        let receiver=Receiver::load(&self.journal)?;
        receiver.check_source(operator)?;
        let mut book=Book::load(&self.journal)?;
        let i=self.assistance_request(&book,action["att_ref"].as_str().unwrap_or(""))?;
        if action["revision"].as_u64()!=Some(book.requests[i].revision) {return Err(invalid("The login request changed; refresh before sharing."));}
        if !self.journal.get_active_lease()?.is_some_and(|l|l.task_ref==book.requests[i].task_ref) {return Err(invalid("Resume the task before delivering a login."));}
        let sites=action["sites"].as_array().filter(|s|!s.is_empty() && s.len()<=20).ok_or_else(||invalid("Name the sites to share once."))?;
        for value in sites {
            let name=value.as_str().ok_or_else(||invalid("Invalid site."))?;
            if crate::logins::independent_site(name) || receiver.rule(name)==Rule::Deny {return Err(denied("This site cannot use cookie sharing."));}
            if !book.requests[i].sites.iter().any(|e|e.site==name && !e.terminal() && !matches!(e.state.as_str(),"signup_in_progress"|"needs_verification"|"needs_reconciliation")) {return Err(invalid("That site is settled or needs setup reconciliation; it cannot be shared."));}
        }
        for value in sites {
            let entry=book.requests[i].sites.iter_mut().find(|e|e.site==value.as_str().unwrap()).unwrap();
            entry.state="deliver_once".into();entry.reason=None;
        }
        book.requests[i].revision+=1; book.requests[i].last_answer=None;
        self.save_assistance(&mut book,i,operator)
    }

    pub(super) fn login_assist(&self, operator: &str, action: &Value) -> Result<Value> {
        // operator_call has already required current unconditional administer.
        let mut book = Book::load(&self.journal)?;
        let att = action["att_ref"].as_str().unwrap_or("");
        let i = self.assistance_request(&book,att)?;
        let revision = action["revision"].as_u64().ok_or_else(|| invalid("Give the request revision."))?;
        let choices = action["decisions"].as_object().filter(|d| !d.is_empty() && d.len()<=20)
            .ok_or_else(||invalid("Give the selected sites and their assistance choices."))?;
        let fingerprint = json!({"revision":revision,"decisions":choices});
        if book.requests[i].last_answer.as_ref()==Some(&fingerprint) { return self.assistance_view(&book.requests[i]); }
        if revision != book.requests[i].revision { return Err(invalid("The login request changed. Refresh before answering.")); }
        // Validate the whole answer before editing any site.
        for (name,choice) in choices {
            if !book.requests[i].sites.iter().any(|e| e.site==*name && !e.terminal()) {return Err(invalid("That site is settled or outside this request."));}
            let choice = choice.as_str().unwrap_or("");
            if !matches!(choice,"defer"|"no_account"|"without_account"|"person_sign_in"|"signed_in"|"decline"|"cancel"|"approve_signup") {
                return Err(invalid("Choose defer, no_account, without_account, person_sign_in, signed_in, decline, cancel or approve_signup."));
            }
            if choice=="approve_signup" && book.requests[i].signup.as_ref().is_none_or(|s| s.proposal.site!=*name || s.state!="proposed" || s.proposal.cost!="free") {
                return Err(invalid("Only the displayed, unchanged free signup proposal may be approved."));
            }
            if !matches!(choice,"defer"|"cancel") && book.requests[i].signup.as_ref().is_some_and(|s| s.proposal.site==*name && matches!(s.state.as_str(),"in_progress"|"needs_verification"|"cancelled_needs_reconciliation")) {
                return Err(invalid("Setup may already have created an account. Reconcile it before changing this choice."));
            }
        }
        for (name,choice) in choices {
            let state = match choice.as_str().unwrap() {
                "defer"=>"deferred", "signed_in"=>"verify_session", "cancel"=>"cancelled",
                "approve_signup"=>"signup_approved", "decline"=>"declined", other=>other,
            };
            let request = &mut book.requests[i];
            let entry = request.sites.iter_mut().find(|e|e.site==*name).unwrap();
            entry.state=state.into(); entry.reason=None;
            entry.person_response=choice.as_str().map(str::to_string);
            entry.responded_at=Some(self.now_iso());
            if let Some(signup)=request.signup.as_mut().filter(|s|s.proposal.site==*name) {
                if state=="signup_approved" {signup.state="approved".into();}
                else if state=="cancelled" {
                    if signup.claim_ref.is_some() {
                        signup.created |= matches!(signup.state.as_str(),"needs_verification"|"complete");
                        signup.state="cancelled_needs_reconciliation".into();
                        entry.state="needs_reconciliation".into();
                    } else {signup.state="cancelled".into();}
                }
                else if !matches!(state,"deferred") {signup.state="superseded".into();}
            }
        }
        book.requests[i].revision+=1;
        book.requests[i].last_answer=Some(fingerprint);
        self.save_assistance(&mut book,i,operator)
    }

    pub(in crate::controller) fn login_checkpoint(&self, task: &TaskRecord, input: &LoginAssistanceInput) -> Result<Value> {
        let mut book=Book::load(&self.journal)?;
        book.remember_attempts();
        let i=self.assistance_request(&book,input.attention.as_str())?;
        let request=&book.requests[i];
        if request.principal!=task.principal || request.agent!=self.task_subject(&task.task_ref,&task.principal) {
            return Err(denied("This login assistance belongs to a different agent identity."));
        }
        if input.action=="continue" {
            if request.task_ref==task.task_ref {return self.assistance_view(request);}
            if request.task_ref!=task.task_ref && self.journal.get_active_lease()?.is_some_and(|l|l.task_ref==request.task_ref) {
                return Err(denied("The original task still holds this computer."));
            }
            book.requests[i].task_ref=task.task_ref.clone();
            // Preserve the original goal; continuation does not widen its authority.
        } else {
            if request.task_ref!=task.task_ref {return Err(invalid("First continue this assistance from the new task."));}
            match input.action.as_str() {
                "recover_signup"=> {
                    if request.signup.as_ref().is_some_and(|s|s.claim_ref.is_some() && s.state!="not_created") {
                        return Err(invalid("Reconcile the setup already attached to this request first."));
                    }
                    let prior=book.signup_attempts.iter().find(|a|a.principal==task.principal && a.agent==request.agent
                        && a.signup.claim_ref==input.claim_ref && input.claim_ref.is_some()
                        && input.site.as_deref()==Some(a.signup.proposal.site.as_str()))
                        .ok_or_else(||invalid("Name a prior setup claim belonging to this agent and identity."))?;
                    if !request.sites.iter().any(|e|Some(e.site.as_str())==input.site.as_deref() && !e.terminal()) {
                        return Err(invalid("Recovery needs an unsettled request for the same site."));
                    }
                    let mut signup=prior.signup.clone();
                    signup.state=if matches!(signup.state.as_str(),"complete"|"needs_verification") {"needs_verification"} else {"cancelled_needs_reconciliation"}.into();
                    let request=&mut book.requests[i];
                    request.sites.iter_mut().find(|e|e.site==signup.proposal.site).unwrap().state="needs_reconciliation".into();
                    request.signup=Some(signup);
                }
                "propose_signup"=> {
                    let proposal=input.proposal.clone().ok_or_else(||invalid("Give the exact signup proposal."))?;
                    if site(&proposal.site)?!=proposal.site || !request.sites.iter().any(|e| e.site==proposal.site && !e.terminal()) {
                        return Err(invalid("Propose setup only for a site in this request."));
                    }
                    for value in [&proposal.identity,&proposal.credential_store,&proposal.summary] {
                        if value.trim().is_empty() || value.len()>400 || value.chars().any(|c|c.is_control()) {
                            return Err(invalid("Give short non-secret identity, credential-store and purpose labels."));
                        }
                    }
                    if proposal.cost!="free" {return Err(invalid("This setup flow supports free accounts only; a paid plan or trial needs a separate scoped decision."));}
                    if request.signup.as_ref().is_some_and(|s|matches!(s.state.as_str(),"in_progress"|"needs_verification"|"cancelled_needs_reconciliation")) {
                        return Err(invalid("Reconcile the prior signup before proposing another."));
                    }
                    if request.signup.as_ref().is_some_and(|s|s.proposal==proposal && matches!(s.state.as_str(),"proposed"|"approved")) {
                        return self.assistance_view(request);
                    }
                    book.requests[i].signup=Some(Signup{proposal:proposal.clone(),state:"proposed".into(),claim_ref:None,evidence_ref:None,created:false});
                    book.requests[i].sites.iter_mut().find(|e|e.site==proposal.site).unwrap().state="signup_proposed".into();
                }
                "claim_signup"=> {
                    if !self.journal.get_active_lease()?.is_some_and(|l|l.task_ref==task.task_ref) {return Err(invalid("Reacquire the computer before claiming account setup."));}
                    let signup=request.signup.as_ref().filter(|s|s.state=="approved" && input.site.as_deref()==Some(s.proposal.site.as_str()))
                        .ok_or_else(||invalid("No unchanged approved signup is available. If an attempt began, reconcile it; do not submit again."))?;
                    if book.signup_attempts.iter().any(|a| a.signup.state!="not_created" && a.signup.proposal.site==signup.proposal.site
                        && a.signup.proposal.identity.trim().eq_ignore_ascii_case(signup.proposal.identity.trim())) {
                        return Err(invalid("An account setup for this identity/site already exists. Reconcile it rather than registering again."));
                    }
                    if book.signup_attempts.len()>=2048 {return Err(invalid("Account setup history is full; use an existing account or administrator recovery. No history was discarded."));}
                    let request=&mut book.requests[i];
                    let signup=request.signup.as_mut().unwrap();
                    signup.state="in_progress".into(); signup.claim_ref=Some(id("signup"));
                    request.sites.iter_mut().find(|e|e.site==signup.proposal.site).unwrap().state="signup_in_progress".into();
                }
                "record_created"=> {
                    let request=&mut book.requests[i];
                    let signup=request.signup.as_mut().filter(|s|matches!(s.state.as_str(),"in_progress"|"needs_verification"|"cancelled_needs_reconciliation") && input.site.as_deref()==Some(s.proposal.site.as_str()))
                        .ok_or_else(||invalid("No claimed signup exists for this site."))?;
                    signup.state="needs_verification".into(); signup.created=true;
                    request.sites.iter_mut().find(|e|e.site==signup.proposal.site).unwrap().state="needs_verification".into();
                }
                "record_not_created"=> {
                    let evidence=input.evidence_ref.as_deref().filter(|s|!s.is_empty() && s.len()<=200 && !s.chars().any(|c|c.is_control()))
                        .ok_or_else(||invalid("Give evidence proving registration had no effect. A timeout or missing confirmation is not that evidence."))?;
                    let request=&mut book.requests[i];
                    let signup=request.signup.as_mut().filter(|s|s.claim_ref.is_some() && !s.created && matches!(s.state.as_str(),"in_progress"|"cancelled_needs_reconciliation")
                        && input.site.as_deref()==Some(s.proposal.site.as_str()))
                        .ok_or_else(||invalid("Only an uncertain claimed attempt may be reconciled as not created."))?;
                    signup.state="not_created".into(); signup.evidence_ref=Some(evidence.into());
                    request.sites.iter_mut().find(|e|e.site==signup.proposal.site).unwrap().state="no_account".into();
                }
                "resolve"=> {
                    let name=input.site.as_deref().ok_or_else(||invalid("Name the resolved site."))?;
                    let evidence=input.evidence_ref.as_deref().filter(|s| !s.is_empty() && s.len()<=200 && !s.chars().any(|c|c.is_control()))
                        .ok_or_else(||invalid("Give a non-secret receipt/checkpoint reference proving the task route and expected account when used."))?;
                    let request=&mut book.requests[i];
                    let entry=request.sites.iter_mut().find(|e|e.site==name).ok_or_else(||invalid("That site is not in the request."))?;
                    if entry.terminal() {return Err(invalid("That site is already settled."));}
                    if let Some(signup)=request.signup.as_mut().filter(|s|s.proposal.site==name && s.claim_ref.is_some() && s.state!="not_created") {
                        if signup.state!="needs_verification" {return Err(invalid("Record the created account and verify recovery/identity before resolving."));}
                        signup.state="complete".into(); signup.evidence_ref=Some(evidence.into());
                    }
                    entry.state="resolved".into(); entry.reason=None;
                }
                _=>return Err(invalid("Login action is continue, propose_signup, claim_signup, recover_signup, record_created, record_not_created or resolve.")),
            }
        }
        book.requests[i].revision+=1;
        book.requests[i].last_answer=None;
        self.save_assistance(&mut book,i,&self.task_subject(&task.task_ref,&task.principal))
    }
}
