//! The live wiring: the real controller behind the transport, start-up and
//! shutdown order (`server.ts:15-38,133-178,319-329`).

use super::policy::{Keys, Policy};
use super::{Engine, Lock, Paths, Server, authority, pairing};
use crate::desktop::run::Cancel;
use crate::error::Result;
use crate::mcp::CallOutcome;
use anyhow::Context;
use serde_json::{Value, json};
use std::os::unix::fs::DirBuilderExt;
use std::rc::Rc;

impl Engine for crate::controller::Controller {
    async fn access_sync(&self)->Result<()> {self.project_access().await}
    fn access_principal(&self, principal: &str) -> Option<bool> { Some(self.access_principal(principal).unwrap_or(false)) }
    fn access_pairing_key(&self, principal: &str) -> Option<String> { self.access_pairing_key(principal) }
    async fn access_pair(&self,p:&str,b:&Value,g:u64,r:&crate::access::PairRights)->Result<()> {self.access_pair(p,b,g,r).await}
    fn access_own_computer(&self,p:&str)->Result<()> {self.access_own_computer(p)}
    async fn access_unpair(&self,p:&str)->Result<()> {self.access_unpair(p).await}
    fn epoch(&self) -> String {
        crate::controller::Controller::epoch(self)
    }
    fn endpoint_id(&self) -> String {
        crate::controller::Controller::endpoint_id(self)
    }
    async fn call(&self, principal: &str, connection_id: &str, client_name: &str, tool: &str, args: Value, cancel: Cancel) -> CallOutcome {
        crate::controller::Controller::call(self, principal, connection_id, client_name, tool, args, cancel).await
    }
    async fn heartbeat(&self, principal: &str, connection_id: &str, answering: bool) -> Result<()> {
        crate::controller::Controller::heartbeat(self, principal, connection_id, answering).await
    }
    async fn disconnect(&self, principal: &str, connection_id: &str) -> Result<()> {
        crate::controller::Controller::disconnect(self, principal, connection_id).await
    }
    async fn admin(&self, action: Value) -> Result<Value> {
        crate::controller::Controller::admin(self, action).await
    }
    async fn operator_call(&self, operator_id: &str, action: Value) -> Result<Value> {
        crate::controller::Controller::operator_call(self, operator_id, action).await
    }
    async fn transfer(&self, principal: &str, connection_id: &str, request: Value) -> Result<Value> {
        crate::controller::Controller::transfer(self, principal, connection_id, request).await
    }
    async fn revoke_viewer_operator(&self, operator_id: &str) -> Result<()> {
        crate::controller::Controller::revoke_viewer_operator(self, operator_id).await
    }
}

pub(super) async fn run_target(paths: Paths) -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
    for dir in [&paths.state_dir, &paths.data_dir, &paths.runtime_dir] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    let lock = Lock::acquire(&paths.lock())?;
    let outcome = serve_target(&paths, &lock, async {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    })
    .await;
    lock.release();
    outcome
}

async fn serve_target(paths: &Paths, lock: &Lock, stop: impl Future<Output = ()>) -> anyhow::Result<()> {
    use crate::controller::{Controller, ControllerOptions, LiveConfig};
    use crate::desktop::chrome::ChromeBridge;

    let policy = Policy::load(&paths.policy)?;
    let keys = Keys::load(&paths.gateway_key, &paths.admin_hash)?;
    let chrome = ChromeBridge::listen(&paths.chrome_socket()).await.context("listen on chrome.sock")?;
    let started = async {
        let config = LiveConfig {
            state_dir: paths.state_dir.clone(),
            data_dir: paths.data_dir.clone(),
            runtime_dir: paths.runtime_dir.clone(),
            install_root: paths.install_root.clone(),
            release_root: paths.release_root.clone(),
            procedures_dir: paths.procedures_dir.clone(),
            policy: policy.raw.clone(),
            operator_grants: authority::operator_grants_source(paths),
            chrome: Some(chrome.clone()),
        };
        let controller = Rc::new(Controller::new(ControllerOptions::live(config)?)?);
        controller.init_access(paths, &policy.raw)?;
        controller.start().await?;
        Ok::<_, crate::error::IbaraError>(controller)
    };
    let controller = match started.await {
        Ok(controller) => controller,
        Err(e) => {
            chrome.close().await;
            return Err(anyhow::anyhow!("{e}"));
        }
    };
    let server = Rc::new(Server::new(controller.clone(), paths.clone(), policy, keys));
    if let Err(e) = server.listen().await {
        server.close();
        controller.shutdown().await;
        chrome.close().await;
        return Err(anyhow::anyhow!("{e}"));
    }
    authority::harden_stored_operator_keys(&server).await;
    server.sync_operator_peers().await;
    eprintln!(
        "{}",
        json!({ "event": "controller_ready", "instanceId": lock.instance_id, "time": crate::ids::now_iso() })
    );
    let pairing = pairing::Pairing::start(server.clone());
    let peer_server=server.clone();
    let peers=tokio::task::spawn_local(async move {
        let mut tick=tokio::time::interval(std::time::Duration::from_secs(2));
        loop {tick.tick().await;peer_server.sync_operator_peers().await;}
    });
    let restart = tokio::select! {
        _ = stop => false,
        _ = controller.restart_requested() => true,
    };
    peers.abort();
    pairing.close();

    let _ = controller.system_pause().await;
    controller.shutdown().await;
    chrome.close().await;
    server.close();
    if restart {
        // A failed exit: the unit's Restart=on-failure starts ibarad again.
        return Err(anyhow::anyhow!("restarting at a person's request"));
    }
    Ok(())
}
