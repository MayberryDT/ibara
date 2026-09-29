//! Routes to a target: the exact ssh argv of `controller/agent/ibara-transport`.
//!
//! Selected routes (`selected-mcp`, `selected-transfer`, `selected-operator`) pin
//! everything: no ssh config, no agent, no forwarding, the per-endpoint known-hosts
//! file matched through `HostKeyAlias=<endpoint_id>`, and credential files that
//! must be private, user-owned and reached without symbolic links. The legacy
//! transfer route (no selected computer) keeps the single-station descriptor behaviour.

use super::directory::SelectedEnvelope;
use super::{current_uid, fail, home_dir, pattern, resolve_path, unsafe_file};
use crate::error::Result;
use base64::Engine;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The ssh program; resolved through `PATH` like `spawnSync("ssh", …)`.
pub const SSH: &str = "ssh";

/// Which selected route (`ibara-transport` argv[1]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// `selected-mcp` → login `ibara-agent`, remote command `mcp`.
    Mcp,
    /// `selected-transfer` → login `ibara-agent`, remote command `transfer-v1`.
    Transfer,
    /// `selected-operator` → login `ibara-op-<principal>`, remote command `operator-v1`.
    Operator,
}

impl RouteKind {
    pub fn remote_command(self) -> &'static str {
        match self {
            RouteKind::Mcp => "mcp",
            RouteKind::Transfer => "transfer-v1",
            RouteKind::Operator => "operator-v1",
        }
    }

    /// The enrolled `route.user` is the logical operator identity, not an SSH login.
    /// Operator calls use their dedicated account; MCP and transfer keep the
    /// forced-command agent gateway account with the same pinned key (ibara-transport:50-57).
    pub fn login(self, route_user: &str) -> Result<String> {
        match self {
            RouteKind::Operator => {
                if !pattern::principal(route_user) {
                    return Err(fail("Verified operator principal cannot select its restricted account."));
                }
                Ok(format!("ibara-op-{route_user}"))
            }
            RouteKind::Mcp | RouteKind::Transfer => Ok("ibara-agent".to_string()),
        }
    }
}

/// `credentialFile(reference, privateKey)` (ibara-transport:28-45): a `file:/` reference,
/// every component reached without a symbolic link, the file owned by this user and
/// either private (identity key) or not group/other writable (known-hosts).
pub fn credential_file(reference: &str, private_key: bool) -> Result<PathBuf> {
    let Some(raw) = reference.strip_prefix("file:").filter(|rest| rest.starts_with('/')) else {
        return Err(fail("Selected route credential reference is unsupported."));
    };
    let filename = resolve_path(Path::new(raw));
    let mut cursor = PathBuf::from("/");
    for part in filename.iter().skip(1) {
        cursor.push(part);
        let stat = fs::symlink_metadata(&cursor).map_err(|_| fail("Selected route credential reference is unavailable."))?;
        if stat.file_type().is_symlink() {
            return Err(unsafe_file("Selected route credential reference cannot traverse symbolic links."));
        }
        if cursor != filename && !stat.is_dir() {
            return Err(fail("Selected route credential path is invalid."));
        }
    }
    let stat = fs::symlink_metadata(&filename).map_err(|_| fail("Selected route credential reference is unavailable."))?;
    let mode_ok = if private_key { stat.mode() & 0o077 == 0 } else { stat.mode() & 0o022 == 0 };
    if !stat.is_file() || stat.uid() != current_uid() || !mode_ok {
        return Err(unsafe_file(if private_key {
            "Identity key must be a private file owned by this user."
        } else {
            "Known-hosts file must be owned by this user and not writable by others."
        }));
    }
    Ok(filename)
}

