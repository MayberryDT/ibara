//! Bounded native recovery for Mousepad work opened by a task on a dedicated
//! desktop. Never infer ownership from a title, never save over an existing file.
use super::*;
use crate::controller::ports::Button;
use crate::store::TaskWindow;

fn owns(o: &TaskWindow, w: &Win) -> bool {
    !o.touched && w.process_start_ticks.is_some() && !w.compositor_instance.is_empty()
        && o.address == w.address && o.pid == w.pid && o.class == w.class
        && o.process_start_ticks == w.process_start_ticks && o.compositor_instance == w.compositor_instance
}

// GTK's overwrite confirmation has no title/accessibility tree on this
// Mousepad build. Escape cancels it; never choose its destructive default.
// Recognize only this exact two-modal stack in one exclusively owned process.
fn save_confirmation(w: &Win, inventory: &[Win]) -> bool {
    let peers: Vec<_> = inventory.iter().filter(|p| p.pid == w.pid).collect();
    w.floating && w.title.is_empty() && peers.len() == 3
        && peers.iter().filter(|p| !p.floating).count() == 1
        && peers.iter().filter(|p| p.floating && p.title == "Save As").count() == 1
        && peers.iter().filter(|p| p.floating && p.title.is_empty()).count() == 1
}

impl Controller {
    pub(super) fn owned_editor_process(&self, window: &Win, inventory: &[Win]) -> Result<bool> {
        if window.floating || !matches!(window.class.as_str(), "org.xfce.mousepad" | "mousepad") { return Ok(false); }
        // The class alone is not an app identity. Restrict this adapter to the
        // installed Mousepad executable, not arbitrary windows with that class.
        if std::fs::read_link(format!("/proc/{}/exe", window.pid)).ok().as_deref() != Some(std::path::Path::new("/usr/bin/mousepad")) {
            return Ok(false);
        }
        let owned = self.journal.owned_windows()?;
        let Some(owner) = owned.iter().find(|o| owns(o, window)) else { return Ok(false); };
        Ok(inventory.iter().filter(|w| w.pid == window.pid).all(|w| {
            if let Some(o) = owned.iter().find(|o| o.address == w.address) {
                o.task_ref == owner.task_ref && owns(o,w)
            } else {
                // A reset may have created this modal immediately before a
                // daemon interruption. The exclusively owned parent/process is
                // its provenance; buttons are still verified before answering.
                w.floating && (matches!(w.title.as_str(), "Save Changes" | "Save As" | "Open File") || save_confirmation(w, inventory))
                    && w.class == window.class && w.process_start_ticks == window.process_start_ticks
                    && w.compositor_instance == window.compositor_instance
            }
        }))
    }

