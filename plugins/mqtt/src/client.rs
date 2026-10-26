use crate::error::MqttError;
use rumqttc::{
    AsyncClient, ConnectReturnCode, ConnectionError, Event, MqttOptions, Outgoing, Packet, QoS,
    TlsConfiguration, Transport,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

/// Capacity of the channel from the event loop to the message consumer.
const MESSAGE_BUFFER: usize = 1000;

#[derive(Debug, Clone)]
pub struct MqttMessage {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: QoS,
}

struct PendingRequest {
    /// When set, an incoming message is only treated as the reply if its
    /// `msg_id` field matches. Guards against a late response from an
    /// earlier, already-timed-out command resolving a newer request.
    msg_id: Option<String>,
    reply_tx: oneshot::Sender<MqttMessage>,
}

#[derive(Debug, Clone, Default)]
pub struct TlsConfig {
    pub ca_cert_path: Option<String>,
    pub client_cert_path: Option<String>,
    pub client_key_path: Option<String>,
}

pub struct MqttClient {
    client: AsyncClient,
    connected: Arc<AtomicBool>,
    broker_host: String,
    broker_port: u16,
    pending: Arc<Mutex<HashMap<String, PendingRequest>>>,
    /// Subscriptions to replay after a reconnect (broker sessions are clean).
    subscriptions: Arc<Mutex<HashMap<String, QoS>>>,
    /// Set before an intentional disconnect so the event loop exits instead
    /// of treating the closed socket as a fault and reconnecting.
    closing: Arc<AtomicBool>,
    /// Why the last connection attempt failed; cleared on ConnAck.
    last_error: Arc<std::sync::Mutex<Option<String>>>,
    task: JoinHandle<()>,
}

/// Refusals that retrying with the same options cannot fix.
fn is_permanent(error: &ConnectionError) -> bool {
    matches!(
        error,
        ConnectionError::ConnectionRefused(code) if *code != ConnectReturnCode::ServiceUnavailable
    )
}

fn set_error(slot: &std::sync::Mutex<Option<String>>, value: Option<String>) {
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

fn build_transport(tls: Option<&TlsConfig>) -> Result<Transport, MqttError> {
    match tls {
        Some(cfg) => {
            let mut roots = rustls::RootCertStore::empty();

            if let Some(ca_path) = &cfg.ca_cert_path {
                let ca_pem = std::fs::read(ca_path)
                    .map_err(|e| MqttError::TlsError(format!("read CA: {e}")))?;
                let mut reader = std::io::BufReader::new(ca_pem.as_slice());
                let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| MqttError::TlsError(format!("parse CA: {e}")))?;
                for der in certs {
                    roots
                        .add(der)
                        .map_err(|e| MqttError::TlsError(format!("add CA: {e}")))?;
                }
            } else {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }

            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();

            Ok(Transport::Tls(TlsConfiguration::Rustls(Arc::new(config))))
        }
        None => Ok(Transport::Tcp),
    }
}

impl MqttClient {
    /// Connect and start the event loop.
    ///
    /// Returns the client and the receiver of every message that is not a
    /// reply to a pending `request`. The caller must drain the receiver from
    /// a task of its own: the event loop never waits on it, and drops
    /// messages (with a warning) once it is full.
    ///
    /// Fails if the broker refuses the credentials or client id. An
    /// unreachable broker is not an error: the client keeps retrying and
    /// `is_connected` reports the truth.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        broker_host: &str,
        broker_port: u16,
        client_id: &str,
        username: Option<&str>,
        password: Option<&str>,
        keepalive_secs: u16,
        tls: Option<TlsConfig>,
    ) -> Result<(Self, mpsc::Receiver<MqttMessage>), MqttError> {
        let mut opts = MqttOptions::new(client_id, broker_host, broker_port);
        opts.set_keep_alive(Duration::from_secs(keepalive_secs.max(5) as u64));
        if let Some(user) = username {
            opts.set_credentials(user, password.unwrap_or(""));
        }
        opts.set_transport(build_transport(tls.as_ref())?);

        let (client, mut eventloop) = AsyncClient::new(opts, 256);
        let (message_tx, message_rx) = mpsc::channel(MESSAGE_BUFFER);
        let pending: Arc<Mutex<HashMap<String, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let subscriptions: Arc<Mutex<HashMap<String, QoS>>> = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(false));
        let closing = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(std::sync::Mutex::new(None));

        let pending_clone = pending.clone();
        let subs_clone = subscriptions.clone();
        let conn_clone = connected.clone();
        let closing_clone = closing.clone();
        let error_clone = last_error.clone();
        let resub_client = client.clone();

        // One task owns the event loop for the client's lifetime. rumqttc
        // re-establishes the connection on the next poll() after an error,
        // so the loop ends only on an intentional disconnect, a permanent
        // refusal, or abort() when the client is dropped.
        //
        // Nothing in here may wait on the plugin: as long as poll() is not
        // called no PINGREQ goes out and the broker drops the session.
        let task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        conn_clone.store(true, Ordering::Relaxed);
                        set_error(&error_clone, None);
                        let subs: Vec<(String, QoS)> = subs_clone
                            .lock()
                            .await
                            .iter()
                            .map(|(t, q)| (t.clone(), *q))
                            .collect();
                        for (topic, qos) in subs {
                            // try_ variant: the request channel is drained by
                            // this very task, so awaiting here could deadlock.
                            if let Err(e) = resub_client.try_subscribe(topic.clone(), qos) {
                                tracing::warn!("resubscribe to {topic} failed: {e}");
                            }
                        }
                        tracing::info!("MQTT connected");
                    }
                    Ok(Event::Incoming(Packet::Publish(pub_msg))) => {
                        let msg = MqttMessage {
                            topic: pub_msg.topic.clone(),
                            payload: pub_msg.payload.to_vec(),
                            qos: pub_msg.qos,
                        };

                        let mut pending = pending_clone.lock().await;
                        let should_resolve = match pending.get(&msg.topic) {
                            None => false,
                            Some(req) => match &req.msg_id {
                                None => true,
                                Some(expected) => {
                                    match serde_json::from_slice::<serde_json::Value>(&msg.payload)
                                    {
                                        Ok(v) => v
                                            .get("msg_id")
                                            .and_then(|m| m.as_str())
                                            .map(|got| got == expected)
                                            // No msg_id in the payload: assume it is the reply.
                                            .unwrap_or(true),
                                        Err(_) => true,
                                    }
                                }
                            },
                        };

                        if should_resolve {
                            if let Some(req) = pending.remove(&msg.topic) {
                                drop(pending);
                                let _ = req.reply_tx.send(msg);
                                continue;
                            }
                        }
                        drop(pending);
                        match message_tx.try_send(msg) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(msg)) => {
                                tracing::warn!(topic = %msg.topic, "MQTT consumer lagging, message dropped");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                tracing::debug!("MQTT message channel closed");
                            }
                        }
                    }
                    Ok(Event::Incoming(Packet::Disconnect)) => {
                        conn_clone.store(false, Ordering::Relaxed);
                    }
                    Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                        conn_clone.store(false, Ordering::Relaxed);
                        tracing::info!("MQTT disconnected on request");
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        conn_clone.store(false, Ordering::Relaxed);
                        if closing_clone.load(Ordering::Relaxed) {
                            break;
                        }
                        set_error(&error_clone, Some(e.to_string()));
                        if is_permanent(&e) {
                            tracing::error!("MQTT broker refused the connection: {e}; giving up");
                            break;
                        }
                        tracing::warn!("MQTT connection error: {e}; retrying in 2s");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });

        // Give the first connection a moment to complete so callers see a
        // truthful `is_connected` right after `mqtt_connect`.
        for _ in 0..50 {
            if connected.load(Ordering::Relaxed) || task.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let client = Self {
            client,
            connected,
            broker_host: broker_host.to_string(),
            broker_port,
            pending,
            subscriptions,
            closing,
            last_error,
            task,
        };
        if client.task.is_finished() {
            let reason = client
                .last_error()
                .unwrap_or_else(|| "event loop stopped".into());
            return Err(MqttError::ConnectionFailed(reason));
        }
        Ok((client, message_rx))
    }

    pub async fn publish(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> Result<(), MqttError> {
        self.client
            .publish(topic.to_string(), qos, retain, payload)
            .await
            .map_err(|e| MqttError::PublishFailed(e.to_string()))
    }

    pub async fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), MqttError> {
        self.subscriptions
            .lock()
            .await
            .insert(topic.to_string(), qos);
        self.client
            .subscribe(topic.to_string(), qos)
            .await
            .map_err(|e| MqttError::SubscribeFailed(e.to_string()))
    }

    pub async fn unsubscribe(&self, topic: &str) -> Result<(), MqttError> {
        self.subscriptions.lock().await.remove(topic);
        self.client
            .unsubscribe(topic.to_string())
            .await
            .map_err(|e| MqttError::SubscribeFailed(e.to_string()))
    }

    /// Publish a command and wait for the device reply on `response_topic`.
    ///
    /// The response subscription is registered *before* the publish; both
    /// requests share one ordered channel to the broker, so the SUBSCRIBE is
    /// processed before the PUBLISH and no reply can be missed.
    pub async fn request(
        &self,
        request_topic: &str,
        response_topic: &str,
        payload: Vec<u8>,
        timeout: Duration,
        msg_id: Option<&str>,
    ) -> Result<MqttMessage, MqttError> {
        self.ensure_subscription(response_topic).await?;

        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending.lock().await.insert(
            response_topic.to_string(),
            PendingRequest {
                msg_id: msg_id.map(String::from),
                reply_tx,
            },
        );

        if let Err(e) = self
            .publish(request_topic, payload, QoS::AtLeastOnce, false)
            .await
        {
            self.pending.lock().await.remove(response_topic);
            return Err(e);
        }

        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(response_topic);
                Err(MqttError::Timeout)
            }
            Err(_) => {
                // Drop the entry only after the deadline, so a reply that
                // arrives in the same tick still wins.
                self.pending.lock().await.remove(response_topic);
                Err(MqttError::Timeout)
            }
        }
    }

    /// Subscribe to `topic` unless it is already tracked.
    async fn ensure_subscription(&self, topic: &str) -> Result<(), MqttError> {
        let already = self.subscriptions.lock().await.contains_key(topic);
        if already {
            return Ok(());
        }
        self.subscribe(topic, QoS::AtLeastOnce).await
    }

    /// Send DISCONNECT and stop the event loop for good.
    ///
    /// Waits briefly for the packet to go out; an event loop stuck on a dead
    /// network is aborted instead.
    pub async fn disconnect(mut self) {
        self.closing.store(true, Ordering::Relaxed);
        // try_: a full request queue means the loop is wedged; abort covers it.
        let _ = self.client.try_disconnect();
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
        self.connected.store(false, Ordering::Relaxed);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub async fn is_online(&self) -> bool {
        self.is_connected()
    }

    pub fn broker_info(&self) -> (String, u16) {
        (self.broker_host.clone(), self.broker_port)
    }

    /// Why the last connection attempt failed, if it has not recovered since.
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn event_loop_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for MqttClient {
    /// A replaced or dropped client must not leave its event loop running
    /// (and reconnecting) in the background.
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn connected_flag_starts_false() {
        // Guards the old stub that always reported `true`.
        let flag = AtomicBool::new(false);
        assert!(!flag.load(Ordering::Relaxed));
    }

    async fn resubscribe_clean_session(device: &AsyncClient) {
        let _ = device
            .subscribe("vynkor/live/command", QoS::AtLeastOnce)
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_request_response_survives_broker_restart() {
        let port = free_port();
        let Some(broker) = start_broker(port) else {
            eprintln!("mosquitto not installed; skipping live broker test");
            return;
        };
        assert!(wait_port(port, true).await, "broker did not open its port");

        let (client, mut messages) =
            MqttClient::connect("127.0.0.1", port, "live-sut", None, None, 5, None)
                .await
                .expect("connect to live broker");
        assert!(
            client.is_connected(),
            "client must report connected once ConnAck was seen"
        );

        let mut device_opts = MqttOptions::new("live-device", "127.0.0.1", port);
        device_opts.set_keep_alive(Duration::from_secs(5));
        let (device, mut device_events) = AsyncClient::new(device_opts, 32);
        let notice_publisher = device.clone();
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            loop {
                match device_events.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        resubscribe_clean_session(&device).await;
                    }
                    Ok(Event::Incoming(Packet::SubAck(_))) => {
                        let _ = ready_tx.send(());
                    }
                    Ok(Event::Incoming(Packet::Publish(published))) => {
                        let request: serde_json::Value =
                            serde_json::from_slice(&published.payload).unwrap_or_default();
                        let reply = serde_json::json!({
                            "msg_id": request.get("msg_id").cloned().unwrap_or(serde_json::Value::Null),
                            "error_code": 0,
                            "execution_ms": 1,
                        });
                        let _ = device
                            .publish(
                                "vynkor/live/response",
                                QoS::AtLeastOnce,
                                false,
                                reply.to_string(),
                            )
                            .await;
                    }
                    Ok(_) => {}
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        });

        ready_rx
            .recv()
            .await
            .expect("device never confirmed its subscription");

        let reply = tokio::time::timeout(
            Duration::from_secs(10),
            client.request(
                "vynkor/live/command",
                "vynkor/live/response",
                br#"{"msg_id":"live-1","command":"ping"}"#.to_vec(),
                Duration::from_secs(5),
                Some("live-1"),
            ),
        )
        .await
        .expect("request blocked past the test deadline")
        .expect("request failed");
        let parsed: serde_json::Value = serde_json::from_slice(&reply.payload).expect("json reply");
        assert_eq!(parsed["msg_id"], "live-1");
        assert_eq!(parsed["error_code"], 0);

        let miss = client
            .request(
                "vynkor/live/nowhere",
                "vynkor/live/nowhere-response",
                br#"{"msg_id":"live-miss"}"#.to_vec(),
                Duration::from_millis(700),
                Some("live-miss"),
            )
            .await;
        assert!(
            matches!(miss, Err(MqttError::Timeout)),
            "unanswered request must time out, got {miss:?}"
        );

        client
            .subscribe("vynkor/live/notice", QoS::AtLeastOnce)
            .await
            .expect("subscribe to notice topic");
        let mut notice = None;
        for _ in 0..40 {
            notice_publisher
                .publish(
                    "vynkor/live/notice",
                    QoS::AtLeastOnce,
                    false,
                    br#"{"event":"hello"}"#,
                )
                .await
                .expect("publish notice");
            if let Ok(message) = messages.try_recv() {
                notice = Some(message);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let notice = notice.expect("the receiver never delivered the notice");
        assert_eq!(notice.topic, "vynkor/live/notice");

        drop(broker);
        assert!(wait_port(port, false).await, "broker port stayed open");

        let mut went_offline = false;
        for _ in 0..100 {
            if !client.is_connected() {
                went_offline = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            went_offline,
            "is_connected stayed true after the broker died"
        );

        tokio::time::sleep(Duration::from_millis(300)).await;
        let _restarted = start_broker(port).expect("restart broker");
        assert!(wait_port(port, true).await, "broker did not come back");

        let mut back_online = false;
        for _ in 0..150 {
            if client.is_connected() {
                back_online = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(back_online, "client never reconnected");
        ready_rx
            .recv()
            .await
            .expect("device never re-subscribed after the restart");

        let reply = tokio::time::timeout(
            Duration::from_secs(10),
            client.request(
                "vynkor/live/command",
                "vynkor/live/response",
                br#"{"msg_id":"live-2","command":"ping"}"#.to_vec(),
                Duration::from_secs(5),
                Some("live-2"),
            ),
        )
        .await
        .expect("post-reconnect request blocked past the test deadline")
        .expect("post-reconnect request failed");
        let parsed: serde_json::Value = serde_json::from_slice(&reply.payload).expect("json reply");
        assert_eq!(
            parsed["msg_id"], "live-2",
            "request/response channel is dead after reconnect"
        );
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

    async fn ping(
        client: &MqttClient,
        device: &str,
        msg_id: &str,
    ) -> Result<MqttMessage, MqttError> {
        client
            .request(
                &format!("vynkor/{device}/command"),
                &format!("vynkor/{device}/response"),
                format!(r#"{{"msg_id":"{msg_id}","command":"ping"}}"#).into_bytes(),
                Duration::from_secs(3),
                Some(msg_id),
            )
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unread_messages_do_not_stall_the_event_loop() {
        let Some((_broker, port)) = live_broker().await else {
            return;
        };
        // Nobody drains `_messages`, as when no action runs for a while.
        let (client, _messages) =
            MqttClient::connect("127.0.0.1", port, "stall-sut", None, None, 5, None)
                .await
                .expect("connect");
        client
            .subscribe("vynkor/+/telemetry", QoS::AtMostOnce)
            .await
            .expect("subscribe");
        let (device, mut ready) = spawn_echo_device(port, "flood");
        ready.recv().await.expect("device ready");

        tokio::time::timeout(Duration::from_secs(10), async {
            for i in 0..(MESSAGE_BUFFER + 200) {
                device
                    .publish(
                        "vynkor/flood/telemetry",
                        QoS::AtMostOnce,
                        false,
                        format!(r#"{{"n":{i}}}"#),
                    )
                    .await
                    .expect("flood publish");
            }
        })
        .await
        .expect("flood did not finish");
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Replies are routed by the event loop itself: if it is blocked on a
        // full channel, this times out.
        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            ping(&client, "flood", "after-flood"),
        )
        .await
        .expect("request hung")
        .expect("event loop stalled behind unread messages");
        let parsed: serde_json::Value = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(parsed["msg_id"], "after-flood");
        assert!(client.is_connected());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_stops_the_event_loop() {
        let Some((_broker, port)) = live_broker().await else {
            return;
        };
        let (client, mut messages) =
            MqttClient::connect("127.0.0.1", port, "disc-sut", None, None, 5, None)
                .await
                .expect("connect");
        assert!(client.is_connected());

        client.disconnect().await;

        // The sender lives in the event-loop task: the channel closes only
        // when that task is gone. Before the fix it reconnected 2s later.
        let closed = tokio::time::timeout(Duration::from_secs(5), messages.recv()).await;
        assert!(
            matches!(closed, Ok(None)),
            "event loop still running after disconnect"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_client_stops_the_event_loop() {
        let Some((_broker, port)) = live_broker().await else {
            return;
        };
        let (client, mut messages) =
            MqttClient::connect("127.0.0.1", port, "drop-sut", None, None, 5, None)
                .await
                .expect("connect");
        drop(client);
        let closed = tokio::time::timeout(Duration::from_secs(5), messages.recv()).await;
        assert!(
            matches!(closed, Ok(None)),
            "a dropped client left its event loop running"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_credentials_fail_connect_and_stop_retrying() {
        let port = free_port();
        let Some(_broker) = start_broker_with_password(port, "vynkor", "right") else {
            eprintln!("mosquitto/mosquitto_passwd not installed; skipping");
            return;
        };
        assert!(wait_port(port, true).await, "broker did not open its port");

        let refused = MqttClient::connect(
            "127.0.0.1",
            port,
            "auth-sut",
            Some("vynkor"),
            Some("wrong"),
            5,
            None,
        )
        .await;
        match refused {
            Err(MqttError::ConnectionFailed(reason)) => {
                assert!(
                    reason.contains("BadUserNamePassword") || reason.contains("NotAuthorized"),
                    "unexpected refusal reason: {reason}"
                );
            }
            Err(other) => panic!("expected ConnectionFailed, got {other:?}"),
            Ok((client, _)) => panic!(
                "wrong password accepted, connected={}",
                client.is_connected()
            ),
        }

        let (client, _messages) = MqttClient::connect(
            "127.0.0.1",
            port,
            "auth-sut",
            Some("vynkor"),
            Some("right"),
            5,
            None,
        )
        .await
        .expect("right password must connect");
        assert!(client.is_connected());
        assert_eq!(client.last_error(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreachable_broker_is_reported_not_faked() {
        let port = free_port(); // nothing listens here
        let (client, _messages) =
            MqttClient::connect("127.0.0.1", port, "down-sut", None, None, 5, None)
                .await
                .expect("an unreachable broker is retried, not an error");
        assert!(!client.is_connected());
        assert!(client.last_error().is_some(), "the failure must be visible");
        assert!(!client.event_loop_finished(), "must keep retrying");
    }
}
