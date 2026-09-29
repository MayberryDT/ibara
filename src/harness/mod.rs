//! `ibara harness`: end-to-end runs of contract 4 through any MCP server
//! command, as a careful agent would make them (docs/internals.md, "End-to-end harness").
//!
//! - `probe`: `initialize`, `tools/list`, `computer_status` and
//!   `help:computer_act`, a quick smoke.
//! - `dogfood`: the standard task. Begin with a `file_content` check, launch
//!   the editor expecting its window, type the line, save it through the Save
//!   As dialog at the task workspace's full path in one checked sequence,
//!   publish it, read it back with `computer_files read`, and finish.
//!   Choices come from the returned frames; no element id is written in.
//! - `approval`: a `computer_files send` is held for approval; `--approve-cmd`
//!   (with `{att}` replaced by the attention reference) answers it, and the
//!   repeated request then runs once.
//!
//! The server command follows `--`, for example `ibara mcp --directory-db …
//! --computer …`, or `tailscale ssh …` into `ibara agent-entry` with
//! `SSH_ORIGINAL_COMMAND=mcp`. Every run writes a JSON report (`--report`)
//! with each request's time and bytes, and exits 0 only when it passed.

mod client;

use crate::ids::{now_iso, now_millis};
use client::{Client, Exchange};
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = "Usage: ibara harness <probe|dogfood|approval> [--report FILE] [--client-name NAME] [--computer NAME] [--approve-cmd CMD] -- SERVER_COMMAND…";
const CALL_TIMEOUT: Duration = Duration::from_secs(150);
const TEXT_LIMIT: usize = 2000;

struct Options {
    report: Option<PathBuf>,
    client_name: String,
    computer: Option<String>,
    approve_cmd: Option<String>,
    server: Vec<String>,
}

fn parse(args: Vec<OsString>) -> Result<(String, Options), String> {
    let mut args = args.into_iter().map(|a| a.to_string_lossy().into_owned());
    let mode = args.next().unwrap_or_default();
    if !matches!(mode.as_str(), "probe" | "dogfood" | "approval") {
        return Err(format!("unknown harness mode '{mode}'"));
    }
    let mut opts = Options { report: None, client_name: "harness".into(), computer: None, approve_cmd: None, server: Vec::new() };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--" => {
                opts.server = args.by_ref().collect();
                break;
            }
            "--report" => opts.report = Some(value("--report")?.into()),
            "--client-name" => opts.client_name = value("--client-name")?,
            "--computer" => opts.computer = Some(value("--computer")?),
            "--approve-cmd" => opts.approve_cmd = Some(value("--approve-cmd")?),
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if opts.server.is_empty() {
        return Err("give the MCP server command after --".into());
    }
    if mode == "approval" && opts.approve_cmd.is_none() {
        return Err("approval needs --approve-cmd".into());
    }
    Ok((mode, opts))
}

/// `ibara harness …`.
pub fn main(args: Vec<OsString>) -> i32 {
    let (mode, opts) = match parse(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("ibara harness: {e}\n{USAGE}");
            return 64;
        }
    };
    let client = match Client::spawn(&opts.server) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("ibara harness: cannot start {}: {e}", opts.server[0]);
            return 1;
        }
    };
    let mut run = Run::new(client, &mode, &opts);
    let outcome = match mode.as_str() {
        "probe" => probe(&mut run, &opts),
        "dogfood" => dogfood(&mut run, &opts),
        _ => approval(&mut run, &opts),
    };
    let passed = run.close(outcome);
    let report = run.report(passed);
    let text = serde_json::to_string_pretty(&report).unwrap_or_default();
    match &opts.report {
        Some(path) => {
            if let Err(e) = std::fs::write(path, format!("{text}\n")) {
                eprintln!("ibara harness: cannot write {}: {e}", path.display());
                return 1;
            }
        }
        None => println!("{text}"),
    }
    let totals = &report["totals"];
    eprintln!(
        "ibara harness {mode}: {} · {} requests ({} tool calls) · {} bytes returned · {} ms{}",
        if passed { "passed" } else { "FAILED" },
        totals["requests"],
        totals["tool_calls"],
        totals["response_bytes"],
        totals["ms"],
        report["failure"].as_str().map(|f| format!(" · {f}")).unwrap_or_default(),
    );
    if passed { 0 } else { 1 }
}

