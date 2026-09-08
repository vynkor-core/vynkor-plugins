//! Error types for MQTT plugin

use std::fmt;

/// MQTT plugin error
#[derive(Debug)]
pub enum MqttError {
    /// Plugin not connected to MQTT broker
    NotConnected,
    /// MQTT client error
    Client(String),
    /// Connection failed
    ConnectionFailed(String),
    /// Subscribe failed
    SubscribeFailed(String),
    /// Publish failed
    PublishFailed(String),
    /// Invalid parameters
    InvalidParams(String),
    /// Action not found
    NotFound(String),
    /// Timeout
    Timeout,
    /// Serialization error
    Serialization(String),
    /// Deserialization error
    Deserialization(String),
    /// Device not found
    DeviceNotFound(String),
    /// OTA error
    OtaError(String),
    /// TLS error
    TlsError(String),
    /// Internal error
    Internal(String),
}

impl fmt::Display for MqttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MqttError::NotConnected => write!(f, "not connected to MQTT broker"),
            MqttError::Client(e) => write!(f, "MQTT client error: {e}"),
            MqttError::ConnectionFailed(e) => write!(f, "connection failed: {e}"),
            MqttError::SubscribeFailed(e) => write!(f, "subscribe failed: {e}"),
            MqttError::PublishFailed(e) => write!(f, "publish failed: {e}"),
            MqttError::InvalidParams(e) => write!(f, "invalid params: {e}"),
            MqttError::NotFound(e) => write!(f, "not found: {e}"),
            MqttError::Timeout => write!(f, "timeout"),
            MqttError::Serialization(e) => write!(f, "serialization error: {e}"),
            MqttError::Deserialization(e) => write!(f, "deserialization error: {e}"),
            MqttError::DeviceNotFound(e) => write!(f, "device not found: {e}"),
            MqttError::OtaError(e) => write!(f, "OTA error: {e}"),
            MqttError::TlsError(e) => write!(f, "TLS error: {e}"),
            MqttError::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl std::error::Error for MqttError {}

impl From<serde_json::Error> for MqttError {
    fn from(e: serde_json::Error) -> Self {
        MqttError::Serialization(e.to_string())
    }
}

impl From<rumqttc::ClientError> for MqttError {
    fn from(e: rumqttc::ClientError) -> Self {
        MqttError::Client(e.to_string())
    }
}
