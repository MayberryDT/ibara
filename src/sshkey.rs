//! One `ssh-ed25519` public key line: the only key shape a computer ever hands
//! to another when pairing, and the only one root writes into `authorized_keys`.
//! Also signing a statement with such a key and checking one (`ssh-keygen -Y`),
//! so a computer can prove it holds the key it offers.

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use sha2::{Digest, Sha256};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Longest armored signature accepted; an Ed25519 one is about 300 bytes.
const SIGNATURE_LIMIT: usize = 2048;
const KEYGEN_TIMEOUT: Duration = Duration::from_secs(10);

/// A validated key line, its base64 blob and its OpenSSH `SHA256:` fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyLine {
    /// `ssh-ed25519 <blob>` or `ssh-ed25519 <blob> <comment>`.
    pub line: String,
    pub blob: String,
    pub fingerprint: String,
}

/// Exactly one ssh-ed25519 line whose blob decodes to its own type, no
/// options, no control characters, a printable comment of at most 200 bytes.
pub fn ed25519_line(value: &str) -> Option<KeyLine> {
    let text = value.trim();
    if text.len() > 400 || text.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return None;
    }
    let rest = text.strip_prefix("ssh-ed25519 ")?;
    let (blob, comment) = match rest.split_once(' ') {
        Some((blob, comment)) => (blob, Some(comment.trim())),
        None => (rest, None),
    };
    let base64_char = |b: u8| b.is_ascii_alphanumeric() || b == b'+' || b == b'/';
    if blob.len() != 68 || !blob.bytes().all(base64_char) {
        return None;
    }
    if let Some(comment) = comment
        && !((1..=200).contains(&comment.len()) && comment.bytes().all(|b| (0x20..=0x7e).contains(&b)))
    {
        return None;
    }
    let bytes = STANDARD_NO_PAD.decode(blob).ok()?;
    let u32_at = |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    if bytes.len() != 51 || u32_at(0) != 11 || &bytes[4..15] != b"ssh-ed25519" || u32_at(15) != 32 {
        return None;
    }
    let fingerprint = format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&bytes)));
    let line = match comment {
        Some(comment) => format!("ssh-ed25519 {blob} {comment}"),
        None => format!("ssh-ed25519 {blob}"),
    };
    Some(KeyLine { line, blob: blob.to_string(), fingerprint })
}

/// Run `ssh-keygen ARGS` with `input` on stdin; its stdout when it succeeds.
async fn keygen(args: &[&std::ffi::OsStr], input: &[u8]) -> Option<Vec<u8>> {
    let mut child = tokio::process::Command::new("ssh-keygen")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let work = async {
        stdin.write_all(input).await.ok()?;
        drop(stdin);
        child.wait_with_output().await.ok()
    };
    let out = tokio::time::timeout(KEYGEN_TIMEOUT, work).await.ok()??;
    out.status.success().then_some(out.stdout)
}

/// `message` signed in `namespace` with the private key at `key`
/// (`ssh-keygen -Y sign`): the armored `SSH SIGNATURE` text.
pub async fn sign(key: &Path, namespace: &str, message: &[u8]) -> Option<String> {
    let args = ["-q".as_ref(), "-Y".as_ref(), "sign".as_ref(), "-n".as_ref(), namespace.as_ref(), "-f".as_ref(), key.as_os_str()];
    let signature = String::from_utf8(keygen(&args, message).await?).ok()?;
    armored(&signature).then_some(signature)
}

/// One armored SSH signature of printable text, at most [`SIGNATURE_LIMIT`] bytes.
fn armored(text: &str) -> bool {
    let text = text.trim();
    text.len() <= SIGNATURE_LIMIT
        && text.starts_with("-----BEGIN SSH SIGNATURE-----\n")
        && text.ends_with("\n-----END SSH SIGNATURE-----")
        && text.bytes().all(|b| b == b'\n' || (0x20..=0x7e).contains(&b))
}

/// Whether `signature` is `key`'s signature of `message` in `namespace`:
/// `ssh-keygen -Y verify` against allowed signers naming only `key`. The two
/// files it reads go in a fresh directory under `scratch`, removed afterwards.
pub async fn verify(key: &KeyLine, namespace: &str, message: &[u8], signature: &str, scratch: &Path) -> bool {
    if !armored(signature) || !namespace.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return false;
    }
    let dir = scratch.join(format!("verify-{}", uuid::Uuid::new_v4().simple()));
    if std::fs::DirBuilder::new().mode(0o700).create(&dir).is_err() {
        return false;
    }
    let (signers, sig) = (dir.join("signers"), dir.join("signature"));
    let written = std::fs::write(&signers, format!("ibara namespaces=\"{namespace}\" ssh-ed25519 {}\n", key.blob))
        .and_then(|()| std::fs::write(&sig, format!("{}\n", signature.trim())));
    let args = [
        "-Y".as_ref(),
        "verify".as_ref(),
        "-f".as_ref(),
        signers.as_os_str(),
        "-I".as_ref(),
        "ibara".as_ref(),
        "-n".as_ref(),
        namespace.as_ref(),
        "-s".as_ref(),
        sig.as_os_str(),
    ];
    let good = written.is_ok() && keygen(&args, message).await.is_some();
    let _ = std::fs::remove_dir_all(&dir);
    good
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// A syntactically valid ed25519 key line (the blob is not a real key).
    fn key(comment: &str) -> String {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[7u8; 32]);
        let text = STANDARD_NO_PAD.encode(&blob);
        if comment.is_empty() { format!("ssh-ed25519 {text}") } else { format!("ssh-ed25519 {text} {comment}") }
    }

    #[test]
    fn unsafe_key_lines_never_reach_authorized_keys() {
        let good = key("vesper");
        for bad in [
            format!("{good}\n{}", key("")),
            format!("command=\"sh\" {good}"),
            good.replace("ssh-ed25519 ", "ssh-rsa "),
            format!("{good}\u{7}"),
            format!("{} {}", key(""), "c".repeat(201)),
            "ssh-ed25519 AAAA".to_string(),
        ] {
            assert!(ed25519_line(&bad).is_none(), "{bad:?}");
        }
        let parsed = ed25519_line(&format!("  {good}  ")).unwrap();
        assert_eq!(parsed.line, good);
        assert!(parsed.fingerprint.starts_with("SHA256:") && !parsed.fingerprint.ends_with('='));
    }

    #[test]
    fn a_blob_that_does_not_decode_to_its_own_type_is_refused() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25518");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[7u8; 32]);
        let text = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&blob);
        assert!(ed25519_line(&format!("ssh-ed25519 {text}")).is_none());
    }
}
