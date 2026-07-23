//! The config-driven payment adapter, exercised against a stub PSP.
//!
//! No network and no Stripe account: a local axum server plays the
//! provider, so these run in CI. What is proven is the thing that makes
//! "bring your own provider" real — that a JSON description drives intent
//! creation, refunds, and signed-callback verification, and that the
//! shipped Stripe preset is a valid instance of that description rather
//! than a special case in the code.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use hmac::{Hmac, Mac};
use lulan_engine::payments::http::{HttpProvider, ProviderConfig, preset};
use lulan_engine::payments::{PaymentError, PaymentEvent, PaymentProvider};
use sha2::Sha256;
use uuid::Uuid;

/// Requests the stub received, so tests can assert what was actually sent.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

async fn stub_psp() -> (SocketAddr, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));

    async fn record(
        State(seen): State<Seen>,
        uri: axum::http::Uri,
        body: String,
    ) -> axum::response::Response {
        seen.lock()
            .unwrap()
            .push((uri.path().to_string(), body.clone()));
        // A Stripe-shaped answer; the pointers in the config find the
        // fields, which is exactly what is under test.
        axum::response::IntoResponse::into_response(axum::Json(serde_json::json!({
            "id": "pi_stub_123",
            "client_secret": "pi_stub_123_secret_abc",
            "status": "requires_payment_method"
        })))
    }

    async fn decline(body: String) -> axum::response::Response {
        let _ = body;
        axum::response::IntoResponse::into_response((
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": { "message": "Your card was declined." }
            })),
        ))
    }

    async fn outage() -> axum::response::Response {
        axum::response::IntoResponse::into_response((
            axum::http::StatusCode::BAD_GATEWAY,
            "upstream is having a day",
        ))
    }

    let app = Router::new()
        .route("/v1/payment_intents", post(record))
        .route("/v1/refunds", post(record))
        .route("/declined", post(decline))
        .route("/outage", post(outage))
        .with_state(seen.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, seen)
}

/// The shipped Stripe preset, pointed at the stub instead of api.stripe.com.
fn stripe_against(addr: SocketAddr) -> ProviderConfig {
    let mut config = ProviderConfig::from_json(preset::STRIPE).expect("preset is valid JSON");
    config.base_url = format!("http://{addr}");
    config
}

/// Cargo runs these test functions on parallel threads, so a per-test
/// `set_var` is concurrent environment mutation — undefined behaviour in
/// edition 2024, which is why the call is `unsafe`. Every secret this
/// binary needs is a fixed constant, so we set them all EXACTLY ONCE,
/// synchronized. After the `Once` completes no test ever writes the
/// environment again; concurrent reads are fine.
fn install_test_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: `Once` guarantees this body runs on one thread with a
        // happens-before edge to every caller, and nothing writes the
        // environment after it.
        unsafe {
            std::env::set_var("LULAN_PAYMENT_SECRET", "sk_test_stub");
            std::env::set_var("LULAN_PAYMENT_WEBHOOK_SECRET", "whsec_stub");
            std::env::set_var("ACME_KEY", "acme-secret");
            std::env::set_var("ACME_WEBHOOK_SECRET", "acme-hook");
        }
    });
}

fn with_secrets<T>(body: impl FnOnce() -> T) -> T {
    install_test_env();
    body()
}

fn stripe_signature(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{body}").as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={timestamp},v1={hex}")
}

