use anyhow::{Result, ensure};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub const MAX_MESSAGE: usize = 64 * 1024;
pub const MAX_VIDEO: usize = 4 * 1024 * 1024;
pub async fn read_json<R: AsyncRead + Unpin>(r: &mut R) -> Result<Value> {
    let n = r.read_u32().await? as usize;
    ensure!(n > 0 && n <= MAX_MESSAGE, "invalid message size");
    let mut b = vec![0; n];
    r.read_exact(&mut b).await?;
    let v: Value = serde_json::from_slice(&b)?;
    ensure!(v.is_object(), "message must be an object");
    Ok(v)
}
pub async fn write_json<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let b = serde_json::to_vec(v)?;
    ensure!(b.len() <= MAX_MESSAGE, "message too large");
    w.write_u32(b.len() as u32).await?;
    w.write_all(&b).await?;
    Ok(())
}
#[derive(Debug)]
pub struct VideoHeader {
    pub keyframe: bool,
    pub sequence: u64,
    pub capture_us: u64,
}
impl VideoHeader {
    pub fn encode(&self) -> [u8; 24] {
        let mut h = [0; 24];
        h[..4].copy_from_slice(b"ISV1");
        h[4] = 1;
        h[5] = u8::from(self.keyframe);
        h[8..16].copy_from_slice(&self.sequence.to_be_bytes());
        h[16..24].copy_from_slice(&self.capture_us.to_be_bytes());
        h
    }
    pub fn decode(b: &[u8]) -> Result<Self> {
        ensure!(
            b.len() == 24 && &b[..4] == b"ISV1" && b[4] == 1 && b[5] <= 1 && b[6] == 0 && b[7] == 0,
            "invalid video header"
        );
        Ok(Self {
            keyframe: b[5] == 1,
            sequence: u64::from_be_bytes(b[8..16].try_into()?),
            capture_us: u64::from_be_bytes(b[16..24].try_into()?),
        })
    }
}
#[derive(Default)]
pub struct VideoGate {
    last: Option<u64>,
    need_keyframe: bool,
}
impl VideoGate {
    pub fn accept(&mut self, seq: u64, key: bool) -> bool {
        if self.last.is_some_and(|s| seq <= s) {
            return false;
        }
        if self.last.is_none() || self.last.is_some_and(|s| s.checked_add(1) != Some(seq)) {
            self.need_keyframe = true;
        }
        self.last = Some(seq);
        if key {
            self.need_keyframe = false;
        }
        !self.need_keyframe
    }
}
#[derive(Default)]
pub struct InputSequence {
    next: Option<u64>,
}
impl InputSequence {
    pub fn expected(&self) -> Option<u64> {
        self.next
    }
    pub fn consume(&mut self, from: u64, count: usize) -> Result<u64> {
        ensure!(count > 0 && count <= 256, "invalid event count");
        let next = from
            .checked_add(count as u64)
            .ok_or_else(|| anyhow::anyhow!("sequence overflow"))?;
        ensure!(self.next.is_none_or(|n| n == from), "input gap");
        self.next = Some(next);
        Ok(next - 1)
    }
}
