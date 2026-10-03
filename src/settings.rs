//! Settings: one plain-text file per computer, `~/.config/ibara/settings.toml`
//! (`$XDG_CONFIG_HOME/ibara/settings.toml` when that is set).
//!
//! The target daemon keeps this computer's own settings in one table per
//! section (`[general]`, `[display]`, `[recovery]`, `[power]`, `[control]`, `[agents]`); the console
//! keeps its settings in `[console]`. A key that is absent has its default;
//! removing a line resets it. The file is edited by hand or through the
//! console, and every reader re-reads it when it changes (checked on each
//! use by size, time and inode), so edits apply without a restart. A value
//! that does not fit its setting is ignored and the default applies.
//!
//! Every setting here has an effect; see docs/internals.md, Settings.

use crate::error::{IbaraError, Result, invalid};
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Who owns a setting: this computer (the target daemon) or its console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Computer,
    Console,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Scope::Computer => "computer",
            Scope::Console => "console",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Bool(bool),
    Choice(&'static [(&'static str, &'static str)], &'static str),
    Number { min: i64, max: i64, default: i64 },
    /// Text up to `max` characters; the default comes from the caller when `None`.
    Text { max: usize, default: Option<&'static str> },
}

struct Section {
    id: &'static str,
    title: &'static str,
    scope: Scope,
}

struct Def {
    section: &'static str,
    key: &'static str,
    title: &'static str,
    help: &'static str,
    kind: Kind,
}

const SECTIONS: &[Section] = &[
    Section { id: "general", title: "General", scope: Scope::Computer },
    Section { id: "display", title: "Display", scope: Scope::Computer },
    Section { id: "recovery", title: "Recovery", scope: Scope::Computer },
    Section { id: "power", title: "Power", scope: Scope::Computer },
    Section { id: "control", title: "Screen", scope: Scope::Computer },
    Section { id: "agents", title: "Agents", scope: Scope::Computer },
    Section { id: "approvals", title: "Approvals", scope: Scope::Console },
    Section { id: "notifications", title: "Notifications", scope: Scope::Console },
    Section { id: "updates", title: "Updates", scope: Scope::Console },
    Section { id: "files", title: "Files", scope: Scope::Console },
    Section { id: "fleet", title: "Fleet", scope: Scope::Console },
];

/// Virtual screen sizes for a computer without a screen.
pub const DISPLAY_SIZES: &[(&str, &str)] = &[
    ("1280x720", "1280 × 720"),
    ("1600x900", "1600 × 900"),
    ("1920x1080", "1920 × 1080"),
    ("2560x1440", "2560 × 1440"),
    ("3840x2160", "3840 × 2160"),
];

/// This computer's own choice for Ask before agents send, spend or delete.
const ASK_FIRST: &[(&str, &str)] = &[("same", "Same as in Settings"), ("on", "On"), ("off", "Off")];
const INPUT_BACKENDS: &[(&str, &str)] = &[("auto", "Automatic"), ("plugin", "Cua Plugin"), ("dispatchers", "Compositor Dispatchers")];

/// Kept in this computer's file but not listed with its settings: what ibara's
/// Settings chose for every computer (`fleet_ask_first`), which the console
/// that changed it writes to each computer it may manage. Its own Settings
/// tab shows it as part of `ask_first`, and resetting the section keeps it.
const UNLISTED: &[&str] = &["fleet_ask_first"];

fn listed(def: &Def) -> bool {
    !UNLISTED.contains(&def.key)
}

