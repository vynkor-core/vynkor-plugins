use std::collections::HashMap;
use std::sync::Arc;

use metrics_plugin::{handle_action, sample_and_store, Config, Rpc, RpcCall};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish, Pong, PluginManifest};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "metrics";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 4] = ["metrics_query", "metrics_latest", "metrics_stats", "status"];

fn manifest() -> PluginManifest {
    PluginManifest { permissions: vec!["PERMISSION_STORAGE".into(), "PERMISSION_EVENT_PUBLISH".into()], actions: ACTIONS.iter().map(|s| s.to_string()).collect(), action_specs: vynkor_plugin_manifest::action_specs(), ..Default::default() }
}

fn unix_millis() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

fn action_response(action_id: String, status: ActionStatus, data_json: Vec<u8>, error: String) -> Envelope {
    Envelope { payload: Some(envelope::Payload::ActionResponse(ActionResponse { action_id, status: status as i32, data_json, error })), ..Default::default() }
}
fn event_envelope(event_type: &str, payload: &Value) -> Envelope {
    Envelope { payload: Some(envelope::Payload::EventPublish(EventPublish { event_type: event_type.to_string(), payload_json: payload.to_string().into_bytes() })), ..Default::default() }
}

async fn serve(mut client: VynkorClient, config: Config) -> Result<(), VynkorError> {
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client.register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token).await?;
    if !ack.accepted { return Err(VynkorError::PermissionDenied(format!("rejected: {}", ack.reject_reason))); }
    println!("[{PLUGIN_ID}] registered interval {}s", config.interval_secs);
    let start = std::time::Instant::now();
    let config = Arc::new(config);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Rpc::new(rpc_tx);
    let mut pending: HashMap<String, (String, oneshot::Sender<Result<Value,String>>)> = HashMap::new();
    let mut seq: u64 = 0;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(config.interval_secs.max(1)));
    // first tick immediately is not desired for metrics - skip first immediate tick
    interval.tick().await;

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env { Ok(e)=>e, Err(_)=>break };
                match env.payload {
                    Some(envelope::Payload::Ping(p)) => {
                        let pong = Envelope { payload: Some(envelope::Payload::Pong(Pong { original_timestamp: p.timestamp, server_timestamp: unix_millis() })), ..Default::default() };
                        let _ = client.send("kernel", pong).await;
                    }
                    Some(envelope::Payload::PluginShutdown(_)) => break,
                    Some(envelope::Payload::Event(e)) => { let _ = client.ack_event(&e.event_id).await; }
                    Some(envelope::Payload::EventPublishAck(_)) => {}
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let rpc = rpc.clone();
                        let out = outbound_tx.clone();
                        let cfg = Arc::clone(&config);
                        let start = start;
                        tokio::spawn(async move {
                            match handle_action(rpc, &cfg, &req.action, &req.params_json, start).await {
                                Ok(res) => {
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionOk, res.data, String::new())).await;
                                    if let Some((t,p)) = res.event { let _ = out.send(event_envelope(&t, &p)).await; }
                                }
                                Err(e) => { let _ = out.send(action_response(req.action_id, ActionStatus::ActionError, Vec::new(), e)).await; }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some((action, reply)) = pending.remove(&resp.action_id) {
                            let result = if resp.status==ActionStatus::ActionOk as i32 { serde_json::from_slice::<Value>(&resp.data_json).map_err(|e| format!("malformed: {e}")) } else { Err(format!("{action} failed: {}", resp.error)) };
                            let _ = reply.send(result);
                        }
                    }
                    other => println!("[{PLUGIN_ID}] unhandled {other:?}"),
                }
            }
            Some(env) = outbound_rx.recv() => { let _ = client.send("kernel", env).await; }
            Some(call) = rpc_rx.recv() => {
                seq+=1;
                let action_id = format!("rpc-{seq}");
                pending.insert(action_id.clone(), (call.action.clone(), call.reply));
                let env = Envelope { payload: Some(envelope::Payload::ActionRequest(ActionRequest { action_id, action: call.action, params_json: call.params_json, timeout_ms: call.timeout_ms, streaming: false, ..Default::default() })), ..Default::default() };
                let _ = client.send("kernel", env).await;
            }
            _ = interval.tick() => {
                let rpc = rpc.clone();
                let out = outbound_tx.clone();
                let cfg = Arc::clone(&config);
                tokio::spawn(async move {
                    match sample_and_store(rpc, &cfg).await {
                        Ok(id) => {
                            // publish sample event best-effort
                            let payload = serde_json::json!({"id": id});
                            let _ = out.send(event_envelope("sample", &payload)).await;
                        }
                        Err(e) => eprintln!("[metrics] sample failed: {e}"),
                    }
                });
            }
        }
    }
    println!("[{PLUGIN_ID}] shutting down");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), VynkorError> {
    let config = Config::from_env();
    let client = VynkorClient::connect_from_env().await?;
    serve(client, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap as StdHashMap};
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::sync::Mutex;
    use vynkor_sdk::proto::{ActionResponse as ProtoActionResponse, EventPublishAck, EventPublishStatus, PluginRegisterAck};

    type Published = Arc<Mutex<Vec<(String, Value)>>>;

    #[derive(Default)]
    struct FakeDb { kv: BTreeMap<String, Value> }
    impl FakeDb {
        fn handle(&mut self, action: &str, params: Value) -> Result<Value, String> {
            fn key(p: &Value) -> String { p.get("key").and_then(Value::as_str).unwrap_or_default().to_string() }
            match action {
                "db_incr" => { let k=key(&params); let d=params.get("delta").and_then(Value::as_i64).unwrap_or(1); let cur=self.kv.get(&k).and_then(Value::as_i64).unwrap_or(0); let nxt=cur+d; self.kv.insert(k, serde_json::json!(nxt)); Ok(serde_json::json!({"ok":true,"value":nxt})) }
                "db_set" => { let k=key(&params); self.kv.insert(k, params.get("value").cloned().unwrap_or(Value::Null)); Ok(serde_json::json!({"ok":true})) }
                "db_get" => { let k=key(&params); match self.kv.get(&k) { Some(v)=>Ok(serde_json::json!({"found":true,"value":v})), None=>Ok(serde_json::json!({"found":false,"value":null})) } }
                "db_keys" => { let prefix=params.get("prefix").and_then(Value::as_str).unwrap_or(""); let keys: Vec<&String>=self.kv.keys().filter(|k| k.starts_with(prefix)).collect(); Ok(serde_json::json!({"keys":keys})) }
                "db_batch_get" => { let mut values=serde_json::Map::new(); if let Some(keys)=params.get("keys").and_then(Value::as_array) { for k in keys { if let Some(k)=k.as_str() { values.insert(k.to_string(), self.kv.get(k).cloned().unwrap_or(Value::Null)); } } } Ok(serde_json::json!({"values":values})) }
                "db_delete" => { let k=key(&params); Ok(serde_json::json!({"deleted":self.kv.remove(&k).is_some()})) }
                other => Err(format!("unknown {other}")),
            }
        }
    }

    enum Cmd { Call { action: String, params: Value, reply: oneshot::Sender<Result<Value,String>> } }
    struct Shim { tx: mpsc::Sender<Cmd>, published: Published }
    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value,String> {
            let (tx,rx)=oneshot::channel();
            self.tx.send(Cmd::Call { action: action.into(), params, reply: tx }).await.expect("shim died");
            tokio::time::timeout(Duration::from_secs(5), rx).await.expect("timeout").expect("dropped")
        }
        async fn published(&self) -> Vec<(String, Value)> { self.published.lock().await.clone() }
    }

    async fn start_plugin(cfg: Config) -> Shim {
        let (ps, ks)=UnixStream::pair().unwrap();
        let pc=VynkorClient::from_stream(ps, None);
        let kc=VynkorClient::from_stream(ks, None);
        tokio::spawn(async move { let _=serve(pc,cfg).await; });
        let (tx, rx)=mpsc::channel(16);
        let published=Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(run_shim(kc, rx, published.clone()));
        Shim { tx, published }
    }

    async fn run_shim(mut kernel: VynkorClient, mut rx: mpsc::Receiver<Cmd>, published: Published) {
        let mut db=FakeDb::default();
        let mut pending: StdHashMap<String, oneshot::Sender<Result<Value,String>>> = StdHashMap::new();
        let mut seq: u64=0;
        loop {
            let env=tokio::time::timeout(Duration::from_secs(5), kernel.recv()).await.expect("timeout").expect("closed");
            if let Some(envelope::Payload::PluginRegister(_))=env.payload {
                let _=kernel.send("metrics", Envelope { payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck { accepted:true, ..Default::default() })), ..Default::default() }).await;
                break;
            }
        }
        loop {
            tokio::select! {
                env = kernel.recv() => {
                    let env=match env {Ok(e)=>e, Err(_)=>break};
                    match env.payload {
                        Some(envelope::Payload::ActionRequest(req)) => {
                            let params: Value=serde_json::from_slice(&req.params_json).unwrap_or(Value::Null);
                            let resp=match db.handle(&req.action, params) {
                                Ok(v)=>ProtoActionResponse { action_id: req.action_id, status: ActionStatus::ActionOk as i32, data_json: serde_json::to_vec(&v).unwrap(), error: String::new() },
                                Err(e)=>ProtoActionResponse { action_id: req.action_id, status: ActionStatus::ActionError as i32, data_json: Vec::new(), error: e },
                            };
                            let _=kernel.send("metrics", Envelope { payload: Some(envelope::Payload::ActionResponse(resp)), ..Default::default() }).await;
                        }
                        Some(envelope::Payload::ActionResponse(r)) => { if let Some(tx)=pending.remove(&r.action_id) { let res=if r.status==ActionStatus::ActionOk as i32 { serde_json::from_slice(&r.data_json).map_err(|e| e.to_string()) } else { Err(r.error) }; let _=tx.send(res); } }
                        Some(envelope::Payload::EventPublish(ev)) => { published.lock().await.push((ev.event_type.clone(), serde_json::from_slice(&ev.payload_json).unwrap_or(Value::Null))); let _=kernel.send("metrics", Envelope { payload: Some(envelope::Payload::EventPublishAck(EventPublishAck { event_id: format!("ev-{seq}"), status: EventPublishStatus::EventPublishOk as i32, error: String::new() })), ..Default::default() }).await; seq+=1; }
                        Some(envelope::Payload::Ping(p)) => { let _=kernel.send("metrics", Envelope { payload: Some(envelope::Payload::Pong(Pong { original_timestamp: p.timestamp, server_timestamp: unix_millis() })), ..Default::default() }).await; }
                        _=>{}
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Call { action, params, reply }) => { seq+=1; let id=format!("t-{seq}"); pending.insert(id.clone(), reply); let env=Envelope { payload: Some(envelope::Payload::ActionRequest(ActionRequest { action_id: id, action, params_json: serde_json::to_vec(&params).unwrap(), timeout_ms:0, streaming:false, ..Default::default() })), ..Default::default() }; let _=kernel.send("metrics", env).await; }
                        None=>break,
                    }
                }
            }
        }
    }

    fn test_cfg() -> Config { Config { interval_secs: 3600, max_samples: 100, db_timeout_ms: 5000 } }

    #[tokio::test]
    async fn query_empty() {
        let shim=start_plugin(test_cfg()).await;
        let res=shim.call("metrics_query", serde_json::json!({})).await.unwrap();
        assert_eq!(res["total"], 0);
    }

    #[tokio::test]
    async fn stats_empty() {
        let shim=start_plugin(test_cfg()).await;
        let res=shim.call("metrics_stats", serde_json::json!({})).await.unwrap();
        assert_eq!(res["count"], 0);
    }

    #[tokio::test]
    async fn status_ok() {
        let shim=start_plugin(test_cfg()).await;
        let res=shim.call("status", serde_json::json!({})).await.unwrap();
        assert_eq!(res["version"], "0.1.0");
        assert!(res["uptime_ms"].as_u64().is_some());
    }
}

#[cfg(test)]
mod manifest_specs_tests {
    use serde_json::Value;

    // Regression guard: an action that reaches the model with an empty
    // description gets dropped by the agent's embedding filter, and one
    // with no risk label falls through to the agent's name-shaped
    // inference instead of this plugin's own judgement.
    //
    // Reads the shipped plugin.json directly, not action_specs(): that
    // resolves the manifest via current_exe(), which in a dev-build test
    // binary finds nothing and returns an empty list — it cannot tell
    // correct wiring from no wiring at all.
    #[test]
    fn shipped_manifest_documents_and_risk_rates_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = vynkor_plugin_manifest::specs_from_manifest(&parsed);
        assert_eq!(
            specs.len(),
            parsed["actions"].as_array().unwrap().len(),
            "every declared action must produce a spec"
        );
        let undocumented: Vec<&str> =
            specs.iter().filter(|s| s.description.is_empty()).map(|s| s.name.as_str()).collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(unrisked.is_empty(), "actions missing a risk label: {unrisked:?}");
    }
}
