//! Dynamic device registry — auto-registers devices with HomeCore on first sight.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use anyhow::Result;
use plugin_sdk_rs::DevicePublisher;
use tracing::{info, warn};

use crate::parser::DeviceUpdate;

/// Tracks which devices have been registered with HomeCore.
/// On first sight of a device_id, registers it automatically.
pub struct DeviceRegistry {
    registered: HashSet<String>,
    publisher: DevicePublisher,
    #[allow(dead_code)]
    plugin_id: String,
    cache_path: PathBuf,
}

impl DeviceRegistry {
    pub fn new(publisher: DevicePublisher, plugin_id: String, config_path: &str) -> Self {
        let cache_path = Path::new(config_path)
            .parent()
            .unwrap_or(Path::new("."))
            .join(".published-device-ids.json");

        Self {
            registered: HashSet::new(),
            publisher,
            plugin_id,
            cache_path,
        }
    }

    /// Process a batch of device updates: register new devices, publish state for all.
    pub async fn process_updates(&mut self, updates: Vec<DeviceUpdate>) {
        for update in &updates {
            if !self.registered.contains(&update.device_id) {
                self.register_device(update).await;
            }
        }

        for update in updates {
            let _ = self.publisher.publish_state(&update.device_id, &update.state).await;
        }
    }

    async fn register_device(&mut self, update: &DeviceUpdate) {
        if let Err(e) = self.publisher
            .register_device_full(
                &update.device_id,
                &update.name,
                Some(update.device_type),
                None, // area — not known from sensor data
                None,
            )
            .await
        {
            warn!(device_id = %update.device_id, error = %e, "Failed to register device");
            return;
        }

        if let Err(e) = self.publisher.subscribe_commands(&update.device_id).await {
            warn!(device_id = %update.device_id, error = %e, "Failed to subscribe commands");
        }

        let _ = self.publisher.publish_availability(&update.device_id, true).await;

        info!(device_id = %update.device_id, device_type = update.device_type, name = %update.name, "Auto-registered new device");
        self.registered.insert(update.device_id.clone());
        let _ = self.save_cache();
    }

    /// Clean up devices that were registered previously but are no longer seen.
    #[allow(dead_code)]
    pub async fn cleanup_stale(&mut self) {
        let previous = self.load_cache();
        for stale_id in previous.iter().filter(|id| !self.registered.contains(*id)) {
            if let Err(e) = self.publisher.unregister_device(&self.plugin_id, stale_id).await {
                warn!(device_id = %stale_id, error = %e, "Failed to unregister stale device");
            } else {
                info!(device_id = %stale_id, "Unregistered stale device");
            }
        }
    }

    #[allow(dead_code)]
    fn load_cache(&self) -> Vec<String> {
        std::fs::read_to_string(&self.cache_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save_cache(&self) -> Result<()> {
        let ids: Vec<&String> = self.registered.iter().collect();
        let payload = serde_json::to_vec_pretty(&ids)?;
        std::fs::write(&self.cache_path, payload)?;
        Ok(())
    }
}