/// The selected ssh argv after the program name (ibara-transport:58-84), given the
/// checked identity and known-hosts paths.
pub fn selected_ssh_args_with(kind: RouteKind, envelope: &SelectedEnvelope, key: &Path, known_hosts: &Path) -> Result<Vec<String>> {
    let login = kind.login(&envelope.route.user)?;
    let mut args: Vec<String> = ["-F", "/dev/null", "-T"].map(String::from).to_vec();
    let options = [
        "BatchMode=yes".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "GlobalKnownHostsFile=/dev/null".to_string(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        format!("HostKeyAlias={}", envelope.endpoint_id),
        "IdentitiesOnly=yes".to_string(),
        "IdentityAgent=none".to_string(),
        "ForwardAgent=no".to_string(),
        "ClearAllForwardings=yes".to_string(),
        "CanonicalizeHostname=no".to_string(),
        "ProxyCommand=none".to_string(),
        "ProxyJump=none".to_string(),
        "PermitLocalCommand=no".to_string(),
        "KbdInteractiveAuthentication=no".to_string(),
        "PasswordAuthentication=no".to_string(),
        "PreferredAuthentications=publickey".to_string(),
        "ConnectTimeout=10".to_string(),
        "ServerAliveInterval=15".to_string(),
        "ServerAliveCountMax=3".to_string(),
    ];
    for option in options {
        args.push("-o".into());
        args.push(option);
    }
    args.extend([
        "-i".into(),
        key.display().to_string(),
        "-p".into(),
        envelope.route.port.to_string(),
        "-l".into(),
        login,
        envelope.route.host.clone(),
        kind.remote_command().into(),
    ]);
    Ok(args)
}

/// The selected ssh argv, checking both credential files first (identity key, then known-hosts).
pub fn selected_ssh_args(kind: RouteKind, envelope: &SelectedEnvelope) -> Result<Vec<String>> {
    let key = credential_file(&envelope.route.identity_file_ref, true)?;
    let known_hosts = credential_file(&envelope.route.known_hosts_file_ref, false)?;
    selected_ssh_args_with(kind, envelope, &key, &known_hosts)
}

/// `Buffer.from(JSON.stringify(envelope)).toString('base64url')`.
pub fn encode_envelope(envelope: &SelectedEnvelope) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(envelope.to_json().to_string())
}

/// Decode and validate the `ibara-transport selected-* <b64>` argument (ibara-transport:22-25).
pub fn decode_envelope(encoded: &str) -> Result<SelectedEnvelope> {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let bytes = engine.decode(encoded).ok().filter(|b| !encoded.is_empty() && engine.encode(b) == encoded);
    let Some(bytes) = bytes else {
        return Err(fail("Invalid selected route envelope encoding."));
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| fail(e.to_string()))?;
    super::directory::validate_selected_envelope(&value)
}

/// The ssh command for a selected route. `IBARA_TRANSPORT_ROUTE_V1` is set in ssh's
/// local environment only; nothing forwards it (no `SendEnv`).
pub fn selected_command(kind: RouteKind, envelope: &SelectedEnvelope) -> Result<Command> {
    let args = selected_ssh_args(kind, envelope)?;
    let mut command = Command::new(SSH);
    command.args(args).env("IBARA_TRANSPORT_ROUTE_V1", encode_envelope(envelope));
    Ok(command)
}

/// `$IBARA_STATION_DESCRIPTOR` or `~/.config/ibara/station.json`.
pub fn station_descriptor_path() -> PathBuf {
    super::env_or("IBARA_STATION_DESCRIPTOR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config/ibara/station.json"))
}

/// The legacy station node (`station.node` matching `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`).
pub fn legacy_station_node(descriptor: &Path) -> Result<String> {
    let text = match fs::read_to_string(descriptor) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(fail("Station descriptor is required before a legacy transfer."));
        }
        Err(e) => return Err(e.into()),
    };
    let station: Value = super::parse_json(&text)?;
    match station.get("node") {
        Some(Value::String(node)) if pattern::node(node) => Ok(node.clone()),
        _ => Err(fail("Invalid station node.")),
    }
}

