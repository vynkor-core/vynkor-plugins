//! MQTT plugin for Vynkor
//!
//! Provides MQTT connectivity for ESP devices and other IoT hardware.
//! Supports the Vynkor Device Protocol (VDP) for device communication.

use mqtt_plugin::MqttPlugin;
use vynkor_sdk::Plugin;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    tracing::info!("MQTT plugin starting");

    let mut plugin = MqttPlugin::new();
    plugin.run().await?;

    Ok(())
}
