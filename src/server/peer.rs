//! Per-principal operator transport (`operator-peer.ts`).
//!
//! A dedicated system UID proves an operator's identity on its own socket
//! `/run/ibara-operator/<p>.sock`: mode 0600 plus exactly one named-user ACL,
//! and the connecting peer's `SO_PEERCRED` uid must equal the uid recorded in
//! `operator-accounts.json`. No bearer is ever copied or accepted there.
//!
//! Fail closed when any of these is true: the account file is missing or
//! malformed, a uid is duplicated or not a system uid, the uid is root or the
//! controller, the account is in `ibara-runtime`, the socket directory is not
//! owned by the controller, the ACL tools cannot prove the socket admits
//! exactly one named user, or the peer uid differs from the socket's principal.

use crate::desktop::run::{Cmd, run};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

const RUNTIME_GROUP: &str = "ibara-runtime";
const ACL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ACCOUNTS_BYTES: u64 = 65_536;

/// Where account names and groups are looked up. Tests point these at fixtures.
#[derive(Debug, Clone)]
pub struct SystemDb {
    pub passwd: PathBuf,
    pub group: PathBuf,
}

impl Default for SystemDb {
    fn default() -> Self {
        SystemDb { passwd: "/etc/passwd".into(), group: "/etc/group".into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub user: String,
    pub uid: u32,
}

pub type Accounts = BTreeMap<String, Account>;

struct Passwd {
    user: String,
    uid: u32,
    gid: u32,
}

/// `^[a-z][a-z0-9_-]{0,22}$`, except `owner`: the principal part of `ibara-op-<p>`.
pub fn valid_peer_principal(p: &str) -> bool {
    lower_ident(p, 23) && p != crate::access::OWNER
}

/// `^[a-z][a-z0-9_-]{0,max-1}$`.
pub fn lower_ident(p: &str, max: usize) -> bool {
    let bytes = p.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= max
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

fn read_lines(file: &Path) -> Vec<String> {
    std::fs::read(file)
        .map(|bytes| String::from_utf8_lossy(&bytes).split('\n').map(str::to_string).collect())
        .unwrap_or_default()
}

fn passwd_by_user(db: &SystemDb, user: &str) -> Option<Passwd> {
    let prefix = format!("{user}:");
    let line = read_lines(&db.passwd).into_iter().find(|l| l.starts_with(&prefix))?;
    let parts: Vec<&str> = line.split(':').collect();
    let uid = parts.get(2)?.parse().ok()?;
    let gid = parts.get(3)?.parse().ok()?;
    if parts[0].is_empty() {
        return None;
    }
    Some(Passwd { user: parts[0].to_string(), uid, gid })
}

fn in_runtime_group(db: &SystemDb, account: &Passwd) -> bool {
    read_lines(&db.group).iter().any(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.first() != Some(&RUNTIME_GROUP) {
            return false;
        }
        fields.get(2) == Some(&account.gid.to_string().as_str())
            || fields.get(3).is_some_and(|members| members.split(',').any(|m| !m.is_empty() && m == account.user))
    })
}

/// Load and validate `operator-accounts.json`. `None` when the file does not
/// exist; an empty map (fail closed) for any other problem. `overridden` is
/// true when `IBARA_OPERATOR_ACCOUNTS` named the file, which lets the
/// controller's own uid own it.
pub fn load_operator_accounts(path: &Path, overridden: bool, controller_uid: u32, db: &SystemDb) -> Option<Accounts> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(Accounts::new()),
    };
    let trusted_owner = meta.uid() == 0 || (overridden && meta.uid() == controller_uid);
    if !meta.file_type().is_file() || meta.nlink() != 1 || !trusted_owner || meta.mode() & 0o022 != 0 || meta.size() > MAX_ACCOUNTS_BYTES
    {
        return Some(Accounts::new());
    }
    let Ok(bytes) = std::fs::read(path) else { return Some(Accounts::new()) };
    let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) else { return Some(Accounts::new()) };
    if parsed.get("schema_version").and_then(Value::as_f64) != Some(1.0) {
        return Some(Accounts::new());
    }
    let Some(entries) = parsed.get("accounts").and_then(Value::as_object) else { return Some(Accounts::new()) };
    let mut accounts = Accounts::new();
    let mut seen = BTreeSet::new();
    for (principal, record) in entries {
        let user = record.get("user").and_then(Value::as_str).unwrap_or("");
        let uid = record
            .get("uid")
            .and_then(crate::server::policy::safe_integer)
            .filter(|uid| *uid > 0.0 && *uid < 1000.0)
            .map(|uid| uid as u32);
        let passwd = passwd_by_user(db, user);
        let valid = valid_peer_principal(principal)
            && user == format!("ibara-op-{principal}")
            && uid.is_some_and(|uid| uid != controller_uid && !seen.contains(&uid))
            && passwd.as_ref().is_some_and(|p| Some(p.uid) == uid && !in_runtime_group(db, p));
        let (true, Some(uid)) = (valid, uid) else { return Some(Accounts::new()) };
        seen.insert(uid);
        accounts.insert(principal.clone(), Account { user: user.to_string(), uid });
    }
    Some(accounts)
}