/// The legacy transfer argv (`legacySsh()`, ibara-client.mjs:12-22). Note the joined
/// `-oUserKnownHostsFile=` argument, as in the Node client.
pub fn legacy_transfer_ssh_args(home: &Path, node: &str) -> Vec<String> {
    let path = |rel: &str| home.join(rel).display().to_string();
    vec![
        "-F".into(), path(".ssh/config"), "-T".into(),
        "-o".into(), "BatchMode=yes".into(),
        "-o".into(), "StrictHostKeyChecking=yes".into(),
        "-o".into(), "IdentitiesOnly=yes".into(),
        "-o".into(), "ForwardAgent=no".into(),
        "-o".into(), "ConnectTimeout=10".into(),
        "-o".into(), "ServerAliveInterval=15".into(),
        "-o".into(), "ServerAliveCountMax=3".into(),
        format!("-oUserKnownHostsFile={}", path(".ssh/known_hosts_ibara")),
        "-i".into(), path(".ssh/ibara_agent_ed25519"),
        "-p".into(), "2222".into(),
        format!("ibara-agent@{node}"),
        "transfer-v1".into(),
    ]
}

/// Send `signal` to the process group led by `pid` (children are spawned with
/// `process_group(0)`, like Node's `detached: true`).
pub(crate) fn kill_group(pid: u32, signal: i32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 0
    {
        // SAFETY: kill has no memory-safety preconditions.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

/// Send `signal` to one process.
pub(crate) fn kill_pid(pid: u32, signal: i32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 0
    {
        // SAFETY: kill has no memory-safety preconditions.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::directory::tests::TempDir;
    use super::super::directory::{SelectedEnvelope, SelectedRoute};
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

    fn envelope(key: &Path, hosts: &Path) -> SelectedEnvelope {
        SelectedEnvelope {
            environment_id: "operator_x".into(),
            computer_id: "computer_7c3b2a19e8d4f6015b9a2c4d".into(),
            endpoint_id: "ibara_9ac2387e0000000000000000000000".into(),
            binding_revision: 1,
            request_id: "request_1".into(),
            expected_authorization_generation: 3,
            record_id: "mcp_1".into(),
            route: SelectedRoute {
                host: "tulip1".into(),
                user: "vesper".into(),
                port: 2222,
                identity_file_ref: format!("file:{}", key.display()),
                known_hosts_file_ref: format!("file:{}", hosts.display()),
            },
        }
    }

    fn file(path: &Path, mode: u32) {
        OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Golden argv from ibara-transport:58-84, written out by hand from the bash source.
    fn golden(login: &str, command: &str) -> String {
        format!(
            "-F /dev/null -T -o BatchMode=yes -o StrictHostKeyChecking=yes -o GlobalKnownHostsFile=/dev/null \
             -o UserKnownHostsFile=/s/kh -o HostKeyAlias=ibara_9ac2387e0000000000000000000000 -o IdentitiesOnly=yes \
             -o IdentityAgent=none -o ForwardAgent=no -o ClearAllForwardings=yes -o CanonicalizeHostname=no \
             -o ProxyCommand=none -o ProxyJump=none -o PermitLocalCommand=no -o KbdInteractiveAuthentication=no \
             -o PasswordAuthentication=no -o PreferredAuthentications=publickey -o ConnectTimeout=10 \
             -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -i /s/key -p 2222 -l {login} tulip1 {command}"
        )
    }

    #[test]
    fn selected_argv_equals_the_bash_transport_for_each_route() {
        let env = envelope(Path::new("/s/key"), Path::new("/s/kh"));
        let (key, kh) = (Path::new("/s/key"), Path::new("/s/kh"));
        let argv = |kind| selected_ssh_args_with(kind, &env, key, kh).unwrap().join(" ");
        assert_eq!(argv(RouteKind::Mcp), golden("ibara-agent", "mcp"));
        assert_eq!(argv(RouteKind::Transfer), golden("ibara-agent", "transfer-v1"));
        assert_eq!(argv(RouteKind::Operator), golden("ibara-op-vesper", "operator-v1"));
    }

    #[test]
    fn legacy_transfer_argv_equals_the_client_route() {
        let home = Path::new("/home/riley");
        assert_eq!(
            legacy_transfer_ssh_args(home, "tulip0").join(" "),
            "-F /home/riley/.ssh/config -T -o BatchMode=yes -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes \
             -o ForwardAgent=no -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
             -oUserKnownHostsFile=/home/riley/.ssh/known_hosts_ibara -i /home/riley/.ssh/ibara_agent_ed25519 \
             -p 2222 ibara-agent@tulip0 transfer-v1"
        );
    }

    #[test]
    fn operator_route_refuses_a_principal_that_cannot_name_an_account() {
        let mut env = envelope(Path::new("/s/key"), Path::new("/s/kh"));
        env.route.user = "Vesper".into();
        let err = selected_ssh_args_with(RouteKind::Operator, &env, Path::new("/k"), Path::new("/h")).err().unwrap();
        assert_eq!(err.message, "Verified operator principal cannot select its restricted account.");
        assert!(selected_ssh_args_with(RouteKind::Mcp, &env, Path::new("/k"), Path::new("/h")).is_ok());
    }

    #[test]
    fn identity_key_readable_by_group_is_refused() {
        let tmp = TempDir::new("cred-mode");
        let (key, kh) = (tmp.0.join("key"), tmp.0.join("kh"));
        file(&key, 0o640);
        file(&kh, 0o644);
        let err = selected_ssh_args(RouteKind::Mcp, &envelope(&key, &kh)).err().unwrap();
        assert_eq!(err.message, "Identity key must be a private file owned by this user.");
    }

    #[test]
    fn known_hosts_writable_by_group_is_refused() {
        let tmp = TempDir::new("cred-kh");
        let (key, kh) = (tmp.0.join("key"), tmp.0.join("kh"));
        file(&key, 0o600);
        file(&kh, 0o664);
        let err = selected_ssh_args(RouteKind::Mcp, &envelope(&key, &kh)).err().unwrap();
        assert_eq!(err.message, "Known-hosts file must be owned by this user and not writable by others.");
    }

    #[test]
    fn credential_reached_through_a_symlink_is_refused() {
        let tmp = TempDir::new("cred-link");
        let real = tmp.0.join("real");
        fs::create_dir(&real).unwrap();
        file(&real.join("key"), 0o600);
        file(&real.join("kh"), 0o600);
        symlink(&real, tmp.0.join("alias")).unwrap();
        let env = envelope(&tmp.0.join("alias/key"), &real.join("kh"));
        let err = selected_ssh_args(RouteKind::Mcp, &env).err().unwrap();
        assert_eq!(err.message, "Selected route credential reference cannot traverse symbolic links.");
        symlink(real.join("key"), tmp.0.join("keylink")).unwrap();
        let env = envelope(&tmp.0.join("keylink"), &real.join("kh"));
        let err = selected_ssh_args(RouteKind::Mcp, &env).err().unwrap();
        assert_eq!(err.message, "Selected route credential reference cannot traverse symbolic links.");
    }

    #[test]
    fn credential_owned_by_another_user_is_refused() {
        let tmp = TempDir::new("cred-owner");
        let key = tmp.0.join("key");
        file(&key, 0o600);
        // /etc/hostname or /etc/passwd: root-owned, not group/other writable.
        let env = envelope(&key, Path::new("/etc/passwd"));
        let err = selected_ssh_args(RouteKind::Mcp, &env).err().unwrap();
        assert_eq!(err.message, "Known-hosts file must be owned by this user and not writable by others.");
    }

    #[test]
    fn non_file_reference_is_refused() {
        assert_eq!(credential_file("ssh-agent:1", true).err().unwrap().message, "Selected route credential reference is unsupported.");
        assert_eq!(credential_file("file:relative", true).err().unwrap().message, "Selected route credential reference is unsupported.");
        assert_eq!(credential_file("file:/nonexistent/ibara/key", true).err().unwrap().message, "Selected route credential reference is unavailable.");
    }

    #[test]
    fn envelope_encoding_round_trips_and_rejects_non_canonical_text() {
        let env = envelope(Path::new("/s/key"), Path::new("/s/kh"));
        assert_eq!(decode_envelope(&encode_envelope(&env)).unwrap(), env);
        assert_eq!(decode_envelope("").err().unwrap().message, "Invalid selected route envelope encoding.");
        assert_eq!(decode_envelope("e30=").err().unwrap().message, "Invalid selected route envelope encoding.");
    }
}
