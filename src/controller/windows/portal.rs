//! Cancel a known owned portal chooser before retiring its blocked browser.
use super::*;
use crate::controller::ports::Button;
use crate::store::TaskWindow;

fn owns(o: &TaskWindow, w: &Win) -> bool {
    !o.touched && w.process_start_ticks.is_some() && !w.compositor_instance.is_empty()
        && o.address == w.address && o.pid == w.pid && o.class == w.class
        && o.process_start_ticks == w.process_start_ticks && o.compositor_instance == w.compositor_instance
}

impl Controller {
    pub(in crate::controller) async fn claim_portal_after_file_input(&self, task_ref: &str, lease: &crate::store::LeaseRecord,
        before: &[Win], browser: &WinKey, gone: &Cancel) -> Result<()> {
        self.assert_authority(lease)?;
        let Some(parent) = before.iter().find(|w| w.key() == *browser && is_browser(&w.class)) else { return Ok(()); };
        if !self.journal.owned_windows()?.iter().any(|o| o.task_ref == task_ref && owns(o, parent))
            || before.iter().any(|w| w.class == "xdg-desktop-portal-gtk") { return Ok(()); }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            self.assert_authority(lease)?;
            if gone.is_cancelled() { return Ok(()); }
            let now = self.desktop.all_windows().await?;
            // all_windows includes hidden surfaces but deliberately has no
            // focus state. Read focus through the visible-window port.
            let focused = self.desktop.windows().await?.into_iter().find(|w| w.focused).map(|w| w.key());
            self.assert_authority(lease)?;
            if !now.iter().any(|w| w.key() == *browser) { return Ok(()); }
            let fresh: Vec<_> = now.iter().filter(|w| !before.iter().any(|b| b.address == w.address)
                && w.floating && w.class == "xdg-desktop-portal-gtk"
                && w.title == "Open File" && w.process_start_ticks.is_some()
                && w.compositor_instance == parent.compositor_instance
                && std::fs::read_link(format!("/proc/{}/exe", w.pid)).ok().as_deref()
                    == Some(std::path::Path::new("/usr/lib/xdg-desktop-portal-gtk"))).collect();
            if fresh.len() > 1 { return Err(IbaraError::new("CONTROL_UNSETTLED", "Ambiguous new file choosers; ownership not assigned.", false)); }
            if let Some(w) = fresh.first().filter(|w| focused.as_ref() == Some(&w.key())) {
                if !self.journal.owned_windows()?.iter().any(|o| o.task_ref == task_ref && owns(o, parent)) { return Ok(()); }
                self.journal.own_window(task_ref, &w.address, w.pid, &w.class, &w.title, w.process_start_ticks, &w.compositor_instance)?;
                self.timeline("portal_owned", Some(task_ref), &lease.principal, "Native file input opened a task-owned chooser.",
                    json!({"browser":browser.address,"window":w.address,"basis":"observed file-input click; absent before; genuine focused portal; unchanged owned browser and authority"}));
                return Ok(());
            }
            if Instant::now() >= deadline { return Ok(()); }
            sleep(POLL).await;
        }
    }

    pub(super) async fn retire_portal_choosers(&self, windows: &[Win], cancel: &Cancel,
        allowed: &impl Fn() -> Result<bool>, receipts: &mut Vec<Value>) -> Result<()> {
        let guard = || -> Result<()> {
            if allowed()? && !cancel.is_cancelled() { Ok(()) }
            else { Err(IbaraError::new("CONTROL_UNSETTLED", "File chooser cleanup authority changed.", false)) }
        };
        for window in windows.iter().filter(|w| w.floating && w.class == "xdg-desktop-portal-gtk"
            && matches!(w.title.as_str(), "Open File" | "Save File" | "Save As")) {
            guard()?;
            if std::fs::read_link(format!("/proc/{}/exe", window.pid)).ok().as_deref()
                != Some(std::path::Path::new("/usr/lib/xdg-desktop-portal-gtk")) { continue; }
            let owned = self.journal.owned_windows()?;
            let Some(owner) = owned.iter().find(|o| owns(o, window)) else { continue; };
            if !windows.iter().any(|w| is_browser(&w.class) && owned.iter()
                .any(|o| o.task_ref == owner.task_ref && owns(o, w))) { continue; }
            self.desktop.act(&Effect::Focus(window.key()), cancel).await?;
            guard()?;
            let page = self.desktop.elements(&window.key(), Some("Cancel"), 60, None).await?;
            let buttons: Vec<_> = page.elements.iter().filter(|e| e.name == "Cancel"
                && matches!(e.role.as_str(), "button" | "push button")
                && e.states.iter().any(|s| s == "enabled")).collect();
            if !page.available || page.next_cursor.is_some() || buttons.len() != 1 {
                return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Owned file chooser Cancel button could not be verified.", false));
            }
            guard()?;
            if !self.journal.owned_windows()?.iter().any(|o| o.task_ref == owner.task_ref && owns(o, window)) {
                return Err(IbaraError::new("CONTROL_UNSETTLED", "File chooser ownership changed.", false));
            }
            self.desktop.act(&Effect::ClickElement { surface:window.key(), element:Box::new((*buttons[0]).clone()), button:Button::Left, double:false }, cancel).await?;
            guard()?;
            let deadline = Instant::now() + CLOSE_WAIT;
            loop {
                guard()?;
                if !self.desktop.all_windows().await?.iter().any(|w| w.key() == window.key()) { break; }
                if Instant::now() >= deadline {
                    return Err(IbaraError::new("CONTROL_UNSETTLED", "File chooser cancellation was not verified; no retry sent.", false));
                }
                sleep(POLL).await;
            }
            receipts.push(json!({"task_ref":owner.task_ref,"window":window.address,"dialog":window.title,
                "action":"Cancel","basis":"untouched task-owned portal incarnation and browser; observed native Cancel button"}));
        }
        Ok(())
    }
}
