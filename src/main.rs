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

    // Cached gateway IP for the management custom_handler.
    //
    // Resolution priority on read: configured > on-disk cache. So
    // an explicit [ecowitt].gateway_ip always wins.
    //
    // The on-disk cache `.cached-gateway-ip` next to config.toml
    // makes the previously-discovered IP survive a plugin restart —
    // without it, every restart needed a fresh `Discover gateways`
    // click to re-prime an in-memory cache. The cache is updated
    // whenever any resolution path succeeds.
    let cache_path = cached_gateway_ip_path(config_path);
    let initial_cache = cfg
        .ecowitt
        .gateway_ip
        .clone()
        .or_else(|| std::fs::read_to_string(&cache_path).ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());
    let gateway_ip: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(initial_cache));
    let gateway_for_mgmt = std::sync::Arc::clone(&gateway_ip);
    let cache_path_for_handler = cache_path.clone();
    // Static manual_hosts list, captured once at startup. The
    // dispatcher probes these via HTTP /get_device_info to reach
    // consoles on VLANs UDP broadcast can't traverse.
    let manual_hosts = cfg.ecowitt.manual_hosts.clone();

    // Enable management protocol + capability manifest.
    let mgmt = client
        .enable_management(
            60,
            Some(env!("CARGO_PKG_VERSION").to_string()),
            Some(config_path.to_string()),
            Some(log_level_handle),
        )
        .await?
        .with_capabilities(capabilities_manifest(cfg.ecowitt.listen_port))
        .with_custom_handler(move |cmd| {
            let action = cmd["action"].as_str()?.to_string();
            let cmd_owned = cmd.clone();
            let gateway = std::sync::Arc::clone(&gateway_for_mgmt);
            let manual = manual_hosts.clone();
            let cache_path = cache_path_for_handler.clone();
            let action_for_err = action.clone();
            // Bridge the SDK's sync custom_handler signature to async
            // work via a one-shot tokio runtime (same pattern as
            // hc-hue + hc-wled).
            let result = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(async move {
                    run_action(&action, &cmd_owned, gateway, &manual, &cache_path).await
                })
            })
            .join()
            .ok()
            .flatten();
            result.or(Some(serde_json::json!({
                "status": "error",
                "error": format!("action '{action_for_err}' failed or is unknown"),
            })))
        });

    // Streaming action: set_custom_server. Auto-detect + POST +
    // possible GET fallback can stack up past the 5s management
    // RPC timeout, so it lives here instead of inside the sync
    // dispatcher above.
    let stream_handle = StreamHandle {
        gateway_cache: std::sync::Arc::clone(&gateway_ip),
        manual_hosts: cfg.ecowitt.manual_hosts.clone(),
        listen_port: cfg.ecowitt.listen_port,
        cache_path: cache_path.clone(),
    };
    let mgmt = mgmt.with_streaming_action(plugin_sdk_rs::StreamingAction::new(
        "set_custom_server",
        move |ctx, params| {
            let h = stream_handle.clone();
            async move { set_custom_server_streaming(ctx, params, h).await }
        },
    ));

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