const DEFS: &[Def] = &[
    Def { section: "control", key: "screen_stream", title: "Screen Stream",
        help: "Automatic uses ibara's stream when graphics can encode it.",
        kind: Kind::Choice(&[("auto", "Automatic"), ("ibara", "ibara"), ("sunshine", "Sunshine")], "auto") },
    Def { section: "control", key: "hand_back_seconds", title: "Hand Back After I Stop For",
        help: "Let agents continue after you stop using the screen.",
        kind: Kind::Choice(&[("3", "3 Seconds"), ("5", "5 Seconds"), ("10", "10 Seconds"), ("30", "30 Seconds"), ("manual", "Only When I Hand Back")], "5") },
    Def {
        section: "updates", key: "check_for_updates", title: "Check for ibara Updates",
        help: "Check for new ibara releases.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "agents",
        key: "input_backend",
        title: "Input Backend",
        help: "Choose how agents use the keyboard and mouse.",
        kind: Kind::Choice(INPUT_BACKENDS, "auto"),
    },
    Def {
        section: "general",
        key: "name",
        title: "Computer Name",
        help: "Name this computer for people and agents.",
        kind: Kind::Text { max: 64, default: None },
    },
    Def {
        section: "display",
        key: "virtual_display_size",
        title: "Virtual Screen Size",
        help: "Set the size of the screen without a monitor.",
        kind: Kind::Choice(DISPLAY_SIZES, "1920x1080"),
    },
    Def {
        section: "display",
        key: "preview_seconds",
        title: "Picture Interval",
        help: "Set the time between screen pictures.",
        kind: Kind::Number { min: 1, max: 30, default: 1 },
    },
    Def {
        section: "recovery",
        key: "auto_resume",
        title: "Resume Agents After a Restart",
        help: "Let agents continue after a restart.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "recovery",
        key: "self_repair",
        title: "Repair Known Problems",
        help: "Fix common screen and agent problems automatically.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "power",
        key: "wake_on_network",
        title: "Wake from the Network",
        help: "Let another computer wake this one.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "control",
        key: "shared_clipboard",
        title: "Shared Clipboard",
        help: "Copy and paste between computers.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "agents",
        key: "ask_first",
        title: "Ask Before Agents Send, Spend or Delete",
        help: "Ask you before agents send, spend or delete.",
        kind: Kind::Choice(ASK_FIRST, "same"),
    },
    Def {
        section: "agents",
        key: "fleet_ask_first",
        title: "Ask Before Agents Send, Spend or Delete, as in Settings",
        help: "Follow the approval choice in Settings.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "approvals",
        key: "agents_ask_first",
        title: "Ask Before Agents Send, Spend or Delete",
        help: "Ask you before agents send, spend or delete.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "notifications",
        key: "notifications",
        title: "Notifications",
        help: "Show alerts for requests and finished tasks.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "notifications",
        key: "approval_notifications",
        title: "Approval Notifications",
        help: "Show approval buttons in desktop alerts.",
        kind: Kind::Bool(true),
    },
    Def {
        section: "files",
        key: "download_folder",
        title: "Download Folder",
        help: "Choose where received files are saved.",
        kind: Kind::Text { max: 1024, default: Some("") },
    },
    Def {
        section: "fleet",
        key: "fleet_preview_seconds",
        title: "Fleet Picture Interval",
        help: "Set the time between fleet pictures.",
        kind: Kind::Number { min: 2, max: 60, default: 5 },
    },
    Def {
        section: "fleet",
        key: "live_video",
        title: "Live Video (Preview)",
        help: "Show live video on the fleet page.",
        kind: Kind::Bool(false),
    },
];

fn scope_of(def: &Def) -> Scope {
    SECTIONS.iter().find(|s| s.id == def.section).map(|s| s.scope).unwrap_or(Scope::Computer)
}

/// The TOML table a setting lives in: its section for this computer's
/// settings, `[console]` for the console's.
fn table_of(def: &Def) -> &'static str {
    match scope_of(def) {
        Scope::Computer => def.section,
        Scope::Console => "console",
    }
}

fn def(key: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.key == key)
}

/// The title a person sees for a setting's key or a section's id.
pub fn title(key: &str) -> Option<&'static str> {
    def(key).map(|d| d.title).or_else(|| SECTIONS.iter().find(|s| s.id == key).map(|s| s.title))
}

/// `~/.config/ibara/settings.toml`, or under `$XDG_CONFIG_HOME` when absolute.
pub fn path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")).join(".config"),
    };
    base.join("ibara").join("settings.toml")
}

const HEADER: &str = "# ibara settings for this computer. Edit by hand if you like: ibara picks up\n\
# changes within a few seconds. Remove a line to go back to its default.\n\
# Every setting is described in the console (Settings) and in ibara's docs/internals.md.\n";

/// What the file says, read once per change of the file.
#[derive(Debug, Clone, Default)]
pub struct Values {
    doc: Option<std::sync::Arc<toml_edit::DocumentMut>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Stamp {
    len: u64,
    mtime_ns: i128,
    ino: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp { len: meta.len(), mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128, ino: meta.ino() })
}

struct Cache {
    path: PathBuf,
    stamp: Option<Stamp>,
    values: Values,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn read_doc(path: &Path) -> Option<toml_edit::DocumentMut> {
    let text = std::fs::read_to_string(path).ok()?;
    match text.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => Some(doc),
        Err(e) => {
            eprintln!("{}", json!({"event": "settings_unreadable", "detail": format!("{}: {}", path.display(), e.message())}));
            None
        }
    }
}

