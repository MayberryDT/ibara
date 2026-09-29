//! The controller store: the journal (`journal.sqlite`), its request
//! fingerprints, backups and the Node-to-Rust migration check.
//!
//! The journal is opened in place and migrated additively, so a machine can
//! switch between the TypeScript controller and `ibarad` without losing
//! endpoint identity, the fingerprint secret, receipts or audits.

pub mod backup;
pub mod canonical;
pub mod journal;
pub mod migrate;
mod records;
mod schema;
mod timeline;
mod windows;

pub use journal::{Clock, FingerprintSecret, Journal, JournalOptions, Remembered, RememberIntent};
pub use records::*;
pub use schema::CORE_SCHEMA_VERSION;
pub use timeline::{AppNote, AttentionItem, NewAttention, NewEvent, TimelineEvent};
pub use windows::TaskWindow;

use std::path::PathBuf;

/// `$IBARA_STATE_DIR`, default `~/.local/state/agent-computer` (`src/server.ts:15`).
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("IBARA_STATE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    home_dir().join(".local/state/agent-computer")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Clip to at most `max` UTF-16 code units, like the TypeScript `clip` on a JS string.
pub(crate) fn clip(text: &str, max: usize) -> String {
    let mut units = 0;
    for (i, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > max {
            return text[..i].to_string();
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests;