#[tokio::test]
async fn preset_drives_intent_creation_and_refunds() {
    let (addr, seen) = stub_psp().await;
    let provider = with_secrets(|| HttpProvider::new(stripe_against(addr)).unwrap());

    let order_id = Uuid::new_v4();
    let intent = provider
        .create_intent(order_id, 245_000, "PHP")
        .await
        .expect("stub accepts the intent");
    assert_eq!(intent.id, "pi_stub_123", "id read via its JSON pointer");
    assert_eq!(
        intent.client_secret.as_deref(),
        Some("pi_stub_123_secret_abc"),
        "client secret is passed through for the browser SDK"
    );

    provider
        .refund("pi_stub_123", 245_000)
        .await
        .expect("stub accepts the refund");

    let seen = seen.lock().unwrap();
    let (path, body) = &seen[0];
    assert_eq!(path, "/v1/payment_intents");
    // Form encoding, minor units untouched, currency lowercased, order id
    // carried in metadata so the PSP dashboard reconciles to a booking.
    assert!(body.contains("amount=245000"), "{body}");
    assert!(body.contains("currency=php"), "{body}");
    assert!(
        body.contains(&format!("order_id%5D={order_id}")),
        "metadata[order_id] must reach the provider: {body}"
    );

    let (path, body) = &seen[1];
    assert_eq!(path, "/v1/refunds");
    assert!(body.contains("payment_intent=pi_stub_123"), "{body}");
    assert!(body.contains("amount=245000"), "{body}");
}

#[tokio::test]
async fn declines_are_final_and_outages_are_retryable() {
    let (addr, _) = stub_psp().await;

    let mut config = stripe_against(addr);
    config.create_intent.path = "/declined".into();
    let provider = with_secrets(|| HttpProvider::new(config).unwrap());
    let err = provider
        .create_intent(Uuid::new_v4(), 100, "PHP")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, PaymentError::Rejected(m) if m.contains("card was declined")),
        "4xx is an answer, and the provider's own words are surfaced: {err:?}"
    );

    let mut config = stripe_against(addr);
    config.create_intent.path = "/outage".into();
    let provider = with_secrets(|| HttpProvider::new(config).unwrap());
    let err = provider
        .create_intent(Uuid::new_v4(), 100, "PHP")
        .await
        .unwrap_err();
    assert!(
        matches!(err, PaymentError::Unavailable(_)),
        "5xx is an outage, not a decline: {err:?}"
    );
}

#[tokio::test]
async fn signed_callbacks_are_verified_and_mapped() {
    let (addr, _) = stub_psp().await;
    let provider = with_secrets(|| HttpProvider::new(stripe_against(addr)).unwrap());
    assert!(
        provider.authenticates_callbacks(),
        "a signing provider must say so — the endpoint drops its API-key requirement on this"
    );

    let body = r#"{"type":"payment_intent.succeeded","data":{"object":{"id":"pi_stub_123"}}}"#;
    let now = chrono::Utc::now().timestamp();

    assert_eq!(
        provider
            .verify_callback(
                Some(&stripe_signature("whsec_stub", now, body)),
                body.as_bytes()
            )
            .unwrap(),
        PaymentEvent::Captured {
            payment_intent_id: "pi_stub_123".into()
        }
    );

    let failed = r#"{"type":"payment_intent.payment_failed","data":{"object":{"id":"pi_x"}}}"#;
    assert_eq!(
        provider
            .verify_callback(
                Some(&stripe_signature("whsec_stub", now, failed)),
                failed.as_bytes()
            )
            .unwrap(),
        PaymentEvent::Failed {
            payment_intent_id: "pi_x".into()
        }
    );

    // Authentic but irrelevant: acknowledged, never acted on.
    let other = r#"{"type":"customer.created","data":{"object":{"id":"cus_1"}}}"#;
    assert_eq!(
        provider
            .verify_callback(
                Some(&stripe_signature("whsec_stub", now, other)),
                other.as_bytes()
            )
            .unwrap(),
        PaymentEvent::Ignored
    );
}

