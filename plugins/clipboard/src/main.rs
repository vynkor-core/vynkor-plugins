use std::collections::HashMap;
use std::sync::Arc;

use clipboard_plugin::lib_rpc::{Config, Rpc, RpcCall};
use clipboard_plugin::{handler, history, providers};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, Pong, PluginManifest};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "clipboard";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 5] = ["clipboard_read", "clipboard_write", "clipboard_providers", "clipboard_history", "clipboard_clear"];

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec!["PERMISSION_CLIPBOARD".into(), "PERMISSION_STORAGE".into()],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: clipboard_plugin::manifest::action_specs(),
        ..Default::default()
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn action_response(action_id: String, status: ActionStatus, data_json: Vec<u8>, error: String) -> Envelope {
    Envelope { payload: Some(envelope::Payload::ActionResponse(ActionResponse { action_id, status: status as i32, data_json, error })), ..Default::default() }
}

async fn handle_action(
    rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
) -> Result<Value, String> {
    match action {
        "clipboard_read" => {
            let h_cfg = handler::Config { timeout_ms: config.timeout_ms, max_bytes: config.max_bytes, provider_pref: config.provider_pref };
            let session = providers::detect_session_from_env().map_err(|e| e)?;
            let runner = providers::RealRunner;
            let res = handler::handle_read(&runner, &h_cfg, session).await?;
            if config.history_enabled {
                if let Some(text) = res.get("text").and_then(Value::as_str) {
                    if res.get("found").and_then(Value::as_bool) == Some(true) {
                        let prov = res.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
                        let rpc2 = rpc.clone();
                        let cfg2 = config.clone();
                        let txt = text.to_string();
                        tokio::spawn(async move {
                            let db = history::Db::new(rpc2, cfg2.db_timeout_ms);
                            let _ = db.append(txt, prov, cfg2.history_limit).await;
                        });
                    }
                }
            }
            Ok(res)
        }
        "clipboard_write" => {
            let v: Value = serde_json::from_slice(params_json).map_err(|e| format!("invalid JSON: {e}"))?;
            let text = v.get("text").and_then(Value::as_str).ok_or("ERR_CLIPBOARD_BAD_PARAMS: missing text".to_string())?;
            let h_cfg = handler::Config { timeout_ms: config.timeout_ms, max_bytes: config.max_bytes, provider_pref: config.provider_pref };
            let session = providers::detect_session_from_env().map_err(|e| e)?;
            let runner = providers::RealRunner;
            let res = handler::handle_write(&runner, &h_cfg, session, text).await?;
            if config.history_enabled {
                let prov = res.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
                let rpc2 = rpc.clone();
                let cfg2 = config.clone();
                let txt = text.to_string();
                tokio::spawn(async move {
                    let db = history::Db::new(rpc2, cfg2.db_timeout_ms);
                    let _ = db.append(txt, prov, cfg2.history_limit).await;
                });
            }
            Ok(res)
        }
        "clipboard_providers" => {
            let session = providers::detect_session_from_env().map_err(|e| e)?;
            Ok(handler::handle_providers(session))
        }
        "clipboard_history" => {
            #[derive(serde::Deserialize)]
            struct Params { query: Option<String>, limit: Option<usize>, offset: Option<usize> }
            let p: Params = serde_json::from_slice(params_json).map_err(|e| format!("invalid params for clipboard_history: {e}"))?;
            let limit = p.limit.unwrap_or(100);
            if limit == 0 || limit > 500 { return Err("params.limit must be between 1 and 500".into()); }
            let offset = p.offset.unwrap_or(0);
            let query = p.query.and_then(|q| { let t=q.trim().to_string(); if t.is_empty() {None} else {Some(t)} });
            let db = history::Db::new(rpc, config.db_timeout_ms);
            let mut entries = db.list().await?;
            if let Some(q) = query {
                let ql = q.to_lowercase();
                entries.retain(|e| e.text.to_lowercase().contains(&ql));
            }
            entries.sort_by(|a,b| b.created_at_ms.cmp(&a.created_at_ms).then_with(|| b.id.parse::<u64>().unwrap_or(0).cmp(&a.id.parse::<u64>().unwrap_or(0))));
            let total = entries.len();
            let page: Vec<&history::ClipEntry> = entries.iter().skip(offset).take(limit).collect();
            Ok(serde_json::json!({"entries": page, "total": total}))
        }
        "clipboard_clear" => {
            let db = history::Db::new(rpc, config.db_timeout_ms);
            let n = db.clear().await?;
            Ok(serde_json::json!({"cleared": n}))
        }
        other => Err(format!("unknown action: {other}")),
    }
}

