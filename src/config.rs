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
    /// Static console IPs to probe via HTTP whenever discovery runs
    /// (`discover_gateways`) or any action needs to resolve a gateway
    /// IP without one explicitly given.
    ///
    /// Use this when consoles live on a VLAN the homeCore host can
    /// route to but UDP broadcast (port 45000) can't reach. Each
    /// listed host is queried via `/get_device_info?` — successful
    /// responses are merged into the discovery results alongside any
    /// UDP-discovered consoles.
    #[serde(default)]
    pub manual_hosts: Vec<String>,
    /// Polling interval in seconds (only used when gateway_ip is set).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Prefix for HomeCore device IDs (default: "ecowitt").
    #[serde(default = "default_device_prefix")]
    pub device_prefix: String,
    /// Username for the gateway's local web UI. Most Ecowitt firmware
    /// hard-codes this to "admin" and only checks the password — kept
    /// here for forward-compatibility and so the field is visible in
    /// config for installations that need to override it.
    #[allow(dead_code)]
    #[serde(default = "default_gateway_username")]
    pub gateway_username: String,
    /// Password for the gateway's local web UI. Leave blank if the
    /// gateway has no password set. Required for `set_*` cgi-bin
    /// endpoints on firmware revisions that gate writes behind the
    /// web-UI login (e.g., GW1100 with a password configured).
    #[serde(default)]
    pub gateway_password: String,
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
fn default_gateway_username() -> String {
    "admin".into()
}
