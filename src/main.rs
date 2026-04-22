use anyhow::{Context, Result};
use clap::Parser;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

mod backend;
mod overlay;
mod stroke;

#[derive(Parser)]
#[command(name = "magicwand", version, about = "Draw a gesture to spawn a floating window")]
struct Cli {
    /// Command to spawn at the drawn bbox (e.g. "kitty").
    #[arg(long, short = 's')]
    spawn: String,

    /// Minimum width in pixels if stroke bbox is smaller.
    #[arg(long, default_value_t = 400)]
    min_width: u32,

    /// Minimum height in pixels if stroke bbox is smaller.
    #[arg(long, default_value_t = 300)]
    min_height: u32,

    /// Extra pixels added to the bbox on each side.
    #[arg(long, default_value_t = 8)]
    padding: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Single-instance: if another magicwand is already running, do nothing.
    // The lock is held by the OS until this process exits.
    let _lock = match acquire_single_instance_lock()? {
        Some(f) => f,
        None => return Ok(()),
    };

    let backend = backend::detect().context("no supported window manager detected")?;

    let outcome = overlay::run().context("overlay failed")?;

    let stroke = match outcome {
        overlay::Outcome::Spawn(s) => s,
        overlay::Outcome::Cancelled => return Ok(()),
    };

    let raw = stroke.bbox(cli.padding);
    if raw.w == 0 && raw.h == 0 {
        return Ok(());
    }

    let bbox = raw.enforce_min(cli.min_width, cli.min_height);
    backend.spawn_floating(&cli.spawn, bbox)?;
    Ok(())
}

fn lock_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc_geteuid() };
            PathBuf::from(format!("/tmp/magicwand-{uid}"))
        });
    dir.join("magicwand.lock")
}

unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

/// Returns `Some(File)` if we acquired the single-instance lock (caller must
/// hold the file alive for the lock to persist), `None` if another instance
/// already has it.
fn acquire_single_instance_lock() -> Result<Option<File>> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("acquiring lock"),
    }
}
