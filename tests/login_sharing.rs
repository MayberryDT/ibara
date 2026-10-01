//! Login sharing end to end: real daemons, paired operator routes, and
//! stand-ins for the two browsers (the person's on the sharing computer and
//! the agent computer's), which speak Chrome's native-messaging frames.
//! Failure cases (see also `.research/login-sharing/failures.md`):
//! - The operator role has no browser socket, or exposes it to another uid.
//! - A disconnected browser is reported as connected.
//! - Browser status returns a tab address, cookies, or an unfiltered peer reply.
//! - A second operator takes the first one's live bridge socket.
//! - Cookie export is exposed through the public console command surface.
//! - A browser is given a job it should not have: the sharing browser writes,
//!   the agent computer's browser is read.
//! - A computer that also runs agents becomes the sharing computer.
//! - A begin with logins waits, raises one item per site, hides a Denied site,
//!   or delivers an Allowed site late or never.
//! - Someone other than the sharing computer answers a login request, or a
//!   generic approval answer settles it; another person's agent skips asking.
//! - A `sign_in` misreports any reason code, asks twice, repeats a declined or
//!   rejected site in the same task, or reports a page it could not read as signed in.
//! - Rules diverge from deny > ask > allow; Remove leaves an All Computers
//!   Allow in force; an offline removal is dropped.
//! - A slow reload reads the old password form as rejection; Share after
//!   Remove ignores the answer; declining while unpinned fails; self-pinning
//!   cuts off the source; partial spread/removal is reported as success.
//! - Site memory is not kept, or a later success does not clear it.
//! - Share With and Sync skip reachable computers or fail on offline ones;
//!   turning sharing off leaves computers sharing.
//! - A bundle shaped like a real site's (same-name cookies across the domain
//!   and host, a subdomain host, a partitioned cookie, a non-secure one) or a
//!   large one (103 cookies) fails, or a partial write fences page reading.
//! - A cookie value appears in any reply, transcript, journal (with its WAL
//!   and SHM), timeline, attention item, settings file or log.
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
mod support;

fn write_frame(stream: &mut UnixStream, value: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value).unwrap();
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)
}

