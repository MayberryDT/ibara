//! Read the actual focused native field in an isolated, bounded process.
//! Cua 0.28 cannot prove tree completeness. No input is sent by this helper.
use super::{run::{self, Cmd, Cancel}, Window};
use crate::error::{Result, IbaraError};
use serde::{Serialize, Deserialize};
use std::{ffi::{CStr, OsString}, time::Duration};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub path: Vec<i32>, pub value: String, pub role: String, pub frame: [i32;4],
    #[serde(default)] pub caret: Option<i32>,
    #[serde(default)] pub selections: Option<Vec<[i32;2]>>,
    #[serde(default)] pub paragraphs: Option<Vec<String>>,
}
impl Field {
    /// Application relayout may change the rectangle after insertion. The
    /// isolated reader validates that fresh rectangle before returning it.
    pub fn confirms_insertion(&self, observed: &Field) -> bool {
        self.path == observed.path && self.role == observed.role && self.value == observed.value &&
            self.caret == observed.caret && self.selections == observed.selections && self.paragraphs == observed.paragraphs
    }
    pub fn after_insertion(&self, text: &str) -> Result<Field> {
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
        if let Some(paragraphs)=&mut after.paragraphs {
            let index=*self.path.last().ok_or_else(||refused("Native paragraph path is unavailable."))? as usize;
            if paragraphs.get(index)!=Some(&self.value) {return Err(refused("Native paragraph context is inconsistent."));}
            paragraphs[index]=after.value.clone();
        }
        Ok(after)
    }
    /// Clipboard newlines split Writer paragraphs; direct native text may
    /// insert soft line breaks. Prove the entire bounded sibling context.
    pub fn after_paste(&self, text: &str) -> Result<Field> {
        let mut after=self.after_insertion(text)?;
        if self.role=="paragraph" && text.contains('\n') {
            let paragraphs=after.paragraphs.as_mut().ok_or_else(||refused("Complete native paragraph context is required for multiline paste."))?;
            let index=*self.path.last().ok_or_else(||refused("Native paragraph path is unavailable."))? as usize;
            let parts:Vec<String>=after.value.split('\n').map(str::to_owned).collect();
            let caret=after.caret.unwrap() as usize;
            let prefix:String=after.value.chars().take(caret).collect();
            let next=index+prefix.matches('\n').count();
            after.caret=Some(prefix.rsplit('\n').next().unwrap().chars().count() as i32);
            paragraphs.splice(index..=index,parts);
            after.value=paragraphs[next].clone();
            *after.path.last_mut().unwrap()=next as i32;
        }
        Ok(after)
    }
}
fn refused(message: impl Into<String>) -> IbaraError { IbaraError::new("CAPABILITY_UNAVAILABLE", message, true).with("execution_not_started", true) }
pub async fn read(window: &Window, env: &[(OsString, OsString)], cancel: Option<&Cancel>) -> Result<Field> {
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
    if window.address.starts_with("gnome:") {cmd=cmd.arg("window-local");}
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
fn present(obj:Obj)->std::result::Result<Obj,String>{if obj.is_null(){Err("Native accessibility returned no object.".into())}else{Ok(obj)}}
impl Api {
    fn child_count(&self,node:Obj)->std::result::Result<i32,String>{
        let mut e=std::ptr::null_mut();let n=unsafe{(self.children)(node,&mut e)};valid(e)?;
        if n<0 {Err("Native child count is invalid.".into())}else{Ok(n)}
    }
    fn child_at(&self,node:Obj,index:i32)->std::result::Result<Obj,String>{
        let mut e=std::ptr::null_mut();let p=unsafe{(self.child)(node,index,&mut e)};valid(e)?;present(p)
    }
    fn paragraph_context(&self,node:Obj,path:&[i32],value:&str)->std::result::Result<Option<Vec<String>>,String>{
        let mut e=std::ptr::null_mut();let parent=unsafe{(self.parent)(node,&mut e)};valid(e)?;
        if parent.is_null(){return Ok(None);}
        let mut e=std::ptr::null_mut();let role=unsafe{(self.role)(parent,&mut e)};valid(e)?;present(role.cast())?;
        if unsafe{CStr::from_ptr(role)}.to_bytes()!=b"document text" {return Ok(None);}
        let count=self.child_count(parent)?;
        if !(1..=512).contains(&count){return Ok(None);}
        let mut values=Vec::new();let mut chars=0usize;
        for i in 0..count {
            let child=self.child_at(parent,i)?;
            let mut e=std::ptr::null_mut();let role=unsafe{(self.role)(child,&mut e)};valid(e)?;present(role.cast())?;
            if unsafe{CStr::from_ptr(role)}.to_bytes()!=b"paragraph" {return Ok(None);}
            let text=unsafe{(self.text)(child)};if text.is_null(){return Ok(None);}
            let mut e=std::ptr::null_mut();let n=unsafe{(self.count)(text,&mut e)};valid(e)?;
            if n<0 || n>131072 {return Ok(None);}
            chars+=n as usize;if chars>131072{return Ok(None);}
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
            found.push(Field{path:path.clone(),value,role,frame,caret:Some(caret),selections:Some(selections),paragraphs});
            }
            }
        }
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
    fn read(&self,pid:u32,rect:[i32;4],window_local:bool)->std::result::Result<Field,String>{
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
                let origin=(frame[0]==rect[0] && frame[1]==rect[1]) || (frame[0]==0 && frame[1]==0);
                if origin && frame[2..]==rect[2..] && unsafe{(self.has)(states,1)}!=0 {windows.push((w,vec![j]));}
            }
        }
        if windows.len()!=1{return Err("The native window identity could not be proved uniquely.".into());}
        let (w,mut path)=windows.pop().unwrap();
        // Root desktop application order can change independently; identity is
        // the selected process plus its top-level window index and field path.
        let mut found=Vec::new();self.walk(w,&mut path,&mut 0,&mut found,window_local.then_some(rect))?;
        if found.len()!=1{return Err("Exactly one focused editable native field is required.".into());}Ok(found.remove(0))
    }
}
/// Private read-only daemon subcommand. The desktop runner owns its deadline.
pub fn main(args:&[String])->u8 {
    let result=(|| {
        if args.len()!=5 && !(args.len()==6 && args[5]=="window-local") {return Err("Expected native-field PID X Y WIDTH HEIGHT [window-local].".into());}
        let pid=args[0].parse::<u32>().map_err(|_|"Invalid native PID.")?;
        let mut rect=[0;4];for (dst,s) in rect.iter_mut().zip(&args[1..]){*dst=s.parse().map_err(|_|"Invalid native window geometry.")?;}
        if pid==0 || rect[2]<=0 || rect[3]<=0{return Err("Invalid native window identity.".into());}
        unsafe{Api::load()}?.read(pid,rect,args.len()==6)
    })();
    match result {Ok(f)=>{println!("{}",serde_json::to_string(&f).unwrap());0},Err(e)=>{eprintln!("{e}");1}}
}
