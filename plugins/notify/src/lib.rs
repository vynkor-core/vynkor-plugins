//! `notify` plugin — desktop/system notifications on the host, delivered by
//! spawning external binaries (`notify-send`, `wall`, `espeak-ng`/`espeak`).
//!
//! All delivery uses argv only — never a shell — so message/title content
//! cannot inject commands. See README.md for the provider matrix,
//! configuration, and security notes.

pub mod handler;
pub mod inbox;
pub mod manifest;
pub mod providers;
pub mod push;
pub mod request;
