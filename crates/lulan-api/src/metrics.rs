//! Prometheus metrics (§11): request counters and latency histograms per
//! (method, route, status), served at `GET /metrics`.
//!
//! The recorder is process-global; `handle()` installs it once so tests
//! that build many routers don't fight over installation.

use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

fn handle() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("install prometheus recorder")
    })
}

/// GET /metrics — Prometheus exposition format.
pub async fn render() -> String {
    handle().render()
}

/// Middleware recording every request. Uses the matched route template
/// (`/v1/orders/{order_id}`), never the raw path — bounded cardinality.
pub async fn record(req: Request, next: Next) -> Response {
    // Touch the recorder so it is installed before first use.
    let _ = handle();

    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let started = Instant::now();

    let response = next.run(req).await;

    let labels = [
        ("method", method),
        ("route", route),
        ("status", response.status().as_u16().to_string()),
    ];
    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_request_duration_seconds", &labels)
        .record(started.elapsed().as_secs_f64());
    response
}
