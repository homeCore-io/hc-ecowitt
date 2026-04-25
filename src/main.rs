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

    // Cached gateway IP for the management custom_handler. Seeded
    // from config; populated by `discover_gateways` so subsequent
    // actions don't have to re-broadcast. A single mutex is plenty —
    // contention is per-action, not per-frame.
    let gateway_ip: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(cfg.ecowitt.gateway_ip.clone()));
    let gateway_for_mgmt = std::sync::Arc::clone(&gateway_ip);

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
            let cmd_owned = cmd.clone();
            let gateway = std::sync::Arc::clone(&gateway_for_mgmt);
            let action_for_err = action.clone();
            // Bridge the SDK's sync custom_handler signature to async
            // work via a one-shot tokio runtime (same pattern as
            // hc-hue + hc-wled).
            let result = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(async move { run_action(&action, &cmd_owned, gateway).await })
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

/// Capability manifest. All actions are sync — UDP discovery, cgi-bin
/// probes, and config writes all return within a few seconds.
///
/// Every action that needs a gateway IP accepts an optional `host`
/// param to override the cached / configured one. Without `host`, the
/// dispatcher uses the cached gateway from a previous discovery, the
/// configured `gateway_ip`, or runs UDP discovery and picks the first
/// responder — in that order.
fn capabilities_manifest() -> hc_types::Capabilities {
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
                    "Point the gateway's data uploads at a custom \
                     server (typically homeCore's hc-ecowitt receiver). \
                     Different firmware revisions accept slightly \
                     different parameter names, so the request is sent \
                     to `/set_customserver` with both modern and \
                     legacy aliases populated. Verify by calling \
                     `get_custom_server` afterwards. Required: server, \
                     port. Optional: path (default `/data/report/`), \
                     interval_secs (default 60), protocol (default \
                     `ecowitt`), enable (default true)."
                        .into(),
                ),
                params: Some(json!({
                    "host": {
                        "type": "string",
                        "description": "Optional gateway IP; falls back to cached → configured → discovered",
                    },
                    "server": {
                        "type": "string",
                        "required": true,
                        "description": "Destination server IP/hostname (where homeCore's receiver listens)",
                    },
                    "port": {
                        "type": "integer",
                        "required": true,
                        "minimum": 1,
                        "maximum": 65535,
                        "description": "Destination port (matches `[ecowitt].listen_port` in homeCore plugin config)",
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
                })),
                stream: false,
                cancelable: false,
                concurrency: Concurrency::default(),
                item_key: None,
                item_operations: None,
                requires_role: RequiresRole::Admin,
                timeout_ms: None,
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
/// `cmd`, falls back to the cached gateway IP, then to UDP discovery.
/// Returns `None` for unknown actions so the SDK falls through.
async fn run_action(
    action: &str,
    cmd: &serde_json::Value,
    gateway_cache: std::sync::Arc<std::sync::Mutex<Option<String>>>,
) -> Option<serde_json::Value> {
    use serde_json::json;
    use std::time::Duration;

    if action == "discover_gateways" {
        return Some(match udp_discovery::discover_gateways(Duration::from_secs(3)).await {
            Ok(found) => {
                // Update the cache with the first responder so
                // subsequent actions can target it without specifying
                // a host.
                let selected = found
                    .first()
                    .and_then(|v| v.get("ip").and_then(|s| s.as_str()).map(str::to_string))
                    .or_else(|| {
                        found
                            .first()
                            .and_then(|v| v.get("host").and_then(|s| s.as_str()).map(str::to_string))
                    });
                if let Some(ref ip) = selected {
                    if let Ok(mut g) = gateway_cache.lock() {
                        *g = Some(ip.clone());
                    }
                }
                json!({
                    "status": "ok",
                    "discovered": found,
                    "count": found.len(),
                    "selected": selected,
                })
            }
            Err(e) => json!({
                "status": "error",
                "error": format!("UDP discovery failed: {e}"),
            }),
        });
    }

    // Every other action needs a gateway IP. Resolve in order:
    //   1. cmd["host"] override
    //   2. cached IP (from prior discovery or initial config)
    //   3. UDP discovery (cache the result)
    let ip = match resolve_gateway_ip(cmd, &gateway_cache).await {
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
        "set_custom_server" => set_custom_server(&ip, cmd).await,
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

/// Resolve the gateway IP for an action: param override → cache → UDP
/// discovery. On discovery success, the cache is updated.
async fn resolve_gateway_ip(
    cmd: &serde_json::Value,
    cache: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
) -> std::result::Result<String, String> {
    if let Some(host) = cmd.get("host").and_then(|v| v.as_str()) {
        let host = host.trim();
        if !host.is_empty() {
            return Ok(host.to_string());
        }
    }
    if let Ok(g) = cache.lock() {
        if let Some(ref ip) = *g {
            if !ip.is_empty() {
                return Ok(ip.clone());
            }
        }
    }
    // Last resort — broadcast and pick the first responder.
    let found =
        udp_discovery::discover_gateways(std::time::Duration::from_secs(3))
            .await
            .map_err(|e| format!("UDP discovery fallback failed: {e}"))?;
    let ip = found
        .first()
        .and_then(|v| v.get("ip").and_then(|s| s.as_str()).map(str::to_string))
        .or_else(|| {
            found
                .first()
                .and_then(|v| v.get("host").and_then(|s| s.as_str()).map(str::to_string))
        })
        .ok_or_else(|| {
            "no host param, no cached gateway, and UDP discovery returned no consoles. \
             Set [ecowitt].gateway_ip in config.toml or pass `host` as an action param."
                .to_string()
        })?;
    if let Ok(mut g) = cache.lock() {
        *g = Some(ip.clone());
    }
    Ok(ip)
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

/// `set_custom_server` posts the gateway's upload destination via
/// `/set_customserver`. Firmware revisions vary in the parameter
/// names they accept — the modern alias names (server / port / path)
/// were standardised around GW2000 firmware ~3.x. We send both the
/// modern names AND legacy aliases (ip / pport / cf_path / etc.) so
/// the same request works across revs without conditional logic.
///
/// Returns the raw response body so the caller can spot firmware
/// quirks; status code 200 isn't a guarantee the change took effect
/// — recommend pairing with `get_custom_server` afterwards.
async fn set_custom_server(
    ip: &str,
    cmd: &serde_json::Value,
) -> Option<serde_json::Value> {
    use serde_json::json;
    let server = cmd.get("server").and_then(|v| v.as_str()).unwrap_or("");
    let port = cmd.get("port").and_then(|v| v.as_u64()).unwrap_or(0);
    if server.is_empty() || port == 0 {
        return Some(json!({
            "status": "error",
            "error": "set_custom_server requires `server` (string) and `port` (integer) params",
        }));
    }
    let path = cmd
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/data/report/");
    let interval = cmd
        .get("interval_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let protocol = cmd
        .get("protocol")
        .and_then(|v| v.as_str())
        .unwrap_or("ecowitt");
    let enable = cmd
        .get("enable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let username = cmd.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let password = cmd.get("password").and_then(|v| v.as_str()).unwrap_or("");

    // Map protocol string to gateway's numeric type. Newer firmware
    // also accepts the string directly; sending both covers the gap.
    let proto_num = match protocol {
        "wunderground" | "wu" => "1",
        _ => "0", // ecowitt / customized
    };
    let enable_str = if enable { "1" } else { "0" };

    // Build the payload with every alias we've seen, so the firmware
    // can pick whichever names it expects. Extra fields are ignored
    // by every firmware we've checked.
    let mut form: Vec<(&str, String)> = vec![
        ("type", proto_num.to_string()),
        ("protocol", protocol.to_string()),
        ("server", server.to_string()),
        ("ip", server.to_string()),
        ("address", server.to_string()),
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

    let url = format!("http://{ip}/set_customserver?");
    match http_post_form(&url, &form).await {
        Ok(body) => Some(json!({
            "status": "ok",
            "host": ip,
            "endpoint": url,
            "raw_response": body,
        })),
        Err(e) => {
            // Some firmware accepts the same params via GET query
            // string instead of form-POST. Fall back once.
            let qs: String = form
                .iter()
                .map(|(k, v)| {
                    format!("{}={}", k, urlencoding(v))
                })
                .collect::<Vec<_>>()
                .join("&");
            let get_url = format!("{url}{qs}");
            match http_get_text(&get_url).await {
                Ok(body) => Some(json!({
                    "status": "ok",
                    "host": ip,
                    "endpoint": get_url,
                    "raw_response": body,
                    "note": "firmware required GET form fallback",
                })),
                Err(e2) => Some(json!({
                    "status": "error",
                    "host": ip,
                    "error": format!("set_customserver: POST failed ({e}); GET fallback also failed ({e2})"),
                })),
            }
        }
    }
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
