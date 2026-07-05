//! Per-caller sliding-window rate limiting for mutating endpoints
//! (Redis ZSET + Lua, atomic check-and-record).
//!
//! Fails OPEN: if Redis is missing or errors, requests pass — the rate
//! limiter protects against abuse, it must never become the outage.
//! Keyed by API key when present (hashed), else by client IP.

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::state::AppState;

const WINDOW_SECS: u64 = 60;

/// Atomic sliding window: drop old entries, count, conditionally add.
/// KEYS[1] = zset key; ARGV = now_ms, window_ms, limit, member.
const LUA: &str = r#"
redis.call('ZREMRANGEBYSCORE', KEYS[1], 0, ARGV[1] - ARGV[2])
local count = redis.call('ZCARD', KEYS[1])
if count >= tonumber(ARGV[3]) then
    return 0
end
redis.call('ZADD', KEYS[1], ARGV[1], ARGV[4])
redis.call('PEXPIRE', KEYS[1], ARGV[2])
return 1
"#;

fn limit_per_window() -> u64 {
    std::env::var("LULAN_RATE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

fn caller_key(req: &Request) -> String {
    if let Some(key) = req
        .headers()
        .get("x-api-key")
        .or_else(|| req.headers().get("authorization"))
        .and_then(|v| v.to_str().ok())
    {
        let digest = Sha256::digest(key.as_bytes());
        return format!(
            "key:{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        );
    }
    let ip = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .unwrap_or("unknown")
        .trim()
        .to_string();
    format!("ip:{ip}")
}

/// Middleware: applies to mutating (non-GET) /v1 requests.
pub async fn limit(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let mutating =
        !matches!(*req.method(), Method::GET | Method::HEAD) && req.uri().path().starts_with("/v1");
    let Some(redis) = (mutating).then_some(state.redis.clone()).flatten() else {
        return next.run(req).await;
    };

    let key = format!("rl:{{{}}}", caller_key(&req));
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let member = format!("{now_ms}-{}", uuid::Uuid::new_v4().simple());

    let mut conn = redis;
    let allowed: Result<i64, _> = redis::Script::new(LUA)
        .key(&key)
        .arg(now_ms)
        .arg(WINDOW_SECS * 1000)
        .arg(limit_per_window())
        .arg(member)
        .invoke_async(&mut conn)
        .await;

    match allowed {
        Ok(0) => ApiError::TooManyRequests.into_response(),
        Ok(_) => next.run(req).await,
        Err(err) => {
            // Fail open — never let the limiter cause the outage.
            tracing::warn!(error = %err, "rate limiter unavailable, failing open");
            next.run(req).await
        }
    }
}