/// One harness run: the server, what each request cost, and what was found.
struct Run {
    client: Option<Client>,
    mode: String,
    server: Vec<String>,
    started_at: String,
    started: Instant,
    calls: Vec<Value>,
    transcript: Vec<Value>,
    facts: Map<String, Value>,
    failure: Option<String>,
    server_exit: Option<i32>,
}

impl Run {
    fn new(client: Client, mode: &str, opts: &Options) -> Run {
        Run {
            client: Some(client),
            mode: mode.to_string(),
            server: opts.server.clone(),
            started_at: now_iso(),
            started: Instant::now(),
            calls: Vec::new(),
            transcript: Vec::new(),
            facts: Map::new(),
            failure: None,
            server_exit: None,
        }
    }

    fn fact(&mut self, key: &str, value: impl Into<Value>) {
        self.facts.insert(key.into(), value.into());
    }

    fn exchange(&mut self, method: &str, params: Value) -> Exchange {
        match self.client.as_mut() {
            Some(client) => client.request(method, params, CALL_TIMEOUT),
            None => Exchange { result: Err("the server is closed".into()), ms: 0, request_bytes: 0, response_bytes: 0 },
        }
    }

    fn record(&mut self, tool: &str, x: &Exchange, image_bytes: usize, status: &str, error_code: Option<&str>) {
        self.calls.push(json!({
            "tool": tool,
            "ms": x.ms,
            "request_bytes": x.request_bytes,
            "response_bytes": x.response_bytes,
            "image_bytes": image_bytes,
            "status": status,
            "error_code": error_code,
        }));
    }

    /// A protocol request (`initialize`, `tools/list`), recorded like a call.
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let x = self.exchange(method, params);
        let (status, code) = match &x.result {
            Ok(_) => ("ok", None),
            Err(_) => ("error", Some("TRANSPORT")),
        };
        self.record(method, &x, 0, status, code);
        x.result.map_err(|e| format!("{method}: {e}"))
    }

    /// `initialize`, `notifications/initialized` and `tools/list`.
    fn handshake(&mut self, client_name: &str) -> Result<(), String> {
        let init = self.rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": client_name, "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        self.fact("server", init.get("serverInfo").cloned().unwrap_or(Value::Null));
        if let Some(client) = self.client.as_mut() {
            client.notify("notifications/initialized", json!({}));
        }
        let listed = self.rpc("tools/list", json!({}))?;
        let tools: Vec<Value> = listed
            .get("tools")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter_map(|t| t.get("name").cloned()).collect())
            .unwrap_or_default();
        let bytes = self.calls.last().and_then(|c| c["response_bytes"].as_u64()).unwrap_or(0);
        self.fact("tools", json!({ "count": tools.len(), "names": tools, "tools_list_bytes": bytes }));
        Ok(())
    }

    /// One `tools/call`. Returns the envelope whatever its status; an error
    /// only when no envelope came back.
    fn tool(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let x = self.exchange("tools/call", json!({ "name": name, "arguments": arguments }));
        let (envelope, image_bytes) = match &x.result {
            Ok(result) => envelope_of(result),
            Err(_) => (None, 0),
        };
        let status = envelope.as_ref().and_then(|e| e["status"].as_str()).unwrap_or("error").to_string();
        let code = envelope.as_ref().and_then(|e| e["error"]["code"].as_str()).map(str::to_string);
        let code = code.or_else(|| envelope.is_none().then(|| "TRANSPORT".to_string()));
        self.record(name, &x, image_bytes, &status, code.as_deref());
        self.transcript.push(json!({
            "tool": name,
            "arguments": arguments,
            "envelope": envelope.clone().unwrap_or_else(|| json!({ "transport_error": x.result.as_ref().err() })),
        }));
        let envelope = envelope.ok_or_else(|| match x.result {
            Err(e) => format!("{name}: {e}"),
            Ok(_) => format!("{name}: the reply carried no envelope"),
        })?;
        for line in envelope["since"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if let Some(pid) = line.strip_prefix("launched pid ").and_then(|p| p.trim().parse::<u64>().ok()) {
                let pids = self.facts.entry("launched_pids").or_insert_with(|| json!([]));
                if let Some(list) = pids.as_array_mut() {
                    list.push(json!(pid));
                }
            }
        }
        Ok(envelope)
    }

    fn close(&mut self, outcome: Result<(), String>) -> bool {
        if let Some(client) = self.client.take() {
            self.server_exit = client.close(Duration::from_secs(5));
        }
        match outcome {
            Ok(()) => true,
            Err(e) => {
                self.failure = Some(e);
                false
            }
        }
    }

    fn report(&self, passed: bool) -> Value {
        let sum = |key: &str| self.calls.iter().map(|c| c[key].as_u64().unwrap_or(0)).sum::<u64>();
        let tool_calls: Vec<&Value> = self.calls.iter().filter(|c| c["tool"].as_str().is_some_and(|t| t.starts_with("computer_") || t.starts_with("browser_"))).collect();
        json!({
            "harness": self.mode,
            "passed": passed,
            "failure": self.failure,
            "started_at": self.started_at,
            "server_command": self.server,
            "server_exit": self.server_exit,
            "totals": {
                "requests": self.calls.len(),
                "tool_calls": tool_calls.len(),
                "ms": sum("ms"),
                "wall_ms": self.started.elapsed().as_millis() as u64,
                "request_bytes": sum("request_bytes"),
                "response_bytes": sum("response_bytes"),
                "tool_response_bytes": tool_calls.iter().map(|c| c["response_bytes"].as_u64().unwrap_or(0)).sum::<u64>(),
                "image_bytes": sum("image_bytes"),
                "errors": self.calls.iter().filter(|c| c["status"] == "error").count(),
            },
            "calls": self.calls,
            "outcome": self.facts,
            "transcript": self.transcript,
        })
    }
}

