//! Isolated read-only AT-SPI observation; GNOME input stays in the Mutter guard.
use super::{Window,atspi::{Tree,RawElement},run::{self,Cmd,Cancel}};
use crate::error::{Result,IbaraError};
use serde_json::{Value,json};
use std::{ffi::OsString,collections::{HashSet,VecDeque},time::Duration};
use zbus::{Connection,Proxy,zvariant::OwnedObjectPath};
type Reference=(String,OwnedObjectPath);
const ACCESSIBLE:&str="org.a11y.atspi.Accessible";
fn refused(message:impl Into<String>)->IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE",message,true).with("execution_not_started",true)
}
pub async fn read(window:&Window,env:&[(OsString,OsString)],cancel:Option<&Cancel>)->Result<Tree> {
    let exe=std::env::current_exe().map_err(|e|refused(e.to_string()))?;
    let mut command=Cmd::new(exe).args(["native-elements".to_owned(),serde_json::to_string(window).map_err(|e|refused(e.to_string()))?])
        .envs(env).timeout(Duration::from_secs(6)).max_output(1024*1024);
    if let Some(cancel)=cancel {command=command.cancel(cancel);}
    let output=run::run(command).await?;
    if !output.success() {return Err(refused(output.failure_text("native elements read")));}
    serde_json::from_slice(&output.stdout).map_err(|_|refused("Invalid native element observation"))
}

