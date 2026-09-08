use crate::error::MqttError;
use crate::MqttPlugin;
use rumqttc::QoS;
use serde_json::{json, Value};
use std::time::Duration;

pub async fn handle_connect(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let broker_host = params["broker_host"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("broker_host is required".into()))?;
    let broker_port = params["broker_port"].as_u64().unwrap_or(1883) as u16;
    let client_id = params["client_id"]
        .as_str()
        .unwrap_or("vynkor-mqtt-plugin");
    let username = params["username"].as_str();
    let password = params["password"].as_str();
    let keepalive = params["keepalive_secs"].as_u64().unwrap_or(60) as u16;

    let tls = params.get("tls").map(|tls_val| {
        crate::client::TlsConfig {
            ca_cert_path: tls_val["ca_cert_path"].as_str().map(String::from),
            client_cert_path: tls_val["client_cert_path"].as_str().map(String::from),
            client_key_path: tls_val["client_key_path"].as_str().map(String::from),
        }
    });

    let client = crate::client::MqttClient::connect(
        broker_host,
        broker_port,
        client_id,
        username,
        password,
        keepalive,
        tls,
    )
    .await?;

    let (host, port) = client.broker_info();
    plugin.client = Some(client);
    tracing::info!("Connected to MQTT broker {host}:{port}");

    Ok(json!({
        "connected": true,
        "broker": format!("{host}:{port}")
    }))
}

pub async fn handle_disconnect(
    plugin: &mut MqttPlugin,
    _params_json: &[u8],
) -> Result<Value, MqttError> {
    if let Some(client) = &plugin.client {
        client.disconnect().await?;
        tracing::info!("Disconnected from MQTT broker");
    }
    plugin.client = None;
    Ok(json!({ "disconnected": true }))
}

pub async fn handle_publish(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let topic = params["topic"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("topic is required".into()))?;
    let payload_str = params["payload"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("payload is required".into()))?;

    let qos = match params["qos"].as_u64().unwrap_or(0) {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtMostOnce,
    };
    let retain = params["retain"].as_bool().unwrap_or(false);
    let encoding = params["encoding"].as_str().unwrap_or("utf8");

    let payload = match encoding {
        "base64" => base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            payload_str,
        )
        .map_err(|e| MqttError::InvalidParams(format!("invalid base64: {e}")))?,
        _ => payload_str.as_bytes().to_vec(),
    };

    let client = plugin
        .client
        .as_ref()
        .ok_or(MqttError::NotConnected)?;

    client.publish(topic, payload, qos, retain).await?;
    plugin.messages_sent += 1;

    Ok(json!({
        "published": true,
        "message_id": plugin.messages_sent
    }))
}

pub async fn handle_subscribe(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let topic = params["topic"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("topic is required".into()))?;
    let qos = match params["qos"].as_u64().unwrap_or(0) {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtMostOnce,
    };

    let client = plugin
        .client
        .as_ref()
        .ok_or(MqttError::NotConnected)?;

    client.subscribe(topic, qos).await?;

    let rx = plugin.stream_manager.add_subscription(topic, qos).await;
    let topic_owned = topic.to_string();

    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(msg) = rx.recv().await {
            tracing::debug!(
                topic = %msg.topic,
                payload_len = msg.payload.len(),
                "MQTT message received"
            );
            let _ = (topic_owned.clone(), msg);
        }
    });

    Ok(json!({
        "subscribed": true,
        "subscription_id": topic
    }))
}

pub async fn handle_unsubscribe(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let topic = params["topic"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("topic is required".into()))?;

    let client = plugin
        .client
        .as_ref()
        .ok_or(MqttError::NotConnected)?;

    client.unsubscribe(topic).await?;
    plugin.stream_manager.remove_subscription(topic).await;

    Ok(json!({ "unsubscribed": true }))
}

pub async fn handle_status(
    plugin: &MqttPlugin,
    _params_json: &[u8],
) -> Result<Value, MqttError> {
    let connected = plugin.client.as_ref().map_or(false, |c| c.is_connected());
    let broker = plugin
        .client
        .as_ref()
        .map(|c| {
            let (host, port) = c.broker_info();
            format!("{host}:{port}")
        })
        .unwrap_or_default();
    let uptime_ms = plugin.start_instant.elapsed().as_millis() as u64;
    let subscriptions = plugin.stream_manager.subscribed_topics().await;

    Ok(json!({
        "connected": connected,
        "broker": broker,
        "uptime_ms": uptime_ms,
        "subscriptions": subscriptions,
        "messages_sent": plugin.messages_sent,
        "messages_received": plugin.messages_received
    }))
}

pub async fn handle_device_list(
    plugin: &MqttPlugin,
    _params_json: &[u8],
) -> Result<Value, MqttError> {
    let devices: Vec<Value> = plugin
        .devices
        .iter()
        .map(|(id, info)| {
            json!({
                "device_id": id,
                "device_type": info.device_type,
                "online": info.online,
                "last_seen": info.last_seen,
                "firmware": info.firmware,
            })
        })
        .collect();

    Ok(json!({ "devices": devices }))
}

