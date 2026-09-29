//! Watch end to end: the pictures a console asks of a computer, and what
//! crosses the network for them.
//!
//! The same world as `tests/pairing.rs`: a real target daemon (Tulip1) and a
//! real console (Vesper) over the fake sshd's forced command. Tulip1 has a
//! desktop session with one unlocked screen, whose pixels are a PPM file behind
//! a fake `grim`. Vesper's `ssh` copies every line it sends and receives into
//! two logs: the network between the two computers.
//!
//! Failure cases this must catch:
//! 1. A picture crosses the network uncompressed or as PNG (a photo wallpaper
//!    is about 1 MB a picture), or the console cannot read the JPEG it asked for.
//! 2. A screen that has not changed is sent again.
//! 3. A screen that changed is taken as unchanged, so the console keeps the old
//!    picture.
//! 4. The console reuses a picture after the shown size changed (the wrong
//!    size) or after its file was released (a picture the shell cannot open).
//! 5. The file the shell reads is not a P6 of the shown size with the screen's
//!    pixels.
//!
//! `IBARA_E2E_EVIDENCE=DIR` keeps every console exchange as JSON lines in DIR.

mod support;

use serde_json::{Map, Value};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use support::*;

/// Omarchy's stay-awake switch, its flag in a file beside it.
const FAKE_TOGGLE_IDLE: &str = r#"#!/bin/sh
flag="$(dirname "$0")/stay-awake"
case "$1" in
  status) if [ -e "$flag" ]; then echo '{"enabled":true}'; else echo '{"enabled":false}'; fi ;;
  stay-awake) touch "$flag" ;;
  allow-idle) rm -f "$flag" ;;
  *) exit 2 ;;
esac
"#;

/// Hyprland with one unlocked screen.
const FAKE_HYPRCTL: &str = r#"#!/bin/sh
case "$*" in
  *monitors*) echo '[{"id":0,"name":"IbaraVirtual","width":1280,"height":720,"x":0,"y":0,"scale":1.0,"focused":true,"solitaryBlockedBy":[],"activeWorkspace":{"id":1,"name":"1"}}]' ;;
  *) echo '[]' ;;
esac
"#;

/// `grim -t ppm -o OUTPUT -`: the screen is the file beside it.
const FAKE_GRIM: &str = "#!/bin/sh\nexec cat \"$(dirname \"$0\")/screen.ppm\"\n";

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const DARK: u8 = 40;
const LIGHT: u8 = 215;

/// A 1280×720 screen: one half dark and the other light, both with a fine
/// texture, as a P6 file.
fn screen(dark_left: bool) -> Vec<u8> {
    let mut ppm = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let base = if (x < WIDTH / 2) == dark_left { DARK } else { LIGHT };
            let texture = ((x * 7 + y * 13) % 9) as u8;
            ppm.extend_from_slice(&[base + texture, base + texture / 2, base]);
        }
    }
    ppm
}

/// Pair `console` to `node` as the same person's computer; its computer id.
fn add_own(console: &mut Console, node: &str) -> String {
    let started = console.ok("pair-start", &[node]);
    let paired = console.settled(started["request_id"].as_str().unwrap());
    assert_eq!(paired["state"], "paired", "{paired}");
    paired["computer_id"].as_str().unwrap().to_string()
}

/// The first object in `value` (at any depth) that `wanted` accepts.
fn find<'a>(value: &'a Value, wanted: &dyn Fn(&Map<String, Value>) -> bool) -> Option<&'a Map<String, Value>> {
    match value {
        Value::Object(fields) if wanted(fields) => Some(fields),
        Value::Object(fields) => fields.values().find_map(|v| find(v, wanted)),
        Value::Array(items) => items.iter().find_map(|v| find(v, wanted)),
        _ => None,
    }
}

/// One direction of the network: the lines added since the last read.
struct Wire {
    log: PathBuf,
    read: usize,
}

impl Wire {
    fn new(log: PathBuf) -> Wire {
        Wire { log, read: 0 }
    }

    fn lines(&mut self) -> Vec<String> {
        let all = fs::read(&self.log).unwrap_or_default();
        let new = String::from_utf8(all[self.read..].to_vec()).unwrap();
        self.read = all.len();
        new.lines().map(str::to_string).collect()
    }

