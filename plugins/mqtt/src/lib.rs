pub mod actions;
pub mod client;
pub mod credentials;
pub mod error;
pub mod policy;
pub mod stream;
#[cfg(test)]
pub(crate) mod test_support;

use crate::client::{MqttClient, MqttMessage};
use crate::policy::CommandPolicy;
use crate::stream::StreamManager;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;
use tokio::sync::mpsc;
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest};
use vynkor_sdk::{Plugin, VynkorClient, VynkorError};

pub const PLUGIN_ID: &str = "mqtt";
pub const PLUGIN_VERSION: &str = "0.1.0";

pub const ACTIONS: &[&str] = &[
    "mqtt_connect",
    "mqtt_disconnect",
    "mqtt_publish",
    "mqtt_subscribe",
    "mqtt_unsubscribe",
    "mqtt_status",
    "mqtt_device_list",
    "mqtt_device_info",
    "mqtt_device_command",
    "mqtt_device_command_confirmed",
    "mqtt_device_telemetry",
    "mqtt_ota_update",
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub device_type: String,
    pub online: bool,
    pub last_seen: u64,
    pub firmware: Option<String>,
    pub ip: Option<String>,
    pub wifi_rssi: Option<i32>,
    pub uptime_s: Option<u32>,
    pub supported_commands: Vec<String>,
    pub pins: Vec<PinInfo>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PinInfo {
    pub name: String,
    pub pin_type: String,
    pub writable: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TelemetryPoint {
    pub device_id: String,
    pub timestamp: u64,
    pub stream: String,
    pub data: serde_json::Value,
}

/// What the plugin has learned from the broker. Written by the consumer
/// task, read by actions.
#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) devices: HashMap<String, DeviceInfo>,
    pub(crate) telemetry: Vec<TelemetryPoint>,
    pub(crate) messages_received: u64,
}

pub struct MqttPlugin {
    pub(crate) client: Option<MqttClient>,
    pub(crate) stream_manager: StreamManager,
    pub(crate) start_instant: Instant,
    pub(crate) messages_sent: u64,
    pub(crate) registry: Arc<Mutex<Registry>>,
    pub(crate) command_policy: CommandPolicy,
}

impl MqttPlugin {
    pub fn new() -> Self {
        Self {
            client: None,
            stream_manager: StreamManager::new(),
            start_instant: Instant::now(),
            messages_sent: 0,
            registry: Arc::new(Mutex::new(Registry::default())),
            command_policy: CommandPolicy::from_env(),
        }
    }

    pub(crate) fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Consume broker messages as they arrive, independently of actions.
    ///
    /// The task ends when the client's event loop does (disconnect, replace,
    /// permanent refusal), because that drops the sending side.
    pub(crate) fn start_consumer(&self, mut messages: mpsc::Receiver<MqttMessage>) {
        let registry = self.registry.clone();
        let streams = self.stream_manager.clone();
        tokio::spawn(async move {
            while let Some(msg) = messages.recv().await {
                registry
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .process(&msg);
                streams.dispatch_sync(&msg);
            }
        });
    }
}

impl Registry {
    pub(crate) fn process(&mut self, msg: &MqttMessage) {
        self.messages_received += 1;
        let topic = &msg.topic;
        let parts: Vec<&str> = topic.split('/').collect();
        if parts.len() < 3 || parts[0] != "vynkor" {
            return;
        }

        let device_id = parts[1].to_string();
        let msg_type = parts[2];

        let json: serde_json::Value = match serde_json::from_slice(&msg.payload) {
            Ok(v) => v,
            Err(_) => return,
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        match msg_type {
            "status" => {
                let online = json["online"].as_bool().unwrap_or(false);

                let entry = self
                    .devices
                    .entry(device_id.clone())
                    .or_insert_with(|| DeviceInfo {
                        device_id: device_id.clone(),
                        device_type: "unknown".into(),
                        online: false,
                        last_seen: 0,
                        firmware: None,
                        ip: None,
                        wifi_rssi: None,
                        uptime_s: None,
                        supported_commands: vec![],
                        pins: vec![],
                    });

                entry.online = online;
                entry.last_seen = now;
                if let Some(kind) = json["device_type"].as_str() {
                    entry.device_type = kind.to_string();
                }
                // Firmware may be reported either pre-formatted or split.
                if let Some(fw) = json["firmware"].as_str() {
                    entry.firmware = Some(fw.to_string());
                } else if let Some(name) = json["firmware_name"].as_str() {
                    entry.firmware = Some(match json["firmware_version"].as_str() {
                        Some(version) => format!("{name} {version}"),
                        None => name.to_string(),
                    });
                }
                if let Some(ip) = json["ip_address"].as_str() {
                    entry.ip = Some(ip.to_string());
                }
                if let Some(rssi) = json["wifi_rssi"].as_i64() {
                    entry.wifi_rssi = Some(rssi as i32);
                }
                if let Some(uptime) = json["uptime_s"].as_u64() {
                    entry.uptime_s = Some(uptime as u32);
                }
                if let Some(cmds) = json["supported_commands"].as_array() {
                    entry.supported_commands = cmds
                        .iter()
                        .filter_map(|c| c.as_str().map(String::from))
                        .collect();
                }
                if let Some(pins) = json["pins"].as_array() {
                    entry.pins = pins
                        .iter()
                        .filter_map(|p| {
                            Some(PinInfo {
                                name: p["name"].as_str()?.to_string(),
                                pin_type: p["pin_type"].as_str().unwrap_or("gpio").to_string(),
                                writable: p["writable"].as_bool().unwrap_or(true),
                            })
                        })
                        .collect();
                }
                tracing::info!(
                    device_id = %device_id,
                    online,
                    pins = entry.pins.len(),
                    "device status observed"
                );
            }
            "telemetry" => {
                // Flat form is what devices send; the nested one is kept for
                // older firmware that wrapped payload in a "telemetry" object.
                let stream = json["stream"]
                    .as_str()
                    .or_else(|| json["telemetry"]["stream"].as_str())
                    .unwrap_or("default")
                    .to_string();
                let data = if json["data"].is_null() {
                    json["telemetry"]["data"].clone()
                } else {
                    json["data"].clone()
                };
                let timestamp = json["timestamp"].as_u64().unwrap_or(now);

                self.telemetry.push(TelemetryPoint {
                    device_id: device_id.clone(),
                    timestamp,
                    stream,
                    data,
                });

                if self.telemetry.len() > 10000 {
                    self.telemetry.drain(..1000);
                }

                if let Some(entry) = self.devices.get_mut(&device_id) {
                    entry.online = true;
                    entry.last_seen = now;
                } else {
                    self.devices.insert(
                        device_id.clone(),
                        DeviceInfo {
                            device_id,
                            device_type: "sensor".into(),
                            online: true,
                            last_seen: now,
                            firmware: None,
                            ip: None,
                            wifi_rssi: None,
                            uptime_s: None,
                            supported_commands: vec![],
                            pins: vec![],
                        },
                    );
                }
            }
            "response" => {
                tracing::debug!("Device response on {topic}");
            }
            _ => {}
        }
    }
}

impl Default for MqttPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for MqttPlugin {
    fn id(&self) -> &str {
        PLUGIN_ID
    }

    fn version(&self) -> &str {
        PLUGIN_VERSION
    }

    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec!["network".into(), "secrets".into()],
            actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
            action_specs: vynkor_plugin_manifest::action_specs(),
            ..Default::default()
        }
    }

    async fn on_init(&mut self, client: &mut VynkorClient) -> Result<(), VynkorError> {
        let Ok(host) = std::env::var("MQTT_PLUGIN_BROKER_HOST") else {
            return Ok(());
        };
        let port = std::env::var("MQTT_PLUGIN_BROKER_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(1883);
        let mut params = serde_json::json!({ "broker_host": host, "broker_port": port });
        if let Ok(username) = std::env::var("MQTT_PLUGIN_USERNAME") {
            params["username"] = username.into();
        }
        if let Some((password, source)) = credentials::resolve_password(client).await {
            tracing::info!(source, "MQTT broker password resolved");
            params["password"] = password.into();
        }
        match actions::handle_connect(self, params.to_string().as_bytes()).await {
            Ok(result) => tracing::info!(%result, "MQTT auto-connect on startup"),
            Err(error) => tracing::warn!(%error, "MQTT auto-connect failed"),
        }
        Ok(())
    }

    async fn on_message(&mut self, envelope: Envelope) -> Result<Option<Envelope>, VynkorError> {
        let Some(envelope::Payload::ActionRequest(req)) = envelope.payload else {
            return Ok(None);
        };

        let reply = match handle_action(&req.action, &req.params_json, self).await {
            Ok(value) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionOk as i32,
                data_json: value.to_string().into_bytes(),
                error: String::new(),
            },
            Err(e) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionError as i32,
                data_json: Vec::new(),
                error: e.to_string(),
            },
        };

        Ok(Some(Envelope {
            payload: Some(envelope::Payload::ActionResponse(reply)),
            ..Default::default()
        }))
    }
}

