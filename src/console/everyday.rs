//! Everyday commands for one computer over its operator route: logs, health,
//! power, its settings and theme, repairs, approvals, tasks, results,
//! procedures, access, and a terminal. Every one names the computer and the
//! controller epoch the plugin knows (`--computer ID --epoch E`, like
//! `operator-control`), and the target decides with its access model: reads
//! need watch, changes need administer. A refusal is said plainly.

use super::envelope::{Fault, Handled};
use super::process::{launch, which};
use super::{Console, Ctx, option, positional, validated_id};
use crate::error::IbaraError;
use crate::operator::client::{TransferLink, collect};
use crate::operator::directory::OperatorDirectory;
use crate::operator::js;
use serde_json::{Value, json};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Ordinary operations: the session's own 10 s transport deadline plus margin.
pub(super) const CALL_DEADLINE: Duration = Duration::from_secs(15);
/// Theme changes and repairs: the target allows 150 s.
pub(super) const SLOW_DEADLINE: Duration = Duration::from_secs(180);

/// The name this console shows for a computer.
pub(super) fn label(console: &Console, computer: &str) -> String {
    OperatorDirectory::open(&console.database)
        .and_then(|d| d.get_computer(computer))
        .ok()
        .flatten()
        .map(|r| r.label)
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| "That computer".into())
}

/// `data.computer_id === computer && data.controller_epoch === epoch`.
fn binds(data: &Value, computer: &str, epoch: &str) -> bool {
    data.get("computer_id").and_then(Value::as_str) == Some(computer) && data.get("controller_epoch").and_then(Value::as_str) == Some(epoch)
}

/// What a command asks the other computer to allow, in plain words.
fn asks(command: &str) -> &'static str {
    match command {
        "operator-logs" => "read its logs",
        "operator-health" => "check on it",
        "operator-power" => "restart, shut down, sleep, lock or update it",
        "operator-settings" => "change its settings",
        "operator-theme" | "theme-fleet" => "change its theme",
        "operator-answer-attention" | "fleet-attention" => "answer its approvals",
        "operator-repair" => "repair it",
        "operator-tasks" | "operator-task" | "operator-artifacts" | "operator-procedures" | "operator-procedure" | "away" => {
            "see what happened on it"
        }
        "operator-task-extend" | "operator-task-revoke" => "change its tasks",
        "operator-procedure-review" => "review its procedures",
        "operator-access-set" | "operator-access-remove" | "operator-access-unpair" => "change who can use it",
        "operator-artifact-save" => "collect its results",
        "open-terminal" => "open a terminal on it",
        "wake" => "wake other computers",
        _ => "do that",
    }
}

/// A refusal from the other computer (or its route) as a failure envelope.
pub(super) fn refusal(ctx: &Ctx, label: &str, error: &IbaraError) -> Value {
    let text = error.message.strip_prefix(&format!("{}: ", error.code)).unwrap_or(&error.message);
    match error.code {
        "PERMISSION_DENIED" if text.contains(" is denied ") => {
            let message = format!("{label} doesn't let this computer {}. Its owner can change that under Access.", asks(&ctx.head.command));
            ctx.failure("PERMISSION_DENIED", &message, "unauthorized", false)
        }
        "PERMISSION_DENIED" if text.contains("binding changed") || text.contains("generation changed") => {
            ctx.failure("STALE_TARGET", &format!("{label} restarted or changed. Refresh it, then try again."), "unauthorized", true)
        }
        "SESSION_UNAVAILABLE" => ctx.failure("OFFLINE", &format!("{label} isn't answering."), "offline", true),
        code => ctx.failure(code, text, "failed", error.retry_safe),
    }
}

/// One operation on one computer: the reply bound to that computer and
/// epoch, or the failure envelope to answer with.
pub(super) async fn call(ctx: &Ctx, computer: &str, epoch: &str, op: &str, fields: Value, deadline: Duration) -> Result<Value, Value> {
    let label = || label(&ctx.console, computer);
    match tokio::time::timeout(deadline, ctx.console.sessions.call(computer, Some(epoch), op, fields)).await {
        Err(_) => Err(ctx.failure("TIMEOUT", &format!("{} isn't answering.", label()), "offline", true)),
        Ok(Err(error)) => Err(refusal(ctx, &label(), &error)),
        Ok(Ok(data)) if !binds(&data, computer, epoch) => Err(ctx.failure(
            "IDENTITY_MISMATCH",
            "The other computer's reply did not match the computer and epoch asked for.",
            "unauthorized",
            false,
        )),
        Ok(Ok(data)) => Ok(data),
    }
}

/// What the plugin passes: the computer and its controller epoch.
pub(super) fn target(ctx: &Ctx) -> Result<(String, String), Fault> {
    Ok((validated_id(option(&ctx.args, "--computer"), "computer_id")?, validated_id(option(&ctx.args, "--epoch"), "controller_epoch")?))
}

fn reference(ctx: &Ctx, name: &str, label: &str) -> Result<String, Fault> {
    validated_id(option(&ctx.args, name), label)
}

