//! First run on this computer: the prompt that connects an agent, and the
//! milestones the console shows.
//!
//! An agent connects itself. The person copies [`CONNECT_PROMPT`] from the
//! console's Connect an Agent card (`connect-prompt`), or prints it with
//! `ibara prompt`, and pastes it to any agent program. The agent adds
//! `ibara mcp` to its own settings, links the packaged skill
//! (`/usr/share/ibara/skills/ibara`, from core `skills/ibara/SKILL.md`) into
//! its skills folder, and keeps a short marked block in its user-level
//! instructions so every session knows when to reach for ibara. ibara writes
//! no agent program's files itself.
//!
//! `onboarding.json` in the operator state directory holds the milestones.
//! Today one: `first_task_done`, set by `ibara mcp` the first time any agent
//! begins a task through this computer.
//!
//! ```json
//! {"first_task_done": true, "first_task_at": "2026-09-26T12:00:00Z"}
//! ```

use super::directory::operator_state_dir;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// What a person pastes to an agent so that it connects itself (the only copy
/// of its text). `ibara mcp` with no computer reaches every added computer.
/// The block's markers let a second connect replace it instead of adding one.
pub const CONNECT_PROMPT: &str = concat!(
    "Connect yourself to ibara, which is installed on this computer. Do these three things:\n\n",
    "1. Add its MCP server, the command `ibara mcp` (stdio, no arguments), to your own MCP settings as a user-level server named `ibara`, keeping all your other settings. If a server named `ibara` already exists, replace it.\n",
    "2. Link the folder /usr/share/ibara/skills/ibara into your user-level skills folder as `ibara`, so updates reach you. If your harness has no skills folder, skip this; step 3 names the file. If an `ibara` skill is already there and is not that link, leave it and tell me.\n",
    "3. Add this block to your user-level instructions file (AGENTS.md, CLAUDE.md or your harness's equivalent). If a block with the same markers is there, replace it:\n\n",
    "<!-- ibara:start -->\n",
    "## ibara\n",
    "The MCP server `ibara` lets you use real computers your person owns: desktop apps, a signed-in browser, the screen and files. When a task needs real input, a real browser session, a visual check or work on another computer, call ibara's `computer_status` and follow the ibara skill (/usr/share/ibara/skills/ibara/SKILL.md). Keep code, git, tests and pages you can fetch where you are.\n",
    "<!-- ibara:end -->\n\n",
    "Then call ibara's `computer_status` tool. If its tools won't appear until you restart, say so and ask me to restart you and paste this prompt again; doing it twice is safe. Tell me which computers you can use and which of the three steps you did."
);

/// The console's `connect-prompt`.
pub fn connect_prompt_json() -> Value {
    json!({"prompt": CONNECT_PROMPT, "first_task_done": first_task_done(&path())})
}

/// `ibara prompt`: print the prompt, for a computer without the console.
pub fn prompt_main(args: Vec<String>) -> ExitCode {
    if !args.is_empty() {
        eprintln!("Usage: ibara prompt");
        return ExitCode::from(64);
    }
    println!("{CONNECT_PROMPT}");
    ExitCode::SUCCESS
}

/// `~/.local/state/ibara/onboarding.json` (or under `$XDG_STATE_HOME`).
pub fn path() -> PathBuf {
    operator_state_dir().join("onboarding.json")
}

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// Whether an agent has begun a task through this computer.
pub fn first_task_done(path: &Path) -> bool {
    read(path).get("first_task_done").and_then(Value::as_bool) == Some(true)
}

/// Record the first task begun; later tasks leave the file alone. Other keys are kept.
pub fn record_first_task(path: &Path) -> std::io::Result<()> {
    let mut state = read(path);
    if state.get("first_task_done").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    state.insert("first_task_done".into(), json!(true));
    state.insert("first_task_at".into(), json!(crate::ids::now_iso()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(state)).unwrap_or_default();
    text.push('\n');
    super::replace_file(path, text.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_task_is_recorded_once_and_other_keys_survive() {
        let dir = super::super::directory::tests::TempDir::new("onboarding");
        let file = dir.0.join("state/onboarding.json");
        assert!(!first_task_done(&file), "no file means no task yet");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, r#"{"welcome_seen": true, "first_task_done": false}"#).unwrap();
        assert!(!first_task_done(&file));
        record_first_task(&file).unwrap();
        assert!(first_task_done(&file));
        let first: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(first["welcome_seen"], true, "unrelated milestones are kept");
        record_first_task(&file).unwrap();
        let again: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(first["first_task_at"], again["first_task_at"], "the first time is not overwritten");
    }
}
