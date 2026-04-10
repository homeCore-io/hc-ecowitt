use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub homecore: HomecoreConfig,
    pub ecowitt: EcowittConfig,
    #[serde(default)]
    pub logging: crate::logging::LoggingConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read config {path}: {e}"))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("Config parse error in {path}: {e}"))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HomecoreConfig {
    #[serde(default = "default_broker_host")]
    pub broker_host: String,
    #[serde(default = "default_broker_port")]
    pub broker_port: u16,
    #[serde(default = "default_plugin_id")]
    pub plugin_id: String,
    #[serde(default)]
    pub password: String,
}

fn default_broker_host() -> String {
    "127.0.0.1".into()
}
fn default_broker_port() -> u16 {
    1883
}
fn default_plugin_id() -> String {
    "plugin.ecowitt".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct EcowittConfig {
    /// Port for the HTTP server that receives POSTs from the gateway.
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    /// Optional: gateway IP for polling mode.
    pub gateway_ip: Option<String>,
    /// Polling interval in seconds (only used when gateway_ip is set).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Prefix for HomeCore device IDs (default: "ecowitt").
    #[serde(default = "default_device_prefix")]
    pub device_prefix: String,
}

fn default_listen_port() -> u16 {
    8888
}
fn default_poll_interval() -> u64 {
    60
}
fn default_device_prefix() -> String {
    "ecowitt".into()
}
