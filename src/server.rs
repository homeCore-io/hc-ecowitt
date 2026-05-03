//! Axum HTTP server that receives POST data from the Ecowitt gateway.
//!
//! The Ecowitt "custom server" feature sends `application/x-www-form-urlencoded`
//! data.  We accept that format and convert to device updates.

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::StatusCode,
    routing::post,
    Form, Router,
};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::form_parser::parse_form_data;
use crate::registry::DeviceRegistry;

/// Max body size for `/data/report` POSTs. Real Ecowitt payloads are
/// ~500 bytes; 8 KiB leaves headroom for future fields without giving
/// a malformed POST a multi-megabyte memory vector. Axum's default is
/// 2 MB, which is several orders of magnitude too generous here.
const REPORT_BODY_LIMIT_BYTES: usize = 8 * 1024;

/// Shared state for the HTTP server and poller.
pub struct SharedState {
    pub registry: Mutex<DeviceRegistry>,
    pub device_prefix: String,
    /// Source-IP allowlist. Empty = accept any (today's behavior); when
    /// populated, only requests whose peer IP is in the set are
    /// processed. Construction uses HashSet so per-request lookup is
    /// O(1) regardless of list size.
    pub allowed_source_ips: HashSet<IpAddr>,
}

/// Returns `true` if the request's peer IP is permitted.
///
/// Pure helper so the rule is unit-testable without spinning up a
/// listener. Empty allowlist means "accept everything," matching the
/// pre-allowlist default behavior.
pub fn ip_allowed(allowed: &HashSet<IpAddr>, peer: IpAddr) -> bool {
    allowed.is_empty() || allowed.contains(&peer)
}

/// Build the axum router.
pub fn router(state: Arc<SharedState>) -> Router {
    Router::new()
        .route("/data/report/", post(handle_report))
        .route("/data/report", post(handle_report))
        .layer(DefaultBodyLimit::max(REPORT_BODY_LIMIT_BYTES))
        .with_state(state)
}

async fn handle_report(
    State(state): State<Arc<SharedState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(fields): Form<HashMap<String, String>>,
) -> StatusCode {
    if !ip_allowed(&state.allowed_source_ips, addr.ip()) {
        warn!(peer = %addr.ip(), "Rejecting POST from non-allowlisted source");
        return StatusCode::FORBIDDEN;
    }
    debug!(
        peer = %addr.ip(),
        fields = fields.len(),
        "Received POST from Ecowitt gateway"
    );

    let updates = parse_form_data(&fields, &state.device_prefix);
    if updates.is_empty() {
        warn!("POST contained no parseable sensor data");
        return StatusCode::OK;
    }

    let count = updates.len();
    let mut registry = state.registry.lock().await;
    registry.process_updates(updates).await;
    debug!(devices = count, "Processed Ecowitt data update");
    StatusCode::OK
}

/// Start the HTTP server on the configured bind address + port.
///
/// `bind_addr` is parsed; on failure we fall back to loopback rather
/// than crashing — the operator typo'd a config field, not a security
/// boundary.
pub async fn serve(bind_addr: &str, port: u16, state: Arc<SharedState>) {
    let ip: IpAddr = bind_addr.parse().unwrap_or_else(|e| {
        tracing::warn!(
            bind_addr,
            error = %e,
            "could not parse [ecowitt].bind_addr; falling back to 127.0.0.1"
        );
        IpAddr::from([127, 0, 0, 1])
    });
    let app = router(state);
    let addr = SocketAddr::new(ip, port);

    tracing::info!(bind = %addr, "Ecowitt HTTP receiver listening");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, %addr, "Failed to bind HTTP listener");
            return;
        }
    };

    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        tracing::error!(error = %e, "HTTP server error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn empty_allowlist_accepts_any() {
        let allow = HashSet::new();
        assert!(ip_allowed(&allow, ip("10.0.0.5")));
        assert!(ip_allowed(&allow, ip("192.168.1.1")));
        assert!(ip_allowed(&allow, ip("::1")));
    }

    #[test]
    fn populated_allowlist_only_admits_listed_peers() {
        let mut allow = HashSet::new();
        allow.insert(ip("10.0.10.50"));
        allow.insert(ip("10.0.10.51"));
        assert!(ip_allowed(&allow, ip("10.0.10.50")));
        assert!(ip_allowed(&allow, ip("10.0.10.51")));
        assert!(!ip_allowed(&allow, ip("10.0.10.99")));
        assert!(!ip_allowed(&allow, ip("127.0.0.1")));
    }

    #[test]
    fn allowlist_distinguishes_v4_from_v6_loopback() {
        // Operators sometimes assume "127.0.0.1" covers "::1" (or vice
        // versa). It doesn't — they're distinct addresses. This test
        // pins that behavior so a refactor doesn't accidentally relax
        // the check.
        let mut allow = HashSet::new();
        allow.insert(ip("127.0.0.1"));
        assert!(ip_allowed(&allow, ip("127.0.0.1")));
        assert!(!ip_allowed(&allow, ip("::1")));
    }
}
