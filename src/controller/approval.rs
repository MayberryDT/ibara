//! What a person reads when an agent's step waits for their approval: who
//! asks, what the step does in plain words, in which app and on which page or
//! window, why it asks first, and the task it is for. The structured request
//! (the step, its exact target, the window's address and process) goes with
//! it separately as details, for a Details view and for agents; the words
//! never carry JSON, window addresses, process ids or tool names.

use super::squash;
use serde_json::{Value, json};

/// A task goal longer than this is left to the details.
const GOAL_MAX: usize = 120;

/// Where a step acts: the app, and the page (host and path) or the window's title.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Place {
    pub app: String,
    pub page: Option<String>,
    pub title: Option<String>,
}

impl Place {
    /// A window of `class` titled `title`, showing `url` when it is a browser page.
    pub fn of_window(class: &str, title: &str, url: Option<&str>) -> Place {
        let app = app_name(class);
        let title = window_title(title, &app);
        Place { page: url.and_then(page_words), title, app }
    }

    fn words(&self) -> String {
        match (&self.page, &self.title) {
            (Some(page), _) => format!("in {} on {page}", self.app),
            (None, Some(title)) => format!("in {} (“{title}”)", self.app),
            (None, None) => format!("in {}", self.app),
        }
    }

    pub fn details(&self) -> Value {
        json!({ "app": self.app, "page": self.page, "window_title": self.title })
    }
}

/// A held request, as the agent's step describes it to a person.
pub(crate) struct Ask {
    /// What it does, as the end of "… wants to": "press Return".
    pub doing: String,
    pub place: Option<Place>,
    /// The effect class it asks first for.
    pub class: &'static str,
    /// `doing` already says why it asks first ("send the file …").
    pub says_why: bool,
    /// An approved step's target changed since, so it asks again.
    pub changed: bool,
    /// A person refused the same step last time (agent/guard.rs), so it asks
    /// whatever its class's rule.
    pub again: bool,
}

/// The sentence a person reads: "codex@vesper wants to press Return in
/// Chromium on shop.example/checkout, which sends something. For the task
/// “Buy the blue mug”."
pub(crate) fn summary(agent: &str, goal: Option<&str>, ask: &Ask) -> String {
    let mut text = format!("{agent} wants to {}", ask.doing);
    if let Some(place) = &ask.place {
        text.push(' ');
        text.push_str(&place.words());
    }
    if let Some(why) = purpose(ask.class).filter(|_| !ask.says_why) {
        text.push_str(", ");
        text.push_str(why);
    }
    text.push('.');
    if ask.changed {
        text.push_str(" What it acts on changed since you approved it, so it asks again.");
    }
    if let Some(goal) = goal.map(|g| squash(g, GOAL_MAX + 1)).filter(|g| !g.is_empty() && g.chars().count() <= GOAL_MAX) {
        text.push_str(&format!(" For the task “{}”.", goal.trim_end_matches('.')));
    }
    text
}

/// Why a step of this effect class asks first, as the end of the sentence.
fn purpose(class: &str) -> Option<&'static str> {
    match class {
        "send" => Some("which sends something"),
        "spend" => Some("which spends money"),
        "destructive" => Some("which deletes or overwrites something"),
        _ => None,
    }
}

/// An app's name as a person knows it, from its window class:
/// `org.xfce.mousepad` is Mousepad.
pub(crate) fn app_name(class: &str) -> String {
    let lower = class.to_ascii_lowercase();
    let last = lower.rsplit('.').next().unwrap_or(&lower);
    let known = match last {
        "chromium" | "chromium-browser" => Some("Chromium"),
        "google-chrome" | "google-chrome-stable" | "chrome" => Some("Google Chrome"),
        "mousepad" => Some("Mousepad"),
        "foot" | "footclient" => Some("Foot"),
        "firefox" => Some("Firefox"),
        "nautilus" => Some("Files"),
        _ => None,
    };
    if let Some(name) = known {
        return name.into();
    }
    let words = class.rsplit('.').next().unwrap_or(class).replace(['-', '_'], " ");
    let words = squash(&words, 40);
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "an app".into(),
    }
}

/// A window's title without the app's name its title ends in
/// ("Untitled 1 - Mousepad" is "Untitled 1").
fn window_title(title: &str, app: &str) -> Option<String> {
    let title = squash(title, 200);
    let cut = [" - ", " — ", " – "].iter().find_map(|sep| {
        let suffix = format!("{sep}{app}");
        let start = title.len().checked_sub(suffix.len())?;
        (title.is_char_boundary(start) && title[start..].eq_ignore_ascii_case(&suffix)).then_some(start)
    });
    let title = match cut {
        Some(end) => title[..end].trim().to_string(),
        None => title,
    };
    let title = squash(&title, 60);
    (!title.is_empty()).then_some(title)
}

/// A web address as a person reads it: its host and path, without the scheme,
/// sign-in, query or fragment ("localhost:8080/signup"). None for an address
/// that is not http or https.
pub(crate) fn page_words(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.is_empty() {
        return None;
    }
    let path = path.trim_end_matches('/');
    let words = if path.is_empty() { host.to_string() } else { format!("{host}/{path}") };
    Some(squash(&words, 80))
}

