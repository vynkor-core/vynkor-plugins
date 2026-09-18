//! `notify` plugin — desktop/system notifications on the host, gated by
//! `PERMISSION_NOTIFY`. See README.md.
//!
//! Delivery spawns external binaries (`notify-send`, `wall`, `espeak-ng`/
//! `espeak`) with argv only — never a shell — so message/title content
//! cannot inject commands. v0.2 adds silent inbox notifications
//! (`silent: true` + `notify_list`/`notify_mark_read`/`notify_delete`) and
//! tts озвучка (`speak: true`, routed through the `tts` plugin).
//!
//! Doesn't use the SDK's `Plugin::run`/`serve` loop: `Plugin::on_message`
//! only gets `&mut self`, not `&mut VynkorClient`, and there is no way to
//! get a second client for the outbound `send_action` call into `tts` —
//! the kernel rejects a second connection under the same `plugin_id`
//! (`vynkor/src/plugins/registry.rs`) and rejects any traffic from an
//! unregistered connection (`vynkor/src/ipc/protocol.rs`). So this plugin
//! drives its own loop, near-identical to the SDK's `serve()`, but calls
//! the `notify_send` handler with the loop's own `&mut VynkorClient` in
//! hand (same rationale and structure as the `ai` plugin). Sequential, one
//! request at a time — same model `ping-pong-rs` and `ai` use.

use notify_plugin::{handler, push};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "notify";
const PLUGIN_VERSION: &str = "0.1.1";

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_NOTIFY".into(), "PERMISSION_NETWORK".into()],
        actions: vec![
            "notify_send".to_string(),
            "notify_providers".to_string(),
            "notify_list".to_string(),
            "notify_mark_read".to_string(),
            "notify_delete".to_string(),
            "push_send".to_string(),
        ],
        action_specs: notify_plugin::manifest::action_specs(),
        ..Default::default()
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn handle_action_request(client: &mut VynkorClient, req: ActionRequest) -> Envelope {
    let outcome = match req.action.as_str() {
        "notify_send" => handler::handle_notify_send(client, &req.params_json).await,
        "notify_providers" => handler::handle_notify_providers(),
        "notify_list" => handler::handle_notify_list(&req.params_json),
        "notify_mark_read" => handler::handle_notify_mark_read(&req.params_json),
        "notify_delete" => handler::handle_notify_delete(&req.params_json),
        "push_send" => push::handle_push_send(client, &req.params_json).await,
        other => {
            return Envelope {
                payload: Some(envelope::Payload::ActionResponse(ActionResponse {
                    action_id: req.action_id,
                    status: ActionStatus::ActionNotFound as i32,
                    data_json: Vec::new(),
                    error: format!("unknown action: {other}"),
                })),
                ..Default::default()
            };
        }
    };
    let reply = match outcome {
        Ok(data_json) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionOk as i32,
            data_json,
            error: String::new(),
        },
        Err(error) => ActionResponse {
            action_id: req.action_id,
            status: ActionStatus::ActionError as i32,
            data_json: Vec::new(),
            error,
        },
    };
    Envelope {
        payload: Some(envelope::Payload::ActionResponse(reply)),
        ..Default::default()
    }
}

async fn serve(mut client: VynkorClient) -> Result<(), VynkorError> {
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client
        .register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token)
        .await?;
    if !ack.accepted {
        return Err(VynkorError::PermissionDenied(format!(
            "registration rejected: {}",
            ack.reject_reason
        )));
    }
    println!("[{PLUGIN_ID}] registered with kernel");

    loop {
        let env = match client.recv().await {
            Ok(env) => env,
            Err(_) => break, // disconnect / EOF
        };
        match env.payload {
            Some(envelope::Payload::Ping(ping)) => {
                let pong = Envelope {
                    payload: Some(envelope::Payload::Pong(Pong {
                        original_timestamp: ping.timestamp,
                        server_timestamp: unix_millis(),
                    })),
                    ..Default::default()
                };
                let _ = client.send("kernel", pong).await;
            }
            Some(envelope::Payload::PluginShutdown(_)) => break,
            Some(envelope::Payload::Event(event)) => {
                // notify declares no event subscriptions; ack defensively so
                // the kernel doesn't retry anything unexpectedly delivered.
                let _ = client.ack_event(&event.event_id).await;
            }
            Some(envelope::Payload::ActionRequest(req)) => {
                let resp = handle_action_request(&mut client, req).await;
                let _ = client.send("kernel", resp).await;
            }
            other => {
                println!("[{PLUGIN_ID}] unhandled message: {other:?}");
            }
        }
    }

    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let socket_path = std::env::var("VYN_SOCKET_PATH")
        .unwrap_or_else(|_| vynkor_wire::socket::default_socket_path());
    let secret = std::env::var("VYN_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };

    serve(client).await
}