/// The current values: the file is read again only when it changed.
pub fn current() -> Values {
    let path = path();
    let now = stamp(&path);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(c) = cache.as_ref()
        && c.path == path
        && c.stamp == now
    {
        return c.values.clone();
    }
    let values = Values { doc: now.as_ref().and_then(|_| read_doc(&path)).map(std::sync::Arc::new) };
    *cache = Some(Cache { path, stamp: now, values: values.clone() });
    values
}

impl Values {
    fn raw(&self, def: &Def) -> Option<&toml_edit::Item> {
        self.doc.as_ref()?.get(table_of(def))?.as_table_like()?.get(def.key)
    }

    /// The stored value when it fits the setting, else `None` (the default applies).
    fn stored(&self, def: &Def) -> Option<Value> {
        let item = self.raw(def)?;
        match def.kind {
            Kind::Bool(_) => item.as_bool().map(Value::Bool),
            Kind::Choice(choices, _) => item.as_str().filter(|v| choices.iter().any(|(c, _)| c == v)).map(|v| json!(v)),
            Kind::Number { min, max, .. } => item.as_integer().filter(|v| (min..=max).contains(v)).map(|v| json!(v)),
            Kind::Text { max, .. } => item.as_str().filter(|v| valid_text(def, v, max).is_ok()).map(|v| json!(v)),
        }
    }

    /// Whether agents here ask a person before they send, spend or delete:
    /// this computer's own choice, else the one ibara's Settings made for
    /// every computer.
    pub fn ask_first(&self) -> bool {
        match self.text("ask_first").as_deref() {
            Some("on") => true,
            Some("off") => false,
            _ => self.bool("fleet_ask_first"),
        }
    }

    pub fn bool(&self, key: &str) -> bool {
        let Some(def) = def(key) else { return false };
        let Kind::Bool(default) = def.kind else { return false };
        self.stored(def).and_then(|v| v.as_bool()).unwrap_or(default)
    }

    pub fn number(&self, key: &str) -> i64 {
        let Some(def) = def(key) else { return 0 };
        let Kind::Number { default, .. } = def.kind else { return 0 };
        self.stored(def).and_then(|v| v.as_i64()).unwrap_or(default)
    }

    /// A choice or text; `None` when unset (text with a caller default).
    pub fn text(&self, key: &str) -> Option<String> {
        let def = def(key)?;
        let stored = self.stored(def).and_then(|v| v.as_str().map(str::to_string));
        match def.kind {
            Kind::Choice(_, default) => Some(stored.unwrap_or_else(|| default.to_string())),
            Kind::Text { default, .. } => stored.or_else(|| default.map(str::to_string)),
            _ => None,
        }
    }
}

fn valid_text(def: &Def, text: &str, max: usize) -> Result<()> {
    if text.chars().count() > max || text.chars().any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')) {
        return Err(invalid(format!("{} must be one line of at most {max} characters.", def.title)));
    }
    if def.key == "name" && text.trim().is_empty() {
        return Err(invalid("A computer name cannot be empty."));
    }
    if def.key == "download_folder" && !text.is_empty() && !Path::new(text).is_absolute() {
        return Err(invalid("The download folder must be a full path, such as /home/you/Downloads."));
    }
    Ok(())
}

/// The value a person typed, as the setting's TOML value.
fn parse(def: &Def, text: &str) -> Result<toml_edit::Value> {
    Ok(match def.kind {
        Kind::Bool(_) => match text {
            "true" | "on" => true.into(),
            "false" | "off" => false.into(),
            _ => return Err(invalid(format!("{} is on or off: use true or false.", def.title))),
        },
        Kind::Choice(choices, _) => {
            if !choices.iter().any(|(c, _)| *c == text) {
                let names: Vec<_> = choices.iter().map(|(c, _)| *c).collect();
                return Err(invalid(format!("{} is one of {}.", def.title, names.join(", "))));
            }
            text.into()
        }
        Kind::Number { min, max, .. } => match text.trim().parse::<i64>() {
            Ok(n) if (min..=max).contains(&n) => n.into(),
            _ => return Err(invalid(format!("{} is a whole number from {min} to {max}.", def.title))),
        },
        Kind::Text { max, .. } => {
            let text = if def.key == "name" { text.trim() } else { text };
            valid_text(def, text, max)?;
            text.into()
        }
    })
}