/// The envelope of a `CallToolResult` (its `structuredContent`, or with
/// images the final text part) and the decoded size of its images.
fn envelope_of(result: &Value) -> (Option<Value>, usize) {
    let content = result.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
    let image_bytes = content
        .iter()
        .filter(|c| c["type"] == "image")
        .filter_map(|c| c["data"].as_str())
        .map(|d| d.len() / 4 * 3 - d.bytes().rev().take_while(|b| *b == b'=').count())
        .sum();
    let envelope = result.get("structuredContent").cloned().or_else(|| {
        content
            .iter()
            .rev()
            .filter_map(|c| c["text"].as_str())
            .find_map(|t| serde_json::from_str::<Value>(t).ok().filter(|v| v.get("situation").is_some()))
    });
    (envelope, image_bytes)
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// The result of an `ok` envelope, else the error as the agent would read it.
fn ok<'a>(envelope: &'a Value, what: &str) -> Result<&'a Value, String> {
    if envelope["status"] == "ok" {
        return Ok(&envelope["result"]);
    }
    let error = &envelope["error"];
    Err(format!(
        "{what}: {} {} {}",
        envelope["status"].as_str().unwrap_or("?"),
        error["code"].as_str().unwrap_or(""),
        clip(error["message"].as_str().unwrap_or(""), 300)
    ))
}

/// The frame of an act whose steps were all `done`; otherwise what each step reported.
fn act_done(envelope: &Value, what: &str) -> Result<Value, String> {
    let result = ok(envelope, what)?;
    let steps = result["steps"].as_array().cloned().unwrap_or_default();
    if steps.is_empty() || steps.iter().any(|s| s["outcome"] != "done") {
        let told: Vec<String> = steps
            .iter()
            .map(|s| format!("step {} {}: {}", s["index"], s["outcome"].as_str().unwrap_or("?"), s["effect"].as_str().unwrap_or("")))
            .collect();
        return Err(format!("{what}: {}", clip(&told.join("; "), 600)));
    }
    Ok(result["frame"].clone())
}

