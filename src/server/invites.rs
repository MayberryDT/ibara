//! Sharing this computer with a friend.
//!
//! The person here chooses Share This Computer, a level (Watch, Use with
//! Approval, Take Control) and when it ends. ibara makes a single-use code
//! that is easy to read aloud (`4H7K-92QX`) and keeps only its SHA-256, with
//! the level and the end, in `<state>/invites.json` (0600). A friend's computer
//! whose signed pairing request carries a valid, unexpired, unused code is
//! accepted without a person here and gets exactly that level, with that end on
//! every grant; the invite is then used up. A wrong, used or expired code gets
//! the same refusal. After 5 of them in 10 minutes from one Tailscale computer,
//! every code from it is refused until the oldest is 10 minutes old.
//!
//! Revoking an unused invite deletes it; revoking a used one ends the pairing
//! it made at once, but not a later one (the same friend's computer upgraded
//! with a newer invite keeps what that invite gave). An invite whose end has
//! passed leaves the list: its code no longer works and the access it gave has
//! ended.

use super::authority;
use crate::access::ShareLevel;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// No 0/O, 1/I/L or U/V to mix up when read aloud.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTWXYZ";
const CODE_LEN: usize = 8;
/// Unused invites kept at once.
const MAX_WAITING: usize = 20;
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW_MS: i64 = 10 * 60_000;
/// Asking computers whose failures are remembered at once.
const MAX_ASKERS: usize = 256;

#[derive(Serialize, Deserialize, Clone)]
struct Invite {
    id: String,
    /// SHA-256 of the code as [`normalize`] gives it, lowercase hex.
    digest: String,
    level: String,
    created_at: i64,
    /// Milliseconds; none: until revoked.
    expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    used: Option<Used>,
}

/// Who used an invite.
#[derive(Serialize, Deserialize, Clone)]
pub struct Used {
    pub at: i64,
    pub principal: String,
    /// The key it paired with. Revoking ends the principal's pairing only while
    /// that pairing still has this key and was made by this invite (its
    /// operator record names the invite).
    pub fingerprint: String,
    pub login: String,
    pub computer: String,
}

