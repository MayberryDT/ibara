//! `ibarad` in the target role: a port of `server.ts`.
//!
//! Start-up order, as in the TypeScript controller: create the state, data
//! and runtime directories, take `controller.lock`, read `policy.json` and the
//! two keys, listen on `chrome.sock`, build and start the controller (epoch
//! rotation, input reset, pause, viewer fencing), listen on `controller.sock`
//! (0660) and `admin.sock` (0600), re-seal stored operator keys, open the
//! per-operator peer sockets, then print `controller_ready` on stderr.
//! SIGTERM or SIGINT: pause, shut the controller down (close, cancel jobs,
//! release input), close the Chrome bridge and every socket, remove the lock.

pub mod authority;
pub mod invites;
mod live;
pub mod pairing;
pub mod peer;
pub mod policy;
pub mod sockets;

use crate::desktop::run::Cancel;
use crate::error::Result;
use crate::mcp::CallOutcome;
use anyhow::Context;
use peer::SystemDb;
use policy::{Keys, Policy};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// What the transport needs from the engine. [`crate::controller::Controller`]
/// implements it; the transport tests use a stub.
#[allow(async_fn_in_trait)]
pub trait Engine {
    fn access_principal(&self, _principal: &str) -> Option<bool> { None }
    /// The key fingerprint of an active access pairing for `principal`.
    fn access_pairing_key(&self, _principal: &str) -> Option<String> { None }
    /// Pair `principal` with what `rights` allow (see [`crate::access::PairRights`]).
    async fn access_pair(&self, _principal:&str, _binding:&Value, _generation:u64, _rights:&crate::access::PairRights)->Result<()> {Ok(())}
    /// The same person's computer paired again with a key it already had.
    fn access_own_computer(&self, _principal:&str)->Result<()> {Ok(())}
    async fn access_unpair(&self, _principal:&str)->Result<()> {Ok(())}
    async fn access_sync(&self)->Result<()> {Ok(())}
    fn epoch(&self) -> String;
    fn endpoint_id(&self) -> String;
    /// One agent tool call. `cancel` fires when the caller hangs up; the
    /// engine stops at its next safe point and still returns an outcome,
    /// which the transport then discards. The future is always polled to the end.
    async fn call(&self, principal: &str, connection_id: &str, client_name: &str, tool: &str, args: Value, cancel: Cancel) -> CallOutcome;
    /// `answering`: whether the session's client answered its pings lately.
    async fn heartbeat(&self, principal: &str, connection_id: &str, answering: bool) -> Result<()>;
    async fn disconnect(&self, principal: &str, connection_id: &str) -> Result<()>;
    async fn admin(&self, action: Value) -> Result<Value>;
    async fn operator_call(&self, operator_id: &str, action: Value) -> Result<Value>;
    async fn transfer(&self, principal: &str, connection_id: &str, request: Value) -> Result<Value>;
    async fn revoke_viewer_operator(&self, operator_id: &str) -> Result<()>;
}

/// Paths and their environment overrides. An empty
/// variable counts as unset, like `process.env.X || default`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub install_root: PathBuf,
    /// The release directory holding `bin/` and `ops/` (`dist/..` in the TypeScript layout).
    pub release_root: PathBuf,
    pub procedures_dir: PathBuf,
    pub policy: PathBuf,
    pub gateway_key: PathBuf,
    pub admin_hash: PathBuf,
    pub operator_accounts: PathBuf,
    /// `IBARA_OPERATOR_ACCOUNTS` was set, which lets the controller's own uid own the file.
    pub operator_accounts_overridden: bool,
    pub operator_socket_dir: PathBuf,
    /// The restricted SSH entry's public host key, pinned by paired computers.
    pub ssh_host_key: PathBuf,
    /// The reviewed station (`display_label` names this computer to others).
    pub station: PathBuf,
}

pub fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_path(key: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    env_nonempty(key).map(PathBuf::from).unwrap_or_else(default)
}

