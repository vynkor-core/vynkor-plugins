//! `phone` plugin — camera (and later audio/hardware) of a phone reached over
//! ssh or run locally. Every remote action is a structured [`remote::RemoteCmd`]
//! executed by a [`transport::Transport`]; nothing caller-supplied ever reaches
//! a command line. See README.md and the design spec.

pub mod config;
pub mod error;
pub mod framing;
pub mod helper;
pub mod params;
pub mod photo;
pub mod remote;
// pub mod status;
pub mod storage;
pub mod stream;
pub mod transport;