/// Defaults that depend on the computer (its usual name).
#[derive(Debug, Clone, Default)]
pub struct Defaults {
    pub name: String,
}

fn default_of(def: &Def, defaults: &Defaults) -> Value {
    match def.kind {
        Kind::Bool(b) => json!(b),
        Kind::Choice(_, d) => json!(d),
        Kind::Number { default, .. } => json!(default),
        Kind::Text { default: Some(d), .. } => json!(d),
        Kind::Text { default: None, .. } => json!(defaults.name),
    }
}

fn setting_json(def: &Def, values: &Values, defaults: &Defaults) -> Value {
    let default = default_of(def, defaults);
    let value = values.stored(def).unwrap_or_else(|| default.clone());
    let mut out = json!({
        "key": def.key, "title": def.title, "help": def.help,
        "type": match def.kind { Kind::Bool(_) => "bool", Kind::Choice(..) => "choice", Kind::Number { .. } => "number", Kind::Text { .. } => "text" },
        "value": value, "default": default, "scope": scope_of(def).name(),
    });
    let details = match def.key {
        "input_backend" => "Automatic uses dispatchers on Hypoland and the Cua plugin on Hyprland. Dispatchers have reduced input safety when the plugin is unavailable.",
        "ask_first" => "Same as in Settings follows your console's approval choice. Turning approvals off applies to your own agents. Other people's agents still ask unless you choose Always Allow. Access rules still apply.",
        "agents_ask_first" => "Turning this off lets your own agents send, spend and delete on computers that follow Settings. Other people's agents still ask. Denied access and permissions to administer or join stay in place.",
        _ => "",
    };
    if !details.is_empty() {
        out["details"] = json!(details);
    }
    match def.kind {
        Kind::Choice(choices, _) => {
            // "Same as in Settings" says what that is here, as last set.
            let fleet = if values.bool("fleet_ask_first") { "on" } else { "off" };
            let label = |v: &str, l: &str| if def.key == "ask_first" && v == "same" { format!("{l} ({fleet})") } else { l.to_string() };
            out["choices"] = choices.iter().map(|(v, l)| json!({"value": v, "label": label(v, l)})).collect();
        }
        Kind::Number { min, max, .. } => {
            out["min"] = json!(min);
            out["max"] = json!(max);
        }
        _ => {}
    }
    out
}

fn section_json(section: &Section, values: &Values, defaults: &Defaults) -> Value {
    let settings: Vec<Value> = DEFS.iter().filter(|d| d.section == section.id && listed(d)).map(|d| setting_json(d, values, defaults)).collect();
    json!({"id": section.id, "title": section.title, "settings": settings})
}

/// `get`: every section of `scope` with its current values.
pub fn get(scope: Scope, defaults: &Defaults) -> Value {
    let values = current();
    let sections: Vec<Value> = SECTIONS.iter().filter(|s| s.scope == scope).map(|s| section_json(s, &values, defaults)).collect();
    json!({"sections": sections})
}

fn scoped_def(scope: Scope, key: &str) -> Result<&'static Def> {
    def(key).filter(|d| scope_of(d) == scope).ok_or_else(|| invalid(format!("There is no setting called {key}.")))
}

/// Serializes read-modify-write between the target daemon and the console,
/// which share the file on a computer that is both.
struct FileLock {
    _held: std::fs::File,
}

impl FileLock {
    fn take(dir: &Path) -> Result<FileLock> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(".settings.lock"))?;
        // SAFETY: flock on a descriptor we own; released when the file closes.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(FileLock { _held: file })
    }
}