pub async fn handle_device_info(
    plugin: &MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = params["device_id"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("device_id is required".into()))?;

    let info = plugin
        .devices
        .get(device_id)
        .ok_or_else(|| MqttError::DeviceNotFound(device_id.to_string()))?;

    Ok(json!({
        "device_id": device_id,
        "device_type": info.device_type,
        "online": info.online,
        "last_seen": info.last_seen,
        "firmware": info.firmware,
        "ip": info.ip,
        "wifi_rssi": info.wifi_rssi,
        "uptime_s": info.uptime_s,
        "supported_commands": info.supported_commands,
        "pins": info.pins,
    }))
}

pub async fn handle_device_command(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = params["device_id"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("device_id is required".into()))?;
    let command = params["command"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("command is required".into()))?;
    let cmd_params = params.get("params").cloned().unwrap_or(json!({}));
    let timeout_ms = params["timeout_ms"].as_u64().unwrap_or(5000);

    let msg_id = uuid::Uuid::new_v4().to_string();
    let request_topic = format!("vynkor/{device_id}/command");
    let response_topic = format!("vynkor/{device_id}/response");

    let command_msg = json!({
        "v": 1,
        "command": command,
        "request_id": msg_id,
        "params": cmd_params,
        "timeout_ms": timeout_ms
    });

    let payload = serde_json::to_vec(&command_msg)
        .map_err(|e| MqttError::Serialization(e.to_string()))?;

    let client = plugin
        .client
        .as_ref()
        .ok_or(MqttError::NotConnected)?;

    let timeout = Duration::from_millis(timeout_ms);
    match client
        .request(&request_topic, &response_topic, payload, timeout)
        .await
    {
        Ok(response) => {
            plugin.messages_sent += 1;
            plugin.messages_received += 1;

            let resp_json: Value = serde_json::from_slice(&response.payload)
                .unwrap_or(json!({"error": "invalid response format"}));

            Ok(json!({
                "success": true,
                "data": resp_json.get("response").cloned().unwrap_or(json!({})),
                "error_code": resp_json.get("error").and_then(|e| e.get("code")),
                "error_message": resp_json.get("error").and_then(|e| e.get("message")),
                "execution_ms": resp_json.get("response").and_then(|r| r.get("execution_ms")),
            }))
        }
        Err(MqttError::Timeout) => {
            plugin.messages_sent += 1;
            Ok(json!({
                "success": false,
                "error": "timeout waiting for device response",
                "timeout_ms": timeout_ms,
            }))
        }
        Err(e) => Err(e),
    }
}

pub async fn handle_device_telemetry(
    plugin: &MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = params["device_id"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("device_id is required".into()))?;

    let telemetry: Vec<Value> = plugin
        .telemetry
        .iter()
        .filter(|t| t.device_id == device_id)
        .map(|t| {
            json!({
                "timestamp": t.timestamp,
                "stream": t.stream,
                "data": t.data,
            })
        })
        .collect();

    Ok(json!({
        "device_id": device_id,
        "telemetry": telemetry
    }))
}