impl Invite {
    fn level(&self) -> Option<ShareLevel> {
        ShareLevel::parse(&self.level)
    }
    fn ended(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
    fn view(&self) -> Value {
        let used = self.used.as_ref().map(|u| json!({"at": u.at, "login": u.login, "computer": u.computer}));
        json!({"id": self.id, "level": self.level, "created_at": self.created_at, "expires_at": self.expires_at, "used": used})
    }
}

/// An invite a pairing request is using: what the friend's computer gets.
#[derive(Debug, Clone)]
pub struct Redeemed {
    pub id: String,
    pub level: ShareLevel,
    pub expires_at: Option<i64>,
}

/// How long an invite lasts, from when it is made.
pub fn lasts_ms(choice: &str) -> Option<Option<i64>> {
    match choice {
        "hour" => Some(Some(60 * 60_000)),
        "day" => Some(Some(24 * 60 * 60_000)),
        "week" => Some(Some(7 * 24 * 60 * 60_000)),
        "never" => Some(None),
        _ => None,
    }
}

/// A code as typed, without case, spaces or dashes; none when it can't be one.
pub fn normalize(code: &str) -> Option<String> {
    let code: String = code.chars().filter(|c| !matches!(c, ' ' | '-')).map(|c| c.to_ascii_uppercase()).collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

fn digest(normalized: &str) -> String {
    super::policy::sha256_hex(normalized.as_bytes())
}

fn fresh_code() -> String {
    let mut n = uuid::Uuid::new_v4().as_u128();
    let mut code = String::with_capacity(CODE_LEN + 1);
    for i in 0..CODE_LEN {
        if i == CODE_LEN / 2 {
            code.push('-');
        }
        code.push(ALPHABET[(n % ALPHABET.len() as u128) as usize] as char);
        n /= ALPHABET.len() as u128;
    }
    code
}

/// The timeline's sentence: `alex@example.com's Bench joined with an invite: Watch until Sep 28, 14:00.`
pub fn joined_summary(login: &str, host_name: &str, invite: &Redeemed) -> String {
    let until = match invite.expires_at {
        Some(at) => format!("until {}", crate::ids::local_short(at)),
        None => "until revoked".to_string(),
    };
    format!("{login}'s {host_name} joined with an invite: {} {until}.", invite.level.label())
}

fn refusal(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

fn unreadable() -> Value {
    refusal("INVITES_UNAVAILABLE", "ibara couldn't read this computer's invites.")
}

pub struct Invites {
    path: PathBuf,
    /// Invites a pairing is using right now, before it is written as used.
    claimed: RefCell<BTreeSet<String>>,
    /// When each asking computer (Tailscale stable node ID) sent a code that didn't work.
    failures: RefCell<BTreeMap<String, Vec<i64>>>,
}

impl Invites {
    pub fn new(path: PathBuf) -> Invites {
        Invites { path, claimed: RefCell::new(BTreeSet::new()), failures: RefCell::new(BTreeMap::new()) }
    }

    /// The invites still in effect (ended ones are dropped from the file).
    fn load(&self, now: i64) -> Option<Vec<Invite>> {
        let map = authority::load(&self.path).ok()?;
        let list: Vec<Invite> = match map.get("invites") {
            None => Vec::new(),
            Some(list) => serde_json::from_value(list.clone()).ok()?,
        };
        let kept: Vec<Invite> = list.iter().filter(|i| !i.ended(now)).cloned().collect();
        if kept.len() != list.len() {
            self.save(&kept).ok()?;
        }
        Some(kept)
    }

    fn save(&self, invites: &[Invite]) -> crate::error::Result<()> {
        let mut map = Map::new();
        map.insert("invites".into(), serde_json::to_value(invites).map_err(|_| authority::unknown_failure())?);
        authority::save(&self.path, &map)
    }

    /// `{op:"invite", level, lasts}` → `{ok, invite:{id, code, level, created_at, expires_at}}`.
    /// The code is shown this once; only its digest is kept.
    pub fn create(&self, request: &Value) -> Value {
        let level = request.get("level").and_then(Value::as_str).and_then(ShareLevel::parse);
        let lasts = request.get("lasts").and_then(Value::as_str).and_then(lasts_ms);
        let (Some(level), Some(lasts)) = (level, lasts) else {
            return refusal("INVALID_ARGUMENT", "Choose Watch, Use with Approval or Take Control, and 1 hour, 1 day, 1 week or until revoked.");
        };
        let now = crate::ids::now_millis();
        let Some(mut invites) = self.load(now) else { return unreadable() };
        if invites.iter().filter(|i| i.used.is_none()).count() >= MAX_WAITING {
            return refusal("TOO_MANY_INVITES", "There are 20 unused invites already. Revoke one, then make a new one.");
        }
        let code = loop {
            let code = fresh_code();
            let taken = normalize(&code).map(|n| digest(&n)).is_none_or(|d| invites.iter().any(|i| i.digest == d));
            if !taken {
                break code;
            }
        };
        let invite = Invite {
            id: crate::ids::id("inv"),
            digest: digest(&normalize(&code).unwrap_or_default()),
            level: level.name().into(),
            created_at: now,
            expires_at: lasts.map(|ms| now + ms),
            used: None,
        };
        let mut view = invite.view();
        view["code"] = json!(code);
        invites.push(invite);
        if self.save(&invites).is_err() {
            return refusal("INVITES_UNAVAILABLE", "ibara couldn't save the invite. Try again.");
        }
        super::pairing::log("invite_created", json!({"invite": view["id"], "level": level.name(), "expires_at": view["expires_at"]}));
        json!({"ok": true, "invite": view})
    }

    /// `{op:"invites"}` → `{ok, invites:[{id, level, created_at, expires_at, used:{at, login, computer}|null}]}`, newest first.
    pub fn list(&self) -> Value {
        let Some(invites) = self.load(crate::ids::now_millis()) else { return unreadable() };
        let list: Vec<Value> = invites.iter().rev().map(Invite::view).collect();
        json!({"ok": true, "invites": list})
    }

    /// A code from the computer Tailscale calls `asker`: its invite, claimed for
    /// this request, or the refusal to send. Every refusal but the rate limit
    /// counts toward it.
    pub fn redeem(&self, code: &str, asker: &str) -> Result<Redeemed, Value> {
        let now = crate::ids::now_millis();
        {
            let mut failures = self.failures.borrow_mut();
            failures.retain(|_, times| {
                times.retain(|t| now - t < FAILURE_WINDOW_MS);
                !times.is_empty()
            });
            if failures.get(asker).is_some_and(|times| times.len() >= MAX_FAILURES) {
                return Err(refusal(
                    "TOO_MANY_TRIES",
                    "Too many invite codes from this computer didn't work. Wait 10 minutes, then try again.",
                ));
            }
        }
        let wanted = normalize(code).map(|n| digest(&n));
        let invites = self.load(now);
        let found = wanted.as_ref().zip(invites.as_ref()).and_then(|(wanted, invites)| {
            invites.iter().find(|i| {
                super::policy::ct_eq(i.digest.as_bytes(), wanted.as_bytes())
                    && i.used.is_none()
                    && !i.ended(now)
                    && !self.claimed.borrow().contains(&i.id)
            })
        });
        match found.and_then(|i| Some((i.id.clone(), i.level()?, i.expires_at))) {
            Some((id, level, expires_at)) => {
                self.claimed.borrow_mut().insert(id.clone());
                Ok(Redeemed { id, level, expires_at })
            }
            None => {
                let mut failures = self.failures.borrow_mut();
                if !failures.contains_key(asker) && failures.len() >= MAX_ASKERS {
                    let oldest = failures.iter().min_by_key(|(_, t)| t.last().copied()).map(|(k, _)| k.clone());
                    if let Some(oldest) = oldest {
                        failures.remove(&oldest);
                    }
                }
                failures.entry(asker.to_string()).or_default().push(now);
                Err(refusal(
                    "INVITE_REFUSED",
                    "That invite code didn't work. Check it with the person who shared the computer, or ask them for a new one.",
                ))
            }
        }
    }

    /// A claimed invite's pairing did not finish: it can be used again.
    pub fn release(&self, id: &str) {
        self.claimed.borrow_mut().remove(id);
    }

    /// A claimed invite's pairing finished: it is used up.
    pub fn use_up(&self, id: &str, used: Used) {
        self.claimed.borrow_mut().remove(id);
        let Some(mut invites) = self.load(crate::ids::now_millis()) else { return };
        if let Some(invite) = invites.iter_mut().find(|i| i.id == id) {
            invite.used = Some(used);
            if let Err(error) = self.save(&invites) {
                super::pairing::log("invite_save_failed", json!({"invite": id, "error": error.message}));
            }
        }
    }

    /// Remove an invite; `Ok(Some(used))` when a friend had used it, whose
    /// pairing the caller ends.
    pub fn revoke(&self, id: &str) -> Result<Option<Used>, Value> {
        let Some(mut invites) = self.load(crate::ids::now_millis()) else { return Err(unreadable()) };
        let Some(at) = invites.iter().position(|i| i.id == id) else {
            return Err(refusal("INVITE_UNKNOWN", "That invite is gone already: it was revoked, or it ended."));
        };
        let removed = invites.remove(at);
        if self.save(&invites).is_err() {
            return Err(refusal("INVITES_UNAVAILABLE", "ibara couldn't revoke the invite. Try again."));
        }
        Ok(removed.used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failure cases this must catch:
    /// 1. A code read aloud and typed in lowercase, with a space or without its
    ///    dash, doesn't match.
    /// 2. A made code uses a character that is easy to mix up (0/O, 1/I/L, U/V)
    ///    or doesn't survive being typed back.
    #[test]
    fn codes_are_easy_to_read_and_type_back() {
        for _ in 0..200 {
            let code = fresh_code();
            assert_eq!(code.len(), 9, "{code}");
            assert_eq!(code.as_bytes()[4], b'-', "{code}");
            assert!(!code.contains(['0', 'O', '1', 'I', 'L', 'U', 'V']), "{code}");
            let typed = code.to_lowercase().replace('-', " ");
            assert_eq!(normalize(&typed), normalize(&code), "{code}");
            assert!(normalize(&code).is_some(), "{code}");
        }
        assert_eq!(normalize("4h7k 92qx").as_deref(), Some("4H7K92QX"));
        assert_eq!(normalize("4H7K-92Q"), None, "too short");
        assert_eq!(normalize("4H7K-92QO"), None, "O is never in a code");
    }
}
