use grammers_client::client::UpdatesConfiguration;
use grammers_client::Update;
use tokio::sync::mpsc;

use crate::EventToPublish;

/// Backoff after a stream error, before retrying `next()`. Reset to this on
/// every success so a single blip doesn't cause a lingering slow-poll.
const RETRY_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(1);
/// Ceiling for the exponential backoff, so a sustained outage still retries
/// often enough to recover promptly once connectivity returns.
const RETRY_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// Past this many consecutive stream errors the process exits so the
/// supervisor respawns it: a connection that never recovers otherwise hangs
/// the plugin indefinitely while it still looks "up".
const MAX_CONSECUTIVE_ERRORS: u32 = 20;

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
        let mut backoff = RETRY_BACKOFF_BASE;
        let mut consecutive_errors: u32 = 0;

        // `UpdateStream::next()` takes `&mut self` and surfaces transient RPC
        // errors (flood waits, timeouts) without consuming the stream — its
        // internal message_box state survives the error. So on error we just
        // back off and retry the same stream instead of letting the task exit
        // and silently killing live updates. Bounded by MAX_CONSECUTIVE_ERRORS.
        loop {
            match stream.next().await {
                Ok(update) => {
                    backoff = RETRY_BACKOFF_BASE;
                    consecutive_errors = 0;
                    handle_update(&client, update, &event_tx).await;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    tracing::warn!(
                        consecutive_errors,
                        "update stream error, retrying in {backoff:?}: {e}"
                    );
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        tracing::error!(
                            consecutive_errors,
                            "update stream stuck after {MAX_CONSECUTIVE_ERRORS} consecutive errors, exiting for a clean supervisor restart"
                        );
                        std::process::exit(1);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RETRY_BACKOFF_MAX);
                }
            }
        }
    });
}

async fn handle_update(
    _client: &grammers_client::Client,
    update: Update,
    event_tx: &mpsc::Sender<EventToPublish>,
) {
    let message = match update {
        Update::NewMessage(m) => m,
        _ => return,
    };
    if message.outgoing() {
        return;
    }

        let peer_str = message
            .peer()
            .map(|p| crate::peer_to_string(p))
            .unwrap_or_else(|_| {
                message
                    .sender()
                    .map(|p| crate::peer_to_string(p))
                    .unwrap_or_default()
            });

        let sender_str = message
            .sender()
            .map(|p| crate::peer_to_string(p))
            .unwrap_or_else(|| peer_str.clone());

        let media_type = message.media().map(|m| match m {
            grammers_client::types::Media::Contact(_) => "contact",
            grammers_client::types::Media::Document(_) => "document",
            grammers_client::types::Media::Geo(_) => "geo",
            grammers_client::types::Media::Photo(_) => "photo",
            grammers_client::types::Media::Poll(_) => "poll",
            grammers_client::types::Media::Sticker(_) => "sticker",
            grammers_client::types::Media::Venue(_) => "venue",
            grammers_client::types::Media::WebPage(_) => "webpage",
            _ => "unknown",
        }).unwrap_or("none");
        let has_media = message.media().is_some();
        let is_voice = matches!(message.media(), Some(grammers_client::types::Media::Document(_)));
        let payload = serde_json::json!({
            "message_id": message.id(),
            "peer": peer_str,
            "sender": sender_str,
            "text": message.text(),
            "date": message.date().to_rfc3339(),
            "has_media": has_media,
            "media_type": media_type,
            "is_voice_guess": is_voice,
        });

        let _ = event_tx
            .send(EventToPublish {
                event_type: "new_message".into(),
                payload,
            })
            .await;
}
