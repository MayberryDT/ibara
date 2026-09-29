//! `ibara control ARGS…`: the administrator route (replaces `agent/ibara-control`).
//!
//! Runs `computerctl ARGS…` on the station named by the descriptor, as its
//! separate administrative account, over Tailscale SSH. Tailscale SSH accepts one
//! remote shell command, so each argument is quoted as POSIX shell data.

use super::transport::station_descriptor_path;
use super::{fail, js, pattern};
use crate::error::Result;
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

/// The station descriptor fields `ibara-control` relies on.
#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    pub station_id: String,
    pub node: String,
    pub operator_account: String,
    pub agent_account: String,
}

/// Why a descriptor cannot be used, with the exit status `ibara-control` used.
#[derive(Debug)]
pub struct StationError {
    pub status: u8,
    pub message: String,
}

/// Load and validate the descriptor (ibara-control:8-23).
pub fn load_station(path: &Path) -> std::result::Result<Station, StationError> {
    if !path.exists() {
        return Err(StationError { status: 2, message: format!("Selected ibara station descriptor does not exist: {}", path.display()) });
    }
    let parsed: Value = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
        .map_err(|message| StationError { status: 1, message })?;
    // `String(x || '')`: falsy values become the empty string.
    let text = |name: &str| {
        let value = parsed.get(name);
        if js::truthy(value) { js::string(value.unwrap_or(&Value::Null)) } else { String::new() }
    };
    let station = Station {
        station_id: text("station_id"),
        node: text("node"),
        operator_account: text("operator_account"),
        agent_account: text("agent_account"),
    };
    if !js::same_number(parsed.get("schema_version"), 1.0)
        || !pattern::account(&station.node)
        || !pattern::account(&station.operator_account)
        || !pattern::account(&station.agent_account)
        || !pattern::id(&station.station_id)
    {
        return Err(StationError { status: 2, message: "Invalid ibara station descriptor.".into() });
    }
    Ok(station)
}

/// `"'" + arg.replaceAll("'", "'\\''") + "'"`.
pub fn posix_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The remote command: `computerctl` and every argument, each quoted; `status` when none.
pub fn remote_command(args: &[String]) -> String {
    let status = ["status".to_string()];
    let args = if args.is_empty() { &status[..] } else { args };
    std::iter::once("computerctl").chain(args.iter().map(String::as_str)).map(posix_quote).collect::<Vec<_>>().join(" ")
}

/// `tailscale ssh <operator_account>@<node> "<quoted command>"`.
pub fn tailscale_command(station: &Station, args: &[String]) -> Command {
    let mut command = Command::new("tailscale");
    command.args(["ssh".to_string(), format!("{}@{}", station.operator_account, station.node), remote_command(args)]);
    command
}

/// The control route for `ibara client --operator` (`ibara-control transfer-session`).
pub fn transfer_session_command() -> Result<Command> {
    let station = load_station(&station_descriptor_path()).map_err(|e| fail(e.message))?;
    Ok(tailscale_command(&station, &["transfer-session".to_string()]))
}

/// `ibara control ARGS…`: replaces this process with tailscale, so its exit status
/// and signals are the command's own, as `spawnSync(…, {stdio:'inherit'})` re-raised them.
pub fn main(args: Vec<String>) -> ExitCode {
    let station = match load_station(&station_descriptor_path()) {
        Ok(station) => station,
        Err(error) => {
            eprintln!("{}", error.message);
            return ExitCode::from(error.status);
        }
    };
    let error = tailscale_command(&station, &args).exec();
    eprintln!("{error}");
    ExitCode::from(127)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_as_posix_shell_data() {
        let args = vec!["logs".to_string(), "it's $(rm -rf ~) `x`".to_string(), String::new()];
        assert_eq!(remote_command(&args), r#"'computerctl' 'logs' 'it'\''s $(rm -rf ~) `x`' ''"#);
        assert_eq!(remote_command(&[]), "'computerctl' 'status'");
    }

    #[test]
    fn descriptor_with_bad_accounts_or_schema_is_refused() {
        let tmp = super::super::directory::tests::TempDir::new("station");
        let path = tmp.0.join("station.json");
        let write = |value: Value| std::fs::write(&path, value.to_string()).unwrap();
        let good = serde_json::json!({"schema_version": 1, "station_id": "station_a", "node": "tulip1",
            "operator_account": "riley", "agent_account": "tulip1"});
        write(good.clone());
        let station = load_station(&path).unwrap();
        assert_eq!(
            tailscale_command(&station, &["pause".into()]).get_args().collect::<Vec<_>>(),
            ["ssh", "riley@tulip1", "'computerctl' 'pause'"]
        );
        for (key, value) in [("schema_version", serde_json::json!(2)), ("node", serde_json::json!("tulip 1")), ("operator_account", serde_json::json!(""))] {
            let mut bad = good.clone();
            bad[key] = value;
            write(bad);
            let err = load_station(&path).err().unwrap();
            assert_eq!((err.status, err.message.as_str()), (2, "Invalid ibara station descriptor."));
        }
        let missing = load_station(&tmp.0.join("absent.json")).err().unwrap();
        assert_eq!(missing.status, 2);
        assert!(missing.message.starts_with("Selected ibara station descriptor does not exist: "));
    }
}
