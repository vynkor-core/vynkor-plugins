//! `chats` plugin — host-wide conversation history (sessions, turns, FTS5
//! search, rev-based sync) in one SQLite file. See README.md.
//!
//! All SQLite work happens in `spawn_blocking` under a single
//! `Mutex<Connection>`: there is exactly one writer, which is what makes the
//! per-transaction revision counter strictly monotonic.

use std::sync::{Arc, Mutex};

use chats_plugin::config::Config;
use chats_plugin::rpc::{Router, Rpc, RpcCall};
use chats_plugin::worker::Worker;
use chats_plugin::{request, schema};
use serde_json::Value;
use tokio::sync::mpsc;
use vynkor_sdk::proto::{
    envelope, ActionResponse, ActionStatus, Envelope, EventPublish, PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "chats";
const PLUGIN_VERSION: &str = "0.1.0";

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_STORAGE".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: request::ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: vynkor_plugin_manifest::action_specs(),
        ..Default::default()
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn action_response(
    action_id: String,
    status: ActionStatus,
    data_json: Vec<u8>,
    error: String,
) -> Envelope {
    Envelope {
        payload: Some(envelope::Payload::ActionResponse(ActionResponse {
            action_id,
            status: status as i32,
            data_json,
            error,
        })),
        ..Default::default()
    }
}

fn event_envelope(event_type: &str, payload: &Value) -> Envelope {
    Envelope {
        payload: Some(envelope::Payload::EventPublish(EventPublish {
            event_type: event_type.to_string(),
            payload_json: payload.to_string().into_bytes(),
        })),
        ..Default::default()
    }
}

async fn serve(mut client: VynkorClient, config: Config) -> Result<(), VynkorError> {
    std::fs::create_dir_all(&config.data_dir).map_err(|e| {
        VynkorError::Internal(format!("cannot create {}: {e}", config.data_dir.display()))
    })?;
    let conn = schema::open(&config.db_path())
        .map_err(|e| VynkorError::Internal(format!("cannot open chats.db: {e}")))?;
    let conn = Arc::new(Mutex::new(conn));
    let config = Arc::new(config);

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

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let mut router = Router::default();

    let worker = Worker::new(Arc::clone(&conn), Rpc::new(rpc_tx), Arc::clone(&config), {
        let out = outbound_tx.clone();
        Arc::new(move || {
            let payload = serde_json::json!({"action": "worker"});
            let _ = out.try_send(event_envelope("changed", &payload));
        })
    });
    let worker_task = worker.spawn();

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env {
                    Ok(e) => e,
                    Err(_) => break,
                };
                match env.payload {
                    Some(envelope::Payload::Ping(ping)) => {
                        let pong = Envelope {
                            payload: Some(envelope::Payload::Pong(Pong {
                                original_timestamp: ping.timestamp,
                                server_timestamp: unix_millis() as u64,
                            })),
                            ..Default::default()
                        };
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    Some(envelope::Payload::Event(event)) => {
                        let _ = client.ack_event(&event.event_id).await;
                    }
                    Some(envelope::Payload::EventPublishAck(_)) => {}
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let conn = Arc::clone(&conn);
                        let config = Arc::clone(&config);
                        let out = outbound_tx.clone();
                        let worker = Arc::clone(&worker);
                        tokio::spawn(async move {
                            let action = req.action.clone();
                            let params = req.params_json.clone();
                            let res = tokio::task::spawn_blocking(move || {
                                let mut guard = conn.lock().unwrap_or_else(|p| p.into_inner());
                                request::handle(&mut guard, &config, &action, &params, unix_millis())
                            })
                            .await
                            .unwrap_or_else(|e| Err(format!("internal error: {e}")));
                            match res {
                                Ok(val) => {
                                    let data = serde_json::to_vec(&val).unwrap_or_default();
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionOk, data, String::new())).await;
                                    if request::is_mutation(&req.action, &val) {
                                        worker.kick();
                                        let payload = serde_json::json!({"action": req.action});
                                        let _ = out.send(event_envelope("changed", &payload)).await;
                                    }
                                }
                                Err(e) => {
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionError, Vec::new(), e)).await;
                                }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => router.incoming(resp),
                    other => {
                        println!("[{PLUGIN_ID}] unhandled message: {other:?}");
                    }
                }
            }
            Some(env) = outbound_rx.recv() => {
                let _ = client.send("kernel", env).await;
            }
            Some(call) = rpc_rx.recv() => {
                router.prune();
                let env = router.outgoing(call);
                let _ = client.send("kernel", env).await;
            }
        }
    }

    if let Some(t) = worker_task {
        t.abort();
    }
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let config = Config::from_env().map_err(VynkorError::Internal)?;
    let socket_path = std::env::var("VYN_SOCKET_PATH")
        .unwrap_or_else(|_| vynkor_wire::socket::default_socket_path());
    let secret = std::env::var("VYN_JWT_SECRET")
        .ok()
        .filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };
    serve(client, config).await
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Reads the shipped plugin.json directly (see vector-db for why not
    // action_specs()): every declared action needs a description and risk,
    // and the declared list must equal what the dispatcher handles.
    #[test]
    fn manifest_actions_have_docs() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(specs.len(), parsed["actions"].as_array().unwrap().len());
        let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        let mut handled: Vec<&str> = chats_plugin::request::ACTIONS.to_vec();
        names.sort();
        handled.sort();
        assert_eq!(
            names, handled,
            "plugin.json actions must match request::ACTIONS"
        );
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            undocumented.is_empty(),
            "missing description: {undocumented:?}"
        );
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(unrisked.is_empty(), "missing risk: {unrisked:?}");
    }
}

#[cfg(test)]
mod serve_tests {
    use std::time::Duration;