/// A key combination as a person reads it: "ctrl+s" is "Ctrl+S".
pub(crate) fn key_words(combo: &str) -> String {
    combo
        .split('+')
        .map(|key| {
            let key = key.trim();
            match key.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => "Ctrl".to_string(),
                "alt" => "Alt".into(),
                "shift" => "Shift".into(),
                "super" | "meta" | "cmd" | "win" => "Super".into(),
                "esc" => "Escape".into(),
                _ if key.chars().count() == 1 => key.to_uppercase(),
                _ => {
                    let spaced = key.replace('_', " ");
                    let mut chars = spaced.chars();
                    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
                }
            }
        })
        .collect::<Vec<String>>()
        .join("+")
}

/// An element as a person reads it: "the “Send” button".
pub(crate) fn element_words(role: &str, name: &str) -> String {
    let kind = match role.to_ascii_lowercase().as_str() {
        "button" | "push button" | "toggle button" => Some("button"),
        "link" => Some("link"),
        "menu item" | "menuitem" | "check menu item" | "radio menu item" => Some("menu item"),
        "check box" | "checkbox" => Some("checkbox"),
        "radio button" | "radio" => Some("option"),
        "tab" | "page tab" => Some("tab"),
        "combo box" | "combobox" | "listbox" | "list box" => Some("list"),
        "text" | "entry" | "textbox" | "searchbox" | "password text" | "text field" | "textarea" | "spin button" => Some("field"),
        _ => None,
    };
    let name = squash(name, 40);
    match (name.is_empty(), kind) {
        (false, Some(kind)) => format!("the “{name}” {kind}"),
        (false, None) => format!("“{name}”"),
        (true, Some(kind)) => format!("a {kind}"),
        (true, None) => "something on the screen".into(),
    }
}

/// A window as the object of a sentence: "the Mousepad window “Untitled 1”".
pub(crate) fn window_words(class: &str, title: &str) -> String {
    let app = app_name(class);
    match window_title(title, &app) {
        Some(title) => format!("the {app} window “{title}”"),
        None => format!("the {app} window"),
    }
}

/// "1 character", "14 characters".
pub(crate) fn characters(count: usize) -> String {
    if count == 1 { "1 character".into() } else { format!("{count} characters") }
}

/// A path as a person reads it, shortened from the left when long.
pub(crate) fn path_words(path: &str) -> String {
    let path = path.trim();
    let count = path.chars().count();
    if count <= 60 {
        return path.to_string();
    }
    let tail: String = path.chars().skip(count - 59).collect();
    format!("…{tail}")
}

/// A command line as a person reads it: its words, quoted where one has a space.
pub(crate) fn command_words(command: &[String]) -> String {
    let words: Vec<String> = command.iter().map(|w| if w.contains(char::is_whitespace) || w.is_empty() { format!("'{w}'") } else { w.clone() }).collect();
    squash(&words.join(" "), 100)
}

#[cfg(test)]
mod tests {
    //! Failure cases for the words themselves (the whole sentence for each
    //! kind of step is tested through the controller):
    //! 1. A page's query or sign-in reaches the words (it can carry what was typed).
    //! 2. A window's title repeats the app's name ("in Mousepad (“Untitled 1 - Mousepad”)").
    //! 3. An app a person does not know by its class reads as the class
    //!    ("org.xfce.mousepad").
    //! 4. A long goal floods the sentence instead of being left to the details.
    use super::*;

    #[test]
    fn a_page_reads_as_its_host_and_path_only() {
        assert_eq!(page_words("http://localhost:8080/signup?email=a%40b.c#top").as_deref(), Some("localhost:8080/signup"));
        assert_eq!(page_words("https://ada:secret@shop.example/").as_deref(), Some("shop.example"));
        assert_eq!(page_words("chrome://newtab/"), None);
        assert_eq!(page_words("about:blank"), None);
    }

    #[test]
    fn a_window_reads_as_its_app_and_title() {
        assert_eq!(Place::of_window("org.xfce.mousepad", "Untitled 1 - Mousepad", None).words(), "in Mousepad (“Untitled 1”)");
        assert_eq!(Place::of_window("chromium", "Sign up - Chromium", Some("http://localhost/signup?x=1")).words(), "in Chromium on localhost/signup");
        assert_eq!(Place::of_window("com.mitchellh.ghostty", "", None).words(), "in Ghostty");
        assert_eq!(window_words("foot", "~/notes"), "the Foot window “~/notes”");
    }

    #[test]
    fn a_long_goal_is_left_to_the_details() {
        let ask = Ask { doing: "press Return".into(), place: None, class: "send", says_why: false, changed: false, again: false };
        assert_eq!(summary("codex@vesper", Some("Sign up"), &ask), "codex@vesper wants to press Return, which sends something. For the task “Sign up”.");
        assert_eq!(summary("codex@vesper", Some(&"word ".repeat(40)), &ask), "codex@vesper wants to press Return, which sends something.");
    }
}
