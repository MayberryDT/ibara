//! Socket-activated projection of grants onto the restricted SSH accounts.
//! Root owns the enrolled public-key inventory. Requests enable or disable those
//! bindings, and may enrol a paired computer's own Ed25519 key for a plain
//! principal (`keys`): the desktop controller already decides who is paired,
//! and the account it gets can only reach that controller's own per-principal
//! socket and agent gateway. Requests never supply paths, account names, uids
//! or commands; root derives each from the principal.
use anyhow::{Context, bail};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
const INVENTORY: &str = "/etc/agent-computer/access-transport.json";
const ACCOUNT_FILE: &str = "/etc/agent-computer/operator-accounts.json";
const AGENT_KEYS: &str = "/etc/agent-computer/ssh/authorized_keys";
/// sshd reads `AuthorizedKeysFile /etc/ibara-operator/authorized_keys/%u`.
const OPERATOR_KEYS: &str = "/etc/ibara-operator/authorized_keys";
/// The forced command of the agent gateway (the socket unit runs this install root).
const ENTRY: &str = "/opt/agent-computer/current/ops/entry.sh";
const CONTROLLER_SOCKET: &str = "/run/agent-computer/controller.sock";
const GATEWAY_KEY: &str = "/etc/agent-computer/gateway.key";
/// Paired computers one computer keeps accounts for.
const MAX_PEERS: usize = 64;
fn key_file(principal: &str) -> String {
    format!("{OPERATOR_KEYS}/ibara-op-{principal}")
}
/// Before the `%u` layout, key files were named by principal.
fn legacy_key_file(principal: &str) -> String {
    format!("{OPERATOR_KEYS}/{principal}")
}
/// The agent gateway line foundation writes for a reviewed principal.
fn agent_line(principal: &str, key: &crate::sshkey::KeyLine) -> String {
    let forced = [ENTRY, principal, CONTROLLER_SOCKET, GATEWAY_KEY].map(|v| format!("'{v}'")).join(" ");
    format!("restrict,command=\"{forced}\" {}", key.line)
}
/// The base64 blob of an inventory `public` line (`restrict ssh-ed25519 BLOB …`).
fn blob_of(public: &Value) -> Option<&str> {
    public.as_str()?.split_whitespace().skip_while(|x| *x != "ssh-ed25519").nth(1)
}
/// The highest system uid below 1000 that neither passwd nor the inventory uses.
fn free_uid(passwd: &str, peers: &serde_json::Map<String, Value>) -> Option<u64> {
    let used: Vec<u64> = passwd
        .lines()
        .filter_map(|l| l.split(':').nth(2)?.parse().ok())
        .chain(peers.values().filter_map(|p| p["uid"].as_u64()))
        .collect();
    (100..1000).rev().find(|uid| !used.contains(uid))
}
/// Merge desktop-vouched keys (`{principal: "ssh-ed25519 …"}`) into the
/// inventory. All or nothing; returns whether anything changed.
fn enroll_keys(inventory: &mut Value, keys: Option<&Value>, passwd: &str) -> anyhow::Result<bool> {
    let keys = match keys {
        None | Some(Value::Null) => return Ok(false),
        Some(Value::Object(keys)) => keys,
        Some(_) => bail!("Invalid keys"),
    };
    let mut next = inventory.clone();
    let peers = next["peers"].as_object_mut().context("Missing enrolled peers")?;
    let mut changed = false;
    for (principal, line) in keys {
        if !crate::server::peer::valid_peer_principal(principal) {
            bail!("Invalid principal");
        }
        let key = line.as_str().and_then(crate::sshkey::ed25519_line).context("Invalid key")?;
        if peers.iter().any(|(other, p)| other != principal && blob_of(&p["public"]) == Some(key.blob.as_str())) {
            bail!("Key belongs to another computer");
        }
        let public = format!("restrict {}\n", key.line);
        match peers.get_mut(principal) {
            Some(peer) if blob_of(&peer["public"]) == Some(key.blob.as_str()) => continue,
            Some(peer) => {
                peer["public"] = json!(public);
                peer["agent_lines"] = json!([agent_line(principal, &key)]);
            }
            None => {
                if peers.len() >= MAX_PEERS {
                    bail!("Too many paired computers");
                }
                let uid = free_uid(passwd, peers).context("No free system uid")?;
                let user = format!("ibara-op-{principal}");
                peers.insert(principal.clone(), json!({"user": user, "uid": uid, "public": public, "agent_lines": [agent_line(principal, &key)]}));
            }
        }
        changed = true;
    }
    if changed {
        *inventory = next;
    }
    Ok(changed)
}
fn read(path: &str) -> anyhow::Result<String> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o022 != 0 {
        bail!("Unsafe root-owned input");
    }
    let s = std::fs::read_to_string(path)?;
    if s.len() > 256 * 1024 {
        bail!("Input too large");
    }
    Ok(s)
}
fn write(path: &str, body: &str, mode: u32) -> anyhow::Result<()> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.is_file() || m.uid() != 0 || m.nlink() != 1 {
            bail!("Unsafe output");
        }
    }
    let temp = format!("{path}.access-{}", std::process::id());
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&temp)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(temp, path)?;
    Ok(())
}
fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("{program} failed; access remains denied until recovery");
    }
    Ok(String::from_utf8(out.stdout)?)
}
fn account(user: &str) -> Option<Vec<String>> {
    std::fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find(|l| l.split(':').next() == Some(user))
        .map(|l| l.split(':').map(str::to_string).collect())
}
fn init(user: &str, enrolling: Option<&str>) -> anyhow::Result<()> {
    if Path::new(INVENTORY).exists() && enrolling.is_none() {
        bail!("Inventory already exists. Re-pair explicitly to replace a reviewed key.");
    }
    let desktop = account(user).context("Desktop account missing")?;
    let uid: u32 = desktop[2].parse()?;
    if uid < 1000 {
        bail!("Expected a desktop user");
    }
    let accounts: Value = serde_json::from_str(&read(ACCOUNT_FILE)?)?;
    let keys = read(AGENT_KEYS)?;
    let mut peers = serde_json::Map::new();
    for (principal, a) in accounts["accounts"].as_object().context("Invalid account inventory")? {
        if enrolling.is_some_and(|wanted| wanted != principal) {
            continue;
        }
        if !crate::server::peer::valid_peer_principal(principal) {
            bail!("Invalid principal");
        }
        let name = format!("ibara-op-{principal}");
        if a["user"] != name {
            bail!("Unexpected account binding");
        }
        let user = account(&name).context("Operator account missing")?;
        if user[5] != "/var/lib/ibara-operator" || user[6] != "/usr/local/sbin/ibara-op-shell" {
            bail!("Operator account not restricted");
        }
        let public = read(&key_file(principal)).or_else(|_| read(&legacy_key_file(principal)))?;
        let blob = public
            .split_whitespace()
            .skip_while(|x| *x != "ssh-ed25519")
            .nth(1)
            .context("Missing public key")?;
        let agent_lines: Vec<_> = keys.lines().filter(|l| l.split_whitespace().any(|x| x == blob)).collect();
        peers.insert(
            principal.clone(),
            json!({"user":name,"uid":a["uid"],"public":public,"agent_lines":agent_lines}),
        );
    }
    // No unreviewed key line may be silently discarded on the first projection.
    for line in keys.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        if enrolling.is_none()
            && !peers
                .values()
                .any(|p| p["agent_lines"].as_array().is_some_and(|a| a.iter().any(|l| l == line)))
        {
            bail!("Unmapped agent key; review transport before import");
        }
    }
    if let Some(principal) = enrolling {
        let mut old: Value = serde_json::from_str(&read(INVENTORY)?)?;
        let mut enrolled = peers.get(principal).context("Newly enrolled principal is absent")?.clone();
        if enrolled["agent_lines"].as_array().is_some_and(|a| a.is_empty()) && old["peers"][principal]["public"] == enrolled["public"] {
            enrolled["agent_lines"] = old["peers"][principal]["agent_lines"].clone();
        }
        old["peers"][principal] = enrolled;
        write(INVENTORY, &old.to_string(), 0o600)?;
    } else {
        write(INVENTORY, &json!({"version":1,"desktop_uid":uid,"peers":peers}).to_string(), 0o600)?;
    }
    println!("Access transport inventory sealed for {user}.");
    Ok(())
}
fn project(inventory: &Value, request: &Value) -> anyhow::Result<()> {
    let peers = inventory["peers"].as_object().context("Missing enrolled peers")?;
    let desired = request["peers"].as_object().context("Missing desired peers")?;
    // An ended pairing that never got an account has nothing to lock; any other
    // computer must have been enrolled first.
    let enabled = |d: &Value| d["operator"] == true || d["agent"] == true;
    if desired.iter().any(|(p, d)| !peers.contains_key(p) && enabled(d)) {
        bail!("Unreviewed computer");
    }
    let mut accounts = serde_json::Map::new();
    let mut agent_lines = Vec::new();
    let mut principals = Vec::new();
    // Retire key files before removing UIDs. The daemon already denies effects.
    for (principal, p) in peers {
        let operator = desired.get(principal).is_some_and(|d| d["operator"] == true);
        let agent = desired.get(principal).is_some_and(|d| d["agent"] == true);
        let name = p["user"].as_str().context("Missing account name")?;
        let uid = p["uid"].as_u64().filter(|u| *u > 0 && *u < 1000).context("Invalid operator uid")?;
        if name != format!("ibara-op-{principal}") {
            bail!("Invalid account name");
        }
        let keyfile = key_file(principal);
        // Key files named by principal predate sshd's `%u` lookup.
        let legacy = legacy_key_file(principal);
        if Path::new(&legacy).exists() {
            std::fs::remove_file(&legacy)?;
        }
        if !operator {
            if Path::new(&keyfile).exists() {
                std::fs::remove_file(&keyfile)?;
            }
            if let Some(a) = account(name) {
                if a[2] != uid.to_string() || a[5] != "/var/lib/ibara-operator" {
                    bail!("Account binding changed");
                }
                let _ = Command::new("/usr/bin/pkill").args(["-KILL", "-u", &uid.to_string()]).status();
                run("/usr/bin/userdel", &[name])?;
            }
        } else {
            if account(name).is_none() {
                // A random unusable-by-knowledge but unlocked hash allows SSH
                // public keys with UsePAM=no. No password is ever exposed.
                let mut random = [0u8; 32];
                std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
                let mut openssl = Command::new("/usr/bin/openssl")
                    .args(["passwd", "-6", "-stdin"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()?;
                openssl
                    .stdin
                    .take()
                    .context("hash input")?
                    .write_all(crate::store::canonical::hex_encode(&random).as_bytes())?;
                let out = openssl.wait_with_output()?;
                if !out.status.success() {
                    bail!("Cannot initialise restricted account");
                }
                let hash = String::from_utf8(out.stdout)?;
                run(
                    "/usr/bin/useradd",
                    &[
                        "--system",
                        "--no-create-home",
                        "--home-dir",
                        "/var/lib/ibara-operator",
                        "--shell",
                        "/usr/local/sbin/ibara-op-shell",
                        "--user-group",
                        "--uid",
                        &uid.to_string(),
                        "--password",
                        hash.trim(),
                        name,
                    ],
                )?;
            }
            let a = account(name).context("Operator missing")?;
            if a[2] != uid.to_string() || a[5] != "/var/lib/ibara-operator" || a[6] != "/usr/local/sbin/ibara-op-shell" {
                bail!("Account binding changed");
            }
            write(&keyfile, p["public"].as_str().context("Missing public key")?, 0o644)?;
            accounts.insert(principal.clone(), json!({"user":name,"uid":uid}));
        }
        if agent {
            principals.push(principal.clone());
            for l in p["agent_lines"].as_array().context("Missing agent binding")? {
                agent_lines.push(l.as_str().context("Invalid agent binding")?.to_string());
            }
        }
    }
    write(AGENT_KEYS, &format!("{}\n", agent_lines.join("\n")), 0o644)?;
    write(ACCOUNT_FILE, &json!({"schema_version":1,"accounts":accounts}).to_string(), 0o644)?;
    let path = "/etc/agent-computer/policy.json";
    let mut policy: Value = serde_json::from_str(&read(path)?)?;
    policy["principals"] = json!(principals);
    write(path, &policy.to_string(), 0o644)?;
    Ok(())
}
pub fn main(args: Vec<std::ffi::OsString>) -> i32 {
    let result = (|| -> anyhow::Result<()> {
        if crate::server::current_uid() != 0 {
            bail!("Root is required");
        }
        if args.first().is_some_and(|s| s == "init") && args.len() == 2 {
            return init(&args[1].to_string_lossy(), None);
        }
        if args.first().is_some_and(|s| s == "enroll") && args.len() == 3 {
            return init(&args[1].to_string_lossy(), Some(&args[2].to_string_lossy()));
        }
        if !args.is_empty() {
            bail!("Usage: ibara access-system [init DESKTOP_USER]");
        }
        let desktop_uid = serde_json::from_str::<Value>(&read(INVENTORY)?)?["desktop_uid"].as_u64();
        // stdin is the accepted AF_UNIX stream supplied by systemd.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                0,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if rc != 0 || Some(cred.uid as u64) != desktop_uid {
            bail!("Caller is not the configured desktop controller");
        }
        // Serialize projection instances; socket activation can accept concurrently.
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open("/run/ibara-access.lock")?;
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("Cannot lock access projection");
        }
        let mut line = String::new();
        std::io::stdin().lock().take(65537).read_line(&mut line)?;
        if line.len() > 65536 {
            bail!("Request too large");
        }
        let request: Value = serde_json::from_str(&line)?;
        // Read under the lock, so a concurrent enrolment is never lost.
        let mut inventory: Value = serde_json::from_str(&read(INVENTORY)?)?;
        if enroll_keys(&mut inventory, request.get("keys"), &std::fs::read_to_string("/etc/passwd")?)? {
            write(INVENTORY, &inventory.to_string(), 0o600)?;
        }
        project(&inventory, &request)?;
        println!("{{\"ok\":true}}");
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ibara access: {e}");
            println!("{{\"ok\":false}}");
            1
        }
    }
}

