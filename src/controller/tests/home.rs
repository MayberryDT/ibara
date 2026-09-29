//! What an agent may name in the desktop person's home folder: anything
//! except what ibara keeps or trusts there, and other accounts' homes.
//! Failure cases, written first:
//! - a file ibara keeps or trusts in the home folder (the console's state and
//!   journal backups, settings, the SSH folder with ibara's key and pins,
//!   update scratch space, older program copies, the services' units and
//!   environment, the console plugin, the page reader's host manifests, the
//!   browsers' flags, the lock at sign-in, the data and state folders) is
//!   read, written, published or sent, by `~/` or its absolute path
//! - one of those folders is listed, or a command runs in it
//! - a check reads one of those files through a link in the workspace
//! - with the home folder named through a symbolic link, another task's
//!   workspace is read by that name
//! - an ordinary file or folder beside them is refused
//! - another account's home inside the home folder is accepted, or a home
//!   folder of `/` lets agents name the whole computer

use super::*;
use crate::controller::checks::{HomeRule, Place};

/// Files ibara keeps or trusts, relative to the home folder.
const IBARA_FILES: [&str; 18] = [
    ".local/state/ibara/backups/before-0.1.0-9-1/journal.sqlite",
    ".local/state/ibara/onboarding.json",
    ".config/ibara/settings.toml",
    ".ssh/ibara_agent_ed25519",
    ".ssh/known_hosts_ibara",
    ".ssh/config",
    ".cache/ibara/update-1/ibara.pkg.tar.zst",
    ".local/bin/ibara",
    ".local/bin/ibarad",
    ".config/systemd/user/agent-computer.service.d/override.conf",
    ".config/environment.d/ibara.conf",
    ".config/omarchy/plugins/io.zet.ibara/manifest.json",
    ".config/chromium/NativeMessagingHosts/io.ibara.chrome.json",
    ".config/google-chrome/NativeMessagingHosts/io.ibara.chrome.json",
    ".config/chromium-flags.conf",
    ".config/omarchy/hooks/post-boot.d/00-ibara-lock-at-sign-in",
    "data/workspaces/task_other/notes.txt",
    "state/pending-promotions/proc_1.json",
];

/// Folders ibara keeps or trusts, relative to the home folder.
const IBARA_FOLDERS: [&str; 10] = [
    ".local/state/ibara",
    ".config/ibara",
    ".ssh",
    ".cache/ibara",
    ".config/systemd/user",
    ".config/environment.d",
    ".config/omarchy/plugins/io.zet.ibara",
    ".config/chromium/NativeMessagingHosts",
    "data/workspaces",
    "state",
];

async fn begin_with(c: &Controller, checks: Value) -> (String, String) {
    let begun = call(c, "computer_begin", json!({ "goal": "Tidy my notes", "request_id": id("b"), "checks": checks })).await;
    assert_eq!(begun["status"], "ok", "{begun}");
    (begun["result"]["task_ref"].as_str().unwrap().to_string(), begun["result"]["workspace"].as_str().unwrap().to_string())
}

fn refused_as(envelope: &Value, says: &str, what: &str) {
    assert_eq!(code(envelope), "INVALID_ARGUMENT", "{what}: {envelope}");
    let message = envelope["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains(says), "{what}: {message}");
}

#[test]
fn ibaras_files_in_the_home_folder_are_refused_and_ordinary_ones_are_not() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        let home = rig.dir.to_str().unwrap().to_string();
        for rel in IBARA_FILES.iter().chain([".config/someapp/app.conf", ".local/bin/tool", ".local/state/someapp/log.txt"].iter()) {
            std::fs::create_dir_all(rig.dir.join(rel).parent().unwrap()).unwrap();
            std::fs::write(rig.dir.join(rel), "secret").unwrap();
        }
        let (task, _) = begin_with(c, json!([])).await;
        // A second apart, well under the per-connection rate limit.
        let tick = || rig.clock.fetch_add(1000, Ordering::SeqCst);
        let files = |args: Value| {
            tick();
            let mut args = args;
            args["task_ref"] = json!(task);
            args["request_id"] = json!(id("f"));
            call(c, "computer_files", args)
        };
        let exec = |cwd: String| {
            tick();
            call(c, "computer_exec", json!({ "task_ref": task, "request_id": id("e"), "command": ["pwd"], "cwd": cwd, "timeout_ms": 5000 }))
        };

        for rel in IBARA_FILES {
            for path in [format!("~/{rel}"), format!("{home}/{rel}")] {
                refused_as(&files(json!({ "op": "read", "path": path })).await, "ibara's own", &format!("read {path}"));
                refused_as(&files(json!({ "op": "write", "path": path, "text": "changed" })).await, "ibara's own", &format!("write {path}"));
                refused_as(&files(json!({ "op": "publish", "path": path })).await, "ibara's own", &format!("publish {path}"));
                let send = json!({ "op": "send", "path": path, "to": { "host": "vesper", "path": "/home/riley/Downloads/copy" } });
                refused_as(&files(send).await, "ibara's own", &format!("send {path}"));
            }
            assert_eq!(std::fs::read_to_string(rig.dir.join(rel)).unwrap(), "secret", "{rel} unchanged");
        }
        for rel in IBARA_FOLDERS {
            for dir in [format!("~/{rel}"), format!("{home}/{rel}")] {
                refused_as(&files(json!({ "op": "list", "dir": dir })).await, "ibara's own", &format!("list {dir}"));
                refused_as(&exec(dir.clone()).await, "ibara's own", &format!("exec in {dir}"));
            }
        }

        // Ordinary files beside them.
        for path in ["~/.config/someapp/app.conf", "~/.local/bin/tool", "~/.local/state/someapp/log.txt"] {
            let read = files(json!({ "op": "read", "path": path })).await;
            assert_eq!(read["result"]["text"], "secret", "{path}: {read}");
        }
        let wrote = files(json!({ "op": "write", "path": "~/.config/someapp/new.conf", "text": "theme = dark" })).await;
        assert_eq!(wrote["status"], "ok", "{wrote}");
        let published = files(json!({ "op": "publish", "path": format!("{home}/.config/someapp/new.conf") })).await;
        assert_eq!(published["status"], "ok", "{published}");
        for dir in ["~/.config", "~/.local/state", "~/.local/bin"] {
            let listed = files(json!({ "op": "list", "dir": dir })).await;
            assert_eq!(listed["status"], "ok", "{dir}: {listed}");
            let ran = exec(dir.to_string()).await;
            assert_eq!(ran["status"], "ok", "{dir}: {ran}");
        }
        let neighbor = rig.dir.parent().unwrap().join("another-person/notes.txt");
        refused_as(&files(json!({ "op": "read", "path": neighbor.to_str().unwrap() })).await, "outside", "another person's home");
    });
}

