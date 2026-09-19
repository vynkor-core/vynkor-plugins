//! `automations` plugin (CAP-01) — a declarative rules engine over the
//! existing primitives: rules are JSON documents in `database`, triggers are
//! kernel event deliveries, dispatch is a kernel-routed action call.
//!
//! Thin by construction: cron-style triggers come from pairing with the
//! `scheduler` plugin (subscribe to `plugin.scheduler.fired` and match on its
//! payload), reactions come from any subscribed event stream. No timers of
//! its own — the loop is calendar's single-reader select over inbound
//! envelopes plus an mpsc channel-fronted [`Rpc`] proxy for outbound
//! `database`/target calls and fire-and-forget publishes, so an in-flight
//! dispatch can never eat a user request (`send_action`'s
//! discard-while-waiting problem).
//!
//! Safety rails:
//! - only events the operator subscribed via `AUTOMATIONS_PLUGIN_EVENT_TYPES`
//!   can reach rules at all (default-deny);
//! - `requires_confirmation` rules never auto-dispatch — they publish a
//!   `needs_confirmation` event; approval = `rule_set` flipping the flag;
//! - cooldowns are marked BEFORE dispatch (at-most-once per window,
//!   calendar semantics);
//! - dispatched actions run under this plugin's own JWT grants — T-19 holds,
//!   no laundering: an ungranted gated target fails into `last_error`.

use std::collections::HashMap;
use std::sync::Arc;

use automations_plugin::{request, store, Rpc, RpcCall};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use vynkor_sdk::proto::{
    envelope, ActionRequest, ActionResponse, ActionStatus, Envelope, EventPublish,
    PluginManifest, Pong,
};
use vynkor_sdk::{VynkorClient, VynkorError};

const PLUGIN_ID: &str = "automations";
const PLUGIN_VERSION: &str = "0.1.0";
const ACTIONS: [&str; 4] = ["rule_set", "rule_get", "rule_list", "rule_delete"];
const DISPATCH_TIMEOUT_MS: u32 = 30_000;
const DB_TIMEOUT_MS: u32 = 5_000;