/// May a peer with `uid` speak for `principal`? (`peerIdentityAllowed`)
pub fn peer_identity_allowed(principal: &str, uid: u32, accounts: &Accounts, controller_uid: u32, db: &SystemDb) -> bool {
    let Some(account) = accounts.get(principal) else { return false };
    if uid != account.uid || uid == 0 || uid >= 1000 || uid == controller_uid {
        return false;
    }
    passwd_by_user(db, &account.user).is_some_and(|p| p.uid == uid && !in_runtime_group(db, &p))
}

/// `<dir>/<principal>.sock`, or `None` for a principal that is not a plain name.
pub fn socket_path_for(dir: &Path, principal: &str) -> Option<PathBuf> {
    valid_peer_principal(principal).then(|| dir.join(format!("{principal}.sock")))
}

/// Owner `rwx`; group and other may neither write nor list (other execute is
/// the traversal bit for the dedicated uids).
pub fn controller_owns_socket_dir(dir: &Path, controller_uid: u32) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        meta.file_type().is_dir() && meta.uid() == controller_uid && meta.mode() & 0o700 == 0o700 && meta.mode() & 0o026 == 0
    })
}

async fn tool(program: &str, args: &[&str]) -> Option<String> {
    let output = run(Cmd::new(program).args(args.iter().copied()).timeout(ACL_TIMEOUT)).await.ok()?;
    output.success().then(|| output.stdout_text())
}

struct Acl {
    named_users: BTreeMap<String, String>,
    group_owner: String,
    named_groups: Vec<String>,
    other: String,
}

fn perm(value: &str) -> String {
    value.split(|c: char| c.is_whitespace() || c == '#').next().unwrap_or("").to_string()
}

async fn acl_entries(file: &Path) -> Option<Acl> {
    let text = tool("getfacl", &["-c", "-p", &file.to_string_lossy()]).await?;
    let mut acl = Acl { named_users: BTreeMap::new(), group_owner: String::new(), named_groups: Vec::new(), other: String::new() };
    for line in text.split('\n') {
        if let Some(rest) = line.strip_prefix("user:") {
            if let Some((name, value)) = rest.split_once(':')
                && !name.is_empty()
            {
                acl.named_users.insert(name.to_string(), perm(value));
            }
        } else if let Some(rest) = line.strip_prefix("group:") {
            if let Some((name, value)) = rest.split_once(':') {
                if name.is_empty() {
                    acl.group_owner = perm(value);
                } else {
                    acl.named_groups.push(name.to_string());
                }
            }
        } else if let Some(value) = line.strip_prefix("other::") {
            acl.other = perm(value);
        }
    }
    (!acl.group_owner.is_empty() && !acl.other.is_empty()).then_some(acl)
}

/// Keep a stored operator bearer private to the controller: a regular
/// single-link file it owns, mode 0600, no named ACL users (`sealOperatorCredential`).
/// Returns whether the key is now provably private.
pub async fn seal_operator_credential(key_path: &Path, controller_uid: u32) -> bool {
    let Ok(before) = std::fs::symlink_metadata(key_path) else { return false };
    if !before.file_type().is_file() || before.nlink() != 1 || before.uid() != controller_uid {
        return false;
    }
    if std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600)).is_err() {
        return false;
    }
    let path = key_path.to_string_lossy();
    let _ = tool("setfacl", &["-b", &path]).await;
    let Ok(after) = std::fs::metadata(key_path) else { return false };
    let named = acl_entries(key_path).await.map(|acl| acl.named_users.len());
    after.uid() == controller_uid && after.mode() & 0o777 == 0o600 && named == Some(0)
}

