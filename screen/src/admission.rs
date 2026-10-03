use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Refused,
    Accepted { input: bool, generation: u64 },
}
struct Ticket {
    hash: [u8; 32],
    cert: String,
    expires: u64,
    input: bool,
    generation: u64,
}
#[derive(Default)]
pub struct Admission {
    pub generation: u64,
    pub open: bool,
    tickets: Vec<Ticket>,
}
fn lower_hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
}
impl Admission {
    pub fn open(&mut self, g: u64) -> Result<()> {
        ensure!(g > self.generation, "stale generation");
        self.generation = g;
        self.open = true;
        self.tickets.clear();
        Ok(())
    }
    pub fn ticket(
        &mut self,
        t: &str,
        c: &str,
        g: u64,
        expires: u64,
        input: bool,
        now: u64,
    ) -> Result<()> {
        ensure!(self.open && g == self.generation, "fenced generation");
        ensure!(
            lower_hex(t, 64) && lower_hex(c, 64),
            "invalid ticket or identity"
        );
        ensure!(expires > now, "expired ticket");
        self.tickets.retain(|x| x.cert != c && x.expires > now);
        ensure!(self.tickets.len() < 4, "viewer limit");
        self.tickets.push(Ticket {
            hash: Sha256::digest(t.as_bytes()).into(),
            cert: c.into(),
            expires: expires.min(now.saturating_add(60_000)),
            input,
            generation: g,
        });
        Ok(())
    }
    pub fn admit(&mut self, t: &str, c: &str, now: u64) -> Decision {
        let hash: [u8; 32] = Sha256::digest(t.as_bytes()).into();
        let Some(i) = self.tickets.iter().position(|x| same(&x.hash, &hash)) else {
            return Decision::Refused;
        };
        let x = &self.tickets[i];
        if !self.open
            || x.generation != self.generation
            || now >= x.expires
            || !same(x.cert.as_bytes(), c.as_bytes())
        {
            self.tickets.remove(i);
            return Decision::Refused;
        }
        Decision::Accepted {
            input: x.input,
            generation: x.generation,
        }
    }
    pub fn revoke(&mut self) -> Result<()> {
        let next = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("generation overflow"))?;
        self.open = false;
        self.generation = next;
        self.tickets.clear();
        Ok(())
    }
}
