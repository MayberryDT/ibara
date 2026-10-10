//! Eligibility of the exactly matched Ubuntu target package.
use super::LIB;
use std::path::Path;
#[path="../../shared/gnome_target.rs"]
mod eligibility;
pub fn target_package()->bool { Path::new(LIB).join("gnome-target.json").exists() }
pub fn require_manifest()->Result<(),String> {eligibility::require_manifest(&crate::version())}
pub fn require_target()->Result<(),String> {eligibility::require_target(&crate::version())}
pub fn require_runtime_target()->Result<(),String> {eligibility::require_runtime_target(&crate::version())}
pub fn require_session()->Result<(),String> {
    require_target()?;
    require_live_session(std::env::vars_os().collect())
}
pub fn require_desktop_session(desktop:&super::Account)->Result<(),String> {
    require_target()?;
    let bus=format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{}/bus",desktop.uid);
    let runtime=format!("XDG_RUNTIME_DIR=/run/user/{}",desktop.uid);
    super::output(std::process::Command::new("runuser").args(["--user",desktop.name.as_str(),"--","/usr/bin/env",
        "XDG_CURRENT_DESKTOP=GNOME",&bus,&runtime,"/usr/lib/ibara/bin/ibara","setup","--check-target"]))
        .map(|_|())
}
fn require_live_session(env:Vec<(std::ffi::OsString,std::ffi::OsString)>)->Result<(),String> {
    if !crate::desktop::gnome::Gnome::selected(&env) {return Err("Run Ubuntu target setup in the GNOME graphical session.".into());}
    let gnome=crate::desktop::gnome::Gnome::new(std::sync::Arc::from(env));
    let state=super::user::runtime()?.block_on(gnome.state()).map_err(|e|e.message)?;
    if state.monitors.len()!=1 || state.monitors[0].scale!=1.0 {
        return Err("This GNOME target candidate supports one output at scale1 only; reconfigure explicitly before setup.".into());
    }
    if state.locked || state.guarded_input_api!=1 || state.cursor_api!=1 {
        return Err("The unlocked GNOME session must load the packaged helper and matching Mutter guard first. Enable ibara@zet.io and sign out/in explicitly; setup will not restart your desktop.".into());
    }
    let addresses=super::run("tailscale",&["ip"])?;
    if !addresses.lines().any(|line|line.parse::<std::net::IpAddr>().is_ok_and(|ip|match ip {
        std::net::IpAddr::V4(ip)=>ip.octets()[0]==100 && (64..=127).contains(&ip.octets()[1]),
        std::net::IpAddr::V6(ip)=>ip.segments()[..3]==[0xfd7a,0x115c,0xa1e0],
    })) {
        return Err("Enroll this computer on your Tailscale network before target setup; setup does not enroll it.".into());
    }
    Ok(())
}
