//! The two concrete package environments Ibara installs on. Unsupported hosts
//! fail before lifecycle effects, rather than guessing a manager from PATH.
use super::{interactive, run};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind { Arch, Ubuntu }

impl Kind {
    pub fn host() -> Result<Self, String> {
        let release = std::fs::read_to_string("/etc/os-release").map_err(|_| "Cannot identify this operating system.")?;
        let field = |name: &str| release.lines().find_map(|line| line.split_once('=').filter(|(key, _)| *key == name).map(|(_, value)| value.trim_matches('"'))).unwrap_or("");
        match (field("ID"), field("VERSION_ID"), std::env::consts::ARCH) {
            ("arch", _, "x86_64") => Ok(Self::Arch),
            ("ubuntu", "26.04", "x86_64") => Ok(Self::Ubuntu),
            _ => Err("Package lifecycle supports Arch x86_64 and Ubuntu 26.04 amd64 only.".into()),
        }
    }

    pub fn manifest(self) -> &'static str {
        match self { Self::Arch => "stable.json", Self::Ubuntu if std::path::Path::new(super::LIB).join("gnome-target.json").exists() => "stable-ubuntu-26.04-amd64-target.json", Self::Ubuntu => "stable-ubuntu-26.04-amd64-operator.json" }
    }

    pub fn version(self, name: &str) -> Option<String> {
        match self {
            Self::Arch => run("pacman", &["-Q", name]).ok()?.split_whitespace().nth(1).map(str::to_owned),
            Self::Ubuntu => {
                let answer = run("dpkg-query", &["-W", "-f=${db:Status-Status} ${Version}", name]).ok()?;
                let (status, version) = answer.trim().split_once(' ')?;
                (status == "installed").then(|| version.to_owned())
            }
        }
    }

    pub fn compare(self, a: &str, b: &str) -> Result<i32, String> {
        match self {
            Self::Arch => run("vercmp", &[a, b])?.trim().parse().map_err(|_| "vercmp gave no answer.".into()),
            Self::Ubuntu => {
                for (op, result) in [("lt", -1), ("eq", 0), ("gt", 1)] {
                    let status = Command::new("dpkg").args(["--compare-versions", a, op, b]).status().map_err(|e| format!("dpkg could not compare versions: {e}"))?;
                    match status.code() { Some(0) => return Ok(result), Some(1) => {}, _ => return Err("dpkg rejected a package version.".into()) }
                }
                Err("dpkg gave no version ordering.".into())
            }
        }
    }

    /// Only filenames for the host's architecture/type may enter its cache.
    pub fn file_version(self, name: &str, file: &str) -> Option<String> {
        if !file.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-:".contains(&b)) { return None; }
        let version = match self {
            Self::Arch => file.strip_prefix(name)?.strip_prefix('-')?.strip_suffix("-x86_64.pkg.tar.zst")?,
            Self::Ubuntu => {
                let suffix=if name=="ibara" {
                    file.strip_prefix("ibara_").or_else(||file.strip_prefix("ibara-operator_")).or_else(||file.strip_prefix("ibara-target_"))?
                } else {file.strip_prefix(name)?.strip_prefix('_')?};
                suffix.strip_suffix(if name=="mutter-common" {"_all.deb"} else {"_amd64.deb"})?
            },
        };
        (version.bytes().next().is_some_and(|b| b.is_ascii_digit()) && version.contains('-')).then(|| version.to_owned())
    }

    pub fn install(self, files: &[PathBuf]) -> Result<(), String> {
        if files.iter().any(|path| !path.is_absolute()) { return Err("Package paths must be absolute.".into()); }
        match self {
            Self::Arch => interactive(Command::new("pacman").args(["-U", "--noconfirm", "--"]).args(files)),
            Self::Ubuntu => interactive(Command::new("apt-get").args(["install", "--yes", "--no-remove", "--allow-downgrades", "--reinstall", "--"]).args(files)),
        }
    }

    pub fn remove(self, names: &[&str]) -> Result<(), String> {
        match self {
            Self::Arch => interactive(Command::new("pacman").args(["-R", "--noconfirm", "--"]).args(names)),
            Self::Ubuntu => interactive(Command::new("apt-get").args(["remove", "--yes", "--"]).args(names)),
        }
    }
}
