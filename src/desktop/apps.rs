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
    /// The Hyprland class its windows get, when known.
    pub class: Option<String>,
}

/// Today's catalog: editor (`<release>/ops/ibara-editor`, Mousepad), terminal
/// (`/usr/bin/foot`), the default browser (`/usr/bin/omarchy launch browser`)
/// and the file manager (`/usr/bin/omarchy launch nautilus`, Files).
pub fn default_catalog(release_root: &Path) -> Vec<App> {
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

/// The launch command for `app_id`. A workspace file may only be opened in
/// the editor, and must be an absolute path the controller already resolved
/// inside the task workspace; it is passed after `--`.
pub fn launch_command(catalog: &[App], app_id: &str, workspace_file: Option<&Path>) -> Result<Cmd> {
    let app = catalog
        .iter()
        .find(|a| a.id == app_id)
        .ok_or_else(|| denied("Launch app_id is not in the approved catalog.").with("field", "app"))?;
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
