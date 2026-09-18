//! `clipboard` plugin — read/write the system clipboard via host binaries.
//!
//! Same delivery model as `notify`: spawn well-known host binaries directly
//! with argv — never a shell — so clipboard content cannot inject commands.
//! Wayland uses `wl-paste`/`wl-copy`, X11 tries `xclip` then `xsel`.
//! Text-only v1 (`text/plain`, UTF-8).

pub mod handler;
pub mod history;
pub mod manifest;
pub mod providers;
pub mod lib_rpc;