/// `os.homedir()`: `$HOME`, else the passwd entry.
pub fn home_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = home_dir();
        let install_root = env_path("IBARA_INSTALL_ROOT", || "/opt/agent-computer".into());
        let release_root = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().and_then(Path::parent).map(Path::to_path_buf))
            .unwrap_or_else(|| install_root.join("current"));
        Paths {
            state_dir: env_path("IBARA_STATE_DIR", || home.join(".local/state/agent-computer")),
            data_dir: env_path("IBARA_DATA_DIR", || home.join(".local/share/agent-computer")),
            runtime_dir: env_path("IBARA_RUNTIME_DIR", || "/run/agent-computer".into()),
            procedures_dir: env_path("IBARA_PROCEDURES_DIR", || install_root.join("procedures-approved")),
            install_root,
            release_root,
            policy: env_path("IBARA_POLICY", || "/etc/agent-computer/policy.json".into()),
            gateway_key: env_path("IBARA_GATEWAY_KEY", || "/etc/agent-computer/gateway.key".into()),
            admin_hash: env_path("IBARA_ADMIN_HASH", || "/etc/agent-computer/admin.sha256".into()),
            operator_accounts_overridden: env_nonempty("IBARA_OPERATOR_ACCOUNTS").is_some(),
            operator_accounts: env_path("IBARA_OPERATOR_ACCOUNTS", || "/etc/agent-computer/operator-accounts.json".into()),
            operator_socket_dir: env_path("IBARA_OPERATOR_SOCKET_DIR", || "/run/ibara-operator".into()),
            ssh_host_key: env_path("IBARA_SSH_HOST_KEY", || "/etc/agent-computer/ssh/host_ed25519.pub".into()),
            station: env_path("IBARA_STATION_FILE", || "/etc/ibara/station.json".into()),
        }
    }
    pub fn lock(&self) -> PathBuf {
        self.state_dir.join("controller.lock")
    }
    pub fn authority(&self) -> PathBuf {
        self.state_dir.join("operator-authority.json")
    }
    pub fn operator_keys(&self) -> PathBuf {
        self.state_dir.join("operator-keys")
    }
    pub fn challenges(&self) -> PathBuf {
        self.state_dir.join("operator-challenges")
    }
    pub fn controller_socket(&self) -> PathBuf {
        self.runtime_dir.join("controller.sock")
    }
    pub fn admin_socket(&self) -> PathBuf {
        self.runtime_dir.join("admin.sock")
    }
    pub fn chrome_socket(&self) -> PathBuf {
        self.runtime_dir.join("chrome.sock")
    }
    /// This computer's own pairing answers (`ibara join`, the console on this computer).
    pub fn pairing_socket(&self) -> PathBuf {
        self.runtime_dir.join("pairing.sock")
    }
    /// Invites to share this computer with a friend (`invites`).
    pub fn invites(&self) -> PathBuf {
        self.state_dir.join("invites.json")
    }
}

/// The transport state shared by every socket.
pub struct Server<E> {
    pub engine: Rc<E>,
    pub paths: Paths,
    pub policy: Policy,
    keys: Keys,
    uid: u32,
    system: SystemDb,
    /// Operators whose stored key could not be kept private; their bearer is refused.
    blocked: RefCell<BTreeSet<String>>,
    peers: RefCell<BTreeMap<String, sockets::Listening>>,
    main: RefCell<Vec<sockets::Listening>>,
    /// Serialises every change to `operator-authority.json`.
    authority_lock: tokio::sync::Mutex<()>,
}