fn read_frame(stream: &mut UnixStream) -> Option<Value> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).ok()?;
    let mut bytes = vec![0; u32::from_le_bytes(header) as usize];
    stream.read_exact(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[test]
fn operator_browser_bridge_has_a_private_status_only_surface() {
    let root = std::env::temp_dir().join(format!("ibara-login-e2e-{}", std::process::id()));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root.join("run"))
        .unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_ibarad"))
        .args(["--role", "operator"])
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("IBARA_OPERATOR_DIRECTORY_DB", root.join("directory.sqlite"))
        .env_remove("IBARA_LOGIN_TESTS")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = root.join("run/ibara/ibarad.sock");
    let until = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    let call = |command: &str| -> Value {
        let mut socket = UnixStream::connect(&socket).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        writeln!(
            socket,
            "{}",
            json!({"id": "login_e2e", "command": command, "args": []})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let result = std::panic::catch_unwind(|| {
        let status = call("login-browser-status");
        assert_eq!(
            status["envelope"]["data"]["connected"], false,
            "disconnected status must be explicit"
        );
        let chrome = root.join("run/ibara/chrome.sock");
        assert_eq!(
            fs::metadata(&chrome).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut extension = UnixStream::connect(&chrome).unwrap();
        extension
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write_frame(&mut extension, &json!({"hello": 1})).unwrap();
        // The sharing computer's browser is given only the share job.
        let jobs = read_frame(&mut extension).expect("the jobs request");
        assert_eq!(
            (jobs["op"].as_str(), &jobs["args"]["jobs"]),
            (Some("jobs"), &json!(["share"])),
            "{jobs}"
        );
        write_frame(
            &mut extension,
            &json!({"id": jobs["id"], "result": {"jobs": ["share"]}}),
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while call("login-browser-status")["envelope"]["data"]["connected"] != true
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let status = call("login-browser-status");
        assert_eq!(status["envelope"]["data"]["connected"], true);
        assert_eq!(
            status["envelope"]["data"].as_object().unwrap().len(),
            1,
            "status reports connection only"
        );
        let refused = call("cookies_read");
        assert!(
            refused["envelope"]["error"].is_object(),
            "no raw export command"
        );
        let mut other = UnixStream::connect(&chrome).unwrap();
        other
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(
            other.read(&mut [0u8; 1]).unwrap(),
            0,
            "second extension refused"
        );
    });
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = fs::remove_dir_all(&root);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

// ---- browser stand-ins --------------------------------------------------------------

/// What a stand-in browser holds and has been asked. Cookie values live only
/// here, in memory; `ops` records metadata (`write tax.test 3`).
#[derive(Default)]
struct Jar {
    cookies: BTreeMap<String, Vec<Value>>,
    /// The focused tab and the page it came from.
    url: String,
    previous: Option<String>,
    /// What `login_page` answers after a reload.
    page: String,
    /// Sites whose writes report these per-cookie failures.
    fail: BTreeMap<String, Value>,
    old_extension: bool,
    reload_delay: Duration,
    commit_at: Option<Instant>,
    remove_fail: bool,
    ops: Vec<String>,
    jobs: Vec<Value>,
}

type Shared = Arc<Mutex<Jar>>;

/// One connected stand-in; dropping it closes the connection (the browser quits).
struct Browser {
    stream: UnixStream,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn site_of(domain: &str) -> String {
    let parts: Vec<&str> = domain.trim_start_matches('.').split('.').collect();
    parts[parts.len().saturating_sub(2)..].join(".")
}

fn browser(path: &Path, jar: Shared) -> Browser {
    let until = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match UnixStream::connect(path) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("{}: {e}", path.display()),
        }
    };
    write_frame(&mut stream, &json!({"hello": 1})).unwrap();
    let kept = stream.try_clone().unwrap();
    std::thread::spawn(move || {
        while let Some(m) = read_frame(&mut stream) {
            let site = m["args"]["site"].as_str().unwrap_or("").to_string();
            let mut jar = jar.lock();
            let result = match m["op"].as_str().unwrap_or("") {
                "jobs" => {
                    jar.jobs.push(m["args"]["jobs"].clone());
                    json!({"jobs": m["args"]["jobs"]})
                }
                "browser_info" => json!({"extensionId": "stand-in"}),
                "cookies_read" => {
                    jar.ops.push(format!("read {site}"));
                    json!({"cookies": jar.cookies.get(&site).cloned().unwrap_or_default()})
                }
                "cookies_count" => json!({"count": jar.cookies.get(&site).map_or(0, Vec::len)}),
                "cookies_write" => {
                    let cookies = m["args"]["cookies"].as_array().cloned().unwrap_or_default();
                    // Do not format the request: it holds values.
                    assert!(
                        cookies
                            .iter()
                            .all(|c| site_of(c["domain"].as_str().unwrap_or("")) == site),
                        "every cookie belongs to the site"
                    );
                    let failed: Vec<Value> = jar
                        .fail
                        .get(&site)
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    jar.ops.push(format!("write {site} {}", cookies.len()));
                    jar.cookies
                        .entry(site.clone())
                        .or_default()
                        .extend(cookies.iter().cloned());
                    json!({"written": cookies.len() - failed.len(), "failed": failed})
                }
                "cookies_remove" => {
                    jar.ops.push(format!("remove {site}"));
                    if jar.remove_fail {
                        json!({"removed": 1, "failed": [{"index": 1, "field": "remove"}]})
                    } else {
                        let removed = jar.cookies.remove(&site).map_or(0, |c| c.len());
                        json!({"removed": removed, "failed": []})
                    }
                }
                "tabs" => {
                    json!({"tabs": [{"id": 7, "title": "Sign in", "url": jar.url, "focused": true}]})
                }
                "login_context" => json!({"url": jar.url, "previous": jar.previous}),
                "login_reload" => {
                    jar.ops.push("reload".into());
                    jar.commit_at = Some(Instant::now() + jar.reload_delay);
                    json!({"reloaded": true, "after_ms": 1})
                }
                "login_page" => {
                    let old = jar.commit_at.is_some_and(|at| Instant::now() < at);
                    let mut result = json!({"page": if old {
                        if m["args"]["after_ms"].is_number() { "unknown" } else { "still_sign_in" }
                    } else if jar.page.is_empty() { "left_sign_in" } else { jar.page.as_str() }});
                    if !jar.old_extension {
                        let requested = m["args"]["after_ms"].as_i64().unwrap_or(0);
                        result["document_ms"] = json!(if old { requested - 1 } else { requested });
                    }
                    result
                }
                op => {
                    jar.ops.push(format!("refused {op}"));
                    drop(jar);
                    if write_frame(
                        &mut stream,
                        &json!({"id": m["id"], "error": {"execution_not_started": true}}),
                    )
                    .is_err()
                    {
                        break;
                    }
                    continue;
                }
            };
            drop(jar);
            if write_frame(&mut stream, &json!({"id": m["id"], "result": result})).is_err() {
                break;
            }
        }
    });
    Browser { stream: kept }
}

fn ops(jar: &Shared) -> Vec<String> {
    jar.lock().ops.clone()
}

/// A cookie of `host` (a leading dot: the whole site) with `value`.
fn cookie(host: &str, name: &str, value: &str, secure: bool) -> Value {
    json!({"domain": host, "hostOnly": !host.starts_with('.'), "httpOnly": true, "name": name, "value": value, "path": "/",
           "sameSite": if secure { "lax" } else { "unspecified" }, "secure": secure, "session": true})
}

/// A bundle shaped like x.com's on 30 September: 17 cookies, 12 on the whole
/// site, host-only ones (one not secure), one name on both the site and its
/// host, a session one, a deep subdomain host and a partitioned one.
fn x_like(site: &str, value: &str) -> Vec<Value> {
    let whole = format!(".{site}");
    let mut cookies: Vec<Value> = (0..11)
        .map(|i| cookie(&whole, &format!("x_{i}"), value, true))
        .collect();
    cookies.push(cookie(&whole, "__cuid", value, true));
    cookies.push(cookie(site, "__cuid", value, true));
    cookies.push(cookie(site, "g_state", value, false));
    let mut lang = cookie(site, "lang", value, true);
    lang["session"] = json!(true);
    cookies.push(lang);
    cookies.push(cookie(
        &format!("sdn.money.{site}"),
        "_immortal|deviceToken",
        value,
        true,
    ));
    let mut clearance = cookie(site, "cf_clearance", value, true);
    clearance["sameSite"] = json!("no_restriction");
    clearance["partitionKey"] =
        json!({"topLevelSite": format!("https://{site}"), "hasCrossSiteAncestor": false});
    cookies.push(clearance);
    cookies
}

// ---- an agent's MCP session -----------------------------------------------------------

/// `ibara agent-entry P` with `mcp`, as the target's sshd runs it for an agent
/// from computer `P`. Every line it wrote is kept for the value search.
struct Agent {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    replies: mpsc::Receiver<Value>,
    transcript: Arc<Mutex<String>>,
    since: Vec<String>,
    next_id: u64,
}

impl Agent {
    fn start(target: &support::Target, principal: &str, client: &str) -> Agent {
        let root = &target.root;
        let mut child = Command::new(support::IBARA)
            .args(["agent-entry", principal])
            .arg(root.join("run/controller.sock"))
            .arg(root.join("gateway.key"))
            .arg(root.join(format!("state/operator-keys/{principal}.key")))
            .env("SSH_ORIGINAL_COMMAND", "mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, replies) = mpsc::channel();
        let transcript = Arc::new(Mutex::new(String::new()));
        let (to_computer, kept) = (stdin.clone(), transcript.clone());
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { break };
                kept.lock().push_str(&line);
                let message: Value = serde_json::from_str(&line).unwrap();
                if message["method"] == "ping" {
                    let _ = writeln!(
                        to_computer.lock(),
                        "{}",
                        json!({"jsonrpc": "2.0", "id": message["id"], "result": {}})
                    );
                } else if tx.send(message).is_err() {
                    break;
                }
            }
        });
        let mut agent = Agent {
            child,
            stdin,
            replies,
            transcript,
            since: Vec::new(),
            next_id: 1,
        };
        agent.request(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                              "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": client, "version": "1"}}}));
        writeln!(
            agent.stdin.lock(),
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        agent
    }

    fn request(&mut self, request: Value) -> Value {
        writeln!(self.stdin.lock(), "{request}").unwrap();
        let reply = self
            .replies
            .recv_timeout(Duration::from_secs(90))
            .expect("agent-entry answered");
        assert_eq!(reply["id"], request["id"], "{reply}");
        reply
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        self.next_id += 1;
        let envelope = self.request(json!({"jsonrpc": "2.0", "id": self.next_id, "method": "tools/call", "params": {"name": tool, "arguments": args}}))
            ["result"]["structuredContent"]
            .clone();
        self.since.extend(
            envelope["since"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string),
        );
        envelope
    }

    fn begin(&mut self, request_id: &str, logins: &[&str]) -> Value {
        let began = self.call("computer_begin", json!({"goal": "File the quarterly report", "request_id": request_id, "logins": logins}));
        assert_eq!(began["status"], "ok", "{began}");
        began
    }

    fn sign_in(&mut self, task: &str, request_id: &str, sites: &[&str]) -> Value {
        let mut action = json!({"kind": "sign_in"});
        if !sites.is_empty() {
            action["sites"] = json!(sites);
        }
        self.call(
            "browser_act",
            json!({"task_ref": task, "request_id": request_id, "action": action}),
        )
    }

    fn finish(&mut self, task: &str, request_id: &str) {
        let done = self.call("computer_finish", json!({"task_ref": task, "request_id": request_id, "outcome": "partial", "summary": "Stopped."}));
        assert_eq!(done["status"], "ok", "{done}");
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn states(reply: &Value) -> BTreeMap<String, String> {
    let rows = reply["result"]["sign_in"]["sites"]
        .as_array()
        .or(reply["result"]["logins"].as_array());
    rows.into_iter()
        .flatten()
        .map(|r| {
            (
                r["site"].as_str().unwrap().to_string(),
                r["state"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn wait_for<T>(what: &str, seconds: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Send the same `sign_in` again until it settles (a person answered, a
/// browser opened, the sharing computer delivered).
fn settled(agent: &mut Agent, task: &str, request_id: &str, sites: &[&str]) -> Value {
    wait_for("the sign_in to settle", 60, || {
        let reply = agent.sign_in(task, request_id, sites);
        (reply["status"] == "ok").then_some(reply)
    })
}

/// A desktop session with one unlocked screen, as other tests give one.
const DESKTOP_HYPRCTL: &str = r#"#!/bin/sh
case "$*" in
  *monitors*) echo '[{"id":0,"name":"IbaraVirtual","width":1920,"height":1080,"x":0,"y":0,"scale":1.0,"focused":true,"solitaryBlockedBy":[],"activeWorkspace":{"id":1,"name":"1"}}]' ;;
  *) echo '[]' ;;
esac
"#;
const DESKTOP_IDLE: &str = r#"#!/bin/sh
flag="$(dirname "$0")/stay-awake"
case "$1" in
  status) if [ -e "$flag" ]; then echo '{"enabled":true}'; else echo '{"enabled":false}'; fi ;;
  stay-awake) touch "$flag" ;;
  *) rm -f "$flag" ;;
esac
"#;

/// Pair `console` to `name` as the same person's computer; its computer id.
fn add_own(console: &mut support::Console, name: &str) -> String {
    let started = console.ok("pair-start", &[name]);
    let paired = console.settled(started["request_id"].as_str().unwrap());
    assert_eq!(paired["state"], "paired", "{paired}");
    paired["computer_id"].as_str().unwrap().to_string()
}

fn hits(haystack: &[u8], needle: &str) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle.as_bytes())
        .count()
}

/// The whole product on real daemons: turning sharing on, begin with logins,
/// every `sign_in` reason code, answers, rules, memory, Share With, Sync,
/// Remove, turning off, and a search for every cookie value afterwards.
#[test]
fn logins_are_shared_as_the_person_decides_and_never_seen() {
    use support::*;
    let world = World::new("login-journeys");
    // Tulip1, the agent computer: a desktop session and a page reader.
    let desk = world.root.join("desk-tulip1");
    fs::create_dir_all(&desk).unwrap();
    write_executable(&desk.join("hyprctl"), DESKTOP_HYPRCTL);
    write_executable(&desk.join("omarchy-toggle-idle"), DESKTOP_IDLE);
    let (hyprctl, idle) = (desk.join("hyprctl"), desk.join("omarchy-toggle-idle"));
    let env: [(&str, &std::ffi::OsStr); 3] = [
        ("IBARA_TEST_HYPRCTL", hyprctl.as_os_str()),
        ("IBARA_TEST_IDLE", idle.as_os_str()),
        ("WAYLAND_DISPLAY", std::ffi::OsStr::new("wayland-e2e")),
    ];
    fs::create_dir_all(world.root.join("target-tulip1/xdg-run")).unwrap();
    fs::set_permissions(
        world.root.join("target-tulip1/xdg-run"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let tulip1 = Target::start_with(&world, node("tulip1"), Some("Tulip1"), 300_000, &env);
    let hazel_target = Target::start(&world, node("hazel"), None, 300_000);

    // Vesper, the person's computer, pairs while its own ibara vouches for it.
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let install = world.root.join("source-install");
    fs::create_dir_all(install.join("browser")).unwrap();
    let no_power = world.root.join("no-power.sock");
    let console_env: [(&str, &std::ffi::OsStr); 2] = [
        ("IBARA_INSTALL_ROOT", install.as_os_str()),
        ("IBARA_POWER_SOCKET", no_power.as_os_str()),
    ];
    let mut vesper = Console::start_with(
        &world,
        "vesper",
        node("vesper"),
        Some(&vesper_target),
        &console_env,
    );
    let tulip1_id = add_own(&mut vesper, "tulip1");
    let hazel_id = add_own(&mut vesper, "hazel");
    let mut hazel = Console::start(&world, "hazel", node("hazel"), Some(&hazel_target));
    let hazel_tulip1 = add_own(&mut hazel, "tulip1");
    let mut replies: Vec<String> = Vec::new();

    // The person's browser, one profile, set up for ibara.
    let profile = vesper.home.join(".config/BraveSoftware/Brave-Origin");
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        profile.join("Local State"),
        r#"{"profile":{"info_cache":{"Default":{}}}}"#,
    )
    .unwrap();
    fs::write(
        install.join("browser/selection.json"),
        json!({"browser": "brave-origin", "profile": "Default", "root": profile}).to_string(),
    )
    .unwrap();
    let values: Vec<String> = (0..4).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let person = Shared::default();
    {
        let mut jar = person.lock();
        for (site, n) in [
            ("tax.test", 3),
            ("idp.test", 2),
            ("mail.test", 1),
            ("news.test", 2),
            ("strict.test", 2),
            ("partial.test", 5),
            ("shop.test", 2),
        ] {
            jar.cookies.insert(
                site.into(),
                (0..n)
                    .map(|i| cookie(&format!(".{site}"), &format!("s{i}"), &values[0], true))
                    .collect(),
            );
        }
        jar.cookies
            .insert("xsite.test".into(), x_like("xsite.test", &values[1]));
        jar.cookies.insert(
            "big.test".into(),
            (0..103)
                .map(|i| cookie(".big.test", &format!("b{i}"), &values[2], true))
                .collect(),
        );
    }
    let vesper_chrome = world.root.join("console-vesper/run/ibara/chrome.sock");
    let mut person_browser = Some(browser(&vesper_chrome, person.clone()));
    let agents_computer = Shared::default();
    {
        let mut tab = agents_computer.lock();
        tab.url = "https://www.tax.test/login".into();
        // This browser writes one of partial.test's cookies only in part.
        tab.fail
            .insert("partial.test".into(), json!([{"index": 2, "field": "set"}]));
    }
    let tulip1_chrome = tulip1.root.join("run/chrome.sock");
    let tulip1_browser = browser(&tulip1_chrome, agents_computer.clone());

    // Don't Share works before a source has ever been pinned, without a rule.
    let mut off_agent = Agent::start(&tulip1, "vesper", "codex");
    wait_for("Tulip1 ready before declining", 40, || {
        off_agent.call("computer_status", json!({}))["situation"]
            .as_str()
            .is_some_and(|s| s.contains("nobody controls"))
            .then_some(())
    });
    let off_begin = off_agent.begin("off-begin", &["tax.test"]);
    let off_att = off_begin["result"]["logins"][0]["attention"]
        .as_str()
        .unwrap();
    let declined = vesper.ok(
        "login-answer",
        &[
            "--computer",
            &tulip1_id,
            "--att",
            off_att,
            "--decisions",
            r#"{"tax.test":"decline"}"#,
        ],
    );
    assert_eq!(declined["sites"][0]["outcome"], "declined", "{declined}");
    assert!(
        vesper.ok("login-rows", &["--computer", &tulip1_id])["rows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    off_agent.finish(
        off_begin["result"]["task_ref"].as_str().unwrap(),
        "off-finish",
    );

    // Even a locally paired console with administer cannot pin to itself.
    let self_id = add_own(&mut hazel, "hazel");
    let self_epoch = hazel.ok("operator-session", &["--computer", &self_id])["controller_epoch"]
        .as_str()
        .unwrap()
        .to_string();
    let mut command = Command::new(IBARA);
    world.machine_env(&mut command, node("hazel"));
    let mut child = command
        .env("HOME", &hazel.home)
        .args([
            "operator",
            "--computer",
            &self_id,
            "--epoch",
            &self_epoch,
            "--op",
            "login_configure",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"enabled":true,"replace":true,"label":"Self","sites":{}}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("also runs agents"),
        "self-pin must be refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A computer that also runs agents cannot share this release.
    let refused = vesper.ask(
        "login-on",
        &[
            "--browser",
            "brave-origin",
            "--profile",
            "Default",
            "--label",
            "Vesper",
        ],
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("also runs agents"),
        "{refused}"
    );
    drop(vesper_target);
    let on = vesper.ok(
        "login-on",
        &[
            "--browser",
            "brave-origin",
            "--profile",
            "Default",
            "--label",
            "Vesper",
        ],
    );
    assert_eq!(on["enabled"], true, "{on}");
    wait_for("each browser's jobs", 10, || {
        (person.lock().jobs.contains(&json!(["share"]))
            && agents_computer
                .lock()
                .jobs
                .contains(&json!(["pages", "receive"])))
        .then_some(())
    });
    for (site, rule) in [
        ("tax.test", "allow"),
        ("strict.test", "allow"),
        ("out.test", "allow"),
        ("xsite.test", "allow"),
        ("big.test", "allow"),
        ("partial.test", "allow"),
        ("bank.test", "allow"),
    ] {
        vesper.ok(
            "login-rule",
            &["--computer", &tulip1_id, "--site", site, "--rule", rule],
        );
    }
    // Deny for All Computers beats this computer's Allow.
    vesper.ok(
        "login-rule",
        &["--computer", "all", "--site", "bank.test", "--rule", "deny"],
    );

    let mut codex = Agent::start(&tulip1, "vesper", "codex");
    wait_for("Tulip1 ready and sharing from Vesper", 40, || {
        let status = codex.call("computer_status", json!({}));
        let line = status["result"]["computers"][0]["capabilities"]
            .as_str()
            .unwrap_or("")
            .to_string();
        (status["situation"]
            .as_str()
            .unwrap_or("")
            .contains("nobody controls")
            && line.contains("logins from Vesper · 6 sites allowed"))
        .then_some(())
    });
    let computer = codex.call("computer_status", json!({}))["result"]["computers"][0]["id"].clone();
    let own = codex.call("computer_status", json!({"ref": computer}));
    let summary = own["result"]["summary"].as_str().unwrap_or("").to_string();
    assert!(summary.contains("logins allowed: big.test, out.test, partial.test, strict.test, tax.test, xsite.test; 1 denied"), "Allowed named, Denied counted: {own}");
    assert!(
        !summary.contains("bank.test"),
        "a Denied site is never named: {own}"
    );

    // J2: one begin, several sites, one approval.
    let began = codex.begin("b1", &["tax.test", "idp.test", "bank.test", "mail.test"]);
    let task = began["result"]["task_ref"].as_str().unwrap().to_string();
    let logins = &began["result"]["logins"];
    let standing = states(&began);
    assert_eq!(
        standing,
        BTreeMap::from([
            ("tax.test".into(), "allowed".into()),
            ("idp.test".into(), "asking".into()),
            ("bank.test".into(), "denied".into()),
            ("mail.test".into(), "asking".into())
        ]),
        "{began}"
    );
    let att = logins[1]["attention"].as_str().unwrap().to_string();
    assert_eq!(
        logins[3]["attention"],
        json!(att),
        "one request for every site asked: {began}"
    );
    assert!(
        logins[0]["attention"].is_null() && logins[2]["attention"].is_null(),
        "{began}"
    );
    let open_logins = |target: &Target| {
        target.admin(&["attention"]).1["result"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| i["kind"] == "login" && i["state"] == "open")
            .collect::<Vec<_>>()
    };
    let asked = open_logins(&tulip1);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        asked[0]["question"],
        "codex@vesper, working on “File the quarterly report” on Tulip1, wants your logins for 2 sites",
        "{asked:?}"
    );
    assert_eq!(
        asked[0]["details"]["sites"],
        json!([{"site": "idp.test", "via": null}, {"site": "mail.test", "via": null}]),
        "{asked:?}"
    );
    wait_for("the Allowed site delivered without asking", 15, || {
        ops(&agents_computer)
            .contains(&"write tax.test 3".to_string())
            .then_some(())
    });
    assert!(
        ops(&person)
            .iter()
            .all(|o| !o.starts_with("write") && !o.starts_with("refused")),
        "the person's browser is only read: {:?}",
        ops(&person)
    );

    // Only the sharing computer answers a login request.
    let epoch = hazel.ok("operator-session", &["--computer", &hazel_tulip1])["controller_epoch"]
        .as_str()
        .unwrap()
        .to_string();
    let generic = hazel.ask(
        "operator-answer-attention",
        &[
            "--computer",
            &hazel_tulip1,
            "--epoch",
            &epoch,
            &att,
            "approve",
        ],
    );
    assert!(
        generic["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("the computer your logins come from"),
        "{generic}"
    );
    let mut command = Command::new(IBARA);
    world.machine_env(&mut command, node("hazel"));
    command
        .env("HOME", &hazel.home)
        .args([
            "operator",
            "--computer",
            &hazel_tulip1,
            "--epoch",
            &epoch,
            "--op",
            "login_answer",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"att_ref": att, "decisions": {"idp.test": "shared"}})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("sharing computer"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(open_logins(&tulip1).len(), 1, "still open");

    // J4: Share one, Don't Share the other.
    let answered = vesper.ok(
        "login-answer",
        &[
            "--computer",
            &tulip1_id,
            "--att",
            &att,
            "--decisions",
            r#"{"tax.test":"share","idp.test":"share","mail.test":"decline"}"#,
        ],
    );
    replies.push(answered.to_string());
    let outcomes: BTreeMap<String, String> = answered["sites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["site"].as_str().unwrap().into(),
                s["outcome"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        BTreeMap::from([
            ("tax.test".into(), "shared".into()),
            ("idp.test".into(), "shared".into()),
            ("mail.test".into(), "declined".into())
        ]),
        "{answered}"
    );
    assert!(open_logins(&tulip1).is_empty(), "the item is answered");
    assert!(ops(&agents_computer).contains(&"write idp.test 2".to_string()));

    // J3 on an Allowed site: nothing asked, a fresh copy, the page opens signed in.
    let allowed = codex.sign_in(&task, "s1", &[]);
    assert_eq!(
        (
            allowed["status"].as_str(),
            states(&allowed).get("tax.test").map(String::as_str)
        ),
        (Some("ok"), Some("shared")),
        "{allowed}"
    );
    assert_eq!(
        allowed["result"]["sign_in"]["page"], "left_sign_in",
        "{allowed}"
    );
    assert!(
        allowed["result"]["frame"].is_object(),
        "the reloaded tab's frame comes with it: {allowed}"
    );
    assert_eq!(
        ops(&agents_computer)
            .iter()
            .filter(|o| *o == "write tax.test 3")
            .count(),
        3,
        "refreshed fresh"
    );
    assert_eq!(
        codex.sign_in(&task, "s1", &[])["result"],
        allowed["result"],
        "a resend replays"
    );

    // Reload commits after the old controller's 500 ms wait. The old form
    // remains fully loaded until then; it must not become rejected_before.
    agents_computer.lock().reload_delay = Duration::from_millis(1100);
    let slow = codex.sign_in(&task, "slow", &["tax.test"]);
    assert_eq!(slow["result"]["sign_in"]["page"], "left_sign_in", "{slow}");
    assert_eq!(states(&slow)["tax.test"], "shared", "{slow}");
    agents_computer.lock().reload_delay = Duration::ZERO;

    // A worker loaded before deployment cannot confirm the new document.
    // It must produce unknown, never a false rejection or remembered failure.
    agents_computer.lock().old_extension = true;
    let old_worker = codex.sign_in(&task, "old-worker", &["tax.test"]);
    assert_eq!(
        old_worker["result"]["sign_in"]["page"], "unknown",
        "{old_worker}"
    );
    assert_eq!(states(&old_worker)["tax.test"], "shared", "{old_worker}");
    agents_computer.lock().old_extension = false;

    // Ask First: held, answered, then the same request goes ahead.
    let held = codex.sign_in(&task, "s2", &["news.test"]);
    assert_eq!(
        (
            held["status"].as_str(),
            states(&held).get("news.test").map(String::as_str)
        ),
        (Some("pending"), Some("waiting_for_person")),
        "{held}"
    );
    let news_att = held["result"]["attention"].as_str().unwrap().to_string();
    assert!(
        held["result"]["next"]
            .as_str()
            .unwrap()
            .starts_with("waiting_for_person:")
            && held["result"]["next"].as_str().unwrap().contains(&news_att),
        "{held}"
    );
    assert_eq!(
        codex.sign_in(&task, "s2", &["news.test"])["result"]["attention"],
        json!(news_att),
        "a resend asks nobody again"
    );
    assert_eq!(open_logins(&tulip1).len(), 1);
    replies.push(
        vesper
            .ok(
                "login-answer",
                &[
                    "--computer",
                    &tulip1_id,
                    "--att",
                    &news_att,
                    "--decisions",
                    r#"{"news.test":"share"}"#,
                ],
            )
            .to_string(),
    );
    let shared = settled(&mut codex, &task, "s2", &["news.test"]);
    assert_eq!(states(&shared)["news.test"], "shared", "{shared}");

    // Declined earlier in this task, and Denied: final at once.
    let declined = codex.sign_in(&task, "s3", &["mail.test"]);
    assert_eq!(
        (
            declined["status"].as_str(),
            states(&declined)["mail.test"].as_str()
        ),
        (Some("ok"), "declined"),
        "{declined}"
    );
    assert!(
        declined["result"]["next"]
            .as_str()
            .unwrap()
            .starts_with("declined:"),
        "{declined}"
    );
    let denied = codex.sign_in(&task, "s4", &["bank.test"]);
    assert_eq!(states(&denied)["bank.test"], "denied", "{denied}");
    assert!(
        open_logins(&tulip1).is_empty(),
        "nothing is asked for a declined or Denied site"
    );

    // Signed out in the person's browser: the agent waits, then it arrives.
    let out = codex.sign_in(&task, "s5", &["out.test"]);
    assert_eq!(
        (out["status"].as_str(), states(&out)["out.test"].as_str()),
        (Some("pending"), "signed_out_there"),
        "{out}"
    );
    assert!(
        out["result"]["next"]
            .as_str()
            .unwrap()
            .starts_with("signed_out_there:"),
        "{out}"
    );
    person.lock().cookies.insert(
        "out.test".into(),
        vec![cookie(".out.test", "o", &values[3], true)],
    );
    assert_eq!(
        states(&settled(&mut codex, &task, "s5", &["out.test"]))["out.test"],
        "shared"
    );

    // The site still asks to sign in: rejected, remembered, and one try per task.
    {
        let mut tab = agents_computer.lock();
        tab.url = "https://strict.test/login?next=/home".into();
        tab.page = "still_sign_in".into();
    }
    let rejected = codex.sign_in(&task, "s6", &[]);
    assert_eq!(
        (
            states(&rejected)["strict.test"].as_str(),
            rejected["result"]["sign_in"]["page"].as_str()
        ),
        ("site_rejected", Some("still_sign_in")),
        "{rejected}"
    );
    assert!(
        rejected["result"]["next"]
            .as_str()
            .unwrap()
            .starts_with("site_rejected:"),
        "{rejected}"
    );
    let writes = ops(&agents_computer).len();
    let again = codex.sign_in(&task, "s7", &["strict.test"]);
    assert_eq!(states(&again)["strict.test"], "site_rejected", "{again}");
    assert_eq!(
        ops(&agents_computer).len(),
        writes,
        "nothing is copied again in this task"
    );
    agents_computer.lock().page = String::new();

    // A bundle shaped like x.com's, and a large one written in batches.
    agents_computer.lock().url = "https://xsite.test/home".into();
    let x = codex.sign_in(&task, "s8", &[]);
    assert_eq!(states(&x)["xsite.test"], "shared", "{x}");
    assert!(
        ops(&agents_computer).contains(&"write xsite.test 17".to_string()),
        "{:?}",
        ops(&agents_computer)
    );
    let big = codex.sign_in(&task, "s9", &["big.test"]);
    assert_eq!(states(&big)["big.test"], "shared", "{big}");
    let batches: Vec<usize> = ops(&agents_computer)
        .iter()
        .filter_map(|o| o.strip_prefix("write big.test "))
        .map(|n| n.parse().unwrap())
        .collect();
    assert!(
        batches.iter().all(|n| *n <= 25) && batches.iter().sum::<usize>() == 103,
        "{batches:?}"
    );

    // A partial write is unknown, says what to do, and page reading goes on.
    let partial = codex.sign_in(&task, "s10", &["partial.test"]);
    assert_eq!(states(&partial)["partial.test"], "unknown", "{partial}");
    assert!(
        partial["result"]["next"]
            .as_str()
            .unwrap()
            .contains("couldn't confirm the whole login was written"),
        "{partial}"
    );
    agents_computer.lock().url = "https://www.tax.test/login".into();
    assert_eq!(
        states(&codex.sign_in(&task, "s11", &[]))["tax.test"],
        "shared",
        "the page reader was not fenced"
    );

    // The person's browser is closed: the agent waits, then it arrives.
    drop(person_browser.take());
    let closed = codex.sign_in(&task, "s12", &["tax.test"]);
    assert_eq!(
        (
            closed["status"].as_str(),
            states(&closed)["tax.test"].as_str()
        ),
        (Some("pending"), "waiting_for_browser"),
        "{closed}"
    );
    person_browser = Some(browser(&vesper_chrome, person.clone()));
    assert_eq!(
        states(&settled(&mut codex, &task, "s12", &["tax.test"]))["tax.test"],
        "shared"
    );
    codex.finish(&task, "f1");

    // Site memory: kept with the rules, seen at the next begin, cleared by a success.
    let settings_path = vesper.home.join(".local/state/ibara/login-sharing.json");
    let memory = |site: &str| -> Value {
        let settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        settings["logins"]["memory"][site].clone()
    };
    wait_for("the rejection kept on the sharing computer", 15, || {
        (memory("strict.test")["result"] == "site_rejected").then_some(())
    });
    let rejections = vesper.ok("login-settings", &[]);
    assert!(
        rejections["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["site"] == "strict.test" && r["computer"] == json!(tulip1_id)),
        "{rejections}"
    );
    wait_for("the memory pushed to Tulip1", 15, || {
        let began = codex.begin(&format!("b2-{}", uuid::Uuid::new_v4()), &["strict.test"]);
        let task = began["result"]["task_ref"].as_str().unwrap().to_string();
        let seen = began["result"]["logins"][0].clone();
        if seen["state"] == "rejected_before" {
            assert_eq!(seen["last_result"], "site_rejected", "{began}");
            agents_computer.lock().url = "https://strict.test/login".into();
            let worked = codex.sign_in(&task, "w1", &[]);
            assert_eq!(
                (
                    states(&worked)["strict.test"].as_str(),
                    worked["result"]["sign_in"]["page"].as_str()
                ),
                ("shared", Some("left_sign_in")),
                "{worked}"
            );
            codex.finish(&task, "f2");
            Some(())
        } else {
            codex.finish(&task, "f2");
            None
        }
    });
    wait_for("a success clears the memory", 15, || {
        (memory("strict.test")["result"] == "worked").then_some(())
    });

    // Another person's agent is asked even for an Allowed site.
    let access = tulip1.admin(&["access"]).1["result"].clone();
    let edit = json!({"subject": "hazel", "capability": "administer", "rule": "deny", "expected_revision": access["revision"]}).to_string();
    assert_eq!(tulip1.admin(&["access_set", &edit]).0, Some(0));
    let mut friend = Agent::start(&tulip1, "hazel", "claude");
    let theirs = friend.begin("h1", &["tax.test"]);
    assert_eq!(theirs["result"]["logins"][0]["state"], "asking", "{theirs}");
    assert!(
        theirs["result"]["logins"][0]["attention"].is_string(),
        "{theirs}"
    );
    friend.finish(theirs["result"]["task_ref"].as_str().unwrap(), "hf");

    // Remove beats an All Computers Allow, and the next request asks again.
    vesper.ok(
        "login-rule",
        &[
            "--computer",
            "all",
            "--site",
            "shop.test",
            "--rule",
            "allow",
        ],
    );
    wait_for("shop.test Allowed through All Computers", 15, || {
        let began = codex.begin(&format!("b3-{}", uuid::Uuid::new_v4()), &["shop.test"]);
        codex.finish(began["result"]["task_ref"].as_str().unwrap(), "f3");
        (began["result"]["logins"][0]["state"] == "allowed").then_some(())
    });
    let removed = vesper.ok(
        "login-remove",
        &["--computer", &tulip1_id, "--site", "shop.test"],
    );
    replies.push(removed.to_string());
    assert!(
        ops(&agents_computer).contains(&"remove shop.test".to_string()),
        "{removed}"
    );
    let rows = vesper.ok("login-rows", &["--computer", &tulip1_id]);
    let row = rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["site"] == "shop.test")
        .unwrap()
        .clone();
    assert_eq!(
        (
            row["rule"].as_str(),
            row["own_rule"].as_str(),
            row["all_rule"].as_str()
        ),
        (Some("ask"), Some("ask"), Some("allow")),
        "{rows}"
    );
    assert!(
        rows["denied_all"]
            .as_array()
            .unwrap()
            .contains(&json!("bank.test")),
        "{rows}"
    );
    let asked_again = codex.begin("b4", &["shop.test"]);
    assert_eq!(
        asked_again["result"]["logins"][0]["state"], "asking",
        "{asked_again}"
    );
    let again_att = asked_again["result"]["logins"][0]["attention"]
        .as_str()
        .unwrap();
    let share_all = vesper.ok(
        "login-answer",
        &[
            "--computer",
            &tulip1_id,
            "--att",
            again_att,
            "--decisions",
            r#"{"shop.test":"share_all"}"#,
        ],
    );
    assert_eq!(share_all["sites"][0]["outcome"], "shared", "{share_all}");
    codex.finish(asked_again["result"]["task_ref"].as_str().unwrap(), "f4");
    // Share also resolves an All Computers Ask First.
    vesper.ok(
        "login-rule",
        &["--computer", "all", "--site", "shop.test", "--rule", "ask"],
    );
    vesper.ok(
        "login-remove",
        &["--computer", &tulip1_id, "--site", "shop.test"],
    );
    let again = codex.begin("b4-share", &["shop.test"]);
    let att = again["result"]["logins"][0]["attention"].as_str().unwrap();
    let share = vesper.ok(
        "login-answer",
        &[
            "--computer",
            &tulip1_id,
            "--att",
            att,
            "--decisions",
            r#"{"shop.test":"share"}"#,
        ],
    );
    assert_eq!(share["sites"][0]["outcome"], "shared", "{share}");
    codex.finish(again["result"]["task_ref"].as_str().unwrap(), "f4-share");

    agents_computer.lock().remove_fail = true;
    let partial_remove = vesper.ok(
        "login-remove",
        &["--computer", &tulip1_id, "--site", "shop.test"],
    );
    assert_eq!(
        (
            partial_remove["removed"].clone(),
            partial_remove["deferred"].clone()
        ),
        (json!(1), json!(true)),
        "{partial_remove}"
    );
    agents_computer.lock().remove_fail = false;

    // A removal while Tulip1's browser is closed goes once it opens.
    drop(tulip1_browser);
    std::thread::sleep(Duration::from_millis(300));
    let deferred = vesper.ok(
        "login-remove",
        &["--computer", &tulip1_id, "--site", "idp.test"],
    );
    assert!(
        deferred["removed"].is_null() && deferred["deferred"] == true,
        "{deferred}"
    );
    let _tulip1_browser = browser(&tulip1_chrome, agents_computer.clone());
    wait_for("the deferred removal", 20, || {
        ops(&agents_computer)
            .contains(&"remove idp.test".to_string())
            .then_some(())
    });

    // Share With and Sync: fresh from the person's browser; Hazel's browser is not running.
    let with = vesper.ok(
        "login-share-with",
        &[
            "--site",
            "news.test",
            "--to",
            &format!("{tulip1_id},{hazel_id}"),
        ],
    );
    replies.push(with.to_string());
    assert_eq!(
        (with["delivered"].clone(), with["deferred"].clone()),
        (json!([tulip1_id]), json!([hazel_id])),
        "{with}"
    );
    let partial_spread = vesper.ok(
        "login-share-with",
        &["--site", "partial.test", "--to", &tulip1_id],
    );
    assert_eq!(
        partial_spread["unknown"],
        json!([tulip1_id]),
        "{partial_spread}"
    );
    assert!(
        partial_spread["deferred"].as_array().unwrap().is_empty(),
        "{partial_spread}"
    );
    let dry = vesper.ok("login-sync", &["--dry-run"]);
    assert!(
        dry["sites"].as_u64().unwrap() >= 6 && dry["computers"].as_u64().unwrap() == 2,
        "{dry}"
    );
    let partial_sync = vesper.ok("login-sync", &[]);
    assert!(
        partial_sync["unknown"]
            .as_array()
            .unwrap()
            .contains(&json!(tulip1_id)),
        "{partial_sync}"
    );
    agents_computer.lock().fail.clear();
    let synced = vesper.ok("login-sync", &[]);
    replies.push(synced.to_string());
    assert!(
        synced["delivered"]
            .as_array()
            .unwrap()
            .contains(&json!(tulip1_id)),
        "{synced}"
    );

    // The sharing computer is away: the agent does other work or waits.
    let away_task = codex.begin("b5", &[])["result"]["task_ref"]
        .as_str()
        .unwrap()
        .to_string();
    drop(person_browser.take());
    drop(vesper);
    let away = codex.sign_in(&away_task, "a1", &["tax.test"]);
    assert_eq!(
        (away["status"].as_str(), states(&away)["tax.test"].as_str()),
        (Some("pending"), "waiting_for_sharing_computer"),
        "{away}"
    );
    let mut vesper = Console::start_with(&world, "vesper", node("vesper"), None, &console_env);
    let _person_browser = browser(&vesper_chrome, person.clone());
    assert_eq!(
        states(&settled(&mut codex, &away_task, "a1", &["tax.test"]))["tax.test"],
        "shared"
    );

    // Off: every computer hears it; logins already copied stay.
    let off = vesper.ok("login-off", &[]);
    assert_eq!(off["enabled"], false, "{off}");
    assert_eq!(off["extension_removed"], false, "{off}");
    let status = codex.call("computer_status", json!({}));
    assert!(
        status["result"]["computers"][0]["capabilities"]
            .as_str()
            .unwrap()
            .contains("logins off"),
        "{status}"
    );
    let off_reply = codex.sign_in(&away_task, "a2", &["tax.test"]);
    assert_eq!(
        (
            off_reply["status"].as_str(),
            states(&off_reply)["tax.test"].as_str()
        ),
        (Some("pending"), "sharing_off"),
        "{off_reply}"
    );
    codex.finish(&away_task, "f5");
    assert!(
        !ops(&agents_computer).iter().any(|o| o == "remove tax.test"),
        "turning off removes nothing already copied"
    );

    // `since` told the agent what happened, without values.
    assert!(
        codex
            .since
            .iter()
            .any(|s| s == "login for tax.test shared" || s == "login for tax.test refreshed"),
        "{:?}",
        codex.since
    );
    assert!(
        codex
            .since
            .iter()
            .any(|s| s == "login for bank.test refused"),
        "{:?}",
        codex.since
    );

    // Not one cookie value anywhere a person, agent or file could see it.
    let journal = rusqlite::Connection::open_with_flags(
        tulip1.root.join("state/journal.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let events: Vec<String> = journal
        .prepare("SELECT kind || ' ' || summary || ' ' || data FROM timeline_events")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(journal);
    let timeline = events.join("\n");
    for kind in [
        "login_shared",
        "login_refreshed",
        "login_refused",
        "login_removed",
    ] {
        assert!(
            events.iter().any(|e| e.starts_with(kind)),
            "the timeline records {kind}"
        );
    }
    let mut searched: Vec<(String, Vec<u8>)> = vec![
        (
            "codex transcript".into(),
            codex.transcript.lock().clone().into_bytes(),
        ),
        (
            "friend transcript".into(),
            friend.transcript.lock().clone().into_bytes(),
        ),
        ("console replies".into(), replies.join("\n").into_bytes()),
        ("timeline".into(), timeline.into_bytes()),
        (
            "attention".into(),
            tulip1.admin(&["attention"]).1.to_string().into_bytes(),
        ),
        ("settings".into(), fs::read(&settings_path).unwrap()),
        (
            "login-settings".into(),
            vesper.ok("login-settings", &[]).to_string().into_bytes(),
        ),
    ];
    for target in [&tulip1, &hazel_target] {
        for name in ["journal.sqlite", "journal.sqlite-wal", "journal.sqlite-shm"] {
            if let Ok(bytes) = fs::read(target.root.join("state").join(name)) {
                searched.push((format!("{} {name}", target.root.display()), bytes));
            }
        }
    }
    for (what, bytes) in &searched {
        for value in &values {
            assert_eq!(hits(bytes, value), 0, "a cookie value in the {what}");
        }
    }
    world.record(json!({"kind": "login-journeys", "regressions": ["slow_reload", "share_after_remove", "decline_unpinned", "self_pin_refused", "partial_spread", "partial_remove"], "cookie_value_hits": 0, "searched": searched.iter().map(|(w, _)| w.clone()).collect::<Vec<_>>()}));
    drop(hazel);
}

// Failure cases: settlement loses command/state; replay runs a command twice;
// stored/operator clips grow or split Unicode; old summaries lose their state
// or projection rewrites history; an unpolled job stays running in the console.
#[test]
fn command_receipts_keep_commands_and_project_live_and_historical_job_states() {
    use support::*;
    let world = World::new("command-receipts");
    // Tulip1, the agent computer: a desktop session.
    let desk = world.root.join("desk-tulip1");
    fs::create_dir_all(&desk).unwrap();
    write_executable(&desk.join("hyprctl"), DESKTOP_HYPRCTL);
    write_executable(&desk.join("omarchy-toggle-idle"), DESKTOP_IDLE);
    let (hyprctl, idle) = (desk.join("hyprctl"), desk.join("omarchy-toggle-idle"));
    let env: [(&str, &std::ffi::OsStr); 3] = [
        ("IBARA_TEST_HYPRCTL", hyprctl.as_os_str()),
        ("IBARA_TEST_IDLE", idle.as_os_str()),
        ("WAYLAND_DISPLAY", std::ffi::OsStr::new("wayland-e2e")),
    ];
    fs::create_dir_all(world.root.join("target-tulip1/xdg-run")).unwrap();
    fs::set_permissions(
        world.root.join("target-tulip1/xdg-run"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let tulip1 = Target::start_with(&world, node("tulip1"), Some("Tulip1"), 300_000, &env);
    let source = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&source));
    let tulip1_id = add_own(&mut vesper, "tulip1");
    let mut codex = Agent::start(&tulip1, "vesper", "codex");
    wait_for("command computer ready", 10, || {
        let status = codex.call("computer_status", json!({}));
        status["situation"].as_str().unwrap_or("").contains("nobody controls").then_some(())
    });
    // Commands through the real MCP and operator routes in a separate world.
    let command_begin = codex.begin("command-begin", &[]);
    let command_task = command_begin["result"]["task_ref"].as_str().unwrap();
    let long_arg = format!("{}COMMAND_TAIL", "é".repeat(2100));
    let commands = [
        ("command-ok", json!(["sh", "-c", "echo once >> runs; exit 0"]), true, "completed", 0),
        ("command-fail", json!(["sh", "-c", "echo once >> runs; exit 7"]), true, "failed", 7),
        ("command-quick-fail", json!(["sh", "-c", "exit 9"]), false, "failed", 9),
        ("command-long", json!(["printf", "%s", long_arg]), false, "completed", 0),
    ];
    let journal = rusqlite::Connection::open(tulip1.root.join("state/journal.sqlite")).unwrap();
    journal.busy_timeout(Duration::from_secs(5)).unwrap();
    let mut command_ops = Vec::new();
    for (request, command, background, state, exit_code) in &commands {
        let args = json!({"task_ref": command_task, "request_id": request, "command": command, "background": background, "timeout_ms": 5000});
        let ran = codex.call("computer_exec", args.clone());
        assert!(ran["error"].is_null(), "{ran}");
        let op = ran["result"]["op_ref"].as_str().unwrap().to_string();
        let waited = codex.call("computer_wait", json!({"task_ref": command_task, "for": {"op": op}, "deadline_ms": 5000}));
        assert_eq!(waited["result"]["met"], true, "{waited}");
        let read = codex.call("computer_status", json!({"ref": op}));
        assert!(read["result"]["summary"].as_str().unwrap().contains(&format!("job {state}")), "{read}");
        let replay = codex.call("computer_exec", args);
        assert_eq!(replay["result"]["job"]["state"], *state, "{replay}");
        assert_eq!(replay["result"]["job"]["exit_code"], *exit_code, "{replay}");
        let raw: String = journal.query_row("SELECT receipt FROM operations WHERE operation_ref = ?", [&op], |r| r.get(0)).unwrap();
        let receipt: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(receipt["summary"], format!("run {command}").chars().take(2000).collect::<String>());
        assert_eq!(receipt["job_state"], *state, "{receipt}");
        command_ops.push((op, receipt));
    }
    assert_eq!(fs::read_to_string(Path::new(command_begin["result"]["workspace"].as_str().unwrap()).join("runs")).unwrap(), "once\nonce\n");
    // The operator must see a quick-path background job settle without an agent
    // polling the operation. The stored receipt still says running.
    let live = codex.call("computer_exec", json!({"task_ref": command_task,
        "request_id": "command-live", "command": ["sh", "-c", "sleep 1; exit 0"], "background": true}));
    let live_op = live["result"]["op_ref"].as_str().unwrap();
    assert_eq!(live["result"]["job"]["state"], "running", "{live}");
    let epoch = vesper.ok("operator-session", &["--computer", &tulip1_id])["controller_epoch"].as_str().unwrap().to_string();
    let projected = wait_for("operator sees settled job without agent lookup", 10, || {
        let details = vesper.ok("operator-task", &["--computer", &tulip1_id, "--epoch", &epoch, "--task", command_task]);
        let r = details["result"]["receipts"].as_array().unwrap().iter().find(|r| r["operation_ref"] == live_op).unwrap();
        (r["job_state"] == "completed").then(|| r.clone())
    });
    world.record(json!({"unpolled_command_receipt": projected}));
    codex.finish(command_task, "command-finish");
    let epoch = vesper.ok("operator-session", &["--computer", &tulip1_id])["controller_epoch"].as_str().unwrap().to_string();
    let details = vesper.ok("operator-task", &["--computer", &tulip1_id, "--epoch", &epoch, "--task", command_task]);
    for ((op, receipt), (_, _, _, state, _)) in command_ops.iter().zip(&commands) {
        let projected = details["result"]["receipts"].as_array().unwrap().iter().find(|r| r["operation_ref"] == *op).unwrap();
        assert_eq!(projected["summary"], receipt["summary"].as_str().unwrap().chars().take(400).collect::<String>());
        assert_eq!(projected["job_state"], *state, "{projected}");
        assert!(!projected.to_string().contains("COMMAND_TAIL"), "{projected}");
    }
    world.record(json!({"command_receipts": details["result"]["receipts"]}));

    // Historical jobs here contain no command. Keep their old summary and
    // recover job_state from it without rewriting the stored receipt.
    let (old_op, old_receipt) = &command_ops[1];
    let old_summary = format!("Job {} is failed.", old_receipt["job_ref"].as_str().unwrap());
    let mut old = old_receipt.clone();
    old.as_object_mut().unwrap().remove("job_state");
    old["summary"] = json!(old_summary);
    journal.execute("UPDATE operations SET receipt = ? WHERE operation_ref = ?", [old.to_string(), old_op.clone()]).unwrap();
    let details = vesper.ok("operator-task", &["--computer", &tulip1_id, "--epoch", &epoch, "--task", command_task]);
    let projected = details["result"]["receipts"].as_array().unwrap().iter().find(|r| r["operation_ref"] == *old_op).unwrap();
    assert_eq!(projected["summary"], old_summary);
    assert_eq!(projected["job_state"], "failed");
    let unchanged: String = journal.query_row("SELECT receipt FROM operations WHERE operation_ref = ?", [old_op], |r| r.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&unchanged).unwrap(), old);
    world.record(json!({"legacy_command_receipt": projected}));
    drop(journal);

}