    /// The one line that holds an object `wanted` accepts: that object and the line's length.
    fn one(&mut self, what: &str, wanted: &dyn Fn(&Map<String, Value>) -> bool) -> (Map<String, Value>, usize) {
        let found: Vec<(Map<String, Value>, usize)> = self
            .lines()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok().and_then(|v| find(&v, wanted).cloned()).map(|o| (o, line.len())))
            .collect();
        assert_eq!(found.len(), 1, "{what}: {found:?}");
        found.into_iter().next().unwrap()
    }
}

/// The P6 file a preview names: its size and pixels.
fn picture(file: &str) -> ((u32, u32), Vec<u8>) {
    let bytes = fs::read(file).unwrap();
    let text = String::from_utf8_lossy(&bytes[..20.min(bytes.len())]).to_string();
    let mut fields = text.split_ascii_whitespace();
    assert_eq!(fields.next(), Some("P6"), "{file}");
    let (w, h): (u32, u32) = (fields.next().unwrap().parse().unwrap(), fields.next().unwrap().parse().unwrap());
    let header = format!("P6\n{w} {h}\n255\n").len();
    assert_eq!(bytes.len(), header + (w * h * 3) as usize, "{file}");
    ((w, h), bytes[header..].to_vec())
}

/// The mean brightness of a 16×16 block in the middle of the left half.
fn left_half(size: (u32, u32), pixels: &[u8]) -> u32 {
    let (cx, cy) = (size.0 / 4, size.1 / 2);
    let mut sum = 0u32;
    for y in cy - 8..cy + 8 {
        for x in cx - 8..cx + 8 {
            let at = ((y * size.0 + x) * 3) as usize;
            sum += pixels[at..at + 3].iter().map(|&v| v as u32).sum::<u32>();
        }
    }
    sum / (16 * 16 * 3)
}

fn frames(dir: &Path) -> usize {
    fs::read_dir(dir).unwrap().count()
}