    use chats_plugin::config::Config;
    use serde_json::{json, Value};
    use tokio::net::UnixStream;
    use vynkor_sdk::proto::{
        envelope, ActionRequest, ActionStatus, Envelope, PluginRegisterAck, PluginShutdown,
    };
    use vynkor_sdk::VynkorClient;

    async fn call(
        kernel: &mut VynkorClient,
        n: usize,
        action: &str,
        params: Value,
    ) -> Result<Value, String> {
        let req = Envelope {
            payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                action_id: format!("a{n}"),
                action: action.into(),
                params_json: serde_json::to_vec(&params).unwrap(),
                timeout_ms: 0,
                streaming: false,
                caller_plugin_id: "caller_x".into(),
            })),
            ..Default::default()
        };
        kernel.send("chats", req).await.unwrap();
        // the response may be followed by a `changed` event; skip events
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
                .await
                .expect("timed out waiting for response")
                .unwrap();
            match env.payload {
                Some(envelope::Payload::ActionResponse(resp)) => {
                    assert_eq!(resp.action_id, format!("a{n}"));
                    return if resp.status == ActionStatus::ActionOk as i32 {
                        Ok(serde_json::from_slice(&resp.data_json).unwrap())
                    } else {
                        Err(resp.error)
                    };
                }
                Some(envelope::Payload::EventPublish(ev)) => assert_eq!(ev.event_type, "changed"),
                other => panic!("unexpected payload: {other:?}"),
            }
        }
    }

    // Drives the production `serve()` loop end to end over a socket pair.
    #[tokio::test]
    async fn serve_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_lookup(|k| {
            (k == "CHATS_DATA_DIR").then(|| dir.path().join("data").to_string_lossy().into_owned())
        })
        .unwrap();

        let (plugin_side, kernel_side) = UnixStream::pair().unwrap();
        let client = VynkorClient::from_stream(plugin_side, None);
        let mut kernel = VynkorClient::from_stream(kernel_side, None);
        let loop_task = tokio::spawn(super::serve(client, config));

        let reg = tokio::time::timeout(Duration::from_secs(5), kernel.recv())
            .await
            .unwrap()
            .unwrap();
        match reg.payload {
            Some(envelope::Payload::PluginRegister(r)) => {
                let mut declared: Vec<_> = r.manifest.unwrap().actions;
                declared.sort();
                let mut want: Vec<String> = chats_plugin::request::ACTIONS
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                want.sort();
                assert_eq!(declared, want);
            }
            other => panic!("expected registration, got {other:?}"),
        }
        let ack = Envelope {
            payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck {
                accepted: true,
                ..Default::default()
            })),
            ..Default::default()
        };
        kernel.send("chats", ack).await.unwrap();

        let r = call(
            &mut kernel,
            1,
            "chat_session_resolve",
            json!({"device": "lap", "kind": "voice", "source": "daemon"}),
        )
        .await
        .unwrap();
        assert_eq!(r["created"], true);
        let sid = r["session_id"].as_str().unwrap().to_string();

        let r = call(
            &mut kernel,
            2,
            "chat_turn_append",
            json!({"session_id": sid, "user_text": "какая погода", "answer_text": "Солнечно, +20", "status": "ok",
                   "tool_trace": [{"tool": "weather_current", "args": {"city": "Tashkent"}, "result": "sunny", "ms": 12}]}),
        )
        .await
        .unwrap();
        assert_eq!(r["seq"], 1);
        let turn_id = r["turn_id"].as_str().unwrap().to_string();
        let rev = r["rev"].as_i64().unwrap();

        let r = call(&mut kernel, 3, "chat_search", json!({"query": "солнеч"}))
            .await
            .unwrap();
        assert_eq!(r["hits"][0]["turn_id"], turn_id.as_str());
        assert_eq!(r["hits"][0]["session_id"], sid.as_str());

        let r = call(&mut kernel, 4, "chat_sync", json!({"since_rev": 0}))
            .await
            .unwrap();
        assert_eq!(r["rev"], rev);
        assert_eq!(r["more"], false);
        assert_eq!(r["review_pending"], false);
        let ops: Vec<&str> = r["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["op"].as_str().unwrap())
            .collect();
        assert_eq!(ops, vec!["session", "turn"]);
        assert_eq!(
            r["changes"][1]["turn"]["tool_trace"][0]["tool"],
            "weather_current"
        );

        let r = call(&mut kernel, 5, "chat_session_read", json!({"id": sid}))
            .await
            .unwrap();
        assert_eq!(r["turns"].as_array().unwrap().len(), 1);

        let r = call(&mut kernel, 6, "chat_delete", json!({"id": sid}))
            .await
            .unwrap();
        let del_rev = r["rev"].as_i64().unwrap();
        let r = call(&mut kernel, 7, "chat_sync", json!({"since_rev": rev}))
            .await
            .unwrap();
        assert_eq!(
            r["changes"],
            json!([{"rev": del_rev, "op": "delete", "id": sid}])
        );
        let e = call(&mut kernel, 8, "chat_session_read", json!({"id": sid}))
            .await
            .unwrap_err();
        assert_eq!(e, format!("unknown session {sid}"));
        let e = call(&mut kernel, 9, "chat_nope", json!({}))
            .await
            .unwrap_err();
        assert!(e.contains("unknown action"));
        let e = call(
            &mut kernel,
            10,
            "chat_turn_append",
            json!({"session_id": "x"}),
        )
        .await
        .unwrap_err();
        assert!(e.contains("invalid params"), "{e}");

        let shutdown = Envelope {
            payload: Some(envelope::Payload::PluginShutdown(PluginShutdown {
                reason: "test done".into(),
                grace_seconds: 0,
            })),
            ..Default::default()
        };
        kernel.send("chats", shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), loop_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