/// The access change body: one JSON object.
fn access_body(ctx: &Ctx) -> Result<Value, Fault> {
    let body = positional(&ctx.args).first().copied().ok_or_else(|| Fault::plain("Expected the access change as JSON."))?;
    let value: Value = serde_json::from_str(body).map_err(|_| Fault::plain("The access change is not valid JSON."))?;
    if !value.is_object() {
        return Err(Fault::plain("The access change must be a JSON object."));
    }
    Ok(value)
}

/// The per-computer commands (`--computer ID --epoch E`).
pub async fn per_computer(ctx: &Ctx) -> Handled {
    let (computer, epoch) = target(ctx)?;
    let command = ctx.head.command.as_str();
    let pos = positional(&ctx.args);
    let text = |name: &str| option(&ctx.args, name).unwrap_or("");
    let (op, fields, deadline) = match command {
        "operator-logs" => ("logs", json!({"which": text("--which"), "lines": text("--lines")}), CALL_DEADLINE),
        "operator-health" => ("health", json!({}), CALL_DEADLINE),
        "operator-power" => ("power", json!({"action": text("--action")}), CALL_DEADLINE),
        "operator-settings" => ("settings", json!({"args": super::fleet::settings_args(&ctx.args)}), CALL_DEADLINE),
        // `REF approve|deny` for an approval; `REF --answer TEXT` (one of its options)
        // or `REF --dismiss` for an agent's question. The computer checks which it is.
        "operator-answer-attention" => {
            let reference = validated_id(pos.first().copied(), "attention")?;
            if ctx.args.iter().any(|a| a == "--dismiss") {
                ("answer_attention", json!({"att_ref": reference, "dismiss": true}), CALL_DEADLINE)
            } else {
                let answer = option(&ctx.args, "--answer").or(pos.get(1).copied()).unwrap_or("");
                if answer.trim().is_empty() || answer.chars().count() > 1000 {
                    return Err(Fault::plain("Choose approve or deny, or give an answer of 1 to 1000 characters."));
                }
                ("answer_attention", json!({"att_ref": reference, "answer": answer}), CALL_DEADLINE)
            }
        }
        "operator-repair" => ("repair", json!({"fix": pos.first().copied().unwrap_or("")}), SLOW_DEADLINE),
        "operator-tasks" => ("tasks", json!({}), CALL_DEADLINE),
        "operator-task" => ("task", json!({"task_ref": reference(ctx, "--task", "task_ref")?}), CALL_DEADLINE),
        "operator-artifacts" => {
            let cursor = option(&ctx.args, "--cursor").filter(|c| !c.is_empty()).map(|c| validated_id(Some(c), "cursor")).transpose()?;
            ("artifacts", json!({"limit": text("--limit"), "cursor": cursor}), CALL_DEADLINE)
        }
        "operator-procedures" => ("procedures", json!({}), CALL_DEADLINE),
        "operator-procedure" => ("procedure", json!({"procedure_ref": reference(ctx, "--ref", "procedure_ref")?}), CALL_DEADLINE),
        "operator-task-extend" => (
            "task_extend",
            json!({"task_ref": reference(ctx, "--task", "task_ref")?, "extra_seconds": text("--seconds")}),
            CALL_DEADLINE,
        ),
        "operator-task-revoke" => ("task_revoke", json!({"task_ref": reference(ctx, "--task", "task_ref")?}), CALL_DEADLINE),
        "operator-procedure-review" => (
            "procedure_review",
            json!({"procedure_ref": reference(ctx, "--ref", "procedure_ref")?, "decision": text("--decision"), "expected_sha256": text("--sha")}),
            CALL_DEADLINE,
        ),
        "operator-access" => ("access", Value::Null, CALL_DEADLINE),
        "operator-access-set" => ("access_set", access_body(ctx)?, CALL_DEADLINE),
        "operator-access-remove" => ("access_remove", access_body(ctx)?, CALL_DEADLINE),
        "operator-access-unpair" => ("access_unpair", access_body(ctx)?, CALL_DEADLINE),
        _ => return Err(Fault::Plain(format!("Unknown command {command}."))),
    };
    let data = match call(ctx, &computer, &epoch, op, fields, deadline).await {
        Ok(data) => data,
        Err(failure) => return Ok(failure),
    };
    match op {
        "health" => remember(&ctx.console, &computer, &data["result"]),
        "settings" if pos.first() != Some(&"get") => {
            if let Some(name) = data["result"].get("key").filter(|k| *k == "name").and(data["result"]["value"].as_str()) {
                adopt_name(&ctx.console, &computer, name);
            }
        }
        // Settled (not itself held for someone's approval): leave it out from now on.
        "answer_attention" if data["result"]["item"].is_object() => {
            if let Some(reference) = pos.first() {
                super::fleet::forget_attention(&ctx.console, &computer, reference);
            }
        }
        _ => {}
    }
    Ok(ctx.ready(data))
}

