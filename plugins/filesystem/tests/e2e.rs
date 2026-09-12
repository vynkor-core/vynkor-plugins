//! Fake-kernel e2e for the filesystem plugin.

use std::sync::Arc;

use filesystem_plugin::config::Config;
use filesystem_plugin::handler::Handler;
use filesystem_plugin::sandbox::Sandbox;
use vynkor_sdk::concurrent::serve_concurrent;
use vynkor_sdk::proto::{envelope, ActionRequest, ActionStatus, Envelope, PluginRegisterAck};
use vynkor_sdk::VynkorClient;

async fn spawn_plugin(sandbox: Sandbox, config: Config, plugin_side: tokio::net::UnixStream) {
    let handler = Arc::new(Handler::new(sandbox, config));
    let client = VynkorClient::from_stream(plugin_side, None);
    tokio::spawn(async move {
        let token = String::new();
        let _ = serve_concurrent(client, &token, handler).await;
    });
}

async fn handshake(client: &mut VynkorClient) {
    let reg = client.recv().await.expect("register frame");
    assert!(matches!(reg.payload, Some(envelope::Payload::PluginRegister(_))));
    let ack = Envelope {
        payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck {
            accepted: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    client.send("filesystem", ack).await.expect("ack");
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
    client.send("filesystem", req).await.expect("send action");
    loop {
        let env = client.recv().await.expect("reply");
        match env.payload {
            Some(envelope::Payload::ActionResponse(resp)) => return resp,
            Some(_) => continue,
            None => panic!("empty envelope"),
        }
    }
}

#[tokio::test]
async fn fs_list_on_allowed_root_returns_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
    let sandbox = Sandbox::from_raw_roots(&[dir.path().to_str().unwrap().to_string()]);
    let config = Config { max_list_entries: 100, max_read_bytes: 1024 * 1024 };

    let (plugin_side, kernel_side) = tokio::net::UnixStream::pair().unwrap();
    spawn_plugin(sandbox, config, plugin_side).await;
    let mut kernel = VynkorClient::from_stream(kernel_side, None);
    handshake(&mut kernel).await;

    let params = serde_json::json!({"path": dir.path().to_str().unwrap()});
    let resp = call_action(&mut kernel, "t1", "fs_list", params.to_string().as_bytes()).await;
    assert_eq!(resp.status, ActionStatus::ActionOk as i32);
    let data: serde_json::Value = serde_json::from_slice(&resp.data_json).unwrap();
    let entries = data["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["name"] == "hello.txt"));
}

#[tokio::test]
async fn unknown_action_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::from_raw_roots(&[dir.path().to_str().unwrap().to_string()]);
    let config = Config { max_list_entries: 100, max_read_bytes: 1024 * 1024 };

    let (plugin_side, kernel_side) = tokio::net::UnixStream::pair().unwrap();
    spawn_plugin(sandbox, config, plugin_side).await;
    let mut kernel = VynkorClient::from_stream(kernel_side, None);
    handshake(&mut kernel).await;

    let resp = call_action(&mut kernel, "t2", "fs_frobnicate", b"{}").await;
    assert_eq!(resp.status, ActionStatus::ActionError as i32);
}