/// The id of the first (best-ranked) choice whose action and choice match.
fn find_choice(frame: &Value, wanted: impl Fn(&Value, &Value) -> bool) -> Option<String> {
    frame["choices"]
        .as_array()?
        .iter()
        .find(|c| wanted(&c["action"], c))
        .and_then(|c| c["choice_id"].as_str())
        .map(str::to_string)
}

/// A frame's window line (`w2 mousepad "Save As" floating`) for a floating Save As.
fn floating_save_as(line: &str) -> bool {
    let window = line.split_whitespace().next().and_then(|id| id.strip_prefix('w')).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    window && line.contains(" \"Save As\"") && line.ends_with(" floating")
}

fn str_at(value: &Value, key: &str) -> Result<String, String> {
    value[key].as_str().map(str::to_string).ok_or_else(|| format!("the reply has no {key}"))
}

fn probe(run: &mut Run, opts: &Options) -> Result<(), String> {
    run.handshake(&opts.client_name)?;
    let status = run.tool("computer_status", json!({}))?;
    let fleet = ok(&status, "computer_status")?.clone();
    run.fact("fleet", fleet);
    run.fact("situation", status["situation"].clone());
    let help = run.tool("computer_status", json!({ "ref": "help:computer_act" }))?;
    let help = ok(&help, "help:computer_act")?;
    let text = help["help"].as_str().unwrap_or("");
    run.fact("help_computer_act_chars", text.chars().count());
    if text.is_empty() {
        return Err("help:computer_act returned no text".into());
    }
    Ok(())
}

fn dogfood(run: &mut Run, opts: &Options) -> Result<(), String> {
    run.handshake(&opts.client_name)?;
    let line = format!("dogfood {}", &now_iso()[..10]);
    let file = "dogfood.txt";
    let stamp = now_millis();
    let rid = move |step: &str| format!("dogfood-{stamp}-{step}");
    run.fact("line", line.as_str());
    let mut begin = json!({
        "goal": format!("Open the editor, write the line \"{line}\", save it as {file} in the task workspace, and publish it."),
        "checks": [{
            "id": "saved",
            "description": format!("{file} in the task workspace holds the line"),
            "check": { "kind": "file_content", "path": file, "contains": line },
        }],
        "request_id": rid("begin"),
    });
    if let Some(computer) = &opts.computer {
        begin["computer"] = json!(computer);
    }
    let begun = run.tool("computer_begin", begin)?;
    let result = ok(&begun, "computer_begin")?.clone();
    let task_ref = str_at(&result, "task_ref")?;
    run.fact("task_ref", task_ref.as_str());
    run.fact("you", result["you"].clone());
    let steps = match result["workspace"].as_str() {
        Some(workspace) => {
            run.fact("workspace", workspace);
            dogfood_steps(run, &task_ref, workspace, &line, file, &rid, &result["frame"])
        }
        None => Err("computer_begin did not name the task workspace, so the Save As path cannot be typed".into()),
    };
    // Always finish, so control is released and the editor window is closed.
    let outcome = if steps.is_ok() { "complete" } else { "blocked" };
    let summary = match &steps {
        Ok(()) => format!("Saved and published {file} with the line \"{line}\"."),
        Err(e) => format!("Stopped: {}", clip(e, 300)),
    };
    let finished = run.tool("computer_finish", json!({ "task_ref": task_ref, "request_id": rid("finish"), "outcome": outcome, "summary": summary }))?;
    let finish = ok(&finished, "computer_finish");
    if let Ok(result) = finish {
        run.fact("finish", result.clone());
    }
    steps?;
    let result = finish?;
    let checks_met = result["checks"].as_array().is_some_and(|c| !c.is_empty() && c.iter().all(|c| c["state"] == "met"));
    run.fact("complete", result["complete"].clone());
    run.fact("checks_met", checks_met);
    match (result["complete"].as_bool(), checks_met) {
        (Some(true), true) => Ok(()),
        _ => Err(format!("finish: complete {} with checks {}", result["complete"], result["checks"])),
    }
}

