//! ibara: the Rust core. See docs/architecture.md and docs/internals.md.

pub mod contract;
pub mod console;
pub mod controller;
pub mod desktop;
pub mod error;
pub mod entry;
pub mod http;
pub mod ids;
pub mod install;
pub mod harness;
pub mod mcp;
pub mod operator;
pub mod server;
pub mod storage;
pub mod store;

pub mod access;
pub mod logins;

pub mod access_system;
pub mod power_system;

pub mod sshkey;
pub mod tailnet;
pub mod settings;
pub mod wake;
pub mod theme;

/// `pkgver-pkgrel` for a packaged build (the PKGBUILD sets IBARA_PKGREL), `pkgver` otherwise.
pub fn version() -> String {
    match option_env!("IBARA_PKGREL") {
        Some(rel) if !rel.is_empty() => format!("{}-{rel}", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}
