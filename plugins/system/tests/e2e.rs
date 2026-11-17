//! Fake-kernel end-to-end: drive the real SDK serve loop over a
//! `UnixStream::pair` — registration handshake included (see
//! `docs/PLUGIN_AUTHORING.md` §3). Uses empty backends so outcomes are
//! deterministic without a desktop: `sys_info` works everywhere, missing
//! capabilities answer `ERR_SYS_NOT_SUPPORTED`.

use std::sync::Arc;

use system_plugin::backends::SystemBackends;
use system_plugin::{SystemPlugin, PLUGIN_ID};
use vynkor_sdk::proto::{envelope, ActionRequest, ActionStatus, Envelope, PluginRegisterAck};
use vynkor_sdk::{Plugin, VynkorClient};

async fn spawn_plugin(kernel_side: tokio::net::UnixStream) {
    let mut plugin = SystemPlugin::new(Arc::new(SystemBackends::default()));
    let client = VynkorClient::from_stream(kernel_side, None);
    tokio::spawn(async move { plugin.serve(client, "").await });
}

/// The shim must ack the register frame before anything else —
/// `register_full` treats the very next inbound frame as the ack.
async fn handshake(client: &mut VynkorClient) {
    let reg = client.recv().await.expect("register frame");
    assert!(matches!(
        reg.payload,
        Some(envelope::Payload::PluginRegister(_))
    ));

    let ack = Envelope {
        payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck {
            accepted: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    client.send(PLUGIN_ID, ack).await.expect("ack");
}

async fn call_action(
    client: &mut VynkorClient,
    action_id: &str,
    action: &str,
    params_json: &[u8],
) -> vynkor_sdk::proto::ActionResponse {
    let req = Envelope {
        payload: Some(envelope::Payload::ActionRequest(ActionRequest {
            action_id: action_id.to_string(),
            action: action.to_string(),
            params_json: params_json.to_vec(),
            ..Default::default()
        })),
        ..Default::default()
    };
    client.send(PLUGIN_ID, req).await.expect("send action");

    loop {
        let env = client.recv().await.expect("reply");
        match env.payload {
            Some(envelope::Payload::ActionResponse(resp)) => return resp,
            Some(_) => continue,
            None => panic!("empty envelope from serve loop"),
        }
    }
}

#[tokio::test]
async fn registration_then_action_roundtrip() {
    let (plugin_side, kernel_side) = tokio::net::UnixStream::pair().expect("socket pair");
    spawn_plugin(plugin_side).await;

    let mut kernel = VynkorClient::from_stream(kernel_side, None);
    handshake(&mut kernel).await;

    let resp = call_action(&mut kernel, "t1", "sys_info", b"").await;
    assert_eq!(resp.action_id, "t1");
    assert_eq!(resp.status, ActionStatus::ActionOk as i32);
    let data: serde_json::Value =
        serde_json::from_slice(&resp.data_json).expect("data_json is json");
    assert!(
        data.get("arch").is_some(),
        "sys_info must carry arch: {data}"
    );
}

#[tokio::test]
async fn missing_backend_surfaces_not_supported_over_the_wire() {
    let (plugin_side, kernel_side) = tokio::net::UnixStream::pair().expect("socket pair");
    spawn_plugin(plugin_side).await;

    let mut kernel = VynkorClient::from_stream(kernel_side, None);
    handshake(&mut kernel).await;

    let resp = call_action(&mut kernel, "t2", "sys_battery", b"").await;
    assert_eq!(resp.action_id, "t2");
    assert_eq!(resp.status, ActionStatus::ActionError as i32);
    assert!(
        resp.error.starts_with("ERR_SYS_NOT_SUPPORTED"),
        "error was: {}",
        resp.error
    );
}

#[tokio::test]
async fn unknown_action_is_action_not_found_status() {
    let (plugin_side, kernel_side) = tokio::net::UnixStream::pair().expect("socket pair");
    spawn_plugin(plugin_side).await;

    let mut kernel = VynkorClient::from_stream(kernel_side, None);
    handshake(&mut kernel).await;

    let resp = call_action(&mut kernel, "t3", "sys_frobnicate", b"").await;
    assert_eq!(resp.status, ActionStatus::ActionNotFound as i32);
}

#[tokio::test]
async fn bad_params_rejected_over_the_wire() {
    let (plugin_side, kernel_side) = tokio::net::UnixStream::pair().expect("socket pair");
    spawn_plugin(plugin_side).await;

    let mut kernel = VynkorClient::from_stream(kernel_side, None);
    handshake(&mut kernel).await;

    let resp = call_action(&mut kernel, "t4", "sys_volume_set", br#"{"percent": 500}"#).await;
    assert_eq!(resp.status, ActionStatus::ActionError as i32);
    // Volume backend absent AND params invalid — parse errors win.
    assert!(
        resp.error.starts_with("ERR_SYS_BAD_PARAMS"),
        "error was: {}",
        resp.error
    );
}
