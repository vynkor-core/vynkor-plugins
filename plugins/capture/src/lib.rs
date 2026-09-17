//! `capture` plugin — screen capture (screenshot, video record) + local
//! OCR for vynkor plugins. Every backend is a host binary spawned by argv,
//! never a shell. See README.md for the full backend-chain table.

pub mod error;
pub mod paths;
pub mod session;
pub mod spawner;
pub mod screenshot;
pub mod portal;
pub mod ocr;
pub mod record;

/// Single crate-wide lock serializing every test that mutates process env
/// vars (`WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`, `DISPLAY`,
/// `CAPTURE_PLUGIN_DIR`, ...). `cargo test` runs this crate's tests
/// multi-threaded by default (no `--test-threads=1` in CI), so a lock
/// private to one test module does nothing to coordinate against
/// env-mutating tests in a sibling module — they all share the same
/// process-global environment regardless of which file declared them.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
