use crate::stroke::Stroke;
use anyhow::Result;

mod wayland;

pub enum Outcome {
    Spawn(Stroke),
    Cancelled,
}

pub fn run() -> Result<Outcome> {
    wayland::run()
}
