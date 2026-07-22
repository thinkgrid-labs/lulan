//! Browser access control.
//!
//! Lulan is headless: the storefront that calls it is somebody else's web
//! app on somebody else's origin, so without CORS headers no browser can
//! reach the API at all — `@lulan/storefront-sdk` works only server-side.
//!
//! Origins are an explicit operator decision (`LULAN_CORS_ALLOWED_ORIGINS`,
//! comma-separated) rather than a permissive default. The API answers with
//! order PII and boarding-pass tokens; which sites may read those is not a
//! question this crate should answer on an operator's behalf.
//!
//! Unset means the layer is not installed at all, rather than installed and
//! empty: a `CorsLayer` with no origins still answers every `OPTIONS` with
//! 200, so mounting one unconditionally would change the API's shape for
//! operators who never asked for browser access.

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Env var naming the browser origins allowed to call this API. Either a
/// comma-separated list of exact origins (`https://book.example.com`) or
/// `*` for any.
pub const ALLOWED_ORIGINS_ENV: &str = "LULAN_CORS_ALLOWED_ORIGINS";

/// Headers a storefront legitimately sends: JSON bodies, both credential
/// forms, and the booking-retry key.
const ALLOWED_HEADERS: [&str; 4] = [
    "content-type",
    "authorization",
    "x-api-key",
    "idempotency-key",
];

/// The layer to install, or `None` to leave the API server-to-server.
pub fn layer_from_env() -> Option<CorsLayer> {
    layer_for(&std::env::var(ALLOWED_ORIGINS_ENV).ok()?)
}

/// `*` allows any origin; anything else is an exact-origin list. `None`
/// when the setting names no usable origin — a typo must not silently
/// open the API, nor quietly alter it.
///
/// Note that `*` cannot be combined with credentialed browser requests per
/// the CORS spec: a storefront sending `Authorization` needs its origin
/// named explicitly.
pub fn layer_for(origins: &str) -> Option<CorsLayer> {
    let allow = if origins.trim() == "*" {
        tracing::info!("CORS: any browser origin allowed");
        AllowOrigin::any()
    } else {
        let parsed: Vec<HeaderValue> = origins
            .split(',')
            .map(str::trim)
            .filter(|o| !o.is_empty())
            .filter_map(|origin| match HeaderValue::from_str(origin) {
                Ok(value) => Some(value),
                Err(_) => {
                    tracing::warn!(%origin, "ignoring unparseable CORS origin");
                    None
                }
            })
            .collect();
        if parsed.is_empty() {
            tracing::warn!("{ALLOWED_ORIGINS_ENV} names no usable origin — browsers stay blocked");
            return None;
        }
        tracing::info!(count = parsed.len(), "CORS: browser origins allowed");
        AllowOrigin::list(parsed)
    };

    Some(
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([Method::GET, Method::POST, Method::DELETE])
            .allow_headers(
                ALLOWED_HEADERS
                    .iter()
                    .map(|h| HeaderName::from_static(h))
                    .collect::<Vec<_>>(),
            )
            .max_age(std::time::Duration::from_secs(600)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use tower::ServiceExt;

    /// A browser preflight: OPTIONS carrying Origin + the method it intends.
    fn preflight(origin: &str) -> Request<Body> {
        Request::builder()
            .method("OPTIONS")
            .uri("/v1/orders")
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap()
    }

    fn base() -> Router {
        Router::new().route("/v1/orders", post(|| async { "ok" }))
    }

    fn allowed_origin(response: &axum::http::Response<Body>) -> Option<&str> {
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
    }

    #[tokio::test]
    async fn named_origins_are_allowed_and_others_are_not() {
        let layer = layer_for("https://book.example.com, https://kiosk.example.com")
            .expect("two valid origins");

        let response = base()
            .layer(layer.clone())
            .oneshot(preflight("https://kiosk.example.com"))
            .await
            .unwrap();
        assert_eq!(allowed_origin(&response), Some("https://kiosk.example.com"));

        let response = base()
            .layer(layer)
            .oneshot(preflight("https://attacker.example.com"))
            .await
            .unwrap();
        assert_eq!(
            allowed_origin(&response),
            None,
            "an unlisted origin must not be echoed back"
        );
    }

    #[tokio::test]
    async fn wildcard_allows_any_origin() {
        let layer = layer_for("*").expect("wildcard is valid");
        let response = base()
            .layer(layer)
            .oneshot(preflight("https://anywhere.example.com"))
            .await
            .unwrap();
        assert_eq!(allowed_origin(&response), Some("*"));
    }

    /// A setting that names nothing usable yields no layer, so the API is
    /// left exactly as it was — neither opened nor reshaped.
    #[tokio::test]
    async fn unusable_settings_install_no_layer() {
        for value in ["", "   ", ",  ,", "not a valid header\u{7f}"] {
            assert!(
                layer_for(value).is_none(),
                "{value:?} must not produce a CORS layer"
            );
        }
    }

    /// Why `None` rather than an empty layer: without one, OPTIONS still
    /// falls through to the router (405 here); with any layer at all it
    /// would be answered 200 by the middleware.
    #[tokio::test]
    async fn absent_layer_leaves_options_to_the_router() {
        let options = || {
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/orders")
                .body(Body::empty())
                .unwrap()
        };
        let response = base().oneshot(options()).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        let response = base()
            .layer(layer_for("*").unwrap())
            .oneshot(options())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
