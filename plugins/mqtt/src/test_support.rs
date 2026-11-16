//! Live-broker helpers shared by the client and action tests.
//!
//! Each test runs its own mosquitto on an ephemeral port; tests skip (and
//! say so) when mosquitto is not installed.

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::sync::mpsc;

pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

pub struct Broker {
    child: Child,
    dir: Option<PathBuf>,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

pub fn start_broker(port: u16) -> Option<Broker> {
    let child = Command::new("mosquitto")
        .args(["-p", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    Some(Broker { child, dir: None })
}

/// A broker that only admits `user`/`password`.
pub fn start_broker_with_password(port: u16, user: &str, password: &str) -> Option<Broker> {
    let dir = std::env::temp_dir().join(format!("mqtt-plugin-test-{port}"));
    std::fs::create_dir_all(&dir).ok()?;
    let passwd = dir.join("passwd");
    let made = Command::new("mosquitto_passwd")
        .arg("-b")
        .arg("-c")
        .arg(&passwd)
        .args([user, password])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !made.success() {
        return None;
    }
    let conf = dir.join("mosquitto.conf");
    std::fs::write(
        &conf,
        format!(
            "listener {port} 127.0.0.1\nallow_anonymous false\npassword_file {}\n",
            passwd.display()
        ),
    )
    .ok()?;
    let child = Command::new("mosquitto")
        .arg("-c")
        .arg(&conf)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    Some(Broker {
        child,
        dir: Some(dir),
    })
}

pub async fn wait_port(port: u16, up: bool) -> bool {
    for _ in 0..100 {
        let open = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok();
        if open == up {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// A fake VDP device: answers every command on `vynkor/{id}/command` with
/// `{"msg_id": <echoed>, "error_code": 0}` on `vynkor/{id}/response`.
///
/// The receiver yields once per SUBACK, i.e. when the device is ready.
pub fn spawn_echo_device(port: u16, id: &str) -> (AsyncClient, mpsc::UnboundedReceiver<()>) {
    let mut opts = MqttOptions::new(format!("device-{id}"), "127.0.0.1", port);
    opts.set_keep_alive(Duration::from_secs(5));
    let (device, mut events) = AsyncClient::new(opts, 64);
    let handle = device.clone();
    let command_topic = format!("vynkor/{id}/command");
    let response_topic = format!("vynkor/{id}/response");
    let (ready_tx, ready_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match events.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    let _ = device.try_subscribe(command_topic.clone(), QoS::AtLeastOnce);
                }
                Ok(Event::Incoming(Packet::SubAck(_))) => {
                    let _ = ready_tx.send(());
                }
                Ok(Event::Incoming(Packet::Publish(published)))
                    if published.topic == command_topic =>
                {
                    let request: serde_json::Value =
                        serde_json::from_slice(&published.payload).unwrap_or_default();
                    let reply = serde_json::json!({
                        "msg_id": request.get("msg_id").cloned().unwrap_or_default(),
                        "error_code": 0,
                    });
                    let _ = device.try_publish(
                        response_topic.clone(),
                        QoS::AtLeastOnce,
                        false,
                        reply.to_string(),
                    );
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    });
    (handle, ready_rx)
}
