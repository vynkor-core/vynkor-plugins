//! MTProto transport layer: per-account session pool + antiban rate limiting.
//!
//! [`session::SessionPool`] owns one [`grammers_client::Client`] per Telegram
//! account; [`antiban::Antiban`] guards every MTProto call with a per-account
//! token bucket, FloodWait jitter and a circuit breaker so a banned account
//! fails fast instead of hammering Telegram.

pub mod antiban;
pub mod session;

pub use antiban::Antiban;
pub use session::SessionPool;