async fn serve(mut client: VynkorClient, config: Config) -> Result<(), VynkorError> {
    let jwt_token = std::env::var("VYN_JWT_TOKEN").unwrap_or_default();
    let ack = client.register_full(PLUGIN_ID, PLUGIN_VERSION, manifest(), &jwt_token).await?;
    if !ack.accepted { return Err(VynkorError::PermissionDenied(format!("registration rejected: {}", ack.reject_reason))); }
    println!("[{PLUGIN_ID}] registered with kernel history={} limit={}", config.history_enabled, config.history_limit);

    let config = Arc::new(config);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Envelope>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Rpc::new(rpc_tx);

    let mut pending: HashMap<String, (String, oneshot::Sender<Result<Value,String>>)> = HashMap::new();
    let mut seq: u64 = 0;

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env { Ok(e) => e, Err(_) => break };
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
                        tokio::spawn(async move {
                            match handle_action(rpc, &cfg, &req.action, &req.params_json).await {
                                Ok(data) => {
                                    let data_json = data.to_string().into_bytes();
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionOk, data_json, String::new())).await;
                                }
                                Err(e) => {
                                    let _ = out.send(action_response(req.action_id, ActionStatus::ActionError, Vec::new(), e)).await;
                                }
                            }
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some((action, reply)) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json).map_err(|e| format!("malformed payload: {e}"))
                            } else { Err(format!("{action} failed: {}", resp.error)) };
                            let _ = reply.send(result);
                        }
                    }
                    other => println!("[{PLUGIN_ID}] unhandled: {other:?}"),
                }
            }
            Some(env) = outbound_rx.recv() => { let _ = client.send("kernel", env).await; }
            Some(call) = rpc_rx.recv() => {
                seq += 1;
                let action_id = format!("rpc-{seq}");
                pending.insert(action_id.clone(), (call.action.clone(), call.reply));
                let env = Envelope { payload: Some(envelope::Payload::ActionRequest(ActionRequest { action_id, action: call.action, params_json: call.params_json, timeout_ms: call.timeout_ms, streaming: false, ..Default::default() })), ..Default::default() };
                let _ = client.send("kernel", env).await;
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

    #[derive(Default)]
    struct FakeDb { kv: BTreeMap<String, Value> }
    impl FakeDb {
        fn handle(&mut self, action: &str, params: Value) -> Result<Value, String> {
            fn key(p: &Value) -> String { p.get("key").and_then(Value::as_str).unwrap_or_default().to_string() }
            match action {
                "db_incr" => {
                    let k = key(&params);
                    let delta = params.get("delta").and_then(Value::as_i64).unwrap_or(1);
                    let cur = self.kv.get(&k).and_then(Value::as_i64).unwrap_or(0);
                    let next = cur + delta;
                    self.kv.insert(k, serde_json::json!(next));
                    Ok(serde_json::json!({"ok": true, "value": next}))
                }
                "db_set" => { let k = key(&params); self.kv.insert(k, params.get("value").cloned().unwrap_or(Value::Null)); Ok(serde_json::json!({"ok": true})) }
                "db_get" => { let k = key(&params); match self.kv.get(&k) { Some(v) => Ok(serde_json::json!({"found": true, "value": v})), None => Ok(serde_json::json!({"found": false, "value": null})) } }
                "db_keys" => { let prefix = params.get("prefix").and_then(Value::as_str).unwrap_or(""); let keys: Vec<&String> = self.kv.keys().filter(|k| k.starts_with(prefix)).collect(); Ok(serde_json::json!({"keys": keys})) }
                "db_batch_get" => { let mut values = serde_json::Map::new(); if let Some(keys) = params.get("keys").and_then(Value::as_array) { for k in keys { if let Some(k) = k.as_str() { values.insert(k.to_string(), self.kv.get(k).cloned().unwrap_or(Value::Null)); } } } Ok(serde_json::json!({"values": values})) }
                "db_delete" => { let k = key(&params); Ok(serde_json::json!({"deleted": self.kv.remove(&k).is_some()})) }
                other => Err(format!("fake db unknown {other}")),
            }
        }
    }

    enum Cmd { Call { action: String, params: Value, reply: oneshot::Sender<Result<Value,String>> } }
    struct Shim { tx: mpsc::Sender<Cmd> }
    impl Shim {
        async fn call(&self, action: &str, params: Value) -> Result<Value,String> {
            let (tx,rx)=oneshot::channel();
            self.tx.send(Cmd::Call { action: action.into(), params, reply: tx }).await.expect("shim died");
            tokio::time::timeout(Duration::from_secs(5), rx).await.expect("timeout").expect("dropped")
        }
    }

    async fn start_plugin(cfg: Config) -> Shim {
        let (ps, ks) = UnixStream::pair().unwrap();
        let pc = VynkorClient::from_stream(ps, None);
        let kc = VynkorClient::from_stream(ks, None);
        tokio::spawn(async move { let _ = serve(pc, cfg).await; });
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(run_shim(kc, rx));
        Shim { tx }
    }

    async fn run_shim(mut kernel: VynkorClient, mut rx: mpsc::Receiver<Cmd>) {
        let mut db = FakeDb::default();
        let mut pending: StdHashMap<String, oneshot::Sender<Result<Value,String>>> = StdHashMap::new();
        let mut seq: u64 = 0;
        loop {
            let env = tokio::time::timeout(Duration::from_secs(5), kernel.recv()).await.expect("timeout").expect("closed");
            if let Some(envelope::Payload::PluginRegister(_)) = env.payload {
                let _ = kernel.send("clipboard", Envelope { payload: Some(envelope::Payload::PluginRegisterAck(PluginRegisterAck { accepted:true, ..Default::default() })), ..Default::default() }).await;
                break;
            }
        }
        loop {
            tokio::select! {
                env = kernel.recv() => {
                    let env = match env { Ok(e)=>e, Err(_)=>break };
                    match env.payload {
                        Some(envelope::Payload::ActionRequest(req)) => {
                            let params: Value = serde_json::from_slice(&req.params_json).unwrap_or(Value::Null);
                            let resp = match db.handle(&req.action, params) {
                                Ok(v) => ProtoActionResponse { action_id: req.action_id, status: ActionStatus::ActionOk as i32, data_json: serde_json::to_vec(&v).unwrap(), error: String::new() },
                                Err(e) => ProtoActionResponse { action_id: req.action_id, status: ActionStatus::ActionError as i32, data_json: Vec::new(), error: e },
                            };
                            let _ = kernel.send("clipboard", Envelope { payload: Some(envelope::Payload::ActionResponse(resp)), ..Default::default() }).await;
                        }
                        Some(envelope::Payload::ActionResponse(r)) => { if let Some(tx) = pending.remove(&r.action_id) { let res = if r.status==ActionStatus::ActionOk as i32 { serde_json::from_slice(&r.data_json).map_err(|e| e.to_string()) } else { Err(r.error) }; let _ = tx.send(res); } }
                        Some(envelope::Payload::EventPublish(ev)) => { let _ = kernel.send("clipboard", Envelope { payload: Some(envelope::Payload::EventPublishAck(EventPublishAck { event_id: format!("ev-{seq}"), status: EventPublishStatus::EventPublishOk as i32, error: String::new() })), ..Default::default() }).await; seq+=1; let _ = ev; }
                        Some(envelope::Payload::Ping(p)) => { let _ = kernel.send("clipboard", Envelope { payload: Some(envelope::Payload::Pong(Pong { original_timestamp: p.timestamp, server_timestamp: unix_millis() })), ..Default::default() }).await; }
                        _ => {}
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Call { action, params, reply }) => { seq+=1; let id=format!("t-{seq}"); pending.insert(id.clone(), reply); let env=Envelope { payload: Some(envelope::Payload::ActionRequest(ActionRequest { action_id: id, action, params_json: serde_json::to_vec(&params).unwrap(), timeout_ms:0, streaming:false, ..Default::default() })), ..Default::default() }; let _ = kernel.send("clipboard", env).await; }
                        None => break,
                    }
                }
            }
        }
    }

    fn test_config() -> Config { Config { timeout_ms: 1000, max_bytes: 1024, provider_pref: providers::ProviderPref::Auto, history_enabled: true, history_limit: 1000, db_timeout_ms: 5000 } }

    #[tokio::test]
    async fn history_initially_empty() {
        let shim = start_plugin(test_config()).await;
        let empty = shim.call("clipboard_history", serde_json::json!({})).await.unwrap();
        assert_eq!(empty["total"], 0);
        assert_eq!(empty["entries"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn history_clear_empty() {
        let shim = start_plugin(test_config()).await;
        let res = shim.call("clipboard_clear", serde_json::json!({})).await.unwrap();
        assert_eq!(res["cleared"], 0);
    }

    #[tokio::test]
    async fn history_limit_validation() {
        let shim = start_plugin(test_config()).await;
        let err = shim.call("clipboard_history", serde_json::json!({"limit": 0})).await.unwrap_err();
        assert!(err.contains("limit"));
    }

    #[tokio::test]
    async fn history_query_validation() {
        let shim = start_plugin(test_config()).await;
        let res = shim.call("clipboard_history", serde_json::json!({"query": "test", "limit": 10})).await.unwrap();
        assert_eq!(res["total"], 0);
    }
}
