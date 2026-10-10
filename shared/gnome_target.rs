use std::{path::Path,os::unix::fs::MetadataExt};
const NATIVE_VERSION:&str="50.1-0ubuntu2.4+ibara2";
pub fn require_manifest(version:&str)->Result<(),String> {
    let path=Path::new("/usr/lib/ibara").join("gnome-target.json");
    let metadata=std::fs::symlink_metadata(&path).map_err(|_|"Ubuntu target setup is unavailable in this operator-only package.".to_owned())?;
    if !metadata.is_file() || metadata.uid()!=0 || metadata.mode()&0o022!=0 || metadata.len()>16384 {
        return Err("The Ubuntu target manifest is not a protected package file.".into());
    }
    let value:serde_json::Value=serde_json::from_slice(&std::fs::read(&path).map_err(|e|e.to_string())?).map_err(|_|"Invalid Ubuntu target manifest")?;
    if value["schema"]!=1 || value["candidate_version"]!=version || value["mutter_version"]!=NATIVE_VERSION || value["development_only"]!=version.contains("-dev.") {
        return Err("The Ubuntu target manifest does not match this ibara package.".into());
    }
    Ok(())
}
pub fn require_target(version:&str)->Result<(),String> {
    require_manifest(version)?;
    if installed_version("ibara").as_deref()!=Some(version) {
        return Err("The installed ibara package is incomplete or differs from the running binary; finish package recovery before target use.".into());
    }
    for package in ["libmutter-18-0","mutter-common","mutter-common-bin","gir1.2-mutter-18"] {
        if installed_version(package).as_deref()!=Some(NATIVE_VERSION) {
            return Err(format!("Ubuntu target needs {package}={NATIVE_VERSION}; install the matching maintained integration explicitly. Stock rollback remains available."));
        }
    }
    Ok(())
}
/// Recheck the installed tuple when dpkg changes, including a partial unpack.
/// The manifest remains checked on every effect; normal polls only stat dpkg.
pub fn require_runtime_target(version:&str)->Result<(),String> {
    require_manifest(version)?;
    fn stamp()->Result<(u64,u64,u64,i64,i64,i64,i64),String> {
        let status=std::fs::metadata("/var/lib/dpkg/status").map_err(|e|format!("Cannot inspect installed native packages: {e}"))?;
        let updates=std::fs::read_dir("/var/lib/dpkg/updates").map_err(|e|format!("Cannot inspect package transaction: {e}"))?;
        for entry in updates {
            let entry=entry.map_err(|e|e.to_string())?;
            if entry.file_name().to_string_lossy().bytes().all(|c|c.is_ascii_digit()) {
                return Err("Ubuntu package installation is unsettled; finish package recovery before target use.".into());
            }
        }
        Ok((status.dev(),status.ino(),status.len(),status.mtime(),status.mtime_nsec(),status.ctime(),status.ctime_nsec()))
    }
    type Stamp=(u64,u64,u64,i64,i64,i64,i64);
    static CHECK:std::sync::Mutex<Option<(Stamp,Result<(),String>)>>=std::sync::Mutex::new(None);
    let before=stamp()?;
    let mut cache=CHECK.lock().map_err(|_|"Native package verification was interrupted".to_owned())?;
    if let Some((previous,result))=&*cache { if *previous==before {return result.clone();} }
    let result=require_target(version);
    if stamp()?!=before {return Err("Ubuntu packages changed during verification; retry after installation settles.".into());}
    *cache=Some((before,result.clone()));
    result
}

fn installed_version(name:&str)->Option<String> {
 let result=std::process::Command::new("dpkg-query").args(["-W","-f=${db:Status-Status} ${Version}",name]).output().ok()?;
 if !result.status.success(){return None;}
 let answer=String::from_utf8(result.stdout).ok()?;
 let (status,version)=answer.trim().split_once(' ')?;
 (status=="installed").then(||version.to_owned())
}
