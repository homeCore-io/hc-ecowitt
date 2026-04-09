//! Axum HTTP server that receives POST data from the Ecowitt gateway.
//!
//! The Ecowitt "custom server" feature sends `application/x-www-form-urlencoded`
//! data.  We accept that format and convert to device updates.

use axum::{extract::State, http::StatusCode, routing::post, Form, Router};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::form_parser::parse_form_data;
use crate::registry::DeviceRegistry;

/// Shared state for the HTTP server and poller.
pub struct SharedState {
    pub registry: Mutex<DeviceRegistry>,
    pub device_prefix: String,
}

/// Build the axum router.
pub fn router(state: Arc<SharedState>) -> Router {
    Router::new()
        .route("/data/report/", post(handle_report))
        .route("/data/report", post(handle_report))
        .with_state(state)
}

async fn handle_report(
    State(state): State<Arc<SharedState>>,
    Form(fields): Form<HashMap<String, String>>,
) -> StatusCode {
    debug!(fields = fields.len(), "Received POST from Ecowitt gateway");

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

/// Start the HTTP server on the given port.
pub async fn serve(port: u16, state: Arc<SharedState>) {
    let app = router(state);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(port, "Ecowitt HTTP receiver listening");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, port, "Failed to bind HTTP listener");
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "HTTP server error");
    }
}
