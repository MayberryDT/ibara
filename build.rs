use std::{fs, path::{Path,PathBuf}, io::Write, process::{Command,Stdio}};
fn output(program:&str,args:&[&str])->Option<String>{let out=Command::new(program).args(args).output().ok()?;out.status.success().then(||String::from_utf8_lossy(&out.stdout).trim().to_owned())}
fn files(path:&Path,out:&mut Vec<PathBuf>){if path.is_dir(){for e in fs::read_dir(path).expect("source directory"){let e=e.expect("source entry");if e.file_type().expect("source type").is_symlink(){continue;}files(&e.path(),out);}}else{out.push(path.to_owned());}}
fn main(){
    println!("cargo:rerun-if-env-changed=IBARA_PKGREL");
    let mut paths=Vec::new();for path in ["src","shared","build.rs","Cargo.toml","Cargo.lock"]{println!("cargo:rerun-if-changed={path}");files(Path::new(path),&mut paths);}paths.sort();
    // Stamp exact compiled source even when a packaged source tar has no .git.
    let mut hash=Command::new("sha256sum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("source hash tool");
    let mut input=hash.stdin.take().unwrap();for p in paths{input.write_all(p.to_string_lossy().as_bytes()).unwrap();input.write_all(&[0]).unwrap();input.write_all(&fs::read(p).expect("source bytes")).unwrap();input.write_all(&[0]).unwrap();}drop(input);
    let result=hash.wait_with_output().expect("source hash");assert!(result.status.success());let digest=String::from_utf8(result.stdout).unwrap();let sha=&digest[..12];
    let date=output("date",&["-u","+%Y-%m-%d"]).expect("build date");
    let dirty=output("git",&["status","--porcelain"]).is_some_and(|s|!s.is_empty());
    let environment=if std::env::var_os("IBARA_PKGREL").is_some(){"production"}else{"private"};
    println!("cargo:rustc-env=IBARA_FACTS_BUILD={environment}-{date}-{sha}{}",if dirty{"-dirty"}else{""});
}