    pub(super) async fn retire_editor(&self, root: &Win, initial: &[Win], cancel: &Cancel,
        allowed: &impl Fn() -> Result<bool>, receipts: &mut Vec<Value>) -> Result<()> {
        let original: Vec<_> = initial.iter().filter(|w| w.pid == root.pid).cloned().collect();
        let owner = self.journal.owned_windows()?.into_iter().find(|o| owns(o,root))
            .ok_or_else(|| IbaraError::new("CONTROL_UNSETTLED", "Editor ownership changed.", false))?;
        let guard = || -> Result<()> {
            if !allowed()? || cancel.is_cancelled() {
                return Err(IbaraError::new("CONTROL_UNSETTLED", "Editor cleanup authority changed.", false));
            }
            Ok(())
        };
        let send = async |effect| {
            guard()?;
            self.desktop.act(&effect, cancel).await?;
            guard()
        };
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut actions = 0;
        let mut closing = None;
        let mut last_dialog = None;
        'settle: loop {
            guard()?;
            let inventory = self.desktop.all_windows().await?;
            let live: Vec<_> = inventory.iter().filter(|w| w.pid == root.pid).collect();
            if live.is_empty() { return Ok(()); }
            if Instant::now() >= deadline || actions >= 16 {
                return Err(IbaraError::new("CONTROL_UNSETTLED", "Editor cleanup did not settle within its bound.", false));
            }
            // The process can exit before Hyprland removes its last window row.
            // With no birth time there is no safe input target: wait for a fresh
            // inventory, without confusing successful exit with PID replacement.
            if live.iter().any(|w| w.process_start_ticks.is_none()) {
                sleep(POLL).await;
                continue;
            }
            let owned = self.journal.owned_windows()?;
            // The close event can remove ownership after the inventory read.
            // Wait without effects for an already-requested close to disappear;
            // never reinterpret a touched/replaced owner as a successful close.
            if live.iter().any(|w| closing.as_ref() == Some(&w.key())
                && !owned.iter().any(|o| o.address == w.address)
                && w.process_start_ticks == root.process_start_ticks
                && w.compositor_instance == root.compositor_instance && w.class == root.class) {
                sleep(POLL).await;
                continue 'settle;
            }
            for w in &live {
                if w.process_start_ticks != root.process_start_ticks || w.compositor_instance != root.compositor_instance || w.class != root.class {
                    return Err(IbaraError::new("STALE_TARGET", "Editor process identity changed.", false));
                }
                if !w.floating || owned.iter().any(|o| o.address == w.address) {
                    if !owned.iter().any(|o| o.task_ref == owner.task_ref && owns(o,w)) || (!w.floating && !original.iter().any(|o| o.key() == w.key())) {
                        return Err(IbaraError::new("CONTROL_UNSETTLED", "Editor is no longer exclusively task-owned.", false));
                    }
                } else if !w.floating || !(matches!(w.title.as_str(), "Save Changes" | "Save As" | "Open File") || save_confirmation(w, &inventory)) {
                    return Err(IbaraError::new("CONTROL_UNSETTLED", "Unexpected editor window appeared during cleanup.", false));
                }
            }
            let dialogs: Vec<_> = live.iter().filter(|w| w.floating).collect();
            if let Some(dialog) = dialogs.iter().find(|w| save_confirmation(w, &inventory)) {
                if last_dialog.as_ref() == Some(&dialog.key()) {
                    sleep(POLL).await;
                    continue;
                }
                guard()?;
                self.journal.own_window(&owner.task_ref, &dialog.address, dialog.pid, &dialog.class, &dialog.title,
                    dialog.process_start_ticks, &dialog.compositor_instance)?;
                send(Effect::Focus(dialog.key())).await?;
                send(Effect::Key { surface: dialog.key(), combo: "Escape".into() }).await?;
                receipts.push(json!({"task_ref":owner.task_ref,"window":dialog.address,"dialog":"Save As confirmation","action":"Escape (cancel)","basis":"exclusive owned Mousepad process; exact Save As modal stack"}));
                last_dialog = Some(dialog.key());
                closing = None;
                actions += 1;
                sleep(POLL).await;
                continue;
            }
            if dialogs.len() > 1 {
                return Err(IbaraError::new("CONTROL_UNSETTLED", "Ambiguous editor dialogs; no automatic answer sent.", false));
            }
            if let Some(dialog) = dialogs.first() {
                let label = match dialog.title.as_str() {
                    "Save Changes" => "Don't Save",
                    "Save As" | "Open File" => "Cancel",
                    _ => return Err(IbaraError::new("CONTROL_UNSETTLED", "Unrecognized editor dialog; no automatic answer sent.", false)),
                };
                if last_dialog.as_ref() == Some(&dialog.key()) {
                    sleep(POLL).await;
                    continue;
                }
                send(Effect::Focus(dialog.key())).await?;
                let page = self.desktop.elements(&dialog.key(), if label == "Cancel" { Some("Cancel") } else { None }, 60, None).await?;
                let buttons: Vec<_> = page.elements.iter().filter(|e| matches!(e.role.as_str(), "button" | "push button") && e.states.iter().any(|s| s == "enabled")).collect();
                let candidates: Vec<_> = buttons.iter().filter(|e| e.name == label).collect();
                // Cua marks this GTK modal tree partial even when all three
                // buttons are present. Require the exact observed controls, no
                // omitted result page, and dispatch by revalidated identity.
                if !page.available || page.next_cursor.is_some() || candidates.len() != 1
                    || (label == "Don't Save" && !(buttons.iter().any(|e| e.name == "Cancel") && buttons.iter().any(|e| e.name == "Save"))) {
                    return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", format!("Editor dialog buttons could not be verified (available={}, truncated={}, total={:?}, buttons={:?}).", page.available, page.truncated, page.total, buttons.iter().map(|b| (&b.role, &b.name)).collect::<Vec<_>>()), false));
                }
                guard()?;
                // Preserve evidence for a retry after dispatch loses its reply.
                self.journal.own_window(&owner.task_ref, &dialog.address, dialog.pid, &dialog.class, &dialog.title,
                    dialog.process_start_ticks, &dialog.compositor_instance)?;
                send(Effect::ClickElement { surface:dialog.key(), element:Box::new((***candidates.first().unwrap()).clone()), button:Button::Left, double:false }).await?;
                receipts.push(json!({"task_ref":owner.task_ref,"window":dialog.address,"dialog":dialog.title,"action":label,"basis":"owned process incarnation; observed native dialog"}));
                last_dialog = Some(dialog.key());
                closing = None;
                actions += 1;
            } else {
                last_dialog = None;
                let main = live[0];
                if closing.as_ref() != Some(&main.key()) {
                    send(Effect::Close(main.key())).await?;
                    closing = Some(main.key());
                    actions += 1;
                }
            }
            sleep(POLL).await;
        }
    }
}
