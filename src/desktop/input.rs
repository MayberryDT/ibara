//! Pointer buttons, key names and typing rules. Input itself goes through Cua
//! ([`super::cua`]); this module turns ibara's vocabulary (`ctrl+s`, `Return`)
//! into Cua's key names and splits text into pieces.

use crate::error::{Result, invalid};
use std::time::Duration;

/// Longest text one `type` call accepts, in Unicode code points.
pub const MAX_TYPE_CODEPOINTS: usize = 6000;
/// Apps take typed text in asynchronously: a GTK file chooser handles the
/// path it was given before it acts on Return, and drops a Return that comes
/// within about 100 ms (measured on Tulip1, Mousepad 0.7). A key or a button
/// press sent straight after typing therefore waits until this long after
/// the typing ended.
pub const AFTER_TYPING: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left = 0,
    Right = 1,
    Middle = 2,
}

impl Button {
    pub fn parse(name: &str) -> Result<Button> {
        match name {
            "left" => Ok(Button::Left),
            "right" => Ok(Button::Right),
            "middle" => Ok(Button::Middle),
            _ => Err(invalid(format!("Unsupported button: {name}.")).with("field", "button")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        }
    }
}

/// Where the agent's named cursor goes before text is typed. Keys never move
/// it, and nothing typed moves the real pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypingCursor {
    /// To the window's only editable text element, when Cua proves its box
    /// ([`super::cua::Cua::to_field`]).
    ToField,
    /// Nowhere: the step's click already put it on the field (a page field
    /// the page reader located), or only the page knows where the text goes.
    Stays,
}

const MODIFIERS: &[&str] = &["ctrl", "control", "shift", "alt", "meta", "super", "win", "logo"];

/// ibara's key name as Cua's Hyprland route names it. Keys without an evdev
/// mapping there (punctuation such as minus or equal, Insert, keypad, Menu)
/// are refused here, before dispatch: Cua 0.28 refuses them as
/// `foreground_unavailable` (Tulip1, 26 September).
pub fn cua_key(name: &str) -> Result<String> {
    let key = name.trim().to_ascii_lowercase();
    let named = match key.as_str() {
        "enter" | "return" => "Return",
        "tab" => "Tab",
        "space" => "space",
        "esc" | "escape" => "Escape",
        "backspace" => "BackSpace",
        "delete" => "Delete",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "Page_Up",
        "pagedown" | "page_down" => "Page_Down",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "ctrl" | "control" => "ctrl",
        "shift" => "shift",
        "alt" => "alt",
        "meta" | "super" | "win" | "logo" => "super",
        _ => "",
    };
    if !named.is_empty() {
        return Ok(named.to_string());
    }
    if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=12).contains(&n)
    {
        return Ok(format!("F{n}"));
    }
    if key.len() == 1 && key.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        return Ok(key);
    }
    Err(invalid(format!(
        "Unsupported key name: {name}. Use letters, digits, Return, Tab, Escape, BackSpace, Delete, arrows, Home, End, PageUp, PageDown, F1-F12 and modifiers."
    ))
    .with("field", "keys"))
}

/// A chord such as `"ctrl+s"` as Cua key names, modifiers first. At most five
/// keys of at most 32 characters, and at least one that is not a modifier.
pub fn cua_keys(combo: &str) -> Result<Vec<String>> {
    let keys: Vec<&str> = combo.split('+').map(str::trim).collect();
    if combo.trim().is_empty() || keys.iter().any(|k| k.is_empty()) {
        return Err(invalid("Key action requires keys, for example \"ctrl+s\".").with("field", "keys"));
    }
    if keys.len() > 5 || keys.iter().any(|k| k.chars().count() > 32) {
        return Err(invalid("A key chord has at most five keys of at most 32 characters.").with("field", "keys"));
    }
    let is_modifier = |k: &&str| MODIFIERS.contains(&k.to_ascii_lowercase().as_str());
    if keys.iter().all(is_modifier) {
        return Err(invalid("Key chord needs a non-modifier key.").with("field", "keys"));
    }
    if keys.iter().filter(|k| !is_modifier(k)).count() > 1 {
        return Err(invalid("A key chord has modifiers and one other key.").with("field", "keys"));
    }
    let (mods, rest): (Vec<&str>, Vec<&str>) = keys.into_iter().partition(is_modifier);
    mods.into_iter().chain(rest).map(cua_key).collect()
}

/// Validate text for one typing call.
pub fn check_text(text: &str) -> Result<()> {
    if text.chars().count() > MAX_TYPE_CODEPOINTS || text.contains('\0') {
        return Err(invalid(format!(
            "Native typing accepts at most {MAX_TYPE_CODEPOINTS} Unicode code points per call and no NUL; split longer text into explicit batches."
        ))
        .with("field", "text")
        .with("execution_not_started", true));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_need_one_real_key_and_stay_small() {
        assert_eq!(cua_keys("ctrl+shift+s").unwrap(), vec!["ctrl", "shift", "s"]);
        assert_eq!(cua_keys("s+ctrl").unwrap(), vec!["ctrl", "s"], "modifiers go first");
        assert!(cua_keys("ctrl+shift").is_err());
        assert!(cua_keys("").is_err());
        assert!(cua_keys("ctrl+").is_err());
        assert!(cua_keys("a+b+c+d+e+f").is_err());
        assert!(cua_keys("insert").is_err(), "no evdev mapping on Cua's route");
    }
}
