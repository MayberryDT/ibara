//! Read the actual focused native field in an isolated, bounded process.
//! Cua 0.28 cannot prove tree completeness. No input is sent by this helper.
use super::{run::{self, Cmd, Cancel}, Window};
use crate::error::{Result, IbaraError};
use serde::{Serialize, Deserialize};
use std::{ffi::OsString, time::Duration};
use zbus::{Connection, Proxy, zvariant::OwnedObjectPath};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub path: Vec<i32>, pub value: String, pub role: String, pub frame: [i32;4],
    #[serde(default)] pub window_frame: Option<[i32;4]>,
    #[serde(default)] pub caret: Option<i32>,
    #[serde(default)] pub selections: Option<Vec<[i32;2]>>,
    #[serde(default)] pub paragraphs: Option<Vec<String>>,
}
impl Field {
    /// Application relayout may change the rectangle after insertion. The
    /// isolated reader validates that fresh rectangle before returning it.
    pub fn confirms_insertion(&self, observed: &Field) -> bool {
        self.window_frame == observed.window_frame && self.path == observed.path && self.role == observed.role && self.value == observed.value &&
            self.caret == observed.caret && self.selections == observed.selections && self.paragraphs == observed.paragraphs
    }
    /// Characters an application added right after a confirmed insertion
    /// and left selected (GTK entry completion): typing replaces them and
    /// Return accepts them. `self` is the expected field after insertion.
    /// Any other difference stays unconfirmed.
    pub fn completion_after(&self, observed: &Field) -> Option<usize> {
        if self.window_frame != observed.window_frame || self.path != observed.path || self.role != observed.role
            || self.paragraphs.is_some() || observed.paragraphs.is_some() || self.selections.as_deref() != Some(&[]) { return None; }
        let caret = usize::try_from(self.caret?).ok()?;
        let split = self.value.char_indices().nth(caret).map(|(byte, _)| byte).unwrap_or(self.value.len());
        let (before, after) = self.value.split_at(split);
        let added = observed.value.chars().count().checked_sub(self.value.chars().count()).filter(|n| *n > 0)?;
        if observed.value.len() < before.len() + after.len() || !observed.value.starts_with(before) || !observed.value.ends_with(after) { return None; }
        let (start, end) = (i32::try_from(caret).ok()?, i32::try_from(caret + added).ok()?);
        let selected = matches!(observed.selections.as_deref(), Some([[a, b]]) if (*a, *b) == (start, end) || (*a, *b) == (end, start));
        (selected && (observed.caret == Some(start) || observed.caret == Some(end))).then_some(added)
    }
    pub fn after_insertion(&self, text: &str) -> Result<Field> {self.inserted(text,false)}
    /// Clipboard newlines split paragraphs; direct native text inserts soft
    /// breaks. Only inserted boundaries may change the sibling structure.
    pub fn after_paste(&self, text: &str) -> Result<Field> {self.inserted(text,true)}
    fn inserted(&self, text: &str, clipboard: bool) -> Result<Field> {
        let count=self.value.chars().count();
        let caret=self.caret.filter(|n| *n>=0 && *n as usize<=count)
            .ok_or_else(||refused("Native caret offset is unavailable."))?;
        let selections=self.selections.as_ref().ok_or_else(||refused("Native selection state is unavailable."))?;
        let (start,end)=match selections.as_slice() {
            []=>(caret,caret),
            [[a,b]] if *a>=0 && *b>=0 && *a as usize<=count && *b as usize<=count=>(*a.min(b),*a.max(b)),
            _=>return Err(refused("Exactly one valid native insertion location is required.")),
        };
        let offset=|n:i32| self.value.char_indices().nth(n as usize).map(|(byte,_)|byte).unwrap_or(self.value.len());
        let mut after=self.clone();
        after.value=format!("{}{}{}",&self.value[..offset(start)],text,&self.value[offset(end)..]);
        if after.value.chars().count()>131072 {return Err(refused("Resulting text exceeds native readback capacity."));}
        after.caret=Some(start+text.chars().count() as i32);
        after.selections=Some(Vec::new());
        let splits=clipboard && self.role=="paragraph" && text.contains('\n');
        if splits && after.paragraphs.is_none() {
            return Err(refused("Complete native paragraph context is required for multiline paste."));
        }
        if let Some(paragraphs)=&mut after.paragraphs {
            let index=*self.path.last().ok_or_else(||refused("Native paragraph path is unavailable."))? as usize;
            if paragraphs.get(index)!=Some(&self.value) {return Err(refused("Native paragraph context is inconsistent."));}
            if splits {
                let mut parts:Vec<String>=text.split('\n').map(str::to_owned).collect();
                let next=index+parts.len()-1;
                after.caret=Some(parts.last().unwrap().chars().count() as i32);
                parts[0]=format!("{}{}",&self.value[..offset(start)],parts[0]);
                parts.last_mut().unwrap().push_str(&self.value[offset(end)..]);
                paragraphs.splice(index..=index,parts);
                after.value=paragraphs[next].clone();
                *after.path.last_mut().unwrap()=next as i32;
            } else {paragraphs[index]=after.value.clone();}
            if paragraphs.len()>512 || paragraphs.iter().map(|p|p.chars().count()).sum::<usize>()>131072 {
                return Err(refused("Resulting paragraphs exceed native readback capacity."));
            }
        }
        Ok(after)
    }
}
fn refused(message: impl Into<String>) -> IbaraError { IbaraError::new("CAPABILITY_UNAVAILABLE", message, true).with("execution_not_started", true) }
pub async fn read(window: &Window, env: &[(OsString, OsString)], cancel: Option<&Cancel>) -> Result<Field> {
    read_with(window, env, cancel, None).await
}
/// After admitted input: only the field at `path` in the same proven
/// window, which must still be the focused editable field. Walking the whole
/// window again (toolbars, sidebar) loses replies while the app is busy
/// with the input. Uniqueness was proved by the full read before input.
pub async fn read_at(window: &Window, env: &[(OsString, OsString)], cancel: Option<&Cancel>, path: &[i32]) -> Result<Field> {
    read_with(window, env, cancel, Some(path)).await
}
async fn read_with(window: &Window, env: &[(OsString, OsString)], cancel: Option<&Cancel>, target: Option<&[i32]>) -> Result<Field> {
    let buffer=if window.address.starts_with("gnome:") && window.buffer_rect.is_some() {Some(super::native_elements::buffer_rect(window).map_err(refused)?)} else {None};
    let rect=buffer.or(window.client_rect).map(|[x,y,width,height]|super::hyprland::Rect{x,y,width,height}).unwrap_or_else(||window.geometry());
    let frame=window.geometry();
    if buffer.is_none() && (rect.width<=0 || rect.height<=0 || rect.x<frame.x || rect.y<frame.y ||
        rect.x.checked_add(rect.width).is_none_or(|end|frame.x.checked_add(frame.width).is_none_or(|right|end>right)) ||
        rect.y.checked_add(rect.height).is_none_or(|end|frame.y.checked_add(frame.height).is_none_or(|bottom|end>bottom))) {
        return Err(refused("Invalid compositor client rectangle."));
    }
    let exe=std::env::current_exe().map_err(|e|refused(e.to_string()))?;
    let mut cmd=Cmd::new(exe).args(["native-field".to_string(),window.pid.to_string(),rect.x.to_string(),rect.y.to_string(),rect.width.to_string(),rect.height.to_string()]).envs(env).timeout(Duration::from_secs(6)).max_output(1024*1024);
    if window.address.starts_with("gnome:") {
        cmd=cmd.arg("window-local");
        if let Some(client)=window.client_rect {
            let frame=window.geometry();
            if client[2]<=0 || client[3]<=0 || client[0]<frame.x || client[1]<frame.y ||
                client[0].checked_add(client[2]).is_none_or(|end|frame.x.checked_add(frame.width).is_none_or(|right|end>right)) ||
                client[1].checked_add(client[3]).is_none_or(|end|frame.y.checked_add(frame.height).is_none_or(|bottom|end>bottom)) {return Err(refused("Invalid compositor client rectangle."));}
            cmd=cmd.args(client.map(|n|n.to_string()));
        }
    }
    if let Some(path)=target {
        if path.is_empty() || path.len()>40 || path.iter().any(|i|*i<0) { return Err(refused("The native field path is invalid.")); }
        cmd=cmd.args(["path".to_string(),path.iter().map(i32::to_string).collect::<Vec<_>>().join(",")]);
    }
    if let Some(c)=cancel { cmd=cmd.cancel(c); }
    let out=run::run(cmd).await?;
    if !out.success() { return Err(refused(out.failure_text("native field read"))); }
    serde_json::from_slice(&out.stdout).map_err(|_|refused("Native focused-field observation was invalid."))
}

