mod config;
mod form_parser;
mod id_map;
mod logging;
mod parser;
mod poller;
mod registry;
mod server;
mod udp_discovery;

use anyhow::Result;
use plugin_sdk_rs::{PluginClient, PluginConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info};

use config::Config;
use registry::DeviceRegistry;
use server::SharedState;

const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_SECS: u64 = 60;

#[tokio::main]
async fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/config.toml".to_string());

    let (_log_guard, log_level_handle, mqtt_log_handle) = init_logging(&config_path);

    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, path = %config_path, "Failed to load config");
            std::process::exit(1);
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        info!(attempt, max = MAX_ATTEMPTS, "Starting hc-ecowitt plugin");
        match try_start(
            &cfg,
            &config_path,
            log_level_handle.clone(),
            mqtt_log_handle.clone(),
        )
        .await
        {
            Ok(()) => return,
            Err(e) => {
                if attempt < MAX_ATTEMPTS {
                    error!(error = %e, attempt, "Startup failed; retrying in {RETRY_DELAY_SECS} s");
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                } else {
                    error!(error = %e, "Startup failed after {MAX_ATTEMPTS} attempts; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn init_logging(
    config_path: &str,
) -> (
    tracing_appender::non_blocking::WorkerGuard,
    hc_logging::LogLevelHandle,
    plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    logging::init_logging(
        config_path,
        "hc-ecowitt",
        "hc_ecowitt=info",
        &bootstrap.logging,
    )
}

async fn try_start(
    cfg: &Config,
    config_path: &str,
    log_level_handle: hc_logging::LogLevelHandle,
    mqtt_log_handle: plugin_sdk_rs::mqtt_log_layer::MqttLogHandle,
) -> Result<()> {
    // --- Plugin SDK connection ---
    let sdk_config = PluginConfig {
        broker_host: cfg.homecore.broker_host.clone(),
        broker_port: cfg.homecore.broker_port,
        plugin_id: cfg.homecore.plugin_id.clone(),
        password: cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config).await?;
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();

    // Stash the gateway IP for the management custom_handler.
    let gateway_for_mgmt = cfg.ecowitt.gateway_ip.clone();

    // Enable management protocol + capability manifest.
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest())
        .with_custom_handler(move |cmd| {
            let action = cmd["action"].as_str()?.to_string();
            let gateway_ip = gateway_for_mgmt.clone();
            // The cgi-bin actions are async (HTTP) and the UDP one is
            // async too. Bridge to async via a one-shot tokio runtime
            // on a fresh thread (same pattern as hc-hue + hc-wled).
            let action_for_err = action.clone();
            let result = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(async move { run_action(&action, gateway_ip.as_deref()).await })
            })
            .join()
            .ok()
            .flatten();
            result.or(Some(serde_json::json!({
                "status": "error",
                "error": format!("action '{action_for_err}' failed or is unknown"),
            })))
        });

    // Publish active status.
    if let Err(e) = client.publish_plugin_status("active").await {
        error!(error = %e, "Failed to publish plugin status");
    }

    // Start SDK event loop (weather sensors are read-only — no commands).
    tokio::spawn(async move {
        if let Err(e) = client.run_managed(|_device_id, _payload| {}, mgmt).await {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Create shared state with dynamic device registry ---
    let registry = DeviceRegistry::new(publisher, cfg.homecore.plugin_id.clone(), config_path);

    let shared = Arc::new(SharedState {
        registry: Mutex::new(registry),
        device_prefix: cfg.ecowitt.device_prefix.clone(),
    });

    info!(
        listen_port = cfg.ecowitt.listen_port,
        gateway_ip = ?cfg.ecowitt.gateway_ip,
        poll_interval = cfg.ecowitt.poll_interval_secs,
        "Ecowitt plugin started"
    );

    // --- Start optional poller ---
    if let Some(ref gateway_ip) = cfg.ecowitt.gateway_ip {
        let ip = gateway_ip.clone();
        let interval = cfg.ecowitt.poll_interval_secs;
        let state = Arc::clone(&shared);
        tokio::spawn(async move {
            poller::run_poller(ip, interval, state).await;
        });
        info!(gateway_ip = %gateway_ip, interval_secs = interval, "Gateway poller started");
    }

    // --- Run HTTP POST receiver (blocks forever) ---
    server::serve(cfg.ecowitt.listen_port, shared).await;

    Ok(())
}

/// Capability manifest. All actions are sync — UDP discovery and
/// cgi-bin probes return within a few seconds.
fn capabilities_manifest() -> hc_types::Capabilities {
    use hc_types::{Action, Capabilities, Concurrency, RequiresRole};
    use serde_json::json;
    Capabilities {
        spec: "1".into(),
        plugin_id: String::new(),
        actions: vec![
            Action {
                id: "discover_gateways".into(),
                label: "Discover gateways".into(),
                description: Some(
                    "Send a UDP CMD_BROADCAST query on port 45000 and \
                     listen ~3 seconds for replies. Returns the list \
                     of consoles on the LAN with MAC, IP, and any \
                     model / firmware strings extractable from the \
                     reply payload."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "discovered": { "type": "array" },
                    "count": { "type": "integer" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "refresh_sensors".into(),
                label: "Refresh sensor inventory".into(),
                description: Some(
                    "Query `/get_sensors_info?page=1` and `?page=2` on \
                     the configured gateway and return the live sensor \
                     list with signal levels and battery values. Useful \
                     after pairing a new external sensor."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "sensors": { "type": "array" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_gateway_info".into(),
                label: "Gateway info".into(),
                description: Some(
                    "Pull `/get_device_info` from the configured \
                     gateway — model, firmware, MAC, network. \
                     Diagnostic snapshot."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "info": { "type": "object" },
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "reboot_gateway".into(),
                label: "Reboot gateway".into(),
                description: Some(
                    "Reboot the configured Ecowitt console via its \
                     web UI's reboot endpoint. Use sparingly — \
                     interrupts uploads for ~30 seconds while the \
                     console restarts."
                        .into(),
                ),
                params: None,
                result: None,
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: None,
            },
        ],
    }
}

/// Manifest action dispatcher. Returns `None` for unknown actions so
/// the SDK's `with_custom_handler` machinery falls through; otherwise
/// returns a JSON `Value` with `status: "ok"` / `"error"` plus the
/// action's data.
async fn run_action(
    action: &str,
    gateway_ip: Option<&str>,
) -> Option<serde_json::Value> {
    use serde_json::json;
    use std::time::Duration;
    match action {
        "discover_gateways" => {
            match udp_discovery::discover_gateways(Duration::from_secs(3)).await {
                Ok(found) => Some(json!({
                    "status": "ok",
                    "discovered": found,
                    "count": found.len(),
                })),
                Err(e) => Some(json!({
                    "status": "error",
                    "error": format!("UDP discovery failed: {e}"),
                })),
            }
        }
        "refresh_sensors" => {
            let Some(ip) = gateway_ip else {
                return Some(json!({
                    "status": "error",
                    "error": "gateway_ip not configured; nothing to query",
                }));
            };
            let mut combined = serde_json::Map::new();
            for page in 1..=2 {
                let url = format!("http://{ip}/get_sensors_info?page={page}");
                match http_get_json(&url).await {
                    Ok(v) => {
                        combined.insert(format!("page_{page}"), v);
                    }
                    Err(e) => {
                        combined.insert(
                            format!("page_{page}"),
                            json!({ "error": e.to_string() }),
                        );
                    }
                }
            }
            Some(json!({
                "status": "ok",
                "sensors": combined,
            }))
        }
        "get_gateway_info" => {
            let Some(ip) = gateway_ip else {
                return Some(json!({
                    "status": "error",
                    "error": "gateway_ip not configured; nothing to query",
                }));
            };
            let url = format!("http://{ip}/get_device_info?");
            match http_get_json(&url).await {
                Ok(v) => Some(json!({
                    "status": "ok",
                    "info": v,
                })),
                Err(e) => Some(json!({
                    "status": "error",
                    "error": format!("get_device_info: {e}"),
                })),
            }
        }
        "reboot_gateway" => {
            let Some(ip) = gateway_ip else {
                return Some(json!({
                    "status": "error",
                    "error": "gateway_ip not configured; nothing to reboot",
                }));
            };
            // Different firmware revs expose reboot at different paths.
            // Try the most common ones in order; first success wins.
            // (`reboot.html` and `reboot.cgi` are the two we've seen.)
            let candidates = [
                format!("http://{ip}/reboot.html"),
                format!("http://{ip}/reboot.cgi"),
                format!("http://{ip}/cgi-bin/reboot"),
            ];
            for url in &candidates {
                match http_get_text(url).await {
                    Ok(_) => {
                        return Some(json!({
                            "status": "ok",
                            "endpoint": url,
                        }));
                    }
                    Err(e) => {
                        tracing::debug!(url = %url, error = %e, "reboot endpoint probe");
                    }
                }
            }
            Some(json!({
                "status": "error",
                "error": "no known reboot endpoint accepted; check gateway firmware",
            }))
        }
        _ => None,
    }
}

async fn http_get_json(url: &str) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("status {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v)
}

async fn http_get_text(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("status {}", resp.status());
    }
    Ok(resp.text().await.unwrap_or_default())
}
