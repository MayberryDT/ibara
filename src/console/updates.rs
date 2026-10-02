//! Verified release awareness. This loop only checks; it never installs.
use crate::install::{update, vercmp};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;
use std::os::unix::fs::DirBuilderExt;

static CHECK: Mutex<()> = Mutex::new(());

fn path() -> std::path::PathBuf {
    crate::operator::directory::operator_state_dir().join("latest.json")
}

pub(super) fn cached() -> Value {
    let mut value = std::fs::read(path()).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object).unwrap_or_else(|| json!({"version": null}));
    value["console_version"] = json!(crate::version());
    value["installed_version"] = json!(crate::install::installed_version().unwrap_or_else(crate::version));
    value["enabled"] = json!(crate::settings::current().bool("check_for_updates"));
    value
}

fn check() -> Result<(), String> {
    let _lock = CHECK.lock().map_err(|e| e.to_string())?;
    let mut record = cached();
    let result: Result<(), String> = (|| {
        let channel = update::Channel::built_in().ok_or("This build has no update channel.")?;
        let dir = update::scratch()?;
        let release = update::latest(&channel, &dir);
        let _ = std::fs::remove_dir_all(dir);
        let release = release?;
        if let Some(earlier) = record["version"].as_str() && vercmp(&release.version, earlier)? < 0 {
            return Err("The signed channel is older than the newest release already checked.".into());
        }
        record["version"] = json!(release.version);
        record["released_at"] = json!(release.released_at);
        record["notes"] = json!(release.notes);
        record["history"] = json!(release.history);
        Ok(())
    })();
    record["checked_at"] = json!(crate::ids::now_millis());
    record["error"] = json!(result.err());
    let path = path();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, format!("{record}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(temp, path).map_err(|e| e.to_string())
}

pub(super) async fn refresh() -> Result<Value, String> {
    tokio::task::spawn_blocking(check).await.map_err(|e| e.to_string())??;
    Ok(cached())
}

pub(super) fn start() {
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut first = true;
        loop {
            tick.tick().await;
            if !crate::settings::current().bool("check_for_updates") { continue; }
            let last = cached()["checked_at"].as_i64().unwrap_or(0);
            if first || crate::ids::now_millis() - last >= 6 * 60 * 60 * 1000 {
                first = false;
                if let Err(e) = refresh().await { eprintln!("Release check: {e}"); }
            }
        }
    });
}
