pub mod actions;
pub mod client;
pub mod error;
pub mod stream;

use crate::client::MqttClient;
use crate::stream::StreamManager;
use std::collections::HashMap;
use std::time::Instant;
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest};
use vynkor_sdk::{Plugin, VynkorError};

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

pub struct MqttPlugin {
    pub(crate) client: Option<MqttClient>,
    pub(crate) stream_manager: StreamManager,
    pub(crate) start_instant: Instant,
    pub(crate) messages_sent: u64,
    pub(crate) messages_received: u64,
    pub(crate) devices: HashMap<String, DeviceInfo>,
    pub(crate) telemetry: Vec<TelemetryPoint>,
}

impl MqttPlugin {
    pub fn new() -> Self {
        Self {
            client: None,
            stream_manager: StreamManager::new(),
            start_instant: Instant::now(),
            messages_sent: 0,
            messages_received: 0,
            devices: HashMap::new(),
            telemetry: Vec::new(),
        }
    }

    pub(crate) fn process_mqtt_message(&mut self, msg: &client::MqttMessage) {
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
                if let Some(fw) = json["firmware"].as_str() {
                    entry.firmware = Some(fw.to_string());
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
            }
            "telemetry" => {
                let stream = json["telemetry"]["stream"]
                    .as_str()
                    .unwrap_or("default")
                    .to_string();
                let data = json["telemetry"]["data"].clone();
                let timestamp = json["timestamp"].as_u64().unwrap_or(0);

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
            ..Default::default()
        }
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
        "mqtt_device_command" => actions::handle_device_command(plugin, params_json).await,
        "mqtt_device_telemetry" => actions::handle_device_telemetry(plugin, params_json).await,
        "mqtt_ota_update" => actions::handle_ota_update(plugin, params_json).await,
        _ => Err(crate::error::MqttError::NotFound(format!(
            "unknown action: {action}"
        ))),
    }
}
