//! Tiny append-only scan logger (SquirrelDisk-AI fork).
//!
//! Every engine decision and phase lands in `<config>/squirreldisk-ai.log`
//! with a timestamp, so "why was my scan slow" is answerable after the fact:
//! elevation state, MFT layout, chunk/parse/assembly timings, fallbacks.

use std::io::Write;

pub fn line(msg: &str) {
    if cfg!(test) {
        return; // tests share one process: no logging into their sandboxes
    }
    let Some(dir) = crate::settings::config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("squirreldisk-ai.log");
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (h, m, s) = ((now / 3600) % 24, (now / 60) % 60, now % 60);
    let _ = writeln!(f, "{h:02}:{m:02}:{s:02} {msg}");
}

/// Is this process elevated? Logged at startup and per scan.
#[cfg(windows)]
pub fn elevated() -> bool {
    std::fs::File::open(r"\\.\C:").is_ok()
}
#[cfg(not(windows))]
pub fn elevated() -> bool {
    true
}