/// Read-only capability probe; never inspects an application's text or controls.
pub async fn probe(env:std::sync::Arc<[(OsString,OsString)]>)->Result<bool> {
    tokio::time::timeout(Duration::from_secs(3),async {
        let session=super::gnome::Gnome::new(env).connection().await?;
        let bus=Proxy::new(&session,"org.a11y.Bus","/org/a11y/bus","org.a11y.Bus").await.map_err(|e|refused(e.to_string()))?;
        let address:String=bus.call("GetAddress",&()).await.map_err(|e|refused(e.to_string()))?;
        if address.is_empty() || address.len()>4096 {return Err(refused("Invalid accessibility bus address"));}
        let connection=zbus::connection::Builder::address(address.as_str()).map_err(|e|refused(e.to_string()))?
            .build().await.map_err(|e|refused(e.to_string()))?;
        let dbus=Proxy::new(&connection,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus").await.map_err(|e|refused(e.to_string()))?;
        let owner:String=dbus.call("GetNameOwner",&("org.a11y.atspi.Registry",)).await.map_err(|e|refused(e.to_string()))?;
        let uid:u32=dbus.call("GetConnectionUnixUser",&(&owner,)).await.map_err(|e|refused(e.to_string()))?;
        Ok(uid==unsafe{libc::geteuid()})
    }).await.map_err(|_|refused("Accessibility bus probe timed out"))?
}

struct Reader {connection:Connection,guid:String,owner:String,root:OwnedObjectPath,window:Window,screen_coords:bool}
impl Reader {
    fn client_rect(&self)->std::result::Result<[i32;4],String> {
        let [x,y,w,h]=self.window.client_rect.unwrap_or([self.window.at[0],self.window.at[1],self.window.size[0],self.window.size[1]]);
        let frame=self.window.geometry();
        if w<=0 || h<=0 || x<frame.x || y<frame.y ||
            x.checked_add(w).is_none_or(|end|frame.x.checked_add(frame.width).is_none_or(|right|end>right)) ||
            y.checked_add(h).is_none_or(|end|frame.y.checked_add(frame.height).is_none_or(|bottom|end>bottom)) {
            return Err("Invalid compositor client rectangle".into());
        }
        Ok([x,y,w,h])
    }
    async fn proxy<'a>(&'a self,path:&'a OwnedObjectPath,interface:&'a str)->std::result::Result<Proxy<'a>,String> {
        // Qt's AT-SPI bridge supports Get but not GetAll. Each observation
        // also needs current property values rather than a cached snapshot.
        zbus::proxy::Builder::new(&self.connection)
            .destination(self.owner.as_str()).map_err(|e|e.to_string())?
            .path(path.as_str()).map_err(|e|e.to_string())?
            .interface(interface).map_err(|e|e.to_string())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build().await.map_err(|e|e.to_string())
    }
    async fn name(&self,path:&OwnedObjectPath)->std::result::Result<String,String> {
        let name:String=self.proxy(path,ACCESSIBLE).await?.get_property("Name").await.map_err(|e|e.to_string())?;
        if name.len()>4096 {return Err("Native element name exceeds its limit".into());}Ok(name)
    }
    async fn bounds(&self,path:&OwnedObjectPath)->std::result::Result<[i32;4],String> {
        let component = self.proxy(path,"org.a11y.atspi.Component").await?;
        // GTK Wayland does not expose meaningful SCREEN positions. Qt's
        // WINDOW coordinates instead use a nested dialog as their origin.
        if !self.screen_coords {
            let (x,y,w,h):(i32,i32,i32,i32)=component.call("GetExtents",&(1u32,)).await.map_err(|e|e.to_string())?;
            return Ok([x,y,w,h]);
        }
        // Normalize Qt SCREEN extents to the exact verified top-level.
        let (x,y,w,h):(i32,i32,i32,i32)=component.call("GetExtents",&(0u32,)).await.map_err(|e|e.to_string())?;
        let (root_x,root_y,_,_):(i32,i32,i32,i32)=self.proxy(&self.root,"org.a11y.atspi.Component").await?
            .call("GetExtents",&(0u32,)).await.map_err(|e|e.to_string())?;
        Ok([x.checked_sub(root_x).ok_or("Invalid native x extent")?,y.checked_sub(root_y).ok_or("Invalid native y extent")?,w,h])
    }
    async fn children(&self,path:&OwnedObjectPath)->std::result::Result<Vec<Reference>,String> {
        let children:Vec<Reference>=self.proxy(path,ACCESSIBLE).await?.call("GetChildren",&()).await.map_err(|e|e.to_string())?;
        if children.len()>4096 {return Err("Native child count exceeds its limit".into());}
        if children.iter().any(|(owner,path)|owner!=&self.owner || path.as_str()=="/org/a11y/atspi/null" || path.as_str().len()>4096) {
            return Err("Cross-process or invalid native child reference".into());
        }
        Ok(children)
    }
    async fn open(window:Window)->std::result::Result<Self,String> {
        let session=zbus::connection::Builder::session().map_err(|e|e.to_string())?
            .method_timeout(Duration::from_millis(500)).build().await.map_err(|e|e.to_string())?;
        let bus=Proxy::new(&session,"org.a11y.Bus","/org/a11y/bus","org.a11y.Bus").await.map_err(|e|e.to_string())?;
        let address:String=bus.call("GetAddress",&()).await.map_err(|e|e.to_string())?;
        let connection=zbus::connection::Builder::address(address.as_str()).map_err(|e|e.to_string())?
            .method_timeout(Duration::from_millis(500)).build().await.map_err(|e|e.to_string())?;
        let dbus=Proxy::new(&connection,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus").await.map_err(|e|e.to_string())?;
        let guid:String=dbus.call("GetId",&()).await.map_err(|e|e.to_string())?;
        let registry=Proxy::new(&connection,"org.a11y.atspi.Registry","/org/a11y/atspi/accessible/root",ACCESSIBLE).await.map_err(|e|e.to_string())?;
        let apps:Vec<Reference>=registry.call("GetChildren",&()).await.map_err(|e|e.to_string())?;
        if apps.len()>512 {return Err("Native application count exceeds its limit".into());}
        let mut matches=Vec::new();
        for (app,path) in apps {
            let owner:String=dbus.call("GetNameOwner",&(app,)).await.map_err(|e|e.to_string())?;
            let pid:u32=dbus.call("GetConnectionUnixProcessID",&(&owner,)).await.map_err(|e|e.to_string())?;
            if pid as i64!=window.pid {continue;}
            let mut reader=Self {connection:connection.clone(),guid:guid.clone(),owner,root:path.clone(),window:window.clone(),screen_coords:false};
            let toolkit:String=reader.proxy(&path,"org.a11y.atspi.Application").await?.get_property("ToolkitName").await.map_err(|e|e.to_string())?;
            reader.screen_coords=toolkit.eq_ignore_ascii_case("qt");
            for (_,path) in reader.children(&path).await? {
                let candidate=Self {root:path.clone(),..reader.clone()};
                let bounds=candidate.bounds(&path).await?;
                let client=reader.client_rect()?;
                if reader.name(&path).await?==window.title && bounds==[0,0,client[2],client[3]] {
                    matches.push(Self {root:path,..reader.clone()});
                }
            }
        }
        if matches.len()!=1 {return Err("Native top-level identity was not proved uniquely".into());}
        Ok(matches.pop().unwrap())
    }
    fn clone(&self)->Self {Self {connection:self.connection.clone(),guid:self.guid.clone(),owner:self.owner.clone(),root:self.root.clone(),window:self.window.clone(),screen_coords:self.screen_coords}}
    async fn tree(&self)->std::result::Result<Tree,String> {
        let mut tree=Tree {available:true,..Tree::default()};
        let mut seen=HashSet::new();
        let mut queue=VecDeque::from([(self.root.clone(),Vec::<String>::new(),String::new())]);
        let mut text_left=12000usize;
        let mut observed_bytes=0usize;
        while let Some((path,chain,ancestor))=queue.pop_front() {
            if tree.elements.len()>=512 {tree.truncated=true;break;}
            if ancestor.len()>8192 || chain.iter().map(String::len).sum::<usize>()>8192 || chain.len()>40 || !seen.insert(path.clone()) {return Err("Native tree is cyclic or exceeds depth limit".into());}
            let node=self.proxy(&path,ACCESSIBLE).await?;
            let role:String=node.call("GetRoleName",&()).await.map_err(|e|e.to_string())?;
            if role.len()>128 {return Err("Invalid native element role".into());}
            let name=self.name(&path).await?;
            let bits:Vec<u32>=node.call("GetState",&()).await.map_err(|e|e.to_string())?;
            if bits.len()>8 {return Err("Invalid native state mask".into());}
            let has=|state:usize|bits.get(state/32).is_some_and(|bits|bits&(1<<(state%32))!=0);
            if has(6) {return Err("Native tree changed during observation".into());}
            let states=[(1,"active"),(7,"editable"),(8,"enabled"),(11,"focusable"),(12,"focused"),
                (16,"modal"),(23,"selected"),(24,"sensitive"),(25,"showing"),(30,"visible")].into_iter()
                .filter(|(state,_)|has(*state)).map(|(_,name)|name.to_owned()).collect::<Vec<_>>();
            let interfaces:Vec<String>=node.call("GetInterfaces",&()).await.map_err(|e|e.to_string())?;
            let mut text=String::new();
            if interfaces.iter().any(|i|i=="org.a11y.atspi.Text") && !role.to_lowercase().contains("password") && text_left>0 {
                let proxy=self.proxy(&path,"org.a11y.atspi.Text").await?;
                let count:i32=proxy.get_property("CharacterCount").await.map_err(|e|e.to_string())?;
                if !(0..=131072).contains(&count) {return Err("Native text count exceeds its limit".into());}
                let end=(count as usize).min(text_left);
                text=proxy.call("GetText",&(0i32,end as i32)).await.map_err(|e|e.to_string())?;
                if text.chars().count()>end {return Err("Native text exceeded requested bounds".into());}
                text_left-=text.chars().count();
                if !text.is_empty() {tree.text.push_str(&text);tree.text.push('\n');}
            }
            let mut actions=Vec::new();
            if interfaces.iter().any(|i|i=="org.a11y.atspi.Action") {
                let descriptions:Vec<(String,String,String)>=self.proxy(&path,"org.a11y.atspi.Action").await?
                    .call("GetActions",&()).await.map_err(|e|e.to_string())?;
                if descriptions.len()>32 {tree.truncated=true;}
                actions=descriptions.into_iter().take(32).map(|action|action.0).collect();
            }
            let frame=if interfaces.iter().any(|i|i=="org.a11y.atspi.Component") {Some(self.bounds(&path).await?)} else {None};
            let selector=json!({"backend":"gnome-atspi","bus_guid":self.guid,"owner":self.owner,"window_path":self.root.as_str(),
                "object_path":path.as_str(),"chain":chain,"pid":self.window.pid,"window_rect":[self.window.at[0],self.window.at[1],self.window.size[0],self.window.size[1]],"client_rect":self.client_rect()?,"local_frame":frame,"coordinate_basis":if self.screen_coords {"screen_top_level"} else {"window"}});
            observed_bytes+=selector.to_string().len()+ancestor.len()+name.len()+role.len()+text.len()+actions.iter().map(String::len).sum::<usize>();
            if observed_bytes>900000 {return Err("Native observation exceeds its byte limit".into());}
            tree.elements.push(RawElement {selector,role:role.clone(),name:name.clone(),states,ancestor:ancestor.clone(),actions,text});
            let mut next_chain=chain;next_chain.push(path.as_str().to_owned());
            let next_ancestor=if ancestor.is_empty() {format!("{role}:{name}")} else {format!("{ancestor}/{role}:{name}")};
            for (_,child) in self.children(&path).await? {
                if tree.elements.len()+queue.len()>=512 {tree.truncated=true;break;}
                queue.push_back((child,next_chain.clone(),next_ancestor.clone()));
            }
        }
        tree.returned_count=tree.elements.len() as u64;
        tree.available_count=(!tree.truncated).then_some(tree.returned_count);
        Ok(tree)
    }
}

