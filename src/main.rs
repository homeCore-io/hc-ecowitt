mod config;
mod form_parser;
mod id_map;
mod logging;
mod parser;
mod poller;
mod registry;
mod server;

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
        match try_start(&cfg, &config_path, log_level_handle.clone(), mqtt_log_handle.clone()).await {
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

fn init_logging(config_path: &str) -> (tracing_appender::non_blocking::WorkerGuard, hc_logging::LogLevelHandle, plugin_sdk_rs::mqtt_log_layer::MqttLogHandle) {
    #[derive(serde::Deserialize, Default)]
    struct Bootstrap {
        #[serde(default)]
        logging: logging::LoggingConfig,
    }
    let bootstrap: Bootstrap = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    logging::init_logging(config_path, "hc-ecowitt", "hc_ecowitt=info", &bootstrap.logging)
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
        plugin_id:   cfg.homecore.plugin_id.clone(),
        password:    cfg.homecore.password.clone(),
    };

    let client = PluginClient::connect(sdk_config).await?;
    mqtt_log_handle.connect(
        client.mqtt_client(),
        &cfg.homecore.plugin_id,
        &cfg.logging.log_forward_level,
    );
    let publisher = client.device_publisher();

    // Enable management protocol.
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?;

    // Publish active status.
    if let Err(e) = client.publish_plugin_status("active").await {
        error!(error = %e, "Failed to publish plugin status");
    }

    // Start SDK event loop (weather sensors are read-only — no commands).
    tokio::spawn(async move {
        if let Err(e) = client
            .run_managed(|_device_id, _payload| {}, mgmt)
            .await
        {
            error!(error = %e, "SDK event loop exited with error");
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Create shared state with dynamic device registry ---
    let registry = DeviceRegistry::new(
        publisher,
        cfg.homecore.plugin_id.clone(),
        config_path,
    );

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