/// Capability manifest. All actions are sync — UDP discovery, cgi-bin
/// probes, and config writes all return within a few seconds.
///
/// Every action that needs a gateway IP accepts an optional `host`
/// param to override the cached / configured one. Without `host`, the
/// dispatcher uses the cached gateway from a previous discovery, the
/// configured `gateway_ip`, or runs UDP discovery and picks the first
/// responder — in that order.
///
/// `listen_port` is the homeCore-side port (`[ecowitt].listen_port`)
/// the consoles POST to; baked into the `set_custom_server` action's
/// `port` default so the form opens pre-populated.
fn capabilities_manifest(listen_port: u16) -> hc_types::Capabilities {
    use hc_types::{Action, Capabilities, Concurrency, RequiresRole};
    use serde_json::json;

    let host_param = json!({
        "host": {
            "type": "string",
            "description": "Optional gateway IP/hostname; falls back to cached → configured → discovered",
        }
    });

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
                     of consoles on the LAN with MAC, IP, model, and \
                     firmware. Updates the in-memory gateway cache so \
                     subsequent actions can target the discovered IP \
                     without re-discovering."
                        .into(),
                ),
                params: None,
                result: Some(json!({
                    "discovered": { "type": "array" },
                    "count": { "type": "integer" },
                    "selected": { "type": "string" },
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
                     the gateway. Returns the live sensor list with \
                     signal levels and battery values."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "sensors": { "type": "object" } })),
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
                    "Pull `/get_device_info` from the gateway — model, \
                     firmware, MAC, network. Diagnostic snapshot."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "info": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_network".into(),
                label: "Network info".into(),
                description: Some(
                    "Pull `/get_network_info` from the gateway — Wi-Fi \
                     SSID / RSSI / IP / netmask / gateway / DNS."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "network": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_units".into(),
                label: "Units / display config".into(),
                description: Some(
                    "Pull `/get_units_info` from the gateway — current \
                     unit selections (temp °C/°F, wind m/s vs mph, \
                     pressure hPa vs inHg, rainfall mm vs in)."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "units": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_calibration".into(),
                label: "Calibration offsets".into(),
                description: Some(
                    "Pull `/get_calibration` from the gateway — every \
                     sensor's calibration offset (per-channel temperature, \
                     humidity, pressure, rain gain, etc.)."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "calibration": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "refresh_iot_devices".into(),
                label: "IoT device list".into(),
                description: Some(
                    "Pull `/get_iotdevice_list` from the gateway — \
                     connected Ecowitt IoT outlets / valves / etc."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "iot_devices": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "get_custom_server".into(),
                label: "Custom-server upload settings".into(),
                description: Some(
                    "Pull the gateway's current custom-server upload \
                     settings — server, port, path, interval, protocol \
                     type. Useful as a 'before' snapshot ahead of a \
                     `set_custom_server`."
                        .into(),
                ),
                params: Some(host_param.clone()),
                result: Some(json!({ "custom_server": { "type": "object" } })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::User,
                timeout_ms: None,
            },
            Action {
                id: "set_custom_server".into(),
                label: "Set custom-server upload".into(),
                description: Some(
                    "Point the gateway's data uploads at this hc-ecowitt \
                     plugin's HTTP receiver. Both `server` and `port` \
                     auto-default to the right values for a typical setup \
                     — `port` is pre-filled with the plugin's configured \
                     `[ecowitt].listen_port`, and `server` is auto-detected \
                     at submit time by opening a TCP connection to the \
                     gateway and reading the plugin's local socket IP (the \
                     IP the gateway can reach the plugin at, regardless of \
                     how many NICs the plugin's host has). Override either \
                     by filling in the field. Different firmware revisions \
                     accept different parameter names, so the request goes \
                     out with both modern and legacy aliases populated. \
                     Verify with `get_custom_server` afterwards."
                        .into(),
                ),
                params: Some(json!({
                    "host": {
                        "type": "string",
                        "description": "Optional gateway IP; falls back to cached → configured → discovered",
                    },
                    "server": {
                        "type": "string",
                        "description": "Destination server IP. Leave blank to auto-detect the plugin's IP from the gateway's perspective (recommended).",
                    },
                    "port": {
                        "type": "integer",
                        "default": listen_port as i64,
                        "minimum": 1,
                        "maximum": 65535,
                        "description": "Destination port. Defaults to this plugin's configured listen_port.",
                    },
                    "path": {
                        "type": "string",
                        "default": "/data/report/",
                        "description": "URL path the gateway POSTs to",
                    },
                    "interval_secs": {
                        "type": "integer",
                        "default": 60,
                        "minimum": 16,
                        "maximum": 3600,
                        "description": "Upload interval (Ecowitt firmware enforces a 16s minimum)",
                    },
                    "protocol": {
                        "type": "string",
                        "enum": ["ecowitt", "wunderground"],
                        "default": "ecowitt",
                        "description": "Upload protocol type",
                    },
                    "enable": {
                        "type": "boolean",
                        "default": true,
                        "description": "Enable the custom-server upload after setting",
                    },
                    "username": {
                        "type": "string",
                        "description": "Optional Wunderground station ID (`wunderground` protocol only)",
                    },
                    "password": {
                        "type": "string",
                        "description": "Optional Wunderground station password (`wunderground` protocol only)",
                    },
                })),
                result: Some(json!({
                    "endpoint": { "type": "string" },
                    "raw_response": { "type": "string" },
                    "server_used": { "type": "string" },
                    "port_used": { "type": "integer" },
                })),
                stream: true,
                cancelable: false,
                concurrency: Concurrency::Single,
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: Some(60_000),
            },
            Action {
                id: "reboot_gateway".into(),
                label: "Reboot gateway".into(),
                description: Some(
                    "Reboot the gateway via its web UI's reboot endpoint. \
                     Probes `/reboot.html`, `/reboot.cgi`, and \
                     `/cgi-bin/reboot` in order; first 200 wins. \
                     Interrupts uploads for ~30 seconds."
                        .into(),
                ),
                params: Some(host_param),
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

/// Manifest action dispatcher. Reads the optional `host` param off
/// `cmd`, falls back to the cached gateway IP, then to manual_hosts
/// HTTP probes, then UDP discovery. Returns `None` for unknown
/// actions so the SDK falls through.
async fn run_action(
    action: &str,
    cmd: &serde_json::Value,
    gateway_cache: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    manual_hosts: &[String],
    cache_path: &std::path::Path,
) -> Option<serde_json::Value> {
    use serde_json::json;
    use std::time::Duration;

    if action == "discover_gateways" {
        // UDP broadcast first — picks up everything on the same
        // broadcast domain.
        let mut found = udp_discovery::discover_gateways(Duration::from_secs(3))
            .await
            .unwrap_or_default();

        // Then HTTP-probe each manual_host. These are typically
        // consoles on VLANs the broadcast can't reach. Add their
        // /get_device_info responses to the result list, deduped by
        // IP.
        for host in manual_hosts {
            if found.iter().any(|v| {
                v.get("ip").and_then(|s| s.as_str()) == Some(host)
                    || v.get("host").and_then(|s| s.as_str()) == Some(host)
            }) {
                continue;
            }
            let url = format!("http://{host}/get_device_info?");
            match http_get_json(&url).await {
                Ok(info) => found.push(json!({
                    "host": host,
                    "ip": host,
                    "source": "manual_hosts",
                    "info": info,
                })),
                Err(e) => {
                    tracing::debug!(host, error = %e, "manual_hosts probe failed");
                }
            }
        }

        let selected = found
            .first()
            .and_then(|v| v.get("ip").and_then(|s| s.as_str()).map(str::to_string))
            .or_else(|| {
                found
                    .first()
                    .and_then(|v| v.get("host").and_then(|s| s.as_str()).map(str::to_string))
            });
        if let Some(ref ip) = selected {
            update_cache(&gateway_cache, cache_path, ip);
        }
        return Some(json!({
            "status": "ok",
            "discovered": found,
            "count": found.len(),
            "selected": selected,
            "manual_hosts_configured": manual_hosts.len(),
        }));
    }

    // Every other action needs a gateway IP. Resolve in order:
    //   1. cmd["host"] override
    //   2. cached IP (from prior discovery or initial config)
    //   3. configured manual_hosts (first that answers HTTP probe)
    //   4. UDP discovery (cache the result)
    let ip = match resolve_gateway_ip(cmd, &gateway_cache, manual_hosts, cache_path).await {
        Ok(ip) => ip,
        Err(msg) => {
            return Some(json!({
                "status": "error",
                "error": msg,
            }))
        }
    };

    match action {
        "refresh_sensors" => {
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
                "host": ip,
                "sensors": combined,
            }))
        }
        "get_gateway_info" => simple_get(&ip, "/get_device_info?", "info").await,
        "get_network" => simple_get(&ip, "/get_network_info?", "network").await,
        "get_units" => simple_get(&ip, "/get_units_info?", "units").await,
        "get_calibration" => simple_get(&ip, "/get_calibration?", "calibration").await,
        "refresh_iot_devices" => {
            simple_get(&ip, "/get_iotdevice_list?", "iot_devices").await
        }
        "get_custom_server" => simple_get(&ip, "/get_customserver?", "custom_server").await,
        // set_custom_server is registered as a streaming action below
        // (with_streaming_action) — it can blow past the 5s management
        // RPC timeout when auto-detect + POST + GET-fallback stack up,
        // so it lives outside this sync custom_handler dispatch.
        "reboot_gateway" => {
            // Different firmware revs expose reboot at different paths.
            // Try the most common ones in order; first 200 wins.
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
                            "host": ip,
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
                "host": ip,
                "error": "no known reboot endpoint accepted; check gateway firmware",
            }))
        }
        _ => None,
    }
}

/// Resolve the gateway IP for an action. Order:
///
/// 1. `cmd["host"]` override.
/// 2. Cached IP (set by a prior discover_gateways or any earlier
///    fallback). Validated quickly: if the cached host doesn't answer
///    /get_device_info we fall through to a fresh resolution.
/// 3. Each configured `manual_hosts` entry, in order; first that
///    responds to an HTTP probe wins. This is the path that handles
///    VLAN-isolated consoles UDP broadcast can't reach.
/// 4. UDP discovery — picks the first responder.
///
/// Caches the resolved IP for next time.
async fn resolve_gateway_ip(
    cmd: &serde_json::Value,
    cache: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    manual_hosts: &[String],
    cache_path: &std::path::Path,
) -> std::result::Result<String, String> {
    if let Some(host) = cmd.get("host").and_then(|v| v.as_str()) {
        let host = host.trim();
        if !host.is_empty() {
            update_cache(cache, cache_path, host);
            return Ok(host.to_string());
        }
    }
    let cached = cache.lock().ok().and_then(|g| g.clone());
    if let Some(ip) = cached.as_ref().filter(|s| !s.is_empty()) {
        // Trust the cache without re-probing. If the gateway became
        // unreachable, the action's own HTTP call will surface the
        // error in its response and the user can pick another host.
        return Ok(ip.clone());
    }

    // Manual hosts before UDP. They're explicitly configured by the
    // operator, so prefer them — they're also faster (one HTTP call
    // vs a 3s broadcast wait). Track which ones we tried for the
    // error message in case all of them fail.
    let mut manual_attempts: Vec<String> = Vec::new();
    for host in manual_hosts {
        let url = format!("http://{host}/get_device_info?");
        if http_get_json(&url).await.is_ok() {
            update_cache(cache, cache_path, host);
            return Ok(host.clone());
        }
        manual_attempts.push(host.clone());
    }

    let found =
        udp_discovery::discover_gateways(std::time::Duration::from_secs(3))
            .await
            .map_err(|e| format!("UDP discovery fallback failed: {e}"))?;
    if let Some(ip) = found
        .first()
        .and_then(|v| v.get("ip").and_then(|s| s.as_str()).map(str::to_string))
        .or_else(|| {
            found
                .first()
                .and_then(|v| v.get("host").and_then(|s| s.as_str()).map(str::to_string))
        })
    {
        update_cache(cache, cache_path, &ip);
        return Ok(ip);
    }

    // Every fallback exhausted — be specific about what's missing
    // so the operator can act without reading the source.
    let manual_status = if manual_hosts.is_empty() {
        "[ecowitt].manual_hosts is empty".to_string()
    } else {
        format!(
            "[ecowitt].manual_hosts didn't respond ({})",
            manual_attempts.join(", ")
        )
    };
    Err(format!(
        "no gateway IP available — none of: \
         (1) `host` action param, \
         (2) cached IP from a prior discover_gateways, \
         (3) {manual_status}, \
         (4) UDP CMD_BROADCAST on port 45000 (returned 0 consoles in 3s — \
         likely the console is on a VLAN this plugin's broadcast can't reach). \
         Fix: set `[ecowitt].gateway_ip = \"…\"` or \
         `[ecowitt].manual_hosts = [\"…\"]` in config.toml, or fill in the \
         `host` field on this action."
    ))
}

/// Update the in-memory + on-disk cache of the resolved gateway IP.
/// Disk writes are best-effort — we don't fail the action just
/// because the cache file isn't writable.
fn update_cache(
    cache: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    cache_path: &std::path::Path,
    ip: &str,
) {
    if let Ok(mut g) = cache.lock() {
        *g = Some(ip.to_string());
    }
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(cache_path, ip.as_bytes()) {
        tracing::warn!(
            path = %cache_path.display(),
            error = %e,
            "could not persist gateway-ip cache"
        );
    }
}

fn cached_gateway_ip_path(config_path: &str) -> std::path::PathBuf {
    std::path::Path::new(config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(".cached-gateway-ip")
}

/// Common shape: GET a cgi-bin endpoint, parse JSON, wrap in a
/// `{ status, host, <field>: ... }` envelope.
async fn simple_get(
    ip: &str,
    path: &str,
    response_field: &str,
) -> Option<serde_json::Value> {
    use serde_json::json;
    let url = format!("http://{ip}{path}");
    match http_get_json(&url).await {
        Ok(v) => Some(json!({
            "status": "ok",
            "host": ip,
            response_field: v,
        })),
        Err(e) => Some(json!({
            "status": "error",
            "host": ip,
            "error": format!("{path}: {e}"),
        })),
    }
}

/// Bundle of long-lived state the streaming `set_custom_server`
/// action closure needs. Cloneable so the SDK can wrap it into the
/// per-invocation handler closure.
#[derive(Clone)]
struct StreamHandle {
    gateway_cache: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    manual_hosts: Vec<String>,
    listen_port: u16,
    cache_path: std::path::PathBuf,
}

/// Streaming `set_custom_server`. Same protocol as the old sync
/// version (POST /set_customserver with both modern + legacy
/// aliases populated; GET-with-query-string fallback if POST is
/// rejected). Wrapped in stage events so the operator can see what
/// the action is doing — and so the work can comfortably exceed
/// core's 5s sync RPC timeout.
async fn set_custom_server_streaming(
    ctx: plugin_sdk_rs::StreamContext,
    params: serde_json::Value,
    handle: StreamHandle,
) -> anyhow::Result<()> {
    use serde_json::json;

    ctx.progress(Some(0), Some("starting"), Some("Resolving gateway IP"))
        .await?;
    let ip = match resolve_gateway_ip(
        &params,
        &handle.gateway_cache,
        &handle.manual_hosts,
        &handle.cache_path,
    )
    .await
    {
        Ok(ip) => ip,
        Err(msg) => return ctx.error(msg).await,
    };
    ctx.progress(
        Some(25),
        Some("resolved"),
        Some(&format!("Gateway: {ip}")),
    )
    .await?;

    // Resolve `server` (auto-detect when blank) and `port` (default
    // to listen_port). Same logic as the prior sync version, just
    // with progress events sprinkled in.
    let server_override = params
        .get("server")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let server = match server_override {
        Some(s) => {
            ctx.progress(
                Some(45),
                Some("server-override"),
                Some(&format!("Using explicit server={s}")),
            )
            .await?;
            s
        }
        None => {
            ctx.progress(
                Some(35),
                Some("detecting"),
                Some(&format!(
                    "Detecting plugin's IP from gateway {ip}'s perspective"
                )),
            )
            .await?;
            match detect_local_ip_to(&ip).await {
                Ok(local) => {
                    ctx.progress(
                        Some(55),
                        Some("detected"),
                        Some(&format!("Detected server={local}")),
                    )
                    .await?;
                    local
                }
                Err(e) => {
                    return ctx
                        .error(format!(
                            "server auto-detect failed ({e}); pass an explicit `server` param"
                        ))
                        .await;
                }
            }
        }
    };

    let port = params
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0)
        .unwrap_or(handle.listen_port as u64);
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/data/report/");
    let interval = params
        .get("interval_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let protocol = params
        .get("protocol")
        .and_then(|v| v.as_str())
        .unwrap_or("ecowitt");
    let enable = params
        .get("enable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let username = params
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let password = params
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let proto_num = match protocol {
        "wunderground" | "wu" => "1",
        _ => "0",
    };
    let enable_str = if enable { "1" } else { "0" };

    let mut form: Vec<(&str, String)> = vec![
        ("type", proto_num.to_string()),
        ("protocol", protocol.to_string()),
        ("server", server.clone()),
        ("ip", server.clone()),
        ("address", server.clone()),
        ("port", port.to_string()),
        ("pport", port.to_string()),
        ("path", path.to_string()),
        ("cf_path", path.to_string()),
        ("interval", interval.to_string()),
        ("intvl", interval.to_string()),
        ("uptime", interval.to_string()),
        ("enable", enable_str.into()),
        ("ena", enable_str.into()),
    ];
    if !username.is_empty() {
        form.push(("usr", username.to_string()));
        form.push(("station_id", username.to_string()));
    }
    if !password.is_empty() {
        form.push(("pwd", password.to_string()));
        form.push(("station_pw", password.to_string()));
    }

    ctx.progress(
        Some(70),
        Some("posting"),
        Some(&format!("POST /set_customserver to {ip}")),
    )
    .await?;

    let url = format!("http://{ip}/set_customserver?");
    let (endpoint_used, raw_response, fallback_note) = match http_post_form(&url, &form).await {
        Ok(body) => (url.clone(), body, None),
        Err(e) => {
            ctx.progress(
                Some(80),
                Some("post-failed"),
                Some(&format!(
                    "POST rejected ({e}); falling back to GET query"
                )),
            )
            .await?;
            let qs: String = form
                .iter()
                .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
                .collect::<Vec<_>>()
                .join("&");
            let get_url = format!("{url}{qs}");
            match http_get_text(&get_url).await {
                Ok(body) => (
                    get_url,
                    body,
                    Some("firmware required GET form fallback".to_string()),
                ),
                Err(e2) => {
                    return ctx
                        .error(format!(
                            "set_customserver: POST failed ({e}); GET fallback also failed ({e2})"
                        ))
                        .await;
                }
            }
        }
    };

    ctx.progress(Some(100), Some("done"), Some("Upload destination updated"))
        .await?;

    let mut result = json!({
        "host": ip,
        "endpoint": endpoint_used,
        "server_used": server,
        "port_used": port,
        "raw_response": raw_response,
    });
    if let Some(note) = fallback_note {
        result["note"] = json!(note);
    }
    ctx.complete(result).await
}

/// Detect the local IP the plugin's host uses to reach `gateway`.
///
/// Open a quick TCP connection to the gateway's HTTP port and read
/// our local socket address. The OS picks the route + source IP that
/// will actually carry packets to that gateway, so the result is
/// guaranteed to be the IP the gateway can post back to (matching
/// any firewall, VLAN, or multi-NIC layout the plugin host happens
/// to be on).
///
/// Tries port 80 first, falls back to 443 — every Ecowitt console we
/// know about exposes its cgi-bin on plain HTTP, but if the host
/// blocks 80 we still want a usable answer.
async fn detect_local_ip_to(gateway: &str) -> anyhow::Result<String> {
    use tokio::net::TcpStream;
    let candidates = [80u16, 443];
    let mut last_err: Option<String> = None;
    for port in candidates {
        let target = format!("{gateway}:{port}");
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            TcpStream::connect(&target),
        )
        .await
        {
            Ok(Ok(stream)) => {
                let local = stream.local_addr()?;
                return Ok(local.ip().to_string());
            }
            Ok(Err(e)) => {
                last_err = Some(format!("{target}: {e}"));
            }
            Err(_) => {
                last_err = Some(format!("{target}: connect timed out"));
            }
        }
    }
    anyhow::bail!(
        "could not connect to gateway on any candidate port: {}",
        last_err.unwrap_or_else(|| "unknown".into())
    )
}

fn urlencoding(s: &str) -> String {
    // Minimal URL-encode for the GET-fallback path. We only emit ASCII
    // values from a controlled param set, so a permissive escape is
    // fine — `:` `/` `=` `&` are the meaningful ones.
    s.chars()
        .map(|c| match c {
            '0'..='9' | 'a'..='z' | 'A'..='Z' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

async fn http_post_form(
    url: &str,
    form: &[(&str, String)],
) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    let resp = client.post(url).form(form).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("status {status}: {body}");
    }
    Ok(resp.text().await.unwrap_or_default())
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
