use crate::error::MqttError;
use crate::MqttPlugin;
use rumqttc::QoS;
use serde_json::{json, Value};
use std::time::Duration;

pub async fn handle_connect(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let broker_host = params["broker_host"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("broker_host is required".into()))?;
    let broker_port = params["broker_port"].as_u64().unwrap_or(1883) as u16;
    let client_id = params["client_id"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| {
            // A fixed id makes two plugin instances fight over one broker session.
            format!("vynkor-mqtt-{}", uuid::Uuid::new_v4().simple())
        });
    let username = params["username"].as_str();
    let password = params["password"].as_str();
    let keepalive = params["keepalive_secs"].as_u64().unwrap_or(60) as u16;

    let tls = params.get("tls").map(|tls_val| crate::client::TlsConfig {
        ca_cert_path: tls_val["ca_cert_path"].as_str().map(String::from),
        client_cert_path: tls_val["client_cert_path"].as_str().map(String::from),
        client_key_path: tls_val["client_key_path"].as_str().map(String::from),
    });

    // Close the previous session first: with an explicit client_id the old
    // and new connection would otherwise keep kicking each other off.
    if let Some(old) = plugin.client.take() {
        old.disconnect().await;
    }

    let (client, messages) = crate::client::MqttClient::connect(
        broker_host,
        broker_port,
        &client_id,
        username,
        password,
        keepalive,
        tls,
    )
    .await?;

    let (host, port) = client.broker_info();
    plugin.start_consumer(messages);

    for topic in ["vynkor/+/status", "vynkor/+/telemetry"] {
        if let Err(e) = client.subscribe(topic, QoS::AtMostOnce).await {
            tracing::warn!("failed to subscribe to {topic}: {e}");
        }
    }

    let connected = client.is_connected();
    let last_error = client.last_error();
    plugin.client = Some(client);
    if connected {
        tracing::info!("Connected to MQTT broker {host}:{port}");
    } else {
        tracing::warn!("MQTT broker {host}:{port} not reachable yet; retrying in background");
    }

    Ok(json!({
        "connected": connected,
        "broker": format!("{host}:{port}"),
        "last_error": last_error,
    }))
}

pub async fn handle_disconnect(
    plugin: &mut MqttPlugin,
    _params_json: &[u8],
) -> Result<Value, MqttError> {
    if let Some(client) = plugin.client.take() {
        client.disconnect().await;
        tracing::info!("Disconnected from MQTT broker");
    }
    Ok(json!({ "disconnected": true }))
}

pub async fn handle_publish(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
) -> Result<Value, MqttError> {
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

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
        "utf8" => payload_str.as_bytes().to_vec(),
        "base64" => base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload_str)
            .map_err(|e| MqttError::InvalidParams(format!("invalid base64: {e}")))?,
        other => {
            return Err(MqttError::InvalidParams(format!(
                "unsupported encoding '{other}': expected utf8 or base64"
            )))
        }
    };

    let client = plugin.client.as_ref().ok_or(MqttError::NotConnected)?;

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
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let topic = params["topic"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("topic is required".into()))?;
    let qos = match params["qos"].as_u64().unwrap_or(0) {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => QoS::AtMostOnce,
    };

    let client = plugin.client.as_ref().ok_or(MqttError::NotConnected)?;

    client.subscribe(topic, qos).await?;

    let mut rx = plugin.stream_manager.add_subscription(topic, qos).await;
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            tracing::trace!(
                topic = %msg.topic,
                payload_len = msg.payload.len(),
                "stream message"
            );
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
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let topic = params["topic"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("topic is required".into()))?;

    let client = plugin.client.as_ref().ok_or(MqttError::NotConnected)?;

    client.unsubscribe(topic).await?;
    plugin.stream_manager.remove_subscription(topic).await;

    Ok(json!({ "unsubscribed": true }))
}

