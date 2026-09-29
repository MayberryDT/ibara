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

pub mod access_system;
pub mod power_system;

pub mod sshkey;
pub mod tailnet;
pub mod settings;
pub mod wake;
pub mod theme;
