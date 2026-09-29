//! The local file chooser (ibara-bridge.mjs:843-862).

use super::Ctx;
use super::envelope::{Fault, Handled};
use super::process::{self, Run, which};
use serde_json::json;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const FILE_SELECT: &str = "/usr/share/omarchy/bin/omarchy-file-select";

/// `path.normalize(p) === p` for an absolute path: no `.`, `..` or empty parts
/// (a single trailing slash survives `normalize`, so it is allowed).
fn normalized_absolute(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else { return false };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    path == "/" || (!rest.is_empty() && rest.split('/').all(|part| !part.is_empty() && part != "." && part != ".."))
}

/// `pick-file` / `pick-folder`: the desktop's portal chooser runs as its own
/// process. Choosing nothing (exit 1) is a decision: `picked` is false, no error.
pub async fn pick_local(ctx: &Ctx) -> Handled {
    if !ctx.args.is_empty() {
        return Err(Fault::plain("The file chooser takes no arguments."));
    }
    let folder = ctx.head.command == "pick-folder";
    let chooser = which("omarchy-file-select").or_else(|| Path::new(FILE_SELECT).exists().then(|| PathBuf::from(FILE_SELECT)));
    let Some(chooser) = chooser else {
        let message = "The desktop file chooser (omarchy-file-select) is unavailable.";
        return Ok(ctx.failure("MISSING_DEPENDENCY", message, "missing-dependency", true));
    };
    let mut args = vec!["--title", if folder { "Choose a folder" } else { "Choose a file to send" }];
    if folder {
        args.push("--directory");
    }
    let options = Run { max_output: 64 * 1024, ..Run::timeout(Duration::from_secs(610)) };
    let out = process::run(&chooser, &args, options).await?;
    if out.status == Some(1) {
        return Ok(ctx.ready(json!({"picked": false})));
    }
    if !out.success() {
        let stderr = out.stderr_text();
        return Ok(ctx.failure("CHOOSER_FAILED", if stderr.is_empty() { "The file chooser did not open." } else { &stderr }, "failed", true));
    }
    // Exactly one line: a trailing newline is allowed; any other break fails the control check.
    let stdout = out.stdout_text();
    let chosen = stdout.strip_suffix('\n').unwrap_or(&stdout);
    if !normalized_absolute(chosen) || chosen.chars().any(|c| (c as u32) < 0x20) {
        return Ok(ctx.failure("CHOOSER_FAILED", "The file chooser returned an unusable path.", "failed", true));
    }
    let meta = std::fs::symlink_metadata(chosen)?;
    if (folder && !meta.is_dir()) || (!folder && !meta.is_file()) {
        let message = if folder { "Choose a folder." } else { "Choose one regular file, not a link or folder." };
        return Ok(ctx.failure("INVALID_ARGUMENT", message, "failed", true));
    }
    let path = Path::new(chosen);
    let name = path.components().next_back().map(|c| match c {
        Component::Normal(n) => n.to_string_lossy().into_owned(),
        _ => String::new(),
    });
    let parent = path.parent().map(|p| p.display().to_string()).unwrap_or_else(|| "/".into());
    let mut data = json!({"picked": true, "path": chosen, "name": name.unwrap_or_default(), "folder": parent});
    if !folder {
        data["size"] = json!(meta.len());
    }
    Ok(ctx.ready(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chosen_paths_must_be_normalized() {
        assert!(normalized_absolute("/home/riley/Downloads/report.pdf"));
        assert!(!normalized_absolute("/home/riley/../etc/shadow"));
        assert!(!normalized_absolute("/home//riley/x"));
        assert!(!normalized_absolute("relative/x"));
    }
}