#[test]
fn watch_sends_a_jpeg_once_and_nothing_while_the_screen_is_still() {
    let world = World::new("watch");
    let desk = world.root.join("desk-tulip1");
    fs::create_dir_all(&desk).unwrap();
    write_executable(&desk.join("hyprctl"), FAKE_HYPRCTL);
    write_executable(&desk.join("omarchy-toggle-idle"), FAKE_TOGGLE_IDLE);
    write_executable(&desk.join("grim"), FAKE_GRIM);
    fs::write(desk.join("screen.ppm"), screen(true)).unwrap();
    let (hyprctl, idle, grim) = (desk.join("hyprctl"), desk.join("omarchy-toggle-idle"), desk.join("grim"));
    let env: [(&str, &OsStr); 4] = [
        ("IBARA_TEST_HYPRCTL", hyprctl.as_os_str()),
        ("IBARA_TEST_IDLE", idle.as_os_str()),
        ("IBARA_TEST_GRIM", grim.as_os_str()),
        ("WAYLAND_DISPLAY", OsStr::new("wayland-e2e")),
    ];
    let _tulip1 = Target::start_with(&world, node("tulip1"), Some("Tulip1"), 300_000, &env);
    // Vesper's ssh: the world's fake sshd, with both directions copied to a log.
    let (sent_log, received_log) = (world.root.join("wire-sent"), world.root.join("wire-received"));
    let wrapper = format!(
        "#!/bin/sh\ntee -a '{}' | '{}' \"$@\" | tee -a '{}'\n",
        sent_log.display(),
        world.root.join("bin/ssh").display(),
        received_log.display()
    );
    write_executable(&world.machine_bin("vesper").join("ssh"), &wrapper);
    let vesper_target = Target::start(&world, node("vesper"), None, 300_000);
    let mut vesper = Console::start(&world, "vesper", node("vesper"), Some(&vesper_target));
    let id = add_own(&mut vesper, "tulip1");
    let e = vesper.ok("operator-session", &["--computer", &id])["controller_epoch"].as_str().unwrap().to_string();
    let previews = world.root.join("console-vesper/run/ibara/previews");
    let (mut sent, mut received) = (Wire::new(sent_log), Wire::new(received_log));
    sent.lines();
    received.lines();
    let mut watch = |vesper: &mut Console, width: &str, height: &str| {
        let envelope = vesper.preview(&["--computer", &id, "--epoch", &e, "--display", "IbaraVirtual", "--quality", "selected", "--width", width, "--height", height]);
        assert!(envelope["error"].is_null(), "{envelope}");
        let result = envelope["data"]["result"].clone();
        assert!(result.get("jpeg").is_none() && result.get("png").is_none(), "the shell never gets the picture's bytes: {result}");
        let (request, _) = sent.one("the observe request", &|o| o.get("op").is_some_and(|op| op == "observe"));
        let (reply, reply_bytes) = received.one("the observe answer", &|o| o.contains_key("frame_sequence"));
        (result["file"].as_str().unwrap().to_string(), request, reply, reply_bytes)
    };

    // 1, 5. The first picture crosses as JPEG, a fraction of the screen's bytes; the
    // file is the shown size, with the screen's pixels.
    let (first, request, reply, bytes) = watch(&mut vesper, "1152", "648");
    assert_eq!((request.get("format"), request.get("previous")), (Some(&Value::from("jpeg")), None), "{request:?}");
    assert!(reply.get("png").is_none() && reply.get("unchanged").is_none(), "{:?}", reply.keys().collect::<Vec<_>>());
    let jpeg = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, reply["jpeg"].as_str().unwrap()).unwrap();
    assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]), "a JPEG picture");
    let raw = (WIDTH * HEIGHT * 3) as usize;
    assert!(bytes < raw / 10, "{bytes} bytes on the network for a {raw}-byte screen");
    let digest = reply["digest"].as_str().unwrap().to_string();
    let (size, pixels) = picture(&first);
    assert_eq!(size, (1152, 648));
    assert!(left_half(size, &pixels).abs_diff(DARK as u32 + 3) <= 8, "{}", left_half(size, &pixels));

    // 2. The same screen: the answer carries no picture, and the shell gets the same file.
    let (again, request, reply, bytes) = watch(&mut vesper, "1152", "648");
    assert_eq!(request.get("previous"), Some(&Value::from(digest.clone())), "{request:?}");
    assert_eq!(reply.get("unchanged"), Some(&Value::Bool(true)), "{reply:?}");
    assert!(reply.get("jpeg").is_none() && reply.get("png").is_none() && bytes < 1024, "{bytes} bytes: {reply:?}");
    assert_eq!(again, first);
    assert_eq!(picture(&again).1, pixels);
    assert_eq!(frames(&previews), 1, "no new file");

    // 3. The screen changes: a new picture with the new pixels.
    fs::write(desk.join("screen.ppm"), screen(false)).unwrap();
    let (changed, request, reply, _) = watch(&mut vesper, "1152", "648");
    assert_eq!(request.get("previous"), Some(&Value::from(digest.clone())));
    assert!(reply.contains_key("jpeg") && reply["digest"] != Value::from(digest.clone()), "{:?}", reply.keys().collect::<Vec<_>>());
    assert_ne!(changed, first);
    let (size, pixels) = picture(&changed);
    assert!(left_half(size, &pixels).abs_diff(LIGHT as u32 + 3) <= 8, "{}", left_half(size, &pixels));

    // 4. Another shown size: the picture comes again, at that size.
    let (smaller, request, reply, _) = watch(&mut vesper, "640", "360");
    assert_eq!(request.get("previous"), None, "{request:?}");
    assert!(reply.contains_key("jpeg"));
    assert_eq!(picture(&smaller).0, (640, 360));

    // 4. The console closed and released every file: the picture comes again.
    assert!(vesper.ask("preview-release", &[])["error"].is_null());
    assert_eq!(frames(&previews), 0);
    let (reopened, request, reply, _) = watch(&mut vesper, "640", "360");
    assert_eq!(request.get("previous"), None, "{request:?}");
    assert!(reply.contains_key("jpeg"));
    assert_eq!(picture(&reopened).0, (640, 360));
    // And the still screen is not sent again.
    let (still, _, reply, _) = watch(&mut vesper, "640", "360");
    assert_eq!((reply.get("unchanged"), still), (Some(&Value::Bool(true)), reopened));
}
