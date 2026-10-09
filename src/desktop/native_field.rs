//! Read the actual focused native field in an isolated, bounded process.
//! Cua 0.28 cannot prove tree completeness. No input is sent by this helper.
use super::{run::{self, Cmd, Cancel}, Window};
use crate::error::{Result, IbaraError};
use serde::{Serialize, Deserialize};
use std::{ffi::{CStr, OsString}, time::Duration};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field { pub path: Vec<i32>, pub value: String, pub role: String, pub frame: [i32;4] }
fn refused(message: impl Into<String>) -> IbaraError { IbaraError::new("CAPABILITY_UNAVAILABLE", message, true).with("execution_not_started", true) }
pub async fn read(window: &Window, env: &[(OsString, OsString)], cancel: Option<&Cancel>) -> Result<Field> {
    let rect=window.geometry();
    let exe=std::env::current_exe().map_err(|e|refused(e.to_string()))?;
    let mut cmd=Cmd::new(exe).args(["native-field".to_string(),window.pid.to_string(),rect.x.to_string(),rect.y.to_string(),rect.width.to_string(),rect.height.to_string()]).envs(env).timeout(Duration::from_secs(6)).max_output(1024*1024);
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
 pid:"atspi_accessible_get_process_id"=>unsafe extern "C" fn(Obj,ErrPtr)->u32,
 states:"atspi_accessible_get_state_set"=>unsafe extern "C" fn(Obj)->Obj,
 has:"atspi_state_set_contains"=>unsafe extern "C" fn(Obj,i32)->i32,
 role:"atspi_accessible_get_role_name"=>unsafe extern "C" fn(Obj,ErrPtr)->*mut libc::c_char,
 component:"atspi_accessible_get_component_iface"=>unsafe extern "C" fn(Obj)->Obj,
 extents:"atspi_component_get_extents"=>unsafe extern "C" fn(Obj,i32,ErrPtr)->*mut Rect,
 text:"atspi_accessible_get_text_iface"=>unsafe extern "C" fn(Obj)->Obj,
 count:"atspi_text_get_character_count"=>unsafe extern "C" fn(Obj,ErrPtr)->i32,
 value:"atspi_text_get_text"=>unsafe extern "C" fn(Obj,i32,i32,ErrPtr)->*mut libc::c_char,
}
fn valid(error:Obj)->std::result::Result<(),String> { if error.is_null(){Ok(())}else{Err("Native accessibility read failed; no field was proved.".into())} }
fn present(obj:Obj)->std::result::Result<Obj,String>{if obj.is_null(){Err("Native accessibility returned no object.".into())}else{Ok(obj)}}
impl Api {
    fn child_count(&self,node:Obj)->std::result::Result<i32,String>{
        let mut e=std::ptr::null_mut();let n=unsafe{(self.children)(node,&mut e)};valid(e)?;
        if !(0..=4096).contains(&n){return Err("Native field traversal exceeds its limit.".into());}Ok(n)
    }
    fn child_at(&self,node:Obj,index:i32)->std::result::Result<Obj,String>{
        let mut e=std::ptr::null_mut();let p=unsafe{(self.child)(node,index,&mut e)};valid(e)?;present(p)
    }
    fn frame(&self,node:Obj)->std::result::Result<[i32;4],String>{
        let c=present(unsafe{(self.component)(node)})?;let mut e=std::ptr::null_mut();let r=unsafe{(self.extents)(c,0,&mut e)};valid(e)?;present(r.cast())?;
        let r=unsafe{&*r};Ok([r.x,r.y,r.width,r.height])
    }
    fn walk(&self,node:Obj,path:&mut Vec<i32>,visited:&mut usize,found:&mut Vec<Field>)->std::result::Result<(),String>{
        *visited+=1;if *visited>4096 || path.len()>40{return Err("Native field traversal was incomplete.".into());}
        let states=present(unsafe{(self.states)(node)})?;
        if unsafe{(self.has)(states,12)}!=0 { // ATSPI_STATE_FOCUSED
            let mut e=std::ptr::null_mut();let r=unsafe{(self.role)(node,&mut e)};valid(e)?;present(r.cast())?;
            let role=unsafe{CStr::from_ptr(r)}.to_str().map_err(|_|"Invalid native field role.")?.to_owned();
            if role.to_lowercase().contains("password"){return Err("Unicode paste into password fields is unavailable.".into());}
            if unsafe{(self.has)(states,7)}==0 || unsafe{(self.has)(states,8)}==0{return Err("The focused native control is not an enabled editable field.".into());}
            let text=present(unsafe{(self.text)(node)})?;
            let mut e=std::ptr::null_mut();let count=unsafe{(self.count)(text,&mut e)};valid(e)?;
            if !(0..=131072).contains(&count){return Err("Native field text exceeds the verification limit.".into());}
            let mut e=std::ptr::null_mut();let s=unsafe{(self.value)(text,0,count,&mut e)};valid(e)?;present(s.cast())?;
            let value=unsafe{CStr::from_ptr(s)}.to_str().map_err(|_|"Invalid native field text.")?.to_owned();
            found.push(Field{path:path.clone(),value,role,frame:self.frame(node)?});
        }
        for i in 0..self.child_count(node)? {let child=self.child_at(node,i)?;path.push(i);self.walk(child,path,visited,found)?;path.pop();}
        Ok(())
    }
    fn read(&self,pid:u32,rect:[i32;4])->std::result::Result<Field,String>{
        if unsafe{(self.init)()}!=0{return Err("Native accessibility initialization failed.".into());}
        unsafe{(self.timeout)(500,500)};
        let root=present(unsafe{(self.desktop)(0)})?;let mut windows=Vec::new();
        for i in 0..self.child_count(root)? {
            let app=self.child_at(root,i)?;let mut e=std::ptr::null_mut();let got=unsafe{(self.pid)(app,&mut e)};
            if !e.is_null() || got!=pid {continue;}
            for j in 0..self.child_count(app)? {
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
        let mut found=Vec::new();self.walk(w,&mut path,&mut 0,&mut found)?;
        if found.len()!=1{return Err("Exactly one focused editable native field is required.".into());}Ok(found.remove(0))
    }
}
/// Private read-only daemon subcommand. The desktop runner owns its deadline.
pub fn main(args:&[String])->u8 {
    let result=(|| {
        if args.len()!=5{return Err("Expected native-field PID X Y WIDTH HEIGHT.".into());}
        let pid=args[0].parse::<u32>().map_err(|_|"Invalid native PID.")?;
        let mut rect=[0;4];for (dst,s) in rect.iter_mut().zip(&args[1..]){*dst=s.parse().map_err(|_|"Invalid native window geometry.")?;}
        if pid==0 || rect[2]<=0 || rect[3]<=0{return Err("Invalid native window identity.".into());}
        unsafe{Api::load()}?.read(pid,rect)
    })();
    match result {Ok(f)=>{println!("{}",serde_json::to_string(&f).unwrap());0},Err(e)=>{eprintln!("{e}");1}}
}