pub async fn handle_status(plugin: &MqttPlugin, _params_json: &[u8]) -> Result<Value, MqttError> {
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
    let last_error = plugin.client.as_ref().and_then(|c| c.last_error());

    Ok(json!({
        "connected": connected,
        "broker": broker,
        "last_error": last_error,
        "uptime_ms": uptime_ms,
        "subscriptions": subscriptions,
        "messages_sent": plugin.messages_sent,
        "messages_received": plugin.registry().messages_received
    }))
}

pub async fn handle_device_list(
    plugin: &MqttPlugin,
    _params_json: &[u8],
) -> Result<Value, MqttError> {
    let devices: Vec<Value> = plugin
        .registry()
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
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = params["device_id"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("device_id is required".into()))?;

    let registry = plugin.registry();
    let info = registry
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

/// `enforce_allowlist` is true for `mqtt_device_command`, which runs without
/// confirmation and so may only send what the allowlist admits; false for
/// `mqtt_device_command_confirmed`, which the agent gates on the user.
/// `device_id` for actions that build `vynkor/{device_id}/...` topics.
///
/// Only `[A-Za-z0-9_.-]{1,64}`: a `/` would address another device's topic
/// (the allowlist matched one id, the command lands under another), and
/// `+`/`#` would turn the response subscription into a wildcard.
fn topic_device_id(params: &Value) -> Result<&str, MqttError> {
    let id = params["device_id"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("device_id is required".into()))?;
    let valid = (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if !valid {
        return Err(MqttError::InvalidParams(format!(
            "device_id {id:?} must be 1-64 chars of A-Z a-z 0-9 _ . -"
        )));
    }
    Ok(id)
}

pub async fn handle_device_command(
    plugin: &mut MqttPlugin,
    params_json: &[u8],
    enforce_allowlist: bool,
) -> Result<Value, MqttError> {
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = topic_device_id(&params)?;
    let command = params["command"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("command is required".into()))?;
    let cmd_params = params.get("params").cloned().unwrap_or(json!({}));
    let timeout_ms = params["timeout_ms"].as_u64().unwrap_or(5000);

    if enforce_allowlist
        && !plugin
            .command_policy
            .allows(device_id, command, &cmd_params)
    {
        return Err(MqttError::NotAllowed(format!(
            "'{command}' on '{device_id}' is not in {}; \
             use mqtt_device_command_confirmed, which asks the user first",
            crate::policy::ENV
        )));
    }

    let msg_id = uuid::Uuid::new_v4().to_string();
    let request_topic = format!("vynkor/{device_id}/command");
    let response_topic = format!("vynkor/{device_id}/response");

    let command_msg = json!({
        "v": 1,
        "command": command,
        "msg_id": msg_id,
        "params": cmd_params,
        "timeout_ms": timeout_ms
    });

    let payload =
        serde_json::to_vec(&command_msg).map_err(|e| MqttError::Serialization(e.to_string()))?;

    let client = plugin.client.as_ref().ok_or(MqttError::NotConnected)?;

    let timeout = Duration::from_millis(timeout_ms);
    match client
        .request(
            &request_topic,
            &response_topic,
            payload,
            timeout,
            Some(&msg_id),
        )
        .await
    {
        Ok(response) => {
            plugin.messages_sent += 1;
            plugin.registry().messages_received += 1;

            let resp_json: Value = serde_json::from_slice(&response.payload)
                .map_err(|e| MqttError::Serialization(format!("device response: {e}")))?;

            let error_code = resp_json["error_code"].as_i64().unwrap_or(0);
            let error_message = resp_json["error"].as_str().unwrap_or_default();

            Ok(json!({
                "success": error_code == 0,
                "msg_id": msg_id,
                "data": resp_json.get("data").cloned().unwrap_or(json!({})),
                "error_code": error_code,
                "error_message": error_message,
                "execution_ms": resp_json.get("execution_ms").cloned().unwrap_or(json!(null)),
            }))
        }
        Err(MqttError::Timeout) => {
            plugin.messages_sent += 1;
            Ok(json!({
                "success": false,
                "msg_id": msg_id,
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
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = params["device_id"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("device_id is required".into()))?;

    let telemetry: Vec<Value> = plugin
        .registry()
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
    let params: Value =
        serde_json::from_slice(params_json).map_err(|e| MqttError::InvalidParams(e.to_string()))?;

    let device_id = topic_device_id(&params)?;
    let firmware_url = params["firmware_url"]
        .as_str()
        .ok_or_else(|| MqttError::InvalidParams("firmware_url is required".into()))?;

    let msg_id = uuid::Uuid::new_v4().to_string();
    let request_topic = format!("vynkor/{device_id}/command");
    let response_topic = format!("vynkor/{device_id}/response");

    let command_msg = json!({
        "v": 1,
        "command": "ota_update",
        "msg_id": msg_id,
        "params": { "url": firmware_url }
    });

    let payload =
        serde_json::to_vec(&command_msg).map_err(|e| MqttError::Serialization(e.to_string()))?;

    let client = plugin.client.as_ref().ok_or(MqttError::NotConnected)?;

    match client
        .request(
            &request_topic,
            &response_topic,
            payload,
            Duration::from_secs(30),
            Some(&msg_id),
        )
        .await
    {
        Ok(response) => {
            let resp_json: Value = serde_json::from_slice(&response.payload).unwrap_or(json!({}));

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
    async fn device_ids_that_would_escape_their_topic_are_rejected() {
        // `*:get_state` is in the default allowlist, so only this check
        // stands between a crafted id and someone else's command topic.
        let mut plugin = MqttPlugin::new();
        for bad in ["a/b", "+", "#", "esp/../x", "", "a\u{0}b", &"x".repeat(65)] {
            let params = json!({"device_id": bad, "command": "get_state"});
            let err = handle_device_command(&mut plugin, params.to_string().as_bytes(), true)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("device_id"), "{bad:?} -> {err}");
            let params = json!({"device_id": bad, "firmware_url": "http://x/fw.bin"});
            let err = handle_ota_update(&mut plugin, params.to_string().as_bytes())
                .await
                .unwrap_err();
            assert!(err.to_string().contains("device_id"), "{bad:?} -> {err}");
        }
    }

    #[tokio::test]
    async fn test_handle_device_command_not_connected() {
        let mut plugin = MqttPlugin::new();
        // get_state: allowlisted by default, so the connection check is reached.
        let params = json!({ "device_id": "esp-test", "command": "get_state" });
        let params_json = serde_json::to_vec(&params).unwrap();
        let result = handle_device_command(&mut plugin, &params_json, true).await;
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
        let result = handle_device_command(&mut plugin, &params_json, true).await;
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
        let params =
            json!({ "device_id": "esp-test", "firmware_url": "http://example.com/fw.bin" });
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

    mod live {
        use super::*;
        use crate::policy::CommandPolicy;
        use crate::test_support::*;

        async fn connected_plugin(port: u16, client_id: &str) -> MqttPlugin {
            let mut plugin = MqttPlugin::new();
            plugin.command_policy =
                CommandPolicy::parse("echo:set_pin:led_d1,*:get_state").unwrap();
            let params =
                json!({"broker_host": "127.0.0.1", "broker_port": port, "client_id": client_id});
            let reply = handle_connect(&mut plugin, params.to_string().as_bytes())
                .await
                .expect("connect");
            assert_eq!(reply["connected"], true);
            plugin
        }

        async fn live_broker() -> Option<(Broker, u16)> {
            let port = free_port();
            let Some(broker) = start_broker(port) else {
                eprintln!("mosquitto not installed; skipping live broker test");
                return None;
            };
            assert!(wait_port(port, true).await, "broker did not open its port");
            Some((broker, port))
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn registry_fills_without_any_action_running() {
            let Some((_broker, port)) = live_broker().await else {
                return;
            };
            let plugin = connected_plugin(port, "registry-sut").await;
            let (device, mut ready) = spawn_echo_device(port, "echo");
            ready.recv().await.expect("device ready");
            device
                .publish(
                    "vynkor/echo/status",
                    rumqttc::QoS::AtLeastOnce,
                    true,
                    r#"{"online":true,"firmware_name":"vynkor-gpio","pins":[{"name":"led_d1"}]}"#,
                )
                .await
                .unwrap();

            let mut found = false;
            for _ in 0..50 {
                if plugin
                    .registry()
                    .devices
                    .get("echo")
                    .is_some_and(|d| d.online)
                {
                    found = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(
                found,
                "status never reached the registry without an action pumping it"
            );
            let info = handle_device_info(&plugin, br#"{"device_id":"echo"}"#)
                .await
                .unwrap();
            assert_eq!(info["firmware"], "vynkor-gpio");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn reconnecting_with_the_same_client_id_keeps_one_stable_session() {
            let Some((_broker, port)) = live_broker().await else {
                return;
            };
            let mut plugin = connected_plugin(port, "same-id").await;
            let params =
                json!({"broker_host": "127.0.0.1", "broker_port": port, "client_id": "same-id"});
            handle_connect(&mut plugin, params.to_string().as_bytes())
                .await
                .expect("second connect");

            // A leaked first session would take the id back within ~2s,
            // kicking the new one off (and so on, forever).
            for _ in 0..40 {
                let status = handle_status(&plugin, b"{}").await.unwrap();
                assert_eq!(status["connected"], true, "session flapped after reconnect");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let (_device, mut ready) = spawn_echo_device(port, "echo");
            ready.recv().await.expect("device ready");
            let reply = handle_device_command(
                &mut plugin,
                br#"{"device_id":"echo","command":"get_state","timeout_ms":3000}"#,
                true,
            )
            .await
            .unwrap();
            assert_eq!(reply["success"], true, "{reply}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn allowlist_gates_the_unconfirmed_action_only() {
            let Some((_broker, port)) = live_broker().await else {
                return;
            };
            let mut plugin = connected_plugin(port, "policy-sut").await;
            let (_device, mut ready) = spawn_echo_device(port, "echo");
            ready.recv().await.expect("device ready");

            let allowed = handle_device_command(
                &mut plugin,
                br#"{"device_id":"echo","command":"set_pin","params":{"name":"led_d1","value":1},"timeout_ms":3000}"#,
                true,
            )
            .await
            .unwrap();
            assert_eq!(allowed["success"], true, "{allowed}");

            let sent_before = plugin.messages_sent;
            let refused = handle_device_command(
                &mut plugin,
                br#"{"device_id":"echo","command":"set_pin","params":{"name":"relay","value":1}}"#,
                true,
            )
            .await;
            match refused {
                Err(MqttError::NotAllowed(msg)) => {
                    assert!(msg.contains("mqtt_device_command_confirmed"), "{msg}")
                }
                other => panic!("expected NotAllowed, got {other:?}"),
            }
            assert_eq!(
                plugin.messages_sent, sent_before,
                "a refused command was published"
            );

            let confirmed = handle_device_command(
                &mut plugin,
                br#"{"device_id":"echo","command":"set_pin","params":{"name":"relay","value":1},"timeout_ms":3000}"#,
                false,
            )
            .await
            .unwrap();
            assert_eq!(confirmed["success"], true, "{confirmed}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn disconnect_reports_disconnected() {
            let Some((_broker, port)) = live_broker().await else {
                return;
            };
            let mut plugin = connected_plugin(port, "disc-sut").await;
            handle_disconnect(&mut plugin, b"{}").await.unwrap();
            let status = handle_status(&plugin, b"{}").await.unwrap();
            assert_eq!(status["connected"], false);
            assert!(matches!(
                handle_publish(&mut plugin, br#"{"topic":"t","payload":"p"}"#).await,
                Err(MqttError::NotConnected)
            ));
        }
    }
}
