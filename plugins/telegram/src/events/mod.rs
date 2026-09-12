use grammers_client::client::UpdatesConfiguration;
use grammers_client::Update;
use tokio::sync::mpsc;

use crate::EventToPublish;

pub fn spawn_live_listener(
    client: grammers_client::Client,
    updates_rx: tokio::sync::mpsc::UnboundedReceiver<grammers_session::updates::UpdatesLike>,
    event_tx: mpsc::Sender<EventToPublish>,
) {
    tokio::spawn(async move {
        let config = UpdatesConfiguration {
            catch_up: true,
            update_queue_limit: Some(100),
        };

        let mut stream = client.stream_updates(updates_rx, config);

        loop {
            match stream.next().await {
                Ok(update) => {
                    handle_update(&client, update, &event_tx).await;
                }
                Err(e) => {
                    tracing::warn!("update stream error: {e}");
                    break;
                }
            }
        }

        stream.sync_update_state();
    });
}

async fn handle_update(
    _client: &grammers_client::Client,
    update: Update,
    event_tx: &mpsc::Sender<EventToPublish>,
) {
    if let Update::NewMessage(message) = update {
        if message.outgoing() {
            return;
        }

        let peer = message.peer();
        let peer_str = peer
            .map(crate::peer_to_string)
            .unwrap_or_default();

        let sender_str = message
            .sender()
            .map(crate::peer_to_string)
            .unwrap_or_default();

        let payload = serde_json::json!({
            "message_id": message.id(),
            "peer": peer_str,
            "sender": sender_str,
            "text": message.text(),
            "date": message.date().to_rfc3339(),
        });

        let _ = event_tx
            .send(EventToPublish {
                event_type: "plugin.telegram.new_message".into(),
                payload,
            })
            .await;
    }
}