async fn handle_action(
    action: &str,
    params_json: &[u8],
    plugin: &mut MqttPlugin,
) -> Result<serde_json::Value, crate::error::MqttError> {
    match action {
        "mqtt_connect" => actions::handle_connect(plugin, params_json).await,
        "mqtt_disconnect" => actions::handle_disconnect(plugin, params_json).await,
        "mqtt_publish" => actions::handle_publish(plugin, params_json).await,
        "mqtt_subscribe" => actions::handle_subscribe(plugin, params_json).await,
        "mqtt_unsubscribe" => actions::handle_unsubscribe(plugin, params_json).await,
        "mqtt_status" => actions::handle_status(plugin, params_json).await,
        "mqtt_device_list" => actions::handle_device_list(plugin, params_json).await,
        "mqtt_device_info" => actions::handle_device_info(plugin, params_json).await,
        "mqtt_device_command" => actions::handle_device_command(plugin, params_json, true).await,
        "mqtt_device_command_confirmed" => {
            actions::handle_device_command(plugin, params_json, false).await
        }
        "mqtt_device_telemetry" => actions::handle_device_telemetry(plugin, params_json).await,
        "mqtt_ota_update" => actions::handle_ota_update(plugin, params_json).await,
        _ => Err(crate::error::MqttError::NotFound(format!(
            "unknown action: {action}"
        ))),
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
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            undocumented.is_empty(),
            "actions missing a description: {undocumented:?}"
        );
        let unrisked: Vec<&str> = specs
            .iter()
            .filter(|s| s.risk == vynkor_sdk::proto::ActionRisk::Unknown as i32)
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            unrisked.is_empty(),
            "actions missing a risk label: {unrisked:?}"
        );
    }
}
