//! Stream manager for MQTT subscriptions (R6 streaming)

use crate::client::MqttMessage;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// Stream subscription
#[derive(Debug)]
pub struct Subscription {
    pub topic: String,
    pub qos: rumqttc::QoS,
    pub sender: mpsc::Sender<MqttMessage>,
}

/// Stream manager for subscriptions
pub struct StreamManager {
    /// Active subscriptions: topic -> subscription
    subscriptions: Arc<RwLock<HashMap<String, Subscription>>>,
}

impl StreamManager {
    /// Create a new stream manager
    pub fn new() -> Self {
        Self {
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a subscription
    pub async fn add_subscription(
        &self,
        topic: &str,
        qos: rumqttc::QoS,
    ) -> mpsc::Receiver<MqttMessage> {
        let (tx, rx) = mpsc::channel(1000);

        let sub = Subscription {
            topic: topic.to_string(),
            qos,
            sender: tx,
        };

        self.subscriptions
            .write()
            .await
            .insert(topic.to_string(), sub);

        rx
    }

    /// Remove a subscription
    pub async fn remove_subscription(&self, topic: &str) -> bool {
        self.subscriptions.write().await.remove(topic).is_some()
    }

    /// Dispatch incoming message to matching subscription
    pub async fn dispatch(&self, msg: &MqttMessage) {
        let subs = self.subscriptions.read().await;

        // Exact match
        if let Some(sub) = subs.get(&msg.topic) {
            let _ = sub.sender.send(msg.clone()).await;
            return;
        }

        // Wildcard match (simple: check if topic starts with subscription prefix)
        for (pattern, sub) in subs.iter() {
            if pattern.ends_with("#") {
                let prefix = pattern.trim_end_matches('#');
                if msg.topic.starts_with(prefix) {
                    let _ = sub.sender.send(msg.clone()).await;
                }
            } else if pattern.contains('+') {
                // Simple + matching (single level)
                let pattern_parts: Vec<&str> = pattern.split('/').collect();
                let topic_parts: Vec<&str> = msg.topic.split('/').collect();

                if pattern_parts.len() == topic_parts.len() {
                    let mut matches = true;
                    for (p, t) in pattern_parts.iter().zip(topic_parts.iter()) {
                        if *p != "+" && *p != *t {
                            matches = false;
                            break;
                        }
                    }
                    if matches {
                        let _ = sub.sender.send(msg.clone()).await;
                    }
                }
            }
        }
    }

    /// Get active subscription count
    pub async fn subscription_count(&self) -> usize {
        self.subscriptions.read().await.len()
    }

    /// Get all subscribed topics
    pub async fn subscribed_topics(&self) -> Vec<String> {
        self.subscriptions
            .read()
            .await
            .keys()
            .cloned()
            .collect()
    }
}

impl Default for StreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_add_subscription() {
        let manager = StreamManager::new();
        let mut rx = manager.add_subscription("test/topic", rumqttc::QoS::AtMostOnce).await;

        assert_eq!(manager.subscription_count().await, 1);
        assert!(manager.subscribed_topics().await.contains(&"test/topic".to_string()));

        let msg = MqttMessage {
            topic: "test/topic".to_string(),
            payload: b"hello".to_vec(),
            qos: rumqttc::QoS::AtMostOnce,
        };

        manager.dispatch(&msg).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.topic, "test/topic");
        assert_eq!(received.payload, b"hello");
    }

    #[tokio::test]
    async fn test_remove_subscription() {
        let manager = StreamManager::new();
        let _rx = manager.add_subscription("test/topic", rumqttc::QoS::AtMostOnce).await;

        assert_eq!(manager.subscription_count().await, 1);

        let removed = manager.remove_subscription("test/topic").await;
        assert!(removed);
        assert_eq!(manager.subscription_count().await, 0);
    }

    #[tokio::test]
    async fn test_remove_nonexistent_subscription() {
        let manager = StreamManager::new();
        let removed = manager.remove_subscription("nonexistent").await;
        assert!(!removed);
    }

    #[tokio::test]
    async fn test_dispatch_wildcard_hash() {
        let manager = StreamManager::new();
        let mut rx = manager.add_subscription("vynkor/+/telemetry", rumqttc::QoS::AtMostOnce).await;

        let msg = MqttMessage {
            topic: "vynkor/esp-kitchen/telemetry".to_string(),
            payload: b"temp=23.5".to_vec(),
            qos: rumqttc::QoS::AtMostOnce,
        };

        manager.dispatch(&msg).await;

        let received = rx.recv().await.unwrap();
        assert_eq!(received.topic, "vynkor/esp-kitchen/telemetry");
    }

    #[tokio::test]
    async fn test_dispatch_no_match() {
        let manager = StreamManager::new();
        let mut rx = manager.add_subscription("test/topic", rumqttc::QoS::AtMostOnce).await;

        let msg = MqttMessage {
            topic: "other/topic".to_string(),
            payload: b"hello".to_vec(),
            qos: rumqttc::QoS::AtMostOnce,
        };

        manager.dispatch(&msg).await;

        let result = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_multiple_subscriptions() {
        let manager = StreamManager::new();
        let mut rx1 = manager.add_subscription("topic1", rumqttc::QoS::AtMostOnce).await;
        let mut rx2 = manager.add_subscription("topic2", rumqttc::QoS::AtMostOnce).await;

        assert_eq!(manager.subscription_count().await, 2);

        let msg1 = MqttMessage {
            topic: "topic1".to_string(),
            payload: b"msg1".to_vec(),
            qos: rumqttc::QoS::AtMostOnce,
        };

        let msg2 = MqttMessage {
            topic: "topic2".to_string(),
            payload: b"msg2".to_vec(),
            qos: rumqttc::QoS::AtMostOnce,
        };

        manager.dispatch(&msg1).await;
        manager.dispatch(&msg2).await;

        let received1 = rx1.recv().await.unwrap();
        let received2 = rx2.recv().await.unwrap();

        assert_eq!(received1.topic, "topic1");
        assert_eq!(received2.topic, "topic2");
    }
}