#[tokio::test]
async fn forged_stale_and_missing_signatures_are_refused() {
    let (addr, _) = stub_psp().await;
    let provider = with_secrets(|| HttpProvider::new(stripe_against(addr)).unwrap());
    let body = r#"{"type":"payment_intent.succeeded","data":{"object":{"id":"pi_stub_123"}}}"#;
    let now = chrono::Utc::now().timestamp();

    for (label, signature) in [
        ("no signature at all", None),
        (
            "wrong secret",
            Some(stripe_signature("whsec_wrong", now, body)),
        ),
        (
            "valid signature over a different body",
            Some(stripe_signature("whsec_stub", now, "{}")),
        ),
        (
            "replayed from an hour ago",
            Some(stripe_signature("whsec_stub", now - 3600, body)),
        ),
        ("garbage header", Some("t=abc,v1=zz".to_string())),
    ] {
        let result = provider.verify_callback(signature.as_deref(), body.as_bytes());
        assert!(
            matches!(result, Err(PaymentError::BadSignature)),
            "{label} must be refused, got {result:?}"
        );
    }

    // Key rotation: the provider sends several signatures, one of which is
    // current. That must pass.
    let rotating = format!(
        "{},v1={}",
        stripe_signature("whsec_stub", now, body),
        "0".repeat(64)
    );
    assert!(
        provider
            .verify_callback(Some(&rotating), body.as_bytes())
            .is_ok(),
        "any valid signature in the header is enough"
    );
}

/// A provider described from scratch — JSON bodies, a custom auth header,
/// a raw signature header, dotted field nesting. Nothing Stripe-shaped.
#[tokio::test]
async fn an_arbitrary_provider_needs_no_rust() {
    let (addr, seen) = stub_psp().await;
    let config = ProviderConfig::from_json(&format!(
        r#"{{
          "name": "acme-pay",
          "base_url": "http://{addr}",
          "encoding": "json",
          "auth": {{ "type": "header", "name": "X-Acme-Key", "value_env": "ACME_KEY" }},
          "create_intent": {{
            "path": "/v1/payment_intents",
            "fields": {{
              "amount": "{{amount_minor}}",
              "currency": "{{currency_upper}}",
              "metadata.booking": "{{order_id}}"
            }},
            "intent_id_pointer": "/id"
          }},
          "refund": {{
            "path": "/v1/refunds",
            "fields": {{ "charge": "{{payment_intent_id}}" }}
          }},
          "webhook": {{
            "signature_header": "x-acme-signature",
            "secret_env": "ACME_WEBHOOK_SECRET",
            "digest": "sha512",
            "encoding": "base64",
            "event_type_pointer": "/event",
            "intent_id_pointer": "/payment/reference",
            "captured_events": ["payment.paid"]
          }}
        }}"#
    ))
    .expect("config parses");

    install_test_env();
    let provider = HttpProvider::new(config).unwrap();
    assert_eq!(provider.name(), "acme-pay");

    let order_id = Uuid::new_v4();
    provider.create_intent(order_id, 4200, "php").await.unwrap();

    let (_, body) = &seen.lock().unwrap()[0];
    let sent: serde_json::Value = serde_json::from_str(body).expect("JSON body");
    assert_eq!(sent["amount"], 4200, "amount is a number, not a string");
    assert_eq!(sent["currency"], "PHP", "currency_upper applied");
    assert_eq!(
        sent["metadata"]["booking"],
        order_id.to_string(),
        "dotted keys nest"
    );

    // SHA-512 + base64 over the raw body, in a plain header.
    let callback = r#"{"event":"payment.paid","payment":{"reference":"acme_9"}}"#;
    let mut mac = Hmac::<sha2::Sha512>::new_from_slice(b"acme-hook").unwrap();
    mac.update(callback.as_bytes());
    let signature = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        mac.finalize().into_bytes(),
    );
    assert_eq!(
        provider
            .verify_callback(Some(&signature), callback.as_bytes())
            .unwrap(),
        PaymentEvent::Captured {
            payment_intent_id: "acme_9".into()
        }
    );
    assert!(matches!(
        provider.verify_callback(Some("bogus"), callback.as_bytes()),
        Err(PaymentError::BadSignature)
    ));
}

#[test]
fn shipped_presets_all_parse() {
    for name in preset::NAMES {
        let source = preset::by_name(name).expect("listed presets exist");
        ProviderConfig::from_json(source).unwrap_or_else(|e| panic!("preset {name}: {e}"));
    }
}