/// What a computer reports about itself that this console keeps: how to
/// wake it (for when it sleeps) and its own name.
pub(super) fn remember(console: &Console, computer: &str, result: &Value) {
    if let Some(wake) = result.get("wake")
        && let Ok(mut directory) = OperatorDirectory::open(&console.database)
        && let Err(e) = directory.set_wake(computer, wake)
    {
        eprintln!("ibarad: could not record how to wake {computer}: {}", e.message);
    }
    if let Some(name) = result.get("name").and_then(Value::as_str) {
        adopt_name(console, computer, name);
    }
}

/// A computer's own name becomes its label here, unless this console's
/// person chose one.
fn adopt_name(console: &Console, computer: &str, name: &str) {
    if let Ok(mut directory) = OperatorDirectory::open(&console.database) {
        let _ = directory.adopt_label(computer, name);
    }
}

/// `operator-artifact-save --computer ID --epoch E --ref REF --to PATH`:
/// collect one result into a new file (never over an existing one), over
/// the computer's operator route.
pub async fn artifact_save(ctx: &Ctx) -> Handled {
    let (computer, epoch) = target(ctx)?;
    let artifact = reference(ctx, "--ref", "artifact_ref")?;
    let destination = option(&ctx.args, "--to").unwrap_or("");
    let path = Path::new(destination);
    if destination.is_empty() || !path.is_absolute() || destination.contains('\0') {
        return Err(Fault::plain("Choose a new file to save the result as, as a full path."));
    }
    if std::fs::symlink_metadata(path).is_ok() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let folder = path.parent().map(|p| p.display().to_string()).unwrap_or_else(|| "/".into());
        return Ok(ctx.failure("DESTINATION_EXISTS", &format!("{name} is already in {folder}. Move or rename it, then save again."), "ready", true));
    }
    let mut link = OperatorLink { console: ctx.console.clone(), computer: computer.clone(), epoch };
    let collected = async {
        let outcome = collect(&mut link, &artifact, path).await;
        let _ = link.request(json!({"kind": "end_transfer"})).await;
        outcome
    };
    match tokio::time::timeout(Duration::from_secs(300), collected).await {
        Err(_) => Ok(ctx.failure("TIMEOUT", "Saving the result took too long. What arrived is kept; save again to continue.", "offline", true)),
        Ok(Err(error)) => Ok(refusal(ctx, &label(&ctx.console, &computer), &error)),
        Ok(Ok(data)) => Ok(ctx.ready(data)),
    }
}

/// A collection's requests over one computer's operator route.
struct OperatorLink {
    console: Arc<Console>,
    computer: String,
    epoch: String,
}

impl TransferLink for OperatorLink {
    fn request(&mut self, request: Value) -> Pin<Box<dyn Future<Output = crate::error::Result<Value>> + Send + '_>> {
        Box::pin(async move {
            let data = self.console.sessions.call(&self.computer, Some(&self.epoch), "artifact_transfer", json!({"request": request})).await?;
            let mut result = data.get("result").cloned().unwrap_or(Value::Null);
            if let Some(map) = result.as_object_mut() {
                for key in ["endpoint_id", "controller_epoch", "authorization_generation"] {
                    map.remove(key);
                }
            }
            Ok(result)
        })
    }
}

/// `open-terminal --computer ID`: a terminal here, signed in to that
/// computer's desktop account with Tailscale SSH (the tailnet's SSH rules
/// decide; ibara only opens the window). The account comes from the
/// computer's health report.
pub async fn open_terminal(ctx: &Ctx) -> Handled {
    let computer = validated_id(option(&ctx.args, "--computer"), "computer_id")?;
    let Some(launcher) = which("omarchy-launch-terminal").or_else(|| which("xdg-terminal-exec")) else {
        return Ok(ctx.failure("MISSING_DEPENDENCY", "No terminal is available on this computer.", "missing-dependency", true));
    };
    let Some(row) = OperatorDirectory::open(&ctx.console.database).ok().and_then(|d| d.get_computer(&computer).ok().flatten()) else {
        return Ok(ctx.failure("UNKNOWN_COMPUTER", "That computer is not in this console.", "failed", false));
    };
    let epoch = match super::fleet::epoch(&ctx.console, &computer).await {
        Ok(epoch) => epoch,
        Err(error) => return Ok(refusal(ctx, &row.label, &error)),
    };
    let health = match call(ctx, &computer, &epoch, "health", json!({}), CALL_DEADLINE).await {
        Ok(data) => data,
        Err(failure) => return Ok(failure),
    };
    let account = js::string_or(health["result"].get("account"), "");
    if !crate::operator::pattern::account(&account) || !crate::operator::pattern::node(&row.host) {
        return Ok(ctx.failure("CAPABILITY_UNAVAILABLE", &format!("{} did not say which account to open.", row.label), "failed", true));
    }
    let destination = format!("{account}@{}", row.host);
    let pid = launch(&[launcher.display().to_string(), "tailscale".into(), "ssh".into(), destination.clone()])?;
    Ok(ctx.ready(json!({"computer_id": computer, "account": account, "destination": destination, "pid": pid})))
}
