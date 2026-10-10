//! The approved app catalog. Only these can be
//! launched; anything else is `PERMISSION_DENIED`.

use super::run::Cmd;
use crate::error::{Result, denied};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The name agents use, e.g. `editor`.
    pub id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The compositor class its windows get, when known.
    pub class: Option<String>,
}

/// Today's catalog: editor (`<release>/ops/ibara-editor`, Mousepad), terminal
/// (`/usr/bin/foot`), the default browser (`/usr/bin/omarchy launch browser`)
/// and the file manager (`/usr/bin/omarchy launch nautilus`, Files).
pub fn default_catalog(release_root: &Path) -> Vec<App> {
    if super::gnome::Gnome::selected(&std::env::vars_os().collect::<Vec<_>>()) {
        return vec![
            App {id:"editor".into(),program:"/usr/bin/gnome-text-editor".into(),
                args:vec!["--standalone".into(),"--new-window".into()],class:Some("org.gnome.TextEditor".into())},
            App {id:"terminal".into(),program:"/usr/bin/ptyxis".into(),
                args:vec!["--standalone".into()],class:Some("org.gnome.Ptyxis".into())},
            gnome_browser(),
            App {id:"files".into(),program:"/usr/bin/nautilus".into(),
                args:vec!["--new-window".into()],class:Some("org.gnome.Nautilus".into())},
        ];
    }
    vec![
        App {
            id: "editor".into(),
            program: release_root.join("ops/ibara-editor"),
            args: Vec::new(),
            class: Some("mousepad".into()),
        },
        App { id: "terminal".into(), program: "/usr/bin/foot".into(), args: Vec::new(), class: Some("foot".into()) },
        App {
            id: "browser".into(),
            program: "/usr/bin/omarchy".into(),
            args: vec!["launch".into(), "browser".into()],
            class: None,
        },
        App {
            id: "files".into(),
            program: "/usr/bin/omarchy".into(),
            args: vec!["launch".into(), "nautilus".into()],
            class: Some("org.gnome.Nautilus".into()),
        },
    ]
}

/// Native packages with the system policy/native-host route. Do not redirect
/// the person's default browser or silently select Ubuntu's snap wrapper.
pub fn gnome_browser() -> App {
    if Path::new("/usr/bin/google-chrome-stable").is_file() {
        App { id:"browser".into(), program:"/usr/bin/google-chrome-stable".into(),
            args:vec!["--new-window".into(),"about:blank".into()], class:Some("google-chrome".into()) }
    } else {
        App { id:"browser".into(), program:"/usr/lib/chromium/chromium".into(),
            args:vec!["--new-window".into(),"about:blank".into()], class:Some("chromium".into()) }
    }
}

pub fn gnome_browser_reader_installed() -> bool {
    let browser = gnome_browser();
    let root = if browser.program == Path::new("/usr/bin/google-chrome-stable") {
        Path::new("/etc/opt/chrome")
    } else { Path::new("/etc/chromium") };
    browser.program.is_file() && root.join("policies/managed/ibara.json").is_file()
        && root.join("native-messaging-hosts/io.ibara.chrome.json").is_file()
}

/// The launch command for `app_id`. A workspace file may only be opened in
/// the editor, and must be an absolute path the controller already resolved
/// inside the task workspace; it is passed after `--`.
pub fn launch_command(catalog: &[App], app_id: &str, workspace_file: Option<&Path>) -> Result<Cmd> {
    let app = catalog
        .iter()
        .find(|a| a.id == app_id)
        .ok_or_else(|| denied("Launch app_id is not in the approved catalog.").with("field", "app"))?;
    if super::gnome::Gnome::selected(&std::env::vars_os().collect::<Vec<_>>()) && !app.program.is_file() {
        return Err(crate::error::IbaraError::new("CAPABILITY_UNAVAILABLE",
            format!("Approved {} application is not installed: {}",app.id,app.program.display()),true)
            .with("execution_not_started",true));
    }
    let mut cmd = Cmd::new(&app.program).args(&app.args);
    if let Some(file) = workspace_file {
        if app.id != "editor" || !file.is_absolute() {
            return Err(denied("Editor file launch requires a controller-resolved task workspace file."));
        }
        cmd = cmd.arg("--").arg(file);
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_catalog_apps_launch_and_only_the_editor_opens_files() {
        let catalog = default_catalog(Path::new("/opt/agent-computer/current"));
        assert_eq!(launch_command(&catalog, "shell", None).unwrap_err().code, "PERMISSION_DENIED");
        assert!(launch_command(&catalog, "editor", Some(Path::new("/w/task/notes.txt"))).is_ok());
        assert!(launch_command(&catalog, "editor", Some(Path::new("notes.txt"))).is_err());
        assert!(launch_command(&catalog, "terminal", Some(Path::new("/w/task/notes.txt"))).is_err());
    }
}
