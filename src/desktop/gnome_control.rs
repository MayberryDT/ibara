//! GNOME control owns one persistent cursor connection across input calls.
//! Native person input retires the lease; a later call must acquire a new one.
use super::{gnome::{Gnome, CursorLease, InputTransaction, InputReceipt}, SurfaceId, input};
use crate::error::{IbaraError, Result};
use std::sync::{Arc, Mutex, atomic::{AtomicU64, AtomicBool, Ordering}};

struct Inner {
    desktop: Gnome,
    label: Mutex<Option<String>>,
    person: AtomicBool,
    revision: AtomicU64,
    lease: tokio::sync::Mutex<Option<(u64, CursorLease)>>,
}

#[derive(Clone)]
pub struct GnomeControl(Arc<Inner>);
impl GnomeControl {
    pub fn new(desktop: Gnome) -> Self {
        Self(Arc::new(Inner { desktop, label: Mutex::new(None), person: AtomicBool::new(false),
            revision: AtomicU64::new(0), lease: tokio::sync::Mutex::new(None) }))
    }
    fn refused() -> IbaraError {
        IbaraError::new("HUMAN_CONTROL", "GNOME control changed hands before input.", false)
            .with("execution_not_started", true)
    }
    pub fn set_agent(&self, label: Option<String>) {
        *self.0.label.lock().unwrap_or_else(|p| p.into_inner()) = label;
        self.0.revision.fetch_add(1, Ordering::SeqCst);
        let me = self.clone();
        tokio::spawn(async move { let _ = me.to_agent().await; });
    }
    pub fn viewer_turn(&self, person: bool) {
        self.0.person.store(person, Ordering::SeqCst);
        self.0.revision.fetch_add(1, Ordering::SeqCst);
        if person {
            let me = self.clone();
            tokio::spawn(async move {
                let mut lease = me.0.lease.lock().await;
                if me.0.person.load(Ordering::SeqCst) {
                    if let Some((_, cursor)) = lease.take() { let _ = cursor.finish().await; }
                }
            });
        }
    }
    pub async fn to_agent(&self) -> Result<()> {
        let state = self.0.desktop.state().await?;
        if state.input_readiness_api != 1 || state.shell_input_blocked {
            return Err(IbaraError::new("CAPABILITY_UNAVAILABLE",
                "GNOME Shell holds input or its readiness helper needs a session reload. A person must dismiss any secure prompt before agent work.", true)
                .with("execution_not_started", true));
        }
        tokio::time::timeout(std::time::Duration::from_secs(3),self.to_agent_inner()).await
            .map_err(|_| IbaraError::new("CONTROL_UNSETTLED", "GNOME cursor handover did not complete within three seconds; application input was not started.", false)
                .with("execution_not_started",true).requires_reconciliation())?
    }
    async fn to_agent_inner(&self) -> Result<()> {
        let mut lease = self.0.lease.lock().await;
        let revision = self.0.revision.load(Ordering::SeqCst);
        let label = self.0.label.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if self.0.person.load(Ordering::SeqCst) || label.is_none() {
            if let Some((_, old)) = lease.take() { old.finish().await?; }
            return Err(Self::refused());
        }
        if let Some((generation, cursor)) = lease.as_ref() {
            if *generation == revision && cursor.active().await.unwrap_or(false) { return Ok(()); }
        }
        if let Some((_, old)) = lease.take() {
            // Person input, lock or helper replacement can already have retired it.
            let _ = old.finish().await;
            self.0.desktop.settle_input().await?;
        }
        loop {
            if revision!=self.0.revision.load(Ordering::SeqCst) || self.0.person.load(Ordering::SeqCst) {
                return Err(Self::refused());
            }
            let state=self.0.desktop.state().await?;
            if state.locked { return Err(Self::refused()); }
            let mut clock=libc::timespec { tv_sec:0,tv_nsec:0 };
            if unsafe {libc::clock_gettime(libc::CLOCK_MONOTONIC,&mut clock)}!=0 {
                return Err(IbaraError::new("SESSION_UNAVAILABLE", "Native person-input clock is unavailable.", false)
                    .with("execution_not_started",true));
            }
            let now=clock.tv_sec as u64*1_000_000+clock.tv_nsec as u64/1_000;
            if now.saturating_sub(state.last_person_us)>=1_000_000 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let cursor = self.0.desktop.begin_cursor(label.as_deref().unwrap()).await?;
        if revision != self.0.revision.load(Ordering::SeqCst) || self.0.person.load(Ordering::SeqCst) {
            cursor.finish().await?;
            return Err(Self::refused());
        }
        *lease = Some((revision, cursor));
        Ok(())
    }
    pub async fn begin_input(&self, surface: &SurfaceId) -> Result<InputTransaction> {
        self.to_agent().await?;
        let lease = self.0.lease.lock().await;
        let (revision, cursor) = lease.as_ref().ok_or_else(Self::refused)?;
        if *revision != self.0.revision.load(Ordering::SeqCst) || self.0.person.load(Ordering::SeqCst) {
            return Err(Self::refused());
        }
        cursor.begin_input(surface, &uuid::Uuid::new_v4().to_string()).await
    }
    pub async fn move_to(&self, x: f64, y: f64) -> Result<()> {
        let lease = self.0.lease.lock().await;
        let (revision, cursor) = lease.as_ref().ok_or_else(Self::refused)?;
        if *revision != self.0.revision.load(Ordering::SeqCst) || self.0.person.load(Ordering::SeqCst) {
            return Err(Self::refused());
        }
        cursor.move_to(x,y).await
    }
    pub async fn release(&self) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(3),self.release_inner()).await
            .map_err(|_| IbaraError::new("CONTROL_UNSETTLED", "GNOME control release has not settled.", false).requires_reconciliation())?
    }
    async fn release_inner(&self) -> Result<()> {
        if let Some((_, cursor)) = self.0.lease.lock().await.take() {
            let released = cursor.finish().await;
            if released.is_ok() { return self.0.desktop.settle_input().await; }
            // A cancelled transaction closes this shared connection first.
            // Reconcile real Shell/native state instead of treating its missing
            // CursorEnd reply as evidence that a modifier is still held.
            loop {
                let state=self.0.desktop.state().await?;
                if state.input_settled==Some(true) && !state.cursor_visible { return Ok(()); }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        // Stock helpers can operate read-only without an input API. On a patched
        // session, also reconcile a connection a background handback just closed.
        if self.0.desktop.state().await?.guarded_input_api==0 { return Ok(()); }
        self.0.desktop.settle_input().await
    }
    pub async fn key(&self, surface: &SurfaceId, combo: &str) -> Result<InputReceipt> {
        let codes = input::cua_keys(combo)?.iter().map(|key| evdev(key)).collect::<Result<Vec<_>>>()?;
        if codes.iter().copied().collect::<std::collections::HashSet<_>>().len()!=codes.len() {
            return Err(crate::error::invalid("A chord may not repeat the same modifier.").with("execution_not_started",true));
        }
        let input = self.begin_input(surface).await?;
        for code in &codes { input.key(*code,true).await?; }
        for code in codes.iter().rev() { input.key(*code,false).await?; }
        input.finish().await
    }
    pub async fn click(&self, surface: &SurfaceId, x: f64, y: f64, button: input::Button,
                       double: bool) -> Result<InputReceipt> {
        let button = match button { input::Button::Left=>1, input::Button::Middle=>2, input::Button::Right=>3 };
        let input = self.begin_input(surface).await?;
        self.move_to(x,y).await?;
        input.motion(x,y).await?;
        for _ in 0..if double { 2 } else { 1 } {
            input.button(button,true).await?;
            input.button(button,false).await?;
        }
        input.finish().await
    }
    pub async fn drag(&self, surface: &SurfaceId, from: (f64,f64), to: (f64,f64),
                      duration: std::time::Duration) -> Result<InputReceipt> {
        let input = self.begin_input(surface).await?;
        self.move_to(from.0,from.1).await?;
        input.motion(from.0,from.1).await?;
        input.button(1,true).await?;
        let start=std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let progress=(start.elapsed().as_secs_f64()/duration.as_secs_f64()).min(1.0);
            let x=from.0+(to.0-from.0)*progress;
            let y=from.1+(to.1-from.1)*progress;
            self.move_to(x,y).await?;
            input.motion(x,y).await?;
            if progress>=1.0 { break; }
        }
        input.button(1,false).await?;
        input.finish().await
    }
    pub async fn scroll(&self, surface: &SurfaceId, x: f64, y: f64, dx: i32, dy: i32) -> Result<InputReceipt> {
        if !(-50..=50).contains(&dx) || !(-50..=50).contains(&dy) {
            return Err(crate::error::invalid("Scroll accepts -50 to 50 wheel notches per axis.")
                .with("execution_not_started", true));
        }
        let input = self.begin_input(surface).await?;
        self.move_to(x,y).await?;
        input.motion(x,y).await?;
        input.scroll(dx,dy).await?;
        input.finish().await
    }
}

// Linux input-event-codes.h evdev codes, matching the existing key vocabulary.
// Text never goes through this mapping or an assumed keyboard layout.
fn evdev(key: &str) -> Result<u32> {
    let named = match key {
        "Escape"=>1, "BackSpace"=>14, "Tab"=>15, "Return"=>28, "ctrl"=>29,
        "shift"=>42, "alt"=>56, "space"=>57, "super"=>125,
        "Home"=>102, "Up"=>103, "Page_Up"=>104, "Left"=>105, "Right"=>106,
        "End"=>107, "Down"=>108, "Page_Down"=>109, "Delete"=>111,
        "F11"=>87, "F12"=>88, _=>0,
    };
    if named!=0 { return Ok(named); }
    if let Some(n)=key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=10).contains(n)) {
        return Ok(58+n);
    }
    if key.len()==1 {
        for (row,start) in [("1234567890",2),("qwertyuiop",16),("asdfghjkl",30),("zxcvbnm",44)] {
            if let Some(index)=row.find(key) { return Ok(start+index as u32); }
        }
    }
    Err(crate::error::invalid("Key has no qualified GNOME evdev mapping.").with("execution_not_started",true))
}
