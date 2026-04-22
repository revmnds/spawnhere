use crate::stroke::Bbox;
use anyhow::{bail, Context, Result};
use std::process::Command;

pub struct Hyprland;

impl super::WmBackend for Hyprland {
    fn spawn_floating(&self, cmd: &str, bbox: Bbox) -> Result<()> {
        let payload = format!(
            "[float;move {} {};size {} {}] {}",
            bbox.x.max(0),
            bbox.y.max(0),
            bbox.w,
            bbox.h,
            cmd
        );

        let status = Command::new("hyprctl")
            .args(["dispatch", "exec", &payload])
            .status()
            .context("executing hyprctl — is Hyprland running?")?;

        if !status.success() {
            bail!("hyprctl dispatch exec exited with {status}");
        }
        Ok(())
    }
}