impl<E: Engine + 'static> Server<E> {
    pub fn new(engine: Rc<E>, paths: Paths, policy: Policy, keys: Keys) -> Server<E> {
        Server::with_system(engine, paths, policy, keys, SystemDb::default())
    }

    /// As [`Server::new`], with passwd and group read from `system`.
    pub fn with_system(engine: Rc<E>, paths: Paths, policy: Policy, keys: Keys, system: SystemDb) -> Server<E> {
        Server {
            engine,
            paths,
            policy,
            keys,
            uid: current_uid(),
            system,
            blocked: RefCell::new(BTreeSet::new()),
            peers: RefCell::new(BTreeMap::new()),
            main: RefCell::new(Vec::new()),
            authority_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Listen on `controller.sock` (0660) and `admin.sock` (0600). Needs a `LocalSet`.
    pub async fn listen(self: &Rc<Self>) -> Result<()> {
        let gateway = sockets::listen(self, &self.paths.controller_socket(), 0o660, sockets::Kind::Gateway).await?;
        self.main.borrow_mut().push(gateway);
        let admin = sockets::listen(self, &self.paths.admin_socket(), 0o600, sockets::Kind::Admin).await?;
        self.main.borrow_mut().push(admin);
        Ok(())
    }

    /// Open, keep or close the per-operator peer sockets (`syncOperatorPeers`).
    pub async fn sync_operator_peers(self: &Rc<Self>) {
        sockets::sync_operator_peers(self).await;
    }

    /// Stop accepting on every socket and remove the socket files.
    pub fn close(&self) {
        for listening in self.main.borrow_mut().drain(..) {
            listening.close();
        }
        let peers = std::mem::take(&mut *self.peers.borrow_mut());
        for (_, listening) in peers {
            listening.close();
        }
    }
}

pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `controller.lock`: `{"pid","instanceId"}`, created exclusively, 0600; a
/// lock left by a dead process is replaced (`server.ts:21-32`).
pub struct Lock {
    path: PathBuf,
    pub instance_id: String,
}

impl Lock {
    pub fn acquire(path: &Path) -> anyhow::Result<Lock> {
        let instance_id = crate::ids::id("instance");
        let body = json!({ "pid": std::process::id(), "instanceId": instance_id }).to_string();
        if create_exclusive(path, body.as_bytes()).is_err() {
            let old: Value = serde_json::from_slice(&std::fs::read(path).context("read controller.lock")?)
                .context("parse controller.lock")?;
            let alive = match old.get("pid").and_then(Value::as_i64).and_then(|pid| i32::try_from(pid).ok()) {
                // SAFETY: signal 0 only checks that the process exists.
                Some(pid) if pid > 0 => {
                    let status = unsafe { libc::kill(pid, 0) };
                    status == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                }
                _ => true,
            };
            if alive {
                anyhow::bail!("A controller already owns the state directory.");
            }
            std::fs::remove_file(path).context("remove stale controller.lock")?;
            create_exclusive(path, body.as_bytes()).context("write controller.lock")?;
        }
        Ok(Lock { path: path.to_path_buf(), instance_id })
    }

    /// Remove the lock if it is still ours.
    pub fn release(&self) {
        let ours = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|lock| lock.get("instanceId").and_then(Value::as_str) == Some(&self.instance_id));
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Write a new file with mode 0600, refusing to replace an existing one (`flag: 'wx'`).
pub fn create_exclusive(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)
}

/// `ibarad [--role target|operator]`. Returns the exit code.
pub fn ibarad_main(args: Vec<String>) -> i32 {
    let role = match args.as_slice() {
        [] => "target",
        [flag, role] if flag == "--role" => role.as_str(),
        [flag] if flag.starts_with("--role=") => &flag["--role=".len()..],
        _ => {
            eprintln!("Usage: ibarad [--role target|operator]");
            return 64;
        }
    };
    match role {
        "target" => {}
        "operator" => return crate::console::main(),
        other => {
            eprintln!("ibarad: unknown role {other}; expected target or operator.");
            return 64;
        }
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("ibarad: {e}");
            return 1;
        }
    };
    let local = tokio::task::LocalSet::new();
    let code = match local.block_on(&runtime, live::run_target(Paths::from_env())) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", json!({ "event": "controller_failed", "error": format!("{e:#}") }));
            1
        }
    };
    drop(local);
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    code
}

