use crate::stroke::Bbox;
use anyhow::{bail, Result};

mod hyprland;

pub trait WmBackend {
    fn spawn_floating(&self, cmd: &str, bbox: Bbox) -> Result<()>;
}

pub fn detect() -> Result<Box<dyn WmBackend>> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Ok(Box::new(hyprland::Hyprland));
    }
    bail!("no supported backend (set HYPRLAND_INSTANCE_SIGNATURE); more backends coming in later milestones");
}
