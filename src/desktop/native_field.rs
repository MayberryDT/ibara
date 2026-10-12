//! Read the actual focused native field in an isolated, bounded process.
//! Cua 0.28 cannot prove tree completeness. No input is sent by this helper.
use super::{run::{self, Cmd, Cancel}, Window};
use crate::error::{Result, IbaraError};
use serde::{Serialize, Deserialize};
use std::{ffi::{CStr, OsString}, time::Duration};

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

// libatspi is already an Ibara runtime dependency. Loading it only here keeps
// desktop IPC and a hung application outside the controller process. Opaque
// GObjects never cross this process boundary; all function signatures match
// the installed public libatspi/glib ABI. Process exit frees the bounded walk.
type Obj=*mut libc::c_void;
type ErrPtr=*mut *mut libc::c_void;
#[repr(C)] struct Rect { x:i32,y:i32,width:i32,height:i32 }
#[repr(C)] struct TextRange { start:i32,end:i32 }
macro_rules! api {
    ($($field:ident : $symbol:literal => $sig:ty),* $(,)?) => {
        struct Api { $($field: $sig),* }
        impl Api {
            unsafe fn load()->std::result::Result<Self,String> {
                let lib=unsafe { libc::dlopen(c"libatspi.so.0".as_ptr(),libc::RTLD_NOW) };
                if lib.is_null() { return Err("Native accessibility library unavailable.".into()); }
                Ok(Self { $($field: {
                    let p=unsafe { libc::dlsym(lib,concat!($symbol,"\0").as_ptr().cast()) };
                    if p.is_null() { return Err(format!("Native accessibility symbol unavailable: {}",$symbol)); }
                    unsafe { std::mem::transmute::<Obj,$sig>(p) }
                }),* })
            }
        }
    }
}
api! {
 init:"atspi_init"=>unsafe extern "C" fn()->i32,
 timeout:"atspi_set_timeout"=>unsafe extern "C" fn(i32,i32),
 iteration:"g_main_context_iteration"=>unsafe extern "C" fn(Obj,i32)->i32,
 desktop:"atspi_get_desktop"=>unsafe extern "C" fn(i32)->Obj,
 children:"atspi_accessible_get_child_count"=>unsafe extern "C" fn(Obj,ErrPtr)->i32,
 child:"atspi_accessible_get_child_at_index"=>unsafe extern "C" fn(Obj,i32,ErrPtr)->Obj,
 parent:"atspi_accessible_get_parent"=>unsafe extern "C" fn(Obj,ErrPtr)->Obj,
 pid:"atspi_accessible_get_process_id"=>unsafe extern "C" fn(Obj,ErrPtr)->u32,
 states:"atspi_accessible_get_state_set"=>unsafe extern "C" fn(Obj)->Obj,
 has:"atspi_state_set_contains"=>unsafe extern "C" fn(Obj,i32)->i32,
 role:"atspi_accessible_get_role_name"=>unsafe extern "C" fn(Obj,ErrPtr)->*mut libc::c_char,
 component:"atspi_accessible_get_component_iface"=>unsafe extern "C" fn(Obj)->Obj,
 extents:"atspi_component_get_extents"=>unsafe extern "C" fn(Obj,i32,ErrPtr)->*mut Rect,
 text:"atspi_accessible_get_text_iface"=>unsafe extern "C" fn(Obj)->Obj,
 count:"atspi_text_get_character_count"=>unsafe extern "C" fn(Obj,ErrPtr)->i32,
 value:"atspi_text_get_text"=>unsafe extern "C" fn(Obj,i32,i32,ErrPtr)->*mut libc::c_char,
 caret:"atspi_text_get_caret_offset"=>unsafe extern "C" fn(Obj,ErrPtr)->i32,
 selections:"atspi_text_get_n_selections"=>unsafe extern "C" fn(Obj,ErrPtr)->i32,
 selection:"atspi_text_get_selection"=>unsafe extern "C" fn(Obj,i32,ErrPtr)->*mut TextRange,
}
fn valid(error:Obj)->std::result::Result<(),String> { if error.is_null(){Ok(())}else{Err("Native accessibility read failed; no field was proved.".into())} }
/// How long one cold LibreOffice accessibility reply may take; the isolated
/// runner's six-second deadline still bounds the whole read.
const COLD_REPLY:std::time::Duration=std::time::Duration::from_secs(3);
fn present(obj:Obj)->std::result::Result<Obj,String>{if obj.is_null(){Err("Native accessibility returned no object.".into())}else{Ok(obj)}}
impl Api {
    fn child_count(&self,node:Obj)->std::result::Result<i32,String>{
        // Properties.Get can return -1 without GError while the isolated
        // process's AT-SPI cache is still being populated. LibreOffice with
        // several documents open takes about two seconds to answer a cold
        // application, so wait for the reply, bounded, then require a real count.
        let n=self.settled(||{let mut e=std::ptr::null_mut();let n=unsafe{(self.children)(node,&mut e)};valid(e).map(|()|(n>=0).then_some(n))})?;
        n.ok_or_else(||"Native child count is invalid after bounded cache refresh.".into())
    }
    fn child_at(&self,node:Obj,index:i32)->std::result::Result<Obj,String>{
        let p=self.settled(||{let mut e=std::ptr::null_mut();let p=unsafe{(self.child)(node,index,&mut e)};valid(e).map(|()|(!p.is_null()).then_some(p))})?;
        p.ok_or_else(||"Native accessibility returned no object.".into())
    }
    /// Ask again while a cold reply is still pending: drain callbacks without
    /// blocking and retry for at most [`COLD_REPLY`]. An error ends it at once.
    fn settled<T>(&self,mut ask:impl FnMut()->std::result::Result<Option<T>,String>)->std::result::Result<Option<T>,String>{
        let start=std::time::Instant::now();
        loop {
            if let Some(value)=ask()? {return Ok(Some(value));}
            if start.elapsed()>=COLD_REPLY {return Ok(None);}
            for _ in 0..64 {
                if unsafe{(self.iteration)(std::ptr::null_mut(),0)}==0 {break;}
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    fn paragraph_context(&self,node:Obj,path:&[i32],value:&str)->std::result::Result<Option<Vec<String>>,String>{
        let mut e=std::ptr::null_mut();let parent=unsafe{(self.parent)(node,&mut e)};valid(e)?;
        if parent.is_null(){return Ok(None);}
        let mut e=std::ptr::null_mut();let role=unsafe{(self.role)(parent,&mut e)};valid(e)?;present(role.cast())?;
        if unsafe{CStr::from_ptr(role)}.to_bytes()!=b"document text" {return Ok(None);}
        let count=self.child_count(parent)?;
        if !(1..=512).contains(&count){return Err("Native paragraph count exceeds readback capacity.".into());}
        let mut values=Vec::new();let mut chars=0usize;
        for i in 0..count {
            let child=self.child_at(parent,i)?;
            let mut e=std::ptr::null_mut();let role=unsafe{(self.role)(child,&mut e)};valid(e)?;present(role.cast())?;
            if unsafe{CStr::from_ptr(role)}.to_bytes()!=b"paragraph" {return Ok(None);}
            let text=unsafe{(self.text)(child)};if text.is_null(){return Ok(None);}
            let mut e=std::ptr::null_mut();let n=unsafe{(self.count)(text,&mut e)};valid(e)?;
            if n<0 || n>131072 {return Err("Native paragraph text exceeds readback capacity.".into());}
            chars+=n as usize;if chars>131072{return Err("Native paragraph context exceeds readback capacity.".into());}
            let mut e=std::ptr::null_mut();let text=unsafe{(self.value)(text,0,n,&mut e)};valid(e)?;present(text.cast())?;
            let text=unsafe{CStr::from_ptr(text)}.to_str().map_err(|_|"Invalid native paragraph text.")?.to_owned();
            if text.chars().count()!=n as usize{return Err("Native paragraph text changed during observation.".into());}
            values.push(text);
        }
        let index=*path.last().ok_or("Native paragraph path is unavailable.")?;
        if index<0 || values.get(index as usize).map(String::as_str)!=Some(value){return Err("Native paragraph context is inconsistent.".into());}
        Ok(Some(values))
    }
    fn frame(&self,node:Obj)->std::result::Result<[i32;4],String>{self.frame_at(node,0)}
    fn frame_at(&self,node:Obj,coordinate:i32)->std::result::Result<[i32;4],String>{
        let c=present(unsafe{(self.component)(node)})?;let mut e=std::ptr::null_mut();let r=unsafe{(self.extents)(c,coordinate,&mut e)};valid(e)?;present(r.cast())?;
        let r=unsafe{&*r};Ok([r.x,r.y,r.width,r.height])
    }
    fn walk(&self,node:Obj,path:&mut Vec<i32>,visited:&mut usize,found:&mut Vec<Field>,origin:Option<[i32;4]>)->std::result::Result<(),String>{
        *visited+=1;if *visited>4096 || path.len()>40{return Err("Native field traversal was incomplete.".into());}
        let states=present(unsafe{(self.states)(node)})?;
        // Hidden/defunct branches cannot contain the current visible edit target.
        if unsafe{(self.has)(states,6)}!=0 || unsafe{(self.has)(states,25)}==0 { return Ok(()); }
        if let Some(field)=self.field(node,states,path,origin)? {found.push(field);}
        // Writer exposes focused paragraphs beneath small virtual containers.
        // Calc's virtual table can contain millions of cells. Search only a
        // bounded child list; large virtual containers cannot be enumerated.
        let count=self.child_count(node).map_err(|e|format!("{e} at native path {path:?}"))?;
        if count>4096 {
            if unsafe{(self.has)(states,31)}!=0 { return Ok(()); }
            return Err("Native field traversal exceeds its limit.".into());
        }
        for i in 0..count {let child=self.child_at(node,i)?;path.push(i);self.walk(child,path,visited,found,origin)?;path.pop();}
        Ok(())
    }
    /// The focused editable field at `node`, if it is one.
    fn field(&self,node:Obj,states:Obj,path:&[i32],origin:Option<[i32;4]>)->std::result::Result<Option<Field>,String>{
        if unsafe{(self.has)(states,12)}!=0 { // ATSPI_STATE_FOCUSED
            let mut e=std::ptr::null_mut();let r=unsafe{(self.role)(node,&mut e)};valid(e)?;present(r.cast())?;
            let role=unsafe{CStr::from_ptr(r)}.to_str().map_err(|_|"Invalid native field role.")?.to_owned();
            if role.to_lowercase().contains("password"){return Err("Unicode paste into password fields is unavailable.".into());}
            // Focused noneditable containers are not fields; still walk children.
            // GTK4 reports SENSITIVE without ENABLED on its editable text view.
            if unsafe{(self.has)(states,7)}!=0 &&
                (unsafe{(self.has)(states,8)}!=0 || unsafe{(self.has)(states,24)}!=0) {
            // Editable focused containers (Calc table) need not expose Text.
            // Continue to their real editor rather than accepting the container.
            let text=unsafe{(self.text)(node)};
            if !text.is_null() {
            let mut e=std::ptr::null_mut();let count=unsafe{(self.count)(text,&mut e)};valid(e)?;
            if !(0..=131072).contains(&count){return Err("Native field text exceeds the verification limit.".into());}
            let mut e=std::ptr::null_mut();let s=unsafe{(self.value)(text,0,count,&mut e)};valid(e)?;present(s.cast())?;
            let value=unsafe{CStr::from_ptr(s)}.to_str().map_err(|_|"Invalid native field text.")?.to_owned();
            if value.chars().count()!=count as usize {return Err("Native character count changed during observation.".into());}
            let mut e=std::ptr::null_mut();let caret=unsafe{(self.caret)(text,&mut e)};valid(e)?;
            if !(0..=count).contains(&caret){return Err("Native caret offset is invalid.".into());}
            let mut e=std::ptr::null_mut();let n=unsafe{(self.selections)(text,&mut e)};valid(e)?;
            if !(0..=1).contains(&n){return Err("Multiple native selections are unsupported.".into());}
            let mut selections=Vec::new();
            if n==1 {
                let mut e=std::ptr::null_mut();let range=unsafe{(self.selection)(text,0,&mut e)};valid(e)?;present(range.cast())?;
                let range=unsafe{&*range};
                if !(0..=count).contains(&range.start) || !(0..=count).contains(&range.end){return Err("Native selection offsets are invalid.".into());}
                selections.push([range.start,range.end]);
            }
            let frame=if let Some(window)=origin {
                let mut frame=self.frame_at(node,1)?; // ATSPI_COORD_TYPE_WINDOW
                if frame[0]<0 || frame[1]<0 || frame[2]<=0 || frame[3]<=0 ||
                    frame[0].checked_add(frame[2]).is_none_or(|end|end>window[2]) ||
                    frame[1].checked_add(frame[3]).is_none_or(|end|end>window[3]) {
                    return Err("Native field bounds lie outside the exact window.".into());
                }
                frame[0]=frame[0].checked_add(window[0]).ok_or("Invalid native field X")?;
                frame[1]=frame[1].checked_add(window[1]).ok_or("Invalid native field Y")?;
                frame
            } else {self.frame(node)?};
            let paragraphs=if role=="paragraph" {self.paragraph_context(node,path,&value)?}else{None};
            return Ok(Some(Field{path:path.to_vec(),value,role,frame,window_frame:origin,caret:Some(caret),selections:Some(selections),paragraphs}));
            }
            }
        }
        Ok(None)
    }
    fn read(&self,pid:u32,rect:[i32;4],window_local:bool,client:Option<[i32;4]>,target:Option<Vec<i32>>)->std::result::Result<Field,String>{
        if unsafe{(self.init)()}!=0{return Err("Native accessibility initialization failed.".into());}
        // A cold LibreOffice accessibility reply can take over one second.
        // The isolated runner still enforces the six-second overall deadline.
        unsafe{(self.timeout)(1500,1500)};
        let root=present(unsafe{(self.desktop)(0)})?;let mut windows=Vec::new();
        let applications=self.child_count(root).map_err(|e|format!("{e} at desktop root"))?;
        if applications>4096{return Err("Native application count exceeds its limit.".into());}
        for i in 0..applications {
            let app=self.child_at(root,i)?;let mut e=std::ptr::null_mut();let got=unsafe{(self.pid)(app,&mut e)};
            if !e.is_null() || got!=pid {continue;}
            let children=self.child_count(app).map_err(|e|format!("{e} at selected application"))?;
            if children>4096{return Err("Native window count exceeds its limit.".into());}
            for j in 0..children {
                let w=self.child_at(app,j)?;
                let frame=self.frame(w)?;
                let states=present(unsafe{(self.states)(w)})?;
                // GTK on Wayland reports screen extents with a window-local
                // zero origin. Require the unique ACTIVE top-level window in
                // the compositor-focused PID and exact size, never size alone.
                let matched=std::iter::once(rect).chain(client).find(|candidate|
                    ((frame[0]==candidate[0] && frame[1]==candidate[1]) || (frame[0]==0 && frame[1]==0)) && frame[2..]==candidate[2..]);
                if let Some(matched)=matched {if unsafe{(self.has)(states,1)}!=0 {windows.push((w,vec![j],matched));}}
            }
        }
        if windows.len()!=1{return Err("The native window identity could not be proved uniquely.".into());}
        let (w,mut path,rect)=windows.pop().unwrap();
        if let Some(target)=target {
            // The same window index, then each child in turn; the node must
            // still be the visible focused editable field.
            if target.first()!=path.first() {return Err("The expected native field is no longer in the focused window.".into());}
            let mut node=w;
            for (depth,&index) in target.iter().enumerate().skip(1) {
                // As the full walk: a hidden or defunct branch holds no
                // current visible field.
                let states=present(unsafe{(self.states)(node)})?;
                if unsafe{(self.has)(states,6)}!=0 || unsafe{(self.has)(states,25)}==0 {return Err("The expected native field is no longer the focused editable field.".into());}
                let count=self.child_count(node).map_err(|e|format!("{e} at native path {:?}",&target[..depth]))?;
                if index>=count {return Err("The expected native field is no longer the focused editable field.".into());}
                node=self.child_at(node,index)?;
            }
            let states=present(unsafe{(self.states)(node)})?;
            if unsafe{(self.has)(states,6)}!=0 || unsafe{(self.has)(states,25)}==0 {return Err("The expected native field is no longer the focused editable field.".into());}
            let origin=window_local.then_some(rect);
            return self.field(node,states,&target,origin)?.ok_or_else(||"The expected native field is no longer the focused editable field.".into());
        }
        // Root desktop application order can change independently; identity is
        // the selected process plus its top-level window index and field path.
        let mut found=Vec::new();self.walk(w,&mut path,&mut 0,&mut found,window_local.then_some(rect))?;
        if found.len()!=1{return Err("Exactly one focused editable native field is required.".into());}Ok(found.remove(0))
    }
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
        unsafe{Api::load()}?.read(pid,rect,args.len()>=6,client,target)
    })();
    match result {Ok(f)=>{println!("{}",serde_json::to_string(&f).unwrap());0},Err(e)=>{eprintln!("{e}");1}}
}