#[test]
fn a_check_does_not_read_ibaras_files_through_a_link() {
    run(async {
        let rig = rig(false);
        let c = &rig.controller;
        std::fs::create_dir_all(rig.dir.join(".config/ibara")).unwrap();
        std::fs::write(rig.dir.join(".config/ibara/settings.toml"), "secret").unwrap();
        let direct = call(
            c,
            "computer_begin",
            json!({ "goal": "Tidy my notes", "request_id": "b0", "checks": [{ "id": "c0", "description": "set", "check": { "kind": "file_content", "path": "~/.config/ibara/settings.toml", "contains": "secret" } }] }),
        )
        .await;
        refused_as(&direct, "ibara's own", "a check of ibara's settings");

        let check = json!([{ "id": "probe", "description": "the notes say secret", "check": { "kind": "file_content", "path": "notes/settings.toml", "contains": "secret" } }]);
        let (task, workspace) = begin_with(c, check).await;
        std::os::unix::fs::symlink(rig.dir.join(".config/ibara"), Path::new(&workspace).join("notes")).unwrap();
        let finish = call(c, "computer_finish", json!({ "task_ref": task, "request_id": "f1", "outcome": "partial", "summary": "stop" })).await;
        let probe = &finish["result"]["checks"][0];
        assert_eq!(probe["state"], "unknown", "{finish}");
        assert!(probe.to_string().contains("ibara's own"), "{probe}");
    });
}

#[test]
fn a_home_folder_named_through_a_link_still_refuses_ibaras_folders() {
    run(async {
        let outer = std::env::temp_dir().join(id("ibara-controller-test"));
        let real = outer.join("home");
        let link = outer.join("link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let clock = Arc::new(AtomicI64::new(1_790_000_000_000));
        let desktop = FakeDesktop::new();
        let controller = open_at(&real, &link, &clock, &desktop, None);
        let rig = Rig { dir: outer, clock, desktop, stream: FakeStream::default(), controller };
        let c = &rig.controller;
        std::fs::create_dir_all(real.join("data/workspaces/task_other")).unwrap();
        std::fs::write(real.join("data/workspaces/task_other/notes.txt"), "secret").unwrap();
        std::fs::write(real.join("shopping.txt"), "milk").unwrap();
        let (task, _) = begin_with(c, json!([])).await;
        let read = |path: String| call(c, "computer_files", json!({ "task_ref": task, "request_id": id("r"), "op": "read", "path": path }));

        let by_link = read(format!("{}/data/workspaces/task_other/notes.txt", link.display())).await;
        refused_as(&by_link, "ibara's own", "another task's workspace through the linked home");
        refused_as(&read("~/state/journal.sqlite".into()).await, "ibara's own", "the journal through ~/");
        let ordinary = read("~/shopping.txt".into()).await;
        assert_eq!(ordinary["result"]["text"], "milk", "{ordinary}");
    });
}

#[test]
fn another_accounts_home_inside_the_home_folder_is_refused() {
    let passwd = "root:x:0:0::/root:/bin/bash\n\
                  nobody:x:65534:65534::/:/usr/bin/nologin\n\
                  riley:x:1000:1000::/home/riley:/bin/bash\n\
                  guest:x:1001:1001::/home/riley/guest:/bin/bash\n";
    let rule = HomeRule::new(Path::new("/home/riley"), &[], passwd);
    assert!(matches!(rule.place(Path::new("/home/riley/guest/notes.txt")), Place::Refused(why) if why.contains("another account")));
    assert!(matches!(rule.place(Path::new("/home/riley/Documents/notes.txt")), Place::Home(rel) if rel == Path::new("Documents/notes.txt")));
    assert!(matches!(rule.place(Path::new("/root/notes.txt")), Place::Outside));

    let rootless = HomeRule::new(Path::new("/"), &[], passwd);
    assert!(matches!(rootless.place(Path::new("/etc/hostname")), Place::Outside));
    assert!(matches!(rootless.place(Path::new("/home/riley/notes.txt")), Place::Outside));
}