/// Change the file: `edit` gets the parsed document (a fresh one with the
/// header when there is none). Comments and unrelated keys stay as written.
fn edit(edit: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<()> {
    let path = path();
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    if !dir.exists() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    }
    let _lock = FileLock::take(&dir)?;
    let mut doc = match std::fs::read_to_string(&path) {
        Ok(text) => text.parse::<toml_edit::DocumentMut>().map_err(|e| {
            IbaraError::new(
                "CAPABILITY_UNAVAILABLE",
                format!("{} is not valid TOML ({}). Fix or remove it, then try again.", path.display(), e.message().trim()),
                false,
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HEADER.parse::<toml_edit::DocumentMut>().expect("header parses"),
        Err(e) => return Err(e.into()),
    };
    edit(&mut doc);
    crate::operator::replace_file(&path, doc.to_string().as_bytes(), 0o600)?;
    Ok(())
}

/// `set KEY VALUE`: the updated setting.
pub fn set(scope: Scope, key: &str, text: &str, defaults: &Defaults) -> Result<Value> {
    let def = scoped_def(scope, key)?;
    let value = parse(def, text)?;
    edit(|doc| {
        let table = table_of(def);
        if doc.get(table).and_then(|t| t.as_table_like()).is_none() {
            doc.insert(table, toml_edit::table());
        }
        if let Some(t) = doc.get_mut(table).and_then(|t| t.as_table_like_mut()) {
            t.insert(def.key, toml_edit::Item::Value(value));
        }
    })?;
    Ok(setting_json(def, &current(), defaults))
}

/// `reset KEY`: the setting back at its default.
pub fn reset(scope: Scope, key: &str, defaults: &Defaults) -> Result<Value> {
    let def = scoped_def(scope, key)?;
    remove(&[def])?;
    Ok(setting_json(def, &current(), defaults))
}

/// `reset --section ID`: every setting of the section back at its default.
pub fn reset_section(scope: Scope, id: &str, defaults: &Defaults) -> Result<Value> {
    let section = SECTIONS
        .iter()
        .find(|s| s.id == id && s.scope == scope)
        .ok_or_else(|| invalid(format!("There is no settings section called {id}.")))?;
    let defs: Vec<&Def> = DEFS.iter().filter(|d| d.section == section.id && listed(d)).collect();
    remove(&defs)?;
    Ok(json!({"sections": [section_json(section, &current(), defaults)]}))
}

fn remove(defs: &[&Def]) -> Result<()> {
    if !path().exists() {
        return Ok(());
    }
    edit(|doc| {
        for def in defs {
            let table = table_of(def);
            let empty = match doc.get_mut(table).and_then(|t| t.as_table_like_mut()) {
                Some(t) => {
                    t.remove(def.key);
                    t.is_empty()
                }
                None => false,
            };
            if empty {
                doc.remove(table);
            }
        }
    })
}

/// `settings get|set KEY VALUE|reset KEY|reset --section ID` for one scope,
/// as the console and the operator operation both take it.
pub fn command(scope: Scope, args: &[&str], defaults: &Defaults) -> Result<Value> {
    match args {
        ["get"] => Ok(get(scope, defaults)),
        ["set", key, value] => set(scope, key, value, defaults),
        ["reset", "--section", id] => reset_section(scope, id, defaults),
        ["reset", key] => reset(scope, key, defaults),
        _ => Err(invalid("Use get, set KEY VALUE, reset KEY or reset --section ID.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(text: &str) -> Values {
        Values { doc: Some(std::sync::Arc::new(text.parse().expect("toml"))) }
    }

    /// Ask before agents send, spend or delete on a computer. Failure cases:
    /// 1. this computer's own choice does not beat the one from Settings, or
    ///    the one from Settings is ignored when it has none;
    /// 2. a value that does not fit turns approvals off;
    /// 3. the computer's Settings tab lists the choice from Settings as a
    ///    setting of its own, or does not say what "Same as in Settings" is.
    #[test]
    fn this_computers_choice_comes_before_the_one_from_settings() {
        let cases = [
            ("", true),
            ("[agents]\nfleet_ask_first = false\n", false),
            ("[agents]\nfleet_ask_first = false\nask_first = \"on\"\n", true),
            ("[agents]\nask_first = \"off\"\n", false),
            ("[agents]\nfleet_ask_first = false\nask_first = \"same\"\n", false),
            ("[agents]\nask_first = \"maybe\"\nfleet_ask_first = \"no\"\n", true),
        ];
        for (text, asks) in cases {
            assert_eq!(values(text).ask_first(), asks, "{text:?}");
        }
        let section = SECTIONS.iter().find(|s| s.id == "agents").unwrap();
        let listed = section_json(section, &values("[agents]\nfleet_ask_first = false\n"), &Defaults::default());
        let keys: Vec<&str> = listed["settings"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
        assert!(!keys.contains(&"fleet_ask_first"));
        let ask = listed["settings"].as_array().unwrap().iter().find(|s| s["key"] == "ask_first").unwrap();
        assert_eq!(ask["choices"][0], json!({"value": "same", "label": "Same as in Settings (off)"}));
    }
}
