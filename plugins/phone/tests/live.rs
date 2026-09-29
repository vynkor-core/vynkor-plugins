//! Live smoke test against a real phone. Ignored by default; run with
//!   PHONE_PLUGIN_LIVE=1 cargo test --manifest-path plugins/phone/Cargo.toml --test live -- --ignored --nocapture
//! Uses the same env as the plugin (PHONE_PLUGIN_SSH_HOST, ...). Takes a back and a
//! front photo and runs a 3 s stream; the phone camera must be free.

use std::time::Duration;

use phone_plugin::config::Config;
use phone_plugin::framing::is_jpeg;
use phone_plugin::params::{PhotoParams, StreamParams};
use phone_plugin::stream::StreamRegistry;
use phone_plugin::{helper, photo, status, transport};
use serde_json::json;

#[tokio::test]
#[ignore]
async fn live_status_setup_photos_and_stream() {
    if std::env::var("PHONE_PLUGIN_LIVE").as_deref() != Ok("1") {
        eprintln!("PHONE_PLUGIN_LIVE!=1; skipping");
        return;
    }
    let cfg = Config::from_env().unwrap();
    cfg.ensure_dir().unwrap();
    let t = transport::from_config(&cfg);
    let reg = StreamRegistry::new();

    let s = status::status(t.as_ref(), &cfg, &reg).await;
    println!("status: {s}");
    assert_eq!(s["reachable"], json!(true), "phone unreachable: {s}");

    let r = helper::setup(t.as_ref(), &cfg).await.unwrap();
    println!("setup: {r:?}");

    for cam in ["back", "front"] {
        let p = PhotoParams::parse(&json!({"camera": cam, "width": 640, "height": 480})).unwrap();
        let v = photo::take(t.as_ref(), &cfg, &reg, &p).await.unwrap();
        println!("photo {cam}: {v}");
        let bytes = std::fs::read(v["path"].as_str().unwrap()).unwrap();
        assert!(is_jpeg(&bytes), "{cam} photo is not a JPEG");
        assert!(bytes.len() > 5_000, "{cam} photo suspiciously small: {}", bytes.len());
    }

    let sp = StreamParams::parse(&json!({"width": 640, "height": 480, "fps": 15, "flash": false})).unwrap();
    let started = reg.start(t.as_ref(), &cfg, &sp).await.unwrap();
    println!("stream: {started}");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let st = reg.status().await;
    println!("stream status: {st}");
    assert!(st["frames"].as_u64().unwrap_or(0) >= 10, "too few frames: {st}");
    let stopped = reg.stop(None).await.unwrap();
    println!("stopped: {stopped}");
    assert_eq!(stopped["stopped"], json!(true));
}