// Raw AT-SPI D-Bus calls, as the element reader makes, never libatspi.
// libatspi 2.60 asks each application it meets for its whole cache
// (Cache.GetItems), and every later call waits behind that dump: 9 s from
// LibreOffice with five documents open. Each read here asks only for the
// objects it walks, fresh, with no client cache. Desktop IPC and a hung
// application stay in this isolated process.
type Reference=(String,OwnedObjectPath);
const ACCESSIBLE:&str="org.a11y.atspi.Accessible";
const TEXT:&str="org.a11y.atspi.Text";
fn failed(step:&str,error:zbus::Error)->String {
    // An application's error message is never copied: it could quote text.
    let cause=match error {zbus::Error::MethodError(name,_,_)=>name.to_string(),other=>other.to_string()};
    format!("Native accessibility read failed at {step} ({}); no field was proved.",cause.chars().take(120).collect::<String>())
}
fn has(states:&[u32],state:usize)->bool {states.get(state/32).is_some_and(|bits|bits&(1<<(state%32))!=0)}
struct Reader {connection:Connection,owner:String}
impl Reader {
    async fn proxy<'a>(&'a self,path:&'a OwnedObjectPath,interface:&'a str)->std::result::Result<Proxy<'a>,String> {
        zbus::proxy::Builder::new(&self.connection)
            .destination(self.owner.as_str()).map_err(|e|failed("proxy",e))?
            .path(path.as_str()).map_err(|e|failed("proxy",e))?
            .interface(interface).map_err(|e|failed("proxy",e))?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build().await.map_err(|e|failed("proxy",e))
    }
    async fn states(&self,node:&OwnedObjectPath)->std::result::Result<Vec<u32>,String> {
        let bits:Vec<u32>=self.proxy(node,ACCESSIBLE).await?.call("GetState",&()).await.map_err(|e|failed("state",e))?;
        if bits.len()>8 {return Err("Invalid native state mask.".into());}Ok(bits)
    }
    async fn role(&self,node:&OwnedObjectPath)->std::result::Result<String,String> {
        let role:String=self.proxy(node,ACCESSIBLE).await?.call("GetRoleName",&()).await.map_err(|e|failed("role",e))?;
        if role.len()>128 {return Err("Invalid native field role.".into());}Ok(role)
    }
    async fn child_count(&self,node:&OwnedObjectPath)->std::result::Result<i32,String> {
        let count:i32=self.proxy(node,ACCESSIBLE).await?.get_property("ChildCount").await.map_err(|e|failed("child count",e))?;
        if count<0 {return Err("Native child count is invalid.".into());}Ok(count)
    }
    async fn child_at(&self,node:&OwnedObjectPath,index:i32)->std::result::Result<OwnedObjectPath,String> {
        let (owner,path):Reference=self.proxy(node,ACCESSIBLE).await?.call("GetChildAtIndex",&(index,)).await.map_err(|e|failed("child",e))?;
        if path.as_str()=="/org/a11y/atspi/null" {return Err("Native accessibility returned no object.".into());}
        if owner!=self.owner || path.as_str().len()>4096 {return Err("Cross-process or invalid native child reference.".into());}
        Ok(path)
    }
    async fn implements(&self,node:&OwnedObjectPath,interface:&str)->std::result::Result<bool,String> {
        let interfaces:Vec<String>=self.proxy(node,ACCESSIBLE).await?.call("GetInterfaces",&()).await.map_err(|e|failed("interfaces",e))?;
        Ok(interfaces.iter().any(|i|i==interface))
    }
    fn frame(&self,node:&OwnedObjectPath)->impl std::future::Future<Output=std::result::Result<[i32;4],String>> {self.frame_at(node,0)}
    async fn frame_at(&self,node:&OwnedObjectPath,coordinate:u32)->std::result::Result<[i32;4],String> {
        let (x,y,w,h):(i32,i32,i32,i32)=self.proxy(node,"org.a11y.atspi.Component").await?.call("GetExtents",&(coordinate,)).await.map_err(|e|failed("extents",e))?;
        Ok([x,y,w,h])
    }
    /// The whole text of a Text object, checked against its own count.
    async fn text(&self,text:&Proxy<'_>,limit:&str,changed:&str)->std::result::Result<(i32,String),String> {
        let count:i32=text.get_property("CharacterCount").await.map_err(|e|failed("character count",e))?;
        if !(0..=131072).contains(&count){return Err(limit.into());}
        let value:String=text.call("GetText",&(0i32,count)).await.map_err(|e|failed("text",e))?;
        if value.chars().count()!=count as usize {return Err(changed.into());}
        Ok((count,value))
    }
    async fn paragraph_context(&self,parent:Option<&OwnedObjectPath>,path:&[i32],value:&str)->std::result::Result<Option<Vec<String>>,String>{
        let Some(parent)=parent else {return Ok(None)};
        if self.role(parent).await?!="document text" {return Ok(None);}
        let count=self.child_count(parent).await?;
        if !(1..=512).contains(&count){return Err("Native paragraph count exceeds readback capacity.".into());}
        let mut values=Vec::new();let mut chars=0usize;
        for i in 0..count {
            let child=self.child_at(parent,i).await?;
            if self.role(&child).await?!="paragraph" {return Ok(None);}
            if !self.implements(&child,TEXT).await? {return Ok(None);}
            let (n,text)=self.text(&self.proxy(&child,TEXT).await?,"Native paragraph text exceeds readback capacity.","Native paragraph text changed during observation.").await?;
            chars+=n as usize;if chars>131072{return Err("Native paragraph context exceeds readback capacity.".into());}
            values.push(text);
        }
        let index=*path.last().ok_or("Native paragraph path is unavailable.")?;
        if index<0 || values.get(index as usize).map(String::as_str)!=Some(value){return Err("Native paragraph context is inconsistent.".into());}
        Ok(Some(values))
    }
    async fn walk(&self,node:OwnedObjectPath,parent:Option<&OwnedObjectPath>,path:&mut Vec<i32>,visited:&mut usize,found:&mut Vec<Field>,origin:Option<[i32;4]>)->std::result::Result<(),String>{
        *visited+=1;if *visited>4096 || path.len()>40{return Err("Native field traversal was incomplete.".into());}
        let states=self.states(&node).await?;
        // Hidden/defunct branches cannot contain the current visible edit target.
        if has(&states,6) || !has(&states,25) { return Ok(()); }
        if let Some(field)=self.field(&node,parent,&states,path,origin).await? {found.push(field);}
        // Writer exposes focused paragraphs beneath small virtual containers.
        // Calc's virtual table can contain millions of cells. Search only a
        // bounded child list; large virtual containers cannot be enumerated.
        let count=self.child_count(&node).await.map_err(|e|format!("{e} at native path {path:?}"))?;
        if count>4096 {
            if has(&states,31) { return Ok(()); }
            return Err("Native field traversal exceeds its limit.".into());
        }
        for i in 0..count {
            let child=self.child_at(&node,i).await?;path.push(i);
            Box::pin(self.walk(child,Some(&node),path,visited,found,origin)).await?;path.pop();
        }
        Ok(())
    }
    /// The focused editable field at `node`, if it is one.
    async fn field(&self,node:&OwnedObjectPath,parent:Option<&OwnedObjectPath>,states:&[u32],path:&[i32],origin:Option<[i32;4]>)->std::result::Result<Option<Field>,String>{
        // ATSPI_STATE_FOCUSED; focused noneditable containers are not fields.
        // GTK4 reports SENSITIVE without ENABLED on its editable text view.
        if !has(states,12) {return Ok(None);}
        let role=self.role(node).await?;
        if role.to_lowercase().contains("password"){return Err("Unicode paste into password fields is unavailable.".into());}
        if !has(states,7) || !(has(states,8) || has(states,24)) {return Ok(None);}
        // Editable focused containers (Calc table) need not expose Text.
        // Continue to their real editor rather than accepting the container.
        if !self.implements(node,TEXT).await? {return Ok(None);}
        let text=self.proxy(node,TEXT).await?;
        let (count,value)=self.text(&text,"Native field text exceeds the verification limit.","Native character count changed during observation.").await?;
        let caret:i32=text.get_property("CaretOffset").await.map_err(|e|failed("caret",e))?;
        if !(0..=count).contains(&caret){return Err("Native caret offset is invalid.".into());}
        let n:i32=text.call("GetNSelections",&()).await.map_err(|e|failed("selections",e))?;
        if !(0..=1).contains(&n){return Err("Multiple native selections are unsupported.".into());}
        let mut selections=Vec::new();
        if n==1 {
            let (start,end):(i32,i32)=text.call("GetSelection",&(0i32,)).await.map_err(|e|failed("selection",e))?;
            if !(0..=count).contains(&start) || !(0..=count).contains(&end){return Err("Native selection offsets are invalid.".into());}
            selections.push([start,end]);
        }
        let frame=if let Some(window)=origin {
            let mut frame=self.frame_at(node,1).await?; // ATSPI_COORD_TYPE_WINDOW
            if frame[0]<0 || frame[1]<0 || frame[2]<=0 || frame[3]<=0 ||
                frame[0].checked_add(frame[2]).is_none_or(|end|end>window[2]) ||
                frame[1].checked_add(frame[3]).is_none_or(|end|end>window[3]) {
                return Err("Native field bounds lie outside the exact window.".into());
            }
            frame[0]=frame[0].checked_add(window[0]).ok_or("Invalid native field X")?;
            frame[1]=frame[1].checked_add(window[1]).ok_or("Invalid native field Y")?;
            frame
        } else {self.frame(node).await?};
        let paragraphs=if role=="paragraph" {self.paragraph_context(parent,path,&value).await?}else{None};
        Ok(Some(Field{path:path.to_vec(),value,role,frame,window_frame:origin,caret:Some(caret),selections:Some(selections),paragraphs}))
    }
}
async fn read_field(pid:u32,rect:[i32;4],window_local:bool,client:Option<[i32;4]>,target:Option<Vec<i32>>)->std::result::Result<Field,String>{
    // One slow reply stays within the runner's six-second deadline.
    let limit=Duration::from_millis(4000);
    let session=zbus::connection::Builder::session().map_err(|e|failed("session bus",e))?
        .method_timeout(limit).build().await.map_err(|e|failed("session bus",e))?;
    let bus=Proxy::new(&session,"org.a11y.Bus","/org/a11y/bus","org.a11y.Bus").await.map_err(|e|failed("accessibility bus",e))?;
    let address:String=bus.call("GetAddress",&()).await.map_err(|e|failed("accessibility bus",e))?;
    if address.is_empty() || address.len()>4096 {return Err("Invalid accessibility bus address.".into());}
    let connection=zbus::connection::Builder::address(address.as_str()).map_err(|e|failed("accessibility bus",e))?
        .method_timeout(limit).build().await.map_err(|e|failed("accessibility bus",e))?;
    let dbus=Proxy::new(&connection,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus").await.map_err(|e|failed("bus",e))?;
    let registry=Proxy::new(&connection,"org.a11y.atspi.Registry","/org/a11y/atspi/accessible/root",ACCESSIBLE).await.map_err(|e|failed("desktop",e))?;
    let applications:Vec<Reference>=registry.call("GetChildren",&()).await.map_err(|e|failed("desktop",e))?;
    if applications.len()>4096{return Err("Native application count exceeds its limit.".into());}
    let mut windows=Vec::new();
    for (name,app) in applications {
        // The PID is the bus owner's, from the bus itself; an application
        // that has gone away is skipped.
        let Ok(owner)=dbus.call::<_,_,String>("GetNameOwner",&(name,)).await else {continue};
        let Ok(got)=dbus.call::<_,_,u32>("GetConnectionUnixProcessID",&(&owner,)).await else {continue};
        if got!=pid {continue;}
        let reader=Reader{connection:connection.clone(),owner};
        let children=reader.child_count(&app).await.map_err(|e|format!("{e} at selected application"))?;
        if children>4096{return Err("Native window count exceeds its limit.".into());}
        for j in 0..children {
            let w=reader.child_at(&app,j).await?;
            let frame=reader.frame(&w).await?;
            let states=reader.states(&w).await?;
            // GTK on Wayland reports screen extents with a window-local
            // zero origin. Require the unique ACTIVE top-level window in
            // the compositor-focused PID and exact size, never size alone.
            let matched=std::iter::once(rect).chain(client).find(|candidate|
                ((frame[0]==candidate[0] && frame[1]==candidate[1]) || (frame[0]==0 && frame[1]==0)) && frame[2..]==candidate[2..]);
            if let Some(matched)=matched {if has(&states,1) {windows.push((Reader{connection:connection.clone(),owner:reader.owner.clone()},w,vec![j],matched));}}
        }
    }
    if windows.len()!=1{return Err("The native window identity could not be proved uniquely.".into());}
    let (reader,w,mut path,rect)=windows.pop().unwrap();
    if let Some(target)=target {
        // The same window index, then each child in turn; the node must
        // still be the visible focused editable field.
        if target.first()!=path.first() {return Err("The expected native field is no longer in the focused window.".into());}
        let (mut parent,mut node)=(None,w);
        for (depth,&index) in target.iter().enumerate().skip(1) {
            // As the full walk: a hidden or defunct branch holds no
            // current visible field.
            let states=reader.states(&node).await?;
            if has(&states,6) || !has(&states,25) {return Err("The expected native field is no longer the focused editable field.".into());}
            let count=reader.child_count(&node).await.map_err(|e|format!("{e} at native path {:?}",&target[..depth]))?;
            if index>=count {return Err("The expected native field is no longer the focused editable field.".into());}
            let child=reader.child_at(&node,index).await?;
            parent=Some(std::mem::replace(&mut node,child));
        }
        let states=reader.states(&node).await?;
        if has(&states,6) || !has(&states,25) {return Err("The expected native field is no longer the focused editable field.".into());}
        let origin=window_local.then_some(rect);
        return reader.field(&node,parent.as_ref(),&states,&target,origin).await?.ok_or_else(||"The expected native field is no longer the focused editable field.".into());
    }
    // Root desktop application order can change independently; identity is
    // the selected process plus its top-level window index and field path.
    let mut found=Vec::new();reader.walk(w,None,&mut path,&mut 0,&mut found,window_local.then_some(rect)).await?;
    if found.len()!=1{return Err("Exactly one focused editable native field is required.".into());}Ok(found.remove(0))
}
/// Private read-only daemon subcommand. The desktop runner owns its deadline.
pub fn main(args:&[String])->u8 {
    let result=(|| {
        let (args,target)=match args {
            [rest @ .., flag, spec] if flag=="path" => {
                let path=spec.split(',').map(|s|s.parse::<i32>().ok().filter(|i|*i>=0)).collect::<Option<Vec<_>>>().filter(|p|!p.is_empty() && p.len()<=40).ok_or("Invalid native field path.")?;
                (rest,Some(path))
            }
            _ => (args,None),
        };
        if args.len()!=5 && !((args.len()==6 || args.len()==10) && args[5]=="window-local") {return Err("Expected native-field PID X Y WIDTH HEIGHT [window-local] [path P].".into());}
        let pid=args[0].parse::<u32>().map_err(|_|"Invalid native PID.")?;
        let mut rect=[0;4];for (dst,s) in rect.iter_mut().zip(&args[1..]){*dst=s.parse().map_err(|_|"Invalid native window geometry.")?;}
        if pid==0 || rect[2]<=0 || rect[3]<=0{return Err("Invalid native window identity.".into());}
        let client=if args.len()==10 {let mut client=[0;4];for (dst,s) in client.iter_mut().zip(&args[6..]){*dst=s.parse().map_err(|_|"Invalid native client geometry.")?;}if client[2]<=0 || client[3]<=0{return Err("Invalid native client identity.".into());}Some(client)}else{None};
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e|e.to_string())?;
        runtime.block_on(read_field(pid,rect,args.len()>=6,client,target))
    })();
    match result {Ok(f)=>{println!("{}",serde_json::to_string(&f).unwrap());0},Err(e)=>{eprintln!("{e}");1}}
}
