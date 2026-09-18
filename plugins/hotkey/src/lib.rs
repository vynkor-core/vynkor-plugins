//! `hotkey` plugin library: global key-combo triggers for the voice
//! pipeline and other event consumers.
//!
//! Split across three concerns:
//! - [`bindings`] — the pure binding store + trigger normalization (the
//!   only logic with unit tests that must stay dependency-free);
//! - [`portal`] — the XDG GlobalShortcuts portal backend (Wayland-native,
//!   press/release semantics, runtime rebind);
//! - [`request`] — action param validation shared by the binary's dispatch.

pub mod bindings;
pub mod manifest;
pub mod portal;
pub mod request;