pub async fn main(args:&[String])->u8 {
    let result=async {
        if args.len()!=1 {return Err("Expected native-elements WINDOW_JSON".into());}
        let window:Window=serde_json::from_str(&args[0]).map_err(|_|"Invalid native window")?;
        if window.pid<=0 || window.title.is_empty() || window.size.iter().any(|n|*n<=0 || *n>16384) {return Err("Invalid native window identity".into());}
        let reader=Reader::open(window).await?;reader.tree().await
    };
    match tokio::time::timeout(Duration::from_secs(5),result).await {
        Ok(Ok(tree))=>{println!("{}",serde_json::to_string(&tree).unwrap());0},
        Ok(Err(error))=>{eprintln!("{error}");1},
        Err(_)=>{eprintln!("Native observation timed out before proving completeness");1},
    }
}

pub fn center(tree:Tree,selector:&Value,role:&str,name:&str,window:&Window)->Result<(f64,f64)> {
    let gone=||IbaraError::new("STALE_TARGET","Native element identity or geometry changed; observe again.",true).with("execution_not_started",true);
    let matches=tree.elements.iter().filter(|node|&node.selector==selector && node.role==role && node.name==name).collect::<Vec<_>>();
    if matches.len()!=1 {return Err(gone());}
    let node=matches[0];
    if !node.states.iter().any(|s|s=="enabled" || s=="sensitive") || !["showing","visible"].iter().all(|s|node.states.iter().any(|state|state==s)) {return Err(gone());}
    let frame: [i32;4]=serde_json::from_value(selector["local_frame"].clone()).map_err(|_|gone())?;
    let [x,y,w,h]=frame;
    let client: [i32;4]=serde_json::from_value(selector["client_rect"].clone()).map_err(|_|gone())?;
    if client!=window.client_rect.unwrap_or([window.at[0],window.at[1],window.size[0],window.size[1]]) {return Err(gone());}
    if x<0 || y<0 || w<=0 || h<=0 || x.checked_add(w).is_none_or(|end|end>client[2]) || y.checked_add(h).is_none_or(|end|end>client[3]) {return Err(gone());}
    Ok((client[0] as f64+x as f64+w as f64/2.0,client[1] as f64+y as f64+h as f64/2.0))
}
