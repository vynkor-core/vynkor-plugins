use crate::error::MqttError;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, Transport, TlsConfiguration};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, Clone)]
pub struct MqttMessage {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: QoS,
}

struct PendingRequest {
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
    connected: Arc<Mutex<bool>>,
    broker_host: String,
    broker_port: u16,
    tls: Option<TlsConfig>,
    message_tx: mpsc::Sender<MqttMessage>,
    message_rx: mpsc::Receiver<MqttMessage>,
    pending: Arc<Mutex<HashMap<String, PendingRequest>>>,
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
                    roots.add(der).map_err(|e| MqttError::TlsError(format!("add CA: {e}")))?;
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
    pub async fn connect(
        broker_host: &str,
        broker_port: u16,
        client_id: &str,
        username: Option<&str>,
        password: Option<&str>,
        keepalive_secs: u16,
        tls: Option<TlsConfig>,
    ) -> Result<Self, MqttError> {
        let mut opts = MqttOptions::new(client_id, broker_host, broker_port);
        opts.set_keep_alive(Duration::from_secs(keepalive_secs as u64));
        if let Some(user) = username {
            opts.set_credentials(user, password.unwrap_or(""));
        }

        let transport = build_transport(tls.as_ref())?;
        opts.set_transport(transport);

        let (client, mut eventloop) = AsyncClient::new(opts, 100);
        let (message_tx, message_rx) = mpsc::channel(1000);
        let pending: Arc<Mutex<HashMap<String, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(Mutex::new(true));

        let tx = message_tx.clone();
        let pending_clone = pending.clone();
        let conn_clone = connected.clone();
        let client_id_str = client_id.to_string();
        let host = broker_host.to_string();
        let port = broker_port;
        let user_owned = username.map(String::from);
        let pass_owned = password.map(String::from);
        let tls_clone = tls.clone();

        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::Publish(pub_msg))) => {
                        let msg = MqttMessage {
                            topic: pub_msg.topic.clone(),
                            payload: pub_msg.payload.to_vec(),
                            qos: pub_msg.qos,
                        };

                        let mut pending = pending_clone.lock().await;
                        if let Some(req) = pending.remove(&msg.topic) {
                            let _ = req.reply_tx.send(msg);
                        } else {
                            drop(pending);
                            let _ = tx.send(msg).await;
                        }
                    }
                    Ok(Event::Incoming(Packet::SubAck(_))) => {}
                    Ok(Event::Incoming(Packet::PubAck(_))) => {}
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("MQTT error: {e}, reconnecting in 2s...");
                        *conn_clone.lock().await = false;
                        tokio::time::sleep(Duration::from_secs(2)).await;

                        let mut opts = MqttOptions::new(&client_id_str, &host, port);
                        opts.set_keep_alive(Duration::from_secs(60));
                        if let Some(ref u) = user_owned {
                            opts.set_credentials(u, pass_owned.as_deref().unwrap_or(""));
                        }
                        if let Ok(transport) = build_transport(tls_clone.as_ref()) {
                            opts.set_transport(transport);
                        }
                        let (_new_client, new_eventloop) = AsyncClient::new(opts, 100);

                        tracing::info!("Reconnected to {host}:{port}");
                        *conn_clone.lock().await = true;
                        eventloop = new_eventloop;
                    }
                }
            }
        });

        Ok(Self {
            client,
            connected,
            broker_host: broker_host.to_string(),
            broker_port,
            tls,
            message_tx,
            message_rx,
            pending,
        })
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
        self.client
            .subscribe(topic.to_string(), qos)
            .await
            .map_err(|e| MqttError::SubscribeFailed(e.to_string()))
    }

    pub async fn unsubscribe(&self, topic: &str) -> Result<(), MqttError> {
        self.client
            .unsubscribe(topic.to_string())
            .await
            .map_err(|e| MqttError::SubscribeFailed(e.to_string()))
    }

    pub async fn request(
        &self,
        request_topic: &str,
        response_topic: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<MqttMessage, MqttError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.pending
            .lock()
            .await
            .insert(response_topic.to_string(), PendingRequest { reply_tx });

        self.publish(request_topic, payload, QoS::AtLeastOnce, false)
            .await?;

        tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| MqttError::Timeout)?
            .map_err(|_| MqttError::Timeout)
    }

    pub async fn disconnect(&self) -> Result<(), MqttError> {
        self.client
            .disconnect()
            .await
            .map_err(|e| MqttError::Client(e.to_string()))?;
        *self.connected.lock().await = false;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        true
    }

    pub async fn is_online(&self) -> bool {
        *self.connected.lock().await
    }

    pub fn broker_info(&self) -> (String, u16) {
        (self.broker_host.clone(), self.broker_port)
    }

    pub async fn recv(&mut self) -> Option<MqttMessage> {
        self.message_rx.recv().await
    }
}
