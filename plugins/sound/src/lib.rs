//! `sound` plugin — the single owner of the speakers.
//!
//! Audio output primitive: `sound_play` spawns a well-known host player
//! binary directly with argv — never a shell — and returns immediately;
//! playback continues in the background. `sound_stop` kills the current
//! clip (or a specific one), `sound_status` reports what is playing.
//! Provider chain: `pw-cat --playback` → `paplay` → `aplay` for wav, and
//! `ffplay` for every other format. Same delivery model as `clipboard` /
//! `notify`: argv-only spawn of host binaries.
//!
//! Scope and non-goals: see ROADMAP.md. All logic lives in the `handler`
//! and `players` modules; `main.rs` only wires the SDK loop.

pub mod handler;
pub mod manifest;
pub mod players;