fn dogfood_steps(run: &mut Run, task_ref: &str, workspace: &str, line: &str, file: &str, rid: &dyn Fn(&str) -> String, frame: &Value) -> Result<(), String> {
    // A Save As left open by an earlier run is not this run's dialog: the
    // measurement needs a desktop without one.
    if let Some(left) = frame["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|l| floating_save_as(l)) {
        return Err(format!("begin: {left} is already open, probably from an earlier run; close it and its editor, then run again"));
    }
    // Launch the editor: the frame's launch choice when it offers one.
    let mut launch = json!({ "task_ref": task_ref, "request_id": rid("launch"), "expect": { "kind": "window", "app": "mousepad" } });
    match find_choice(frame, |a, _| a["kind"] == "launch" && a["app"] == "editor") {
        Some(choice) => {
            run.fact("launch_route", format!("choice {choice}"));
            launch["choice"] = json!(choice);
        }
        None => {
            run.fact("launch_route", "action (no launch choice offered)");
            launch["action"] = json!({ "kind": "launch", "app": "editor" });
        }
    }
    let launched = run.tool("computer_act", launch)?;
    let frame = act_done(&launched, "launch")?;
    // A careful agent does not type into a prompt it did not expect.
    if let Some(dialog) = frame["lines"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|l| l.starts_with("dialog ")) {
        return Err(format!("launch: {dialog} is in front of the editor; not typing into it"));
    }

    // Type the line through the frame's typing choice, checking it landed.
    let typing = find_choice(&frame, |a, c| a["kind"] == "type" && c["param"] == "text").ok_or("the frame after the launch offers no typing choice")?;
    run.fact("type_route", format!("choice {typing}"));
    let typed = run.tool(
        "computer_act",
        json!({ "task_ref": task_ref, "request_id": rid("type"), "choice": typing, "text": line, "expect": { "kind": "text", "text": line } }),
    )?;
    let frame = act_done(&typed, "type")?;

    // Save As at the full workspace path, in one checked sequence.
    let path = format!("{}/{file}", workspace.trim_end_matches('/'));
    let mut first = json!({ "expect": { "kind": "dialog", "title": "Save As" } });
    match find_choice(&frame, |a, _| a["kind"] == "key" && a["keys"].as_str().is_some_and(|k| k.eq_ignore_ascii_case("ctrl+s"))) {
        Some(choice) => {
            run.fact("save_route", format!("choice {choice}"));
            first["choice"] = json!(choice);
        }
        None => {
            run.fact("save_route", "action key ctrl+s");
            first["action"] = json!({ "kind": "key", "keys": "ctrl+s" });
        }
    }
    let steps = json!([
        first,
        { "action": { "kind": "type", "text": path } },
        { "action": { "kind": "key", "keys": "Return" }, "expect": { "kind": "dialog", "title": "Save As", "gone": true } },
        { "expect": { "kind": "file", "path": file, "contains": line } },
    ]);
    let saved = run.tool("computer_act", json!({ "task_ref": task_ref, "request_id": rid("save"), "steps": steps }))?;
    act_done(&saved, "save as")?;

    let published = run.tool("computer_files", json!({ "task_ref": task_ref, "request_id": rid("publish"), "op": "publish", "path": file }))?;
    let artifact = ok(&published, "publish")?.clone();
    run.fact("artifact", artifact);

    // Read the bytes back independently of the typed check.
    let read = run.tool("computer_files", json!({ "task_ref": task_ref, "request_id": rid("read"), "op": "read", "path": file }))?;
    let text = ok(&read, "read")?["text"].as_str().unwrap_or("").to_string();
    let matches = text.trim_end_matches(['\n', '\r']) == line;
    run.fact("verified_file", json!({ "path": path, "text": clip(&text, TEXT_LIMIT), "matches": matches }));
    if !matches {
        return Err(format!("read: {file} holds {:?}, not {line:?}", clip(&text, 200)));
    }
    Ok(())
}

