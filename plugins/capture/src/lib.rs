//! `capture` plugin — screen capture (screenshot, video record) + local
//! OCR for vynkor plugins. Every backend is a host binary spawned by argv,
//! never a shell. See README.md for the full backend-chain table.

pub mod error;
pub mod paths;
pub mod session;
pub mod spawner;
pub mod screenshot;