/// After `listen`: mode 0600, clear the ACL, grant `u:<user>:rw`, then prove
/// the socket admits exactly that one named user (`armPrincipalSocket`).
pub async fn arm_principal_socket(dir: &Path, principal: &str, account: &Account, controller_uid: u32, db: &SystemDb) -> bool {
    let Some(passwd) = passwd_by_user(db, &account.user) else { return false };
    if !controller_owns_socket_dir(dir, controller_uid) || passwd.uid != account.uid || in_runtime_group(db, &passwd) {
        return false;
    }
    let Some(socket) = socket_path_for(dir, principal) else { return false };
    let Ok(meta) = std::fs::symlink_metadata(&socket) else { return false };
    if !meta.file_type().is_socket() || meta.uid() != controller_uid {
        return false;
    }
    if std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).is_err() {
        return false;
    }
    let path = socket.to_string_lossy();
    if tool("setfacl", &["-b", &path]).await.is_none() {
        return false;
    }
    if tool("setfacl", &["-m", &format!("u:{}:rw", account.user), &path]).await.is_none() {
        return false;
    }
    let Some(acl) = acl_entries(&socket).await else { return false };
    acl.named_groups.is_empty()
        && acl.group_owner == "---"
        && acl.other == "---"
        && acl.named_users.len() == 1
        && acl.named_users.get(&account.user).is_some_and(|p| p == "rw-")
}

/// Remove the principal's socket file if it is a socket (never follows a link).
pub fn disarm_principal_socket(dir: &Path, principal: &str) -> std::io::Result<()> {
    let Some(socket) = socket_path_for(dir, principal) else { return Ok(()) };
    match std::fs::symlink_metadata(&socket) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(&socket),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, accounts: &str, group: &str) -> (PathBuf, SystemDb) {
        let passwd = dir.join("passwd");
        std::fs::write(&passwd, "root:x:0:0::/root:/bin/bash\nibara-op-vesper:x:959:958::/var/lib/ibara-operator:/usr/local/sbin/ibara-op-shell\nibara-op-hazel:x:957:956::/var/lib/ibara-operator:/usr/local/sbin/ibara-op-shell\n").unwrap();
        let groups = dir.join("group");
        std::fs::write(&groups, group).unwrap();
        let file = dir.join("operator-accounts.json");
        std::fs::write(&file, accounts).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        (file, SystemDb { passwd, group: groups })
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ibara-peer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uid() -> u32 {
        unsafe { libc::getuid() }
    }

    #[test]
    fn account_file_problems_fail_closed() {
        let dir = temp("closed");
        let good = r#"{"schema_version":1,"accounts":{"vesper":{"user":"ibara-op-vesper","uid":959}}}"#;
        let cases = [
            // wrong user name for the principal
            r#"{"schema_version":1,"accounts":{"vesper":{"user":"ibara-op-hazel","uid":957}}}"#,
            // uid differs from passwd
            r#"{"schema_version":1,"accounts":{"vesper":{"user":"ibara-op-vesper","uid":958}}}"#,
            // duplicate uid
            r#"{"schema_version":1,"accounts":{"vesper":{"user":"ibara-op-vesper","uid":959},"hazel":{"user":"ibara-op-hazel","uid":959}}}"#,
            // not a system uid
            r#"{"schema_version":1,"accounts":{"vesper":{"user":"ibara-op-vesper","uid":1959}}}"#,
            // wrong schema
            r#"{"schema_version":2,"accounts":{"vesper":{"user":"ibara-op-vesper","uid":959}}}"#,
            "not json",
        ];
        for case in cases {
            let (file, db) = fixture(&dir, case, "");
            assert_eq!(load_operator_accounts(&file, true, uid(), &db), Some(Accounts::new()), "{case}");
        }
        // In ibara-runtime by membership.
        let (file, db) = fixture(&dir, good, "ibara-runtime:x:960:tulip1,ibara-op-vesper\n");
        assert_eq!(load_operator_accounts(&file, true, uid(), &db), Some(Accounts::new()));
        // Owned by the controller but not named by the override.
        let (file, db) = fixture(&dir, good, "");
        assert_eq!(load_operator_accounts(&file, false, uid(), &db), Some(Accounts::new()));
        // Group-writable.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert_eq!(load_operator_accounts(&file, true, uid(), &db), Some(Accounts::new()));
        // Missing file: no accounts at all.
        assert_eq!(load_operator_accounts(&dir.join("absent.json"), true, uid(), &db), None);
        // And the valid shape loads.
        let (file, db) = fixture(&dir, good, "ibara-runtime:x:960:tulip1\n");
        let accounts = load_operator_accounts(&file, true, uid(), &db).unwrap();
        assert_eq!(accounts.get("vesper"), Some(&Account { user: "ibara-op-vesper".into(), uid: 959 }));
        assert!(!peer_identity_allowed("vesper", 957, &accounts, uid(), &db));
        assert!(!peer_identity_allowed("hazel", 957, &accounts, uid(), &db));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn socket_dir_must_be_private_to_the_controller() {
        let dir = temp("sockdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o731)).unwrap();
        assert!(!controller_owns_socket_dir(&dir, uid()));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o711)).unwrap();
        assert!(controller_owns_socket_dir(&dir, uid()));
        assert!(!controller_owns_socket_dir(&dir, uid() + 1));
        assert_eq!(socket_path_for(&dir, "../x"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
