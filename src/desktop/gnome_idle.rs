//! Connection-owned GNOME idle inhibition; never changes desktop preferences.
use super::gnome::Gnome;
use crate::error::{Result,IbaraError};
use std::{sync::Arc,ffi::OsString,time::Duration,os::unix::fs::MetadataExt};
use tokio::sync::Mutex;
use zbus::{Connection,Proxy};
const SERVICE:&str="org.gnome.SessionManager";
const PATH:&str="/org/gnome/SessionManager";
fn unavailable(message:impl Into<String>)->IbaraError {IbaraError::new("CAPABILITY_UNAVAILABLE",message,true)}
struct Lease {connection:Option<Connection>,owner:String,cookie:u32}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(connection)=self.connection.take() {
            if let Ok(runtime)=tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {let _=connection.close().await;});
            }
        }
    }
}
#[derive(Default)]
struct State {lease:Option<Lease>,persistent:bool}
pub struct Idle {gnome:Gnome,state:Mutex<State>}
impl Idle {
    pub fn new(env:Arc<[(OsString,OsString)]>)->Self {Self {gnome:Gnome::new(env),state:Mutex::new(State::default())}}
    async fn owner(connection:&Connection)->Result<String> {
        let bus=Proxy::new(connection,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus").await.map_err(|e|unavailable(e.to_string()))?;
        let owner:String=bus.call("GetNameOwner",&(SERVICE,)).await.map_err(|e|unavailable(e.to_string()))?;
        let uid:u32=bus.call("GetConnectionUnixUser",&(&owner,)).await.map_err(|e|unavailable(e.to_string()))?;
        let pid:u32=bus.call("GetConnectionUnixProcessID",&(&owner,)).await.map_err(|e|unavailable(e.to_string()))?;
        let executable=std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|e|unavailable(e.to_string()))?;
        let expected=std::path::Path::new("/usr/libexec/gnome-session-service");
        let metadata=std::fs::metadata(expected).map_err(|e|unavailable(e.to_string()))?;
        if uid!=unsafe{libc::geteuid()} || executable!=expected || metadata.uid()!=0 || metadata.mode()&0o022!=0 {
            return Err(unavailable("GNOME idle service identity was not proved."));
        }
        let proxy=Proxy::new(connection,owner.as_str(),PATH,SERVICE).await.map_err(|e|unavailable(e.to_string()))?;
        let active:bool=proxy.get_property("SessionIsActive").await.map_err(|e|unavailable(e.to_string()))?;
        if !active {return Err(unavailable("GNOME idle service has no active session."));}
        Ok(owner)
    }
    pub async fn status(&self)->Result<()> {
        tokio::time::timeout(Duration::from_secs(3),async {
            let connection=self.gnome.connection().await?;
            let owner=Self::owner(&connection).await?;
            let state=self.state.lock().await;
            if state.lease.as_ref().is_some_and(|lease|lease.owner!=owner) {return Err(unavailable("GNOME idle service changed; inhibitor ownership was lost."));}
            Ok(())
        }).await.map_err(|_|unavailable("GNOME idle status timed out."))?
    }
    pub async fn set_inhibited(&self,active:bool)->Result<()> {self.set(active,false).await.map(|_|())}
    pub async fn keep_awake(&self)->Result<bool> {self.set(true,true).await}
    async fn set(&self,active:bool,persistent:bool)->Result<bool> {
        tokio::time::timeout(Duration::from_secs(3),async {
            let mut state=self.state.lock().await;
            if !active && state.persistent {return Ok(false);}
            if active {
                if let Some(lease)=&state.lease {
                    let connection=lease.connection.as_ref().unwrap();
                    if Self::owner(connection).await?==lease.owner {
                        state.persistent|=persistent;return Ok(false);
                    }
                    // A replaced unique owner cannot inherit this cookie.
                    state.lease=None;
                }
                let connection=self.gnome.connection().await?;
                let owner=Self::owner(&connection).await?;
                let proxy=Proxy::new(&connection,owner.as_str(),PATH,SERVICE).await.map_err(|e|unavailable(e.to_string()))?;
                let cookie:u32=proxy.call("Inhibit",&("io.zet.ibara",0u32,"ibara agent work",8u32)).await.map_err(|e|unavailable(e.to_string()))?;
                drop(proxy);
                state.lease=Some(Lease {connection:Some(connection),owner,cookie});
                state.persistent|=persistent;return Ok(true);
            }
            if let Some(mut lease)=state.lease.take() {
                let connection=lease.connection.take().unwrap();
                let result=async {
                    let proxy=Proxy::new(&connection,lease.owner.as_str(),PATH,SERVICE).await.map_err(|e|unavailable(e.to_string()))?;
                    proxy.call::<_,_,()>("Uninhibit",&(lease.cookie,)).await.map_err(|e|unavailable(e.to_string()))
                }.await;
                let _=connection.close().await;
                result?;
            }
            Ok(false)
        }).await.map_err(|_|unavailable("GNOME idle ownership transition timed out."))?
    }
}