fn approval(run: &mut Run, opts: &Options) -> Result<(), String> {
    run.handshake(&opts.client_name)?;
    let stamp = now_millis();
    let rid = move |step: &str| format!("approval-{stamp}-{step}");
    let mut begin = json!({ "goal": "Write approval.txt in the task workspace and send it to the computer the agent works from.", "request_id": rid("begin") });
    if let Some(computer) = &opts.computer {
        begin["computer"] = json!(computer);
    }
    let begun = run.tool("computer_begin", begin)?;
    let result = ok(&begun, "computer_begin")?;
    let task_ref = str_at(result, "task_ref")?;
    // A send reaches only the computer the agent works from: its principal's.
    let home = str_at(&result["you"], "principal")?;
    run.fact("task_ref", task_ref.as_str());
    let steps = approval_steps(run, &task_ref, &home, opts.approve_cmd.as_deref().unwrap_or(""), &rid);
    let finished = run.tool(
        "computer_finish",
        json!({ "task_ref": task_ref, "request_id": rid("finish"), "outcome": "partial", "summary": "Approval harness: the send ran after approval; no collector will verify it." }),
    )?;
    if let Ok(result) = ok(&finished, "computer_finish") {
        run.fact("finish", result.clone());
    }
    steps
}

fn approval_steps(run: &mut Run, task_ref: &str, home: &str, approve_cmd: &str, rid: &dyn Fn(&str) -> String) -> Result<(), String> {
    let written = run.tool(
        "computer_files",
        json!({ "task_ref": task_ref, "request_id": rid("write"), "op": "write", "path": "approval.txt", "text": format!("approval {}\n", now_iso()) }),
    )?;
    ok(&written, "write")?;
    let send = json!({
        "task_ref": task_ref,
        "request_id": rid("send"),
        "op": "send",
        "path": "approval.txt",
        "to": { "host": home, "path": "/tmp/ibara-harness/approval.txt" },
    });
    let held = run.tool("computer_files", send.clone())?;
    if held["status"] != "pending" {
        return Err(format!("send: expected pending, got {} {}", held["status"], held["error"]));
    }
    let att = str_at(&held["result"], "attention")?;
    run.fact("held", held["result"].clone());

    // The same request again while nobody has answered: still held, nothing runs.
    let still = run.tool("computer_files", send.clone())?;
    run.fact("before_answer_status", still["status"].clone());
    if still["status"] != "pending" {
        return Err(format!("send before the answer: expected pending, got {}", still["status"]));
    }

    let command = approve_cmd.replace("{att}", &att);
    let answered = std::process::Command::new("sh").arg("-c").arg(&command).output().map_err(|e| format!("approve command: {e}"))?;
    run.fact(
        "approve_command",
        json!({
            "command": command,
            "exit": answered.status.code(),
            "stdout": clip(&String::from_utf8_lossy(&answered.stdout), TEXT_LIMIT),
            "stderr": clip(&String::from_utf8_lossy(&answered.stderr), TEXT_LIMIT),
        }),
    );
    if !answered.status.success() {
        return Err(format!("approve command exited {:?}", answered.status.code()));
    }
    let waited = run.tool("computer_wait", json!({ "task_ref": task_ref, "for": { "attention": att }, "deadline_ms": 20000 }))?;
    let waited = ok(&waited, "wait for the answer")?.clone();
    run.fact("answer", waited.clone());
    if waited["met"] != true {
        return Err(format!("wait: the attention item was not answered: {waited}"));
    }

    let ran = run.tool("computer_files", send.clone())?;
    let ran = ok(&ran, "send after approval")?.clone();
    run.fact("sent", ran.clone());
    let replayed = run.tool("computer_files", send)?;
    let replayed = ok(&replayed, "send replayed")?.clone();
    let same = replayed == ran;
    run.fact("replay_identical", same);
    if !same {
        return Err(format!("the replayed send differs: {replayed} vs {ran}"));
    }
    let status = run.tool("computer_files", json!({ "task_ref": task_ref, "request_id": rid("status"), "op": "status" }))?;
    let artifacts = ok(&status, "files status")?["artifacts"].as_array().cloned().unwrap_or_default();
    let sent = artifacts.iter().filter(|a| a["path"].as_str().is_some_and(|p| p.ends_with("approval.txt"))).count();
    run.fact("artifacts_for_the_file", sent);
    if sent != 1 {
        return Err(format!("expected exactly one artifact for approval.txt, found {sent}"));
    }
    Ok(())
}