/// Root key enrolment, isolated: root cannot be exercised end to end here.
/// Ways it could fail, each checked below:
/// 1. A principal that is not `^[a-z][a-z0-9_-]{0,22}$` names a path or another account.
/// 2. A key line carries options, a second line, another key type or a malformed blob.
/// 3. A key already bound to another principal makes the agent gateway ambiguous.
/// 4. Unbounded enrolment exhausts system accounts.
/// 5. Re-sending an unchanged key drops the reviewed agent lines it already has.
/// 6. A replaced key keeps the old key's agent access.
/// 7. A new principal gets a uid that is taken, not a system uid, or an agent line for another principal.
/// 8. A request without keys (an older controller) changes the inventory.
#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn key(fill: u8, comment: &str) -> String {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[fill; 32]);
        format!("ssh-ed25519 {} {comment}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(&blob))
    }

    const REVIEWED_LINE: &str = "restrict,command=\"'/opt/agent-computer/current/ops/entry.sh' 'vesper' '/run/agent-computer/controller.sock' '/etc/agent-computer/gateway.key'\" ssh-ed25519 REVIEWED";
    const PASSWD: &str = "root:x:0:0::/root:/bin/bash\nsshd:x:999:999::/:/usr/bin/nologin\nibara-op-vesper:x:959:959::/var/lib/ibara-operator:/usr/local/sbin/ibara-op-shell\nriley:x:1000:1000::/home/riley:/bin/bash\n";

    fn inventory() -> Value {
        json!({"version": 1, "desktop_uid": 1000, "peers": {
            "vesper": {"user": "ibara-op-vesper", "uid": 959, "public": format!("restrict {}\n", key(1, "vesper")), "agent_lines": [REVIEWED_LINE]},
        }})
    }

    #[test]
    fn invalid_principals_and_key_lines_are_refused_whole() {
        for principal in ["../etc", "Vesper", "", "a-name-longer-than-twenty-three", "9lives"] {
            let mut inv = inventory();
            assert!(enroll_keys(&mut inv, Some(&json!({principal: key(2, "x")})), PASSWD).is_err(), "{principal:?}");
            assert_eq!(inv, inventory(), "{principal:?}");
        }
        for line in [
            format!("command=\"sh\" {}", key(2, "x")),
            format!("{}\n{}", key(2, "x"), key(3, "y")),
            key(2, "x").replace("ssh-ed25519", "ssh-rsa"),
            "ssh-ed25519 AAAA".to_string(),
        ] {
            let mut inv = inventory();
            assert!(enroll_keys(&mut inv, Some(&json!({"hazel": line})), PASSWD).is_err(), "{line:?}");
            assert_eq!(inv, inventory());
        }
    }

    #[test]
    fn a_key_bound_to_another_principal_is_refused() {
        let mut inv = inventory();
        assert!(enroll_keys(&mut inv, Some(&json!({"hazel": key(1, "copied")})), PASSWD).is_err());
        assert_eq!(inv, inventory());
    }

    #[test]
    fn enrolment_is_bounded() {
        let mut inv = inventory();
        for n in 0..MAX_PEERS - 1 {
            inv["peers"][format!("p{n}")] = json!({"user": format!("ibara-op-p{n}"), "uid": 800 + n, "public": "restrict ssh-ed25519 X\n", "agent_lines": []});
        }
        assert!(enroll_keys(&mut inv, Some(&json!({"hazel": key(2, "x")})), PASSWD).is_err());
    }

    #[test]
    fn an_unchanged_key_keeps_its_reviewed_agent_lines() {
        let mut inv = inventory();
        assert!(!enroll_keys(&mut inv, Some(&json!({"vesper": key(1, "another comment")})), PASSWD).unwrap());
        assert_eq!(inv, inventory());
    }

    #[test]
    fn a_replaced_key_loses_the_old_keys_agent_access() {
        let mut inv = inventory();
        assert!(enroll_keys(&mut inv, Some(&json!({"vesper": key(4, "new")})), PASSWD).unwrap());
        let peer = &inv["peers"]["vesper"];
        assert_eq!(peer["public"], format!("restrict {}\n", key(4, "new")));
        assert_eq!(peer["uid"], 959, "the account keeps its uid");
        let lines = peer["agent_lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].as_str().unwrap().ends_with(&key(4, "new")) && lines[0] != REVIEWED_LINE, "{lines:?}");
    }

    #[test]
    fn a_new_principal_gets_a_free_system_uid_and_its_own_agent_line() {
        let mut inv = inventory();
        assert!(enroll_keys(&mut inv, Some(&json!({"hazel": key(5, "hazel")})), PASSWD).unwrap());
        let peer = &inv["peers"]["hazel"];
        assert_eq!(peer["user"], "ibara-op-hazel");
        assert_eq!(peer["uid"], 998, "999 is sshd's, 959 is in use; the highest free system uid");
        assert_eq!(peer["public"], format!("restrict {}\n", key(5, "hazel")));
        assert_eq!(
            peer["agent_lines"],
            json!([format!("restrict,command=\"'/opt/agent-computer/current/ops/entry.sh' 'hazel' '/run/agent-computer/controller.sock' '/etc/agent-computer/gateway.key'\" {}", key(5, "hazel"))])
        );
    }

    #[test]
    fn a_request_without_keys_changes_nothing() {
        let mut inv = inventory();
        assert!(!enroll_keys(&mut inv, None, PASSWD).unwrap());
        assert!(!enroll_keys(&mut inv, Some(&Value::Null), PASSWD).unwrap());
        assert_eq!(inv, inventory());
    }
}