pub async fn handle_ota_update(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value = serde_json::from_slice(params_json)
        .map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = params["device_id"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("device_id is required".into()))?;
    let firmware_url = params["firmware_url"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("firmware_url is required".into()))?;

    let msg_id = uuid::Uuid::new_v4().to_string();
    let request_topic = format!("vynkor/{device_id}/command");
    let response_topic = format!("vynkor/{device_id}/response");

    let command_msg = json!({
        "v": 1,
        "command": "ota_update",
        "request_id": msg_id,
        "params": { "url": firmware_url }
    });

    let payload = serde_json::to_vec(&command_msg)
        .map_err(|e| MqttError::Serialization(e.to_string()))?;

    let client = plugin
        .client
        .as_ref()
        .ok_or(MqttError::NotConnected)?;

    match client
        .request(
            &request_topic,
            &response_topic,
            payload,
            Duration::from_secs(30),
        )
        .await
    {
        Ok(response) => {
            let resp_json: Value = serde_json::from_slice(&response.payload)
                .unwrap_or(json!({}));

            Ok(json!({
                "started": true,
                "ota_id": msg_id,
                "device_ack": resp_json,
            }))
        }
        Err(MqttError::Timeout) => Ok(json!({
            "started": true,
            "ota_id": msg_id,
            "device_ack": "timeout (device may still be updating)",
        })),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MqttPlugin;

    #[test]
    fn test_parse_connect_params() {
        let params = json!({
            "broker_host": "mqtt.local",
            "broker_port": 1883,
            "client_id": "test-client"
        });
        let params_json = serde_json::to_vec(&params).unwrap();
        let parsed: Value = serde_json::from_slice(&params_json).unwrap();
        assert_eq!(parsed["broker_host"].as_str().unwrap(), "mqtt.local");
        assert_eq!(parsed["broker_port"].as_u64().unwrap(), 1883);
    }

    #[test]
    fn test_parse_publish_params() {
        let params = json!({
            "topic": "test/topic",
            "payload": "hello",
            "qos": 1,
            "retain": false
        });
        let params_json = serde_json::to_vec(&params).unwrap();
        let parsed: Value = serde_json::from_slice(&params_json).unwrap();
        assert_eq!(parsed["topic"].as_str().unwrap(), "test/topic");
        assert_eq!(parsed["payload"].as_str().unwrap(), "hello");
    }

    #[test]
    fn test_parse_subscribe_params() {
        let params = json!({
            "topic": "vynkor/+/telemetry",
            "qos": 1
        });
        let params_json = serde_json::to_vec(&params).unwrap();
        let parsed: Value = serde_json::from_slice(&params_json).unwrap();
        assert_eq!(parsed["topic"].as_str().unwrap(), "vynkor/+/telemetry");
    }

    #[test]
    fn test_parse_command_params() {
        let params = json!({
            "device_id": "esp-kitchen",
            "command": "set_relay",
            "params": { "pin": 5, "state": true },
            "timeout_ms": 5000
        });
        let params_json = serde_json::to_vec(&params).unwrap();
        let parsed: Value = serde_json::from_slice(&params_json).unwrap();
        assert_eq!(parsed["device_id"].as_str().unwrap(), "esp-kitchen");
        assert_eq!(parsed["command"].as_str().unwrap(), "set_relay");
    }

    #[tokio::test]
    async fn test_handle_status_without_connection() {
        let mut plugin = MqttPlugin::new();
        let params = json!({});
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_status(&mut plugin, &params_json).await;
        assert!(result.is_ok());
        let status = result.unwrap();
        assert_eq!(status["connected"].as_bool().unwrap(), false);
        assert_eq!(status["messages_sent"].as_u64().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_handle_connect_invalid_params() {
        let mut plugin = MqttPlugin::new();
        let params = json!({});
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_connect(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::InvalidParams(msg) => assert!(msg.contains("broker_host")),
            _ => panic!("Expected InvalidParams error"),
        }
    }

    #[tokio::test]
    async fn test_handle_publish_not_connected() {
        let mut plugin = MqttPlugin::new();
        let params = json!({ "topic": "test/topic", "payload": "hello" });
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_publish(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::NotConnected => {}
            _ => panic!("Expected NotConnected error"),
        }
    }

    #[tokio::test]
    async fn test_handle_subscribe_not_connected() {
        let mut plugin = MqttPlugin::new();
        let params = json!({ "topic": "test/topic" });
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_subscribe(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::NotConnected => {}
            _ => panic!("Expected NotConnected error"),
        }
    }

    #[tokio::test]
    async fn test_handle_device_command_not_connected() {
        let mut plugin = MqttPlugin::new();
        let params = json!({ "device_id": "esp-test", "command": "set_relay" });
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_device_command(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::NotConnected => {}
            _ => panic!("Expected NotConnected error"),
        }
    }

    #[tokio::test]
    async fn test_handle_device_command_invalid_params() {
        let mut plugin = MqttPlugin::new();
        let params = json!({});
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_device_command(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::InvalidParams(msg) => assert!(msg.contains("device_id")),
            _ => panic!("Expected InvalidParams error"),
        }
    }

    #[tokio::test]
    async fn test_handle_device_list_empty() {
        let mut plugin = MqttPlugin::new();
        let params = json!({});
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_device_list(&mut plugin, &params_json).await;
        assert!(result.is_ok());
        let devices = result.unwrap();
        assert!(devices["devices"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_handle_device_telemetry_empty() {
        let mut plugin = MqttPlugin::new();
        let params = json!({ "device_id": "esp-test" });
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_device_telemetry(&mut plugin, &params_json).await;
        assert!(result.is_ok());
        let telemetry = result.unwrap();
        assert_eq!(telemetry["device_id"].as_str().unwrap(), "esp-test");
        assert!(telemetry["telemetry"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_handle_ota_update_not_connected() {
        let mut plugin = MqttPlugin::new();
        let params = json!({ "device_id": "esp-test", "firmware_url": "http://example.com/fw.bin" });
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_ota_update(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::NotConnected => {}
            _ => panic!("Expected NotConnected error"),
        }
    }

    #[tokio::test]
    async fn test_handle_ota_update_invalid_params() {
        let mut plugin = MqttPlugin::new();
        let params = json!({});
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_ota_update(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::InvalidParams(msg) => assert!(msg.contains("device_id")),
            _ => panic!("Expected InvalidParams error"),
        }
    }

    #[tokio::test]
    async fn test_handle_device_info_not_found() {
        let mut plugin = MqttPlugin::new();
        let params = json!({ "device_id": "nonexistent" });
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_device_info(&mut plugin, &params_json).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            MqttError::DeviceNotFound(id) => assert_eq!(id, "nonexistent"),
            _ => panic!("Expected DeviceNotFound error"),
        }
    }
}
