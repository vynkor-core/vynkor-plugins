pub mod cascade;
pub mod config;
pub mod db;
pub mod embed;
pub mod request;
pub mod rpc;
pub mod schema;
pub mod search;
pub mod service;
pub mod store;
pub mod summary;
pub mod sync;
pub mod worker;

#[cfg(test)]
pub(crate) mod testkit;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