fn manifest() -> PluginManifest {
    PluginManifest {
        permissions: vec![
            "PERMISSION_STORAGE".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: vynkor_plugin_manifest::action_specs(),
        ..Default::default()
    }
}

/// Operator-declared event streams that may drive rules. Default-deny.
fn subscribed_event_types() -> Vec<String> {
    std::env::var("AUTOMATIONS_PLUGIN_EVENT_TYPES")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

enum Outbound {
    Reply(Envelope),
    Publish(&'static str, Value),
}

async fn handle_action_request(rpc: Rpc, req: ActionRequest) -> Result<Vec<u8>, String> {
    let parsed = request::parse_request(&req.action, &req.params_json)?;
    let db = store::Db::new(rpc, DB_TIMEOUT_MS);
    apply(db, parsed).await
}

async fn apply(db: store::Db, parsed: request::AutomationRequest) -> Result<Vec<u8>, String> {
    match parsed {
        request::AutomationRequest::RuleSet { mut doc } => {
            if doc.id.is_empty() {
                if db.list().await?.len() >= store::MAX_RULES {
                    return Err(format!("rule cap reached ({})", store::MAX_RULES));
                }
                doc.id = db.next_id().await?.to_string();
            } else if let Some(existing) = db.get(&doc.id).await? {
                doc.created_at_ms = existing.created_at_ms;
                doc.last_fired_ms = existing.last_fired_ms;
                doc.fire_count = existing.fire_count;
                doc.last_error = existing.last_error;
                if !doc.enabled && existing.enabled {
                    doc.enabled = true; // approval happens by explicit enabled flag only
                }
            } else {
                return Err(format!("rule not found: {}", doc.id));
            }
            doc.updated_at_ms = store::now_ms();
            db.put(&doc).await?;
            serde_json::to_vec(&serde_json::json!({"id": doc.id, "stored": true}))
                .map_err(|e| format!("failed to encode response: {e}"))
        }
        request::AutomationRequest::RuleGet { id } => match db.get(&id).await? {
            Some(rule) => serde_json::to_vec(&serde_json::json!({"found": true, "rule": rule}))
                .map_err(|e| format!("failed to encode response: {e}")),
            None => serde_json::to_vec(&serde_json::json!({"found": false, "rule": null}))
                .map_err(|e| format!("failed to encode response: {e}")),
        },
        request::AutomationRequest::RuleList => {
            let mut rules = db.list().await?;
            rules.sort_by(|a, b| b.id.cmp(&a.id));
            serde_json::to_vec(&serde_json::json!({"total": rules.len(), "rules": rules}))
                .map_err(|e| format!("failed to encode response: {e}"))
        }
        request::AutomationRequest::RuleDelete { id } => {
            let deleted = db.delete(&id).await?;
            serde_json::to_vec(&serde_json::json!({"deleted": deleted}))
                .map_err(|e| format!("failed to encode response: {e}"))
        }
    }
}

/// Evaluate one delivered event against every enabled rule and fire matches.
async fn on_event(
    rpc: Rpc,
    subscribed: &[String],
    event_type: String,
    payload_json: Vec<u8>,
    out: mpsc::Sender<Outbound>,
) {
    if !subscribed.contains(&event_type) {
        return;
    }
    let payload: Value = serde_json::from_slice(&payload_json).unwrap_or(Value::Null);
    let db = store::Db::new(rpc.clone(), DB_TIMEOUT_MS);
    let rules = match db.list().await {
        Ok(rules) => rules,
        Err(e) => {
            eprintln!("[{PLUGIN_ID}] rule listing failed for {event_type}: {e}");
            return;
        }
    };
    for mut rule in rules {
        if !rule.enabled || rule.trigger.event_type != event_type {
            continue;
        }
        if !rule.conditions_hold(&payload) || !rule.cooldown_ok(store::now_ms()) {
            continue;
        }
        // Mark before dispatch — at-most-once per cooldown window.
        rule.last_fired_ms = store::now_ms();
        rule.fire_count += 1;
        if rule.requires_confirmation {
            if let Err(e) = db.put(&rule).await {
                eprintln!("[{PLUGIN_ID}] persisting hold state failed: {e}");
            }
            let _ = out
                .send(Outbound::Publish(
                    "needs_confirmation",
                    serde_json::json!({
                        "rule_id": rule.id,
                        "name": rule.name,
                        "action": rule.action.target_action,
                        "event_type": event_type,
                    }),
                ))
                .await;
            continue;
        }
        match rpc.call_action(&rule.action.target_action, rule.action.params_json.clone(), DISPATCH_TIMEOUT_MS).await {
            Ok(_) => rule.last_error.clear(),
            Err(e) => {
                rule.last_error = e;
                eprintln!(
                    "[{PLUGIN_ID}] rule {} -> '{}' failed: {}",
                    rule.id, rule.action.target_action, rule.last_error
                );
            }
        }
        if let Err(e) = db.put(&rule).await {
            eprintln!("[{PLUGIN_ID}] persisting fire state for rule {} failed: {e}", rule.id);
        }
        let _ = out
            .send(Outbound::Publish(
                "triggered",
                serde_json::json!({
                    "rule_id": rule.id,
                    "name": rule.name,
                    "event_type": event_type,
                    "ok": rule.last_error.is_empty(),
                }),
            ))
            .await;
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

    let subscribed = subscribed_event_types();
    if !subscribed.is_empty() {
        client.subscribe(subscribed.clone()).await?;
    }
    println!("[{PLUGIN_ID}] registered; subscribed to {subscribed:?}");

    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(64);
    let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCall>(64);
    let rpc = Arc::new(Rpc::new(rpc_tx));

    let mut pending: HashMap<String, oneshot::Sender<Result<Value, String>>> = HashMap::new();

    loop {
        tokio::select! {
            env = client.recv() => {
                let env = match env {
                    Ok(env) => env,
                    Err(_) => break,
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
                        let _ = client.ack_event(&event.event_id).await;
                        let task_rpc = (*rpc).clone();
                        let subscribed = subscribed.clone();
                        let out = out_tx.clone();
                        tokio::spawn(async move {
                            on_event(
                                task_rpc,
                                &subscribed,
                                event.event_type.clone(),
                                event.payload_json.clone(),
                                out,
                            )
                            .await;
                        });
                    }
                    Some(envelope::Payload::ActionRequest(req)) => {
                        let task_rpc = (*rpc).clone();
                        let out = out_tx.clone();
                        tokio::spawn(async move {
                            let reply =
                                match handle_action_request(task_rpc, req.clone()).await {
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
                            let _ = out
                                .send(Outbound::Reply(Envelope {
                                    payload: Some(envelope::Payload::ActionResponse(reply)),
                                    ..Default::default()
                                }))
                                .await;
                        });
                    }
                    Some(envelope::Payload::ActionResponse(resp)) => {
                        if let Some(waiter) = pending.remove(&resp.action_id) {
                            let result = if resp.status == ActionStatus::ActionOk as i32 {
                                serde_json::from_slice::<Value>(&resp.data_json)
                                    .map_err(|e| format!("malformed payload: {e}"))
                            } else {
                                Err(resp.error)
                            };
                            let _ = waiter.send(result);
                        }
                    }
                    other => {
                        println!("[{PLUGIN_ID}] unhandled message: {other:?}");
                    }
                }
            }
            Some(call) = rpc_rx.recv() => {
                let action_id = format!("{PLUGIN_ID}-{}", unix_millis());
                pending.insert(action_id.clone(), call.reply);
                let req_env = Envelope {
                    payload: Some(envelope::Payload::ActionRequest(ActionRequest {
                        action_id,
                        action: call.action,
                        params_json: call.params_json,
                        timeout_ms: call.timeout_ms,
                        streaming: false,
                        caller_plugin_id: PLUGIN_ID.to_string(),
                    })),
                    ..Default::default()
                };
                let _ = client.send("kernel", req_env).await;
            }
            Some(out) = out_rx.recv() => {
                match out {
                    Outbound::Reply(env) => {
                        let _ = client.send("kernel", env).await;
                    }
                    Outbound::Publish(kind, payload) => {
                        let env = Envelope {
                            payload: Some(envelope::Payload::EventPublish(EventPublish {
                                event_type: kind.to_string(),
                                payload_json: serde_json::to_vec(&payload)
                                    .unwrap_or_default(),
                            })),
                            ..Default::default()
                        };
                        let _ = client.send("kernel", env).await;
                    }
                }
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
    let secret = std::env::var("VYN_JWT_SECRET").ok().filter(|s| !s.is_empty());
    let client = match secret {
        Some(s) => VynkorClient::connect_with_secret(&socket_path, s.as_bytes()).await?,
        None => VynkorClient::connect(&socket_path).await?,
    };
    serve(client).await
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
