//! `ibara join`: answer requests to add this computer from a terminal, for a
//! computer with no screen. It lists the computers waiting to be added, with the
//! code both screens show, and asks about each one; `--accept CODE` and
//! `--decline CODE` answer one without asking. Run it as this computer's
//! desktop user or as root; it talks to the target daemon's pairing socket.

use super::block_on;
use crate::server::pairing::ask_local;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, Write};

const USAGE: &str = "Usage: ibara join [--accept CODE | --decline CODE]";

/// `482 913`, `482913` → `482913`; anything else is not a code.
fn digits(code: &str) -> Option<String> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn describe(request: &Value) -> String {
    format!("{} ({})", text(request, "from_computer"), text(request, "from_owner"))
}

async fn waiting() -> Result<Vec<Value>, String> {
    match ask_local(&json!({"op": "requests"})).await {
        Ok(reply) if reply.get("ok") == Some(&json!(true)) => Ok(reply["requests"].as_array().cloned().unwrap_or_default()),
        Ok(reply) => Err(text(&reply["error"], "message")),
        Err(_) => Err("ibara isn't running on this computer, so no computer can ask to be added.".into()),
    }
}

async fn answer(request: &Value, accept: bool) -> Result<(), String> {
    let asked = json!({"op": "answer", "request_id": text(request, "request_id"), "answer": if accept { "accept" } else { "decline" }});
    let reply = ask_local(&asked).await.map_err(|_| "ibara on this computer stopped answering.".to_string())?;
    let who = describe(request);
    let done = match reply.get("state").and_then(Value::as_str) {
        Some("paired") => format!("Added {who}. It can now see and use this computer."),
        Some("declined") => format!("Declined {who}."),
        Some("expired") => return Err(format!("The request from {who} expired. Ask again from that computer.")),
        Some("canceled") => return Err(format!("{who} canceled its request.")),
        _ if reply.get("ok") != Some(&json!(true)) => return Err(text(&reply["error"], "message")),
        _ => return Err(format!("ibara could not finish adding {who}. Its log on this computer says why.")),
    };
    println!("{done}");
    Ok(())
}

async fn run(args: &[String]) -> Result<(), String> {
    let requests = waiting().await?;
    if let [flag, code] = args {
        let wanted = digits(code).ok_or_else(|| format!("{code} is not a six-digit code."))?;
        let request = requests
            .iter()
            .find(|r| digits(&text(r, "code")).as_deref() == Some(wanted.as_str()))
            .ok_or_else(|| format!("No computer is waiting to be added with code {}.", code.trim()))?;
        return answer(request, flag == "--accept").await;
    }
    if requests.is_empty() {
        println!("No computers are waiting to be added to this computer.");
        return Ok(());
    }
    let now = crate::ids::now_millis();
    println!("Computers waiting to be added to this computer:");
    for request in &requests {
        let left = (request["expires_at"].as_i64().unwrap_or(now) - now).max(0);
        println!("  {}  {}, {} min left", text(request, "code"), describe(request), (left + 59_999) / 60_000);
    }
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(0) } != 1 {
        println!("To add one, check that its screen shows the same code, then run: ibara join --accept CODE");
        return Ok(());
    }
    let mut failed = false;
    for request in &requests {
        print!("Add {}? Its screen should show {}. [y = add, n = decline, Enter = decide later] ", describe(request), text(request, "code"));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            break;
        }
        let outcome = match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => answer(request, true).await,
            "n" | "no" => answer(request, false).await,
            _ => Ok(()),
        };
        if let Err(message) = outcome {
            eprintln!("{message}");
            failed = true;
        }
    }
    // Each failure was already shown.
    if failed { Err(String::new()) } else { Ok(()) }
}

pub fn main(args: Vec<OsString>) -> i32 {
    let args: Vec<String> = args.into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let shape_ok = args.is_empty() || (args.len() == 2 && matches!(args[0].as_str(), "--accept" | "--decline"));
    if !shape_ok {
        eprintln!("{USAGE}");
        return 64;
    }
    block_on(async move {
        match run(&args).await {
            Ok(()) => 0,
            Err(message) => {
                if !message.is_empty() {
                    eprintln!("{message}");
                }
                1
            }
        }
    })
}
