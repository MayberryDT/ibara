use anyhow::{Result, ensure};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, InputEvent, KeyCode, RelativeAxisCode, UinputAbsSetup,
};
use serde_json::Value;
use std::collections::BTreeSet;
pub struct Input {
    keyboard: VirtualDevice,
    pointer: VirtualDevice,
    held: BTreeSet<u16>,
    width: u32,
    height: u32,
}
impl Input {
    pub fn new(w: u32, h: u32) -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for n in 1..256 {
            keys.insert(KeyCode(n));
        }
        let keyboard = VirtualDevice::builder()?
            .name("Ibara Screen Keyboard")
            .with_keys(&keys)?
            .build()?;
        let mut buttons = AttributeSet::<KeyCode>::new();
        for n in 272..280 {
            buttons.insert(KeyCode(n));
        }
        let mut rel = AttributeSet::new();
        for axis in [
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
        ] {
            rel.insert(axis);
        }
        let pointer = VirtualDevice::builder()?
            .name("Ibara Screen Pointer")
            .with_keys(&buttons)?
            .with_relative_axes(&rel)?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_X,
                AbsInfo::new(0, 0, w as i32 - 1, 0, 0, 0),
            ))?
            .with_absolute_axis(&UinputAbsSetup::new(
                AbsoluteAxisCode::ABS_Y,
                AbsInfo::new(0, 0, h as i32 - 1, 0, 0, 0),
            ))?
            .build()?;
        Ok(Self {
            keyboard,
            pointer,
            held: BTreeSet::new(),
            width: w,
            height: h,
        })
    }
    pub fn held(&self) -> usize {
        self.held.len()
    }
    pub fn deliver(&mut self, e: &Value) -> Result<()> {
        if e.get("release_all") == Some(&Value::Bool(true)) {
            return self.settle();
        }
        if let Some(a) = e.get("move").and_then(Value::as_array) {
            ensure!(a.len() == 2, "invalid move");
            let x = a[0].as_f64().ok_or_else(|| anyhow::anyhow!("invalid x"))?;
            let y = a[1].as_f64().ok_or_else(|| anyhow::anyhow!("invalid y"))?;
            ensure!(x.is_finite() && y.is_finite(), "invalid motion");
            self.pointer.emit(&[
                InputEvent::new(3, 0, x.clamp(0., (self.width - 1) as f64) as i32),
                InputEvent::new(3, 1, y.clamp(0., (self.height - 1) as f64) as i32),
            ])?;
            return Ok(());
        }
        for (name, button) in [("key", false), ("button", true)] {
            if let Some(a) = e.get(name).and_then(Value::as_array) {
                ensure!(a.len() == 2, "invalid key");
                let code = a[0]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("invalid code"))?;
                ensure!(
                    if button {
                        (272..280).contains(&code)
                    } else {
                        (1..256).contains(&code)
                    },
                    "invalid code"
                );
                let down = a[1]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("invalid key state"))?;
                let d = if button {
                    &mut self.pointer
                } else {
                    &mut self.keyboard
                };
                d.emit(&[InputEvent::new(1, code as u16, i32::from(down))])?;
                if down {
                    self.held.insert(code as u16);
                } else {
                    self.held.remove(&(code as u16));
                }
                return Ok(());
            }
        }
        if let Some(a) = e.get("wheel").and_then(Value::as_array) {
            ensure!(a.len() == 2, "invalid wheel");
            let dx = a[0]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("invalid wheel"))?
                .clamp(-12000, 12000) as i32;
            let dy = a[1]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("invalid wheel"))?
                .clamp(-12000, 12000) as i32;
            self.pointer.emit(&[
                InputEvent::new(2, 12, dx),
                InputEvent::new(2, 11, dy),
                InputEvent::new(2, 6, dx / 120),
                InputEvent::new(2, 8, dy / 120),
            ])?;
            return Ok(());
        }
        anyhow::bail!("unknown input event")
    }
    pub fn settle(&mut self) -> Result<()> {
        let keys: Vec<_> = self.held.iter().copied().collect();
        let mut failure = None;
        for code in keys {
            let d = if code >= 272 {
                &mut self.pointer
            } else {
                &mut self.keyboard
            };
            match d.emit(&[InputEvent::new(1, code, 0)]) {
                Ok(()) => {
                    self.held.remove(&code);
                }
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            return Err(e.into());
        }
        Ok(())
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}
