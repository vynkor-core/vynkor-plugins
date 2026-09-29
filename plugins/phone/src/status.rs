//! `phone_status`: is the phone reachable, is the helper installed, is a
//! stream running. Never fails: an unreachable phone is a *result*.

use std::time::Instant;

use serde_json::{json, Value};

use crate::config::Config;
use crate::error::PhoneError;
use crate::helper;
use crate::stream::StreamRegistry;
use crate::transport::Transport;

pub async fn status(t: &dyn Transport, cfg: &Config, reg: &StreamRegistry) -> Value {
    let started = Instant::now();
    let checked = helper::check(t, cfg).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let stream_active = reg.is_active().await;
    let (reachable, helper_state, error) = match checked {
        Ok(s) => (true, s.as_str(), None),
        Err(PhoneError::Unreachable(m)) => (false, "unknown", Some(PhoneError::Unreachable(m).to_string())),
        Err(e) => (true, "unknown", Some(e.to_string())),
    };
    let mut v = json!({
        "transport": t.describe(),
        "host": cfg.ssh_host,
        "reachable": reachable,
        "helper": helper_state,
        "stream_active": stream_active,
        "latency_ms": latency_ms,
    });
    if let Some(e) = error {
        v["error"] = json!(e);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake::FakeTransport;
    use crate::transport::Output;

    fn cfg() -> Config {
        Config::from_lookup(|_| None).unwrap()
    }

    #[tokio::test]
    async fn reports_ok_missing_and_unreachable_without_erroring() {
        let reg = StreamRegistry::new();

        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(format!("{}\n", helper::file_name()).into_bytes()));
        let v = status(&t, &cfg(), &reg).await;
        assert_eq!(v["reachable"], json!(true));
        assert_eq!(v["helper"], json!("ok"));
        assert_eq!(v["transport"], json!("fake"));
        assert_eq!(v["stream_active"], json!(false));

        let t = FakeTransport::new();
        t.push_run(FakeTransport::ok(Vec::new()));
        assert_eq!(status(&t, &cfg(), &reg).await["helper"], json!("missing"));

        let t = FakeTransport::new();
        t.push_run(Ok(Output { code: 255, stdout: vec![], stderr: "no route".into() }));
        let v = status(&t, &cfg(), &reg).await;
        assert_eq!(v["reachable"], json!(false));
        assert!(v["error"].as_str().unwrap().starts_with("ERR_PHONE_UNREACHABLE"));

        let t = FakeTransport::new(); // no scripted result -> backend error, but reachable
        let v = status(&t, &cfg(), &reg).await;
        assert_eq!(v["reachable"], json!(true));
        assert!(v["error"].as_str().unwrap().starts_with("ERR_PHONE_BACKEND"));
    }
}
