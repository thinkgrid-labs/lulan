//! Per-caller sliding-window rate limiting (Redis ZSET + Lua, atomic
//! check-and-record).
//!
//! Fails OPEN: if Redis is missing or errors, requests pass — the limiter
//! protects against abuse, it must never become the outage.
//!
//! Two things this gets right that the first version did not:
//!
//! - **Reads are limited too.** Only mutations were counted, which left
//!   `/v1/trips/search` and `/v1/trips/{id}/availability` — unauthenticated,
//!   and the most database-expensive endpoints in the API — with no ceiling
//!   at all. Reads get their own, larger budget so a busy seat map cannot
//!   exhaust a caller's ability to book.
//! - **The client is identified from the connection**, not from a header
//!   it controls. `X-Forwarded-For` was read unconditionally, so rotating
//!   one header evaded the limit entirely. It is now consulted only as far
//!   as the operator says there are proxies in front.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::state::AppState;

const WINDOW_SECS: u64 = 60;
const DEFAULT_WRITE_LIMIT: u64 = 300;
const DEFAULT_READ_LIMIT: u64 = 1_200;

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

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// How many reverse proxies sit in front of this process.
///
/// Zero — the default — means `X-Forwarded-For` is ignored entirely and
/// the peer address is used. Behind one proxy (Caddy, nginx, an ALB) set
/// 1, and the last-but-zero entry is taken. Counting from the RIGHT is
/// what makes this safe: a client can prepend anything it likes to the
/// header, but it cannot forge the entries its own proxy appends.
fn trusted_proxy_hops() -> usize {
    std::env::var("LULAN_TRUSTED_PROXY_HOPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The address to attribute this request to.
fn client_ip(req: &Request, hops: usize) -> Option<IpAddr> {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());

    if hops == 0 {
        return peer;
    }
    // Walk in from the right by the number of proxies we actually trust;
    // everything further left is client-supplied and worthless.
    let forwarded: Vec<&str> = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').map(str::trim).collect())
        .unwrap_or_default();
    forwarded
        .len()
        .checked_sub(hops)
        .and_then(|i| forwarded.get(i))
        .and_then(|ip| ip.parse().ok())
        .or(peer)
}

/// Who to charge. A credential is a better identity than an address —
/// it survives NAT and mobile hand-off — so it wins when present.
fn caller_key(req: &Request, hops: usize) -> String {
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
    match client_ip(req, hops) {
        Some(ip) => format!("ip:{ip}"),
        // No connection info (in-process tests) and no trusted header.
        // One shared bucket is the safe reading of "we cannot tell these
        // callers apart".
        None => "ip:unknown".to_string(),
    }
}

/// Middleware over `/v1`. Reads and writes are counted in separate
/// buckets, so browsing seat maps never spends the budget a customer
/// needs to complete a booking.
pub async fn limit(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if !req.uri().path().starts_with("/v1") {
        return next.run(req).await;
    }
    let Some(redis) = state.redis.clone() else {
        return next.run(req).await;
    };

    let read = matches!(*req.method(), Method::GET | Method::HEAD);
    let (bucket, limit) = if read {
        ("r", env_u64("LULAN_RATE_LIMIT_READS", DEFAULT_READ_LIMIT))
    } else {
        ("w", env_u64("LULAN_RATE_LIMIT", DEFAULT_WRITE_LIMIT))
    };

    let key = format!("rl:{bucket}:{{{}}}", caller_key(&req, trusted_proxy_hops()));
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
        .arg(limit)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn request(xff: Option<&str>, peer: Option<&str>) -> Request {
        let mut builder = axum::http::Request::builder().uri("/v1/trips/search");
        if let Some(xff) = xff {
            builder = builder.header("x-forwarded-for", xff);
        }
        let mut req = builder.body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            req.extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        }
        req
    }

    /// The bug: a client could pick its own bucket by sending a header.
    #[test]
    fn forwarded_headers_are_ignored_without_a_trusted_proxy() {
        let req = request(Some("9.9.9.9"), Some("10.0.0.5:5555"));
        assert_eq!(
            caller_key(&req, 0),
            "ip:10.0.0.5",
            "with no proxy configured the header must not be believed"
        );
    }

    /// Behind one proxy, the entry that proxy appended is the client —
    /// and it is the LAST one, which is precisely the one a client cannot
    /// forge.
    #[test]
    fn one_hop_takes_the_entry_the_proxy_appended() {
        let req = request(Some("9.9.9.9, 203.0.113.7"), Some("10.0.0.5:5555"));
        assert_eq!(caller_key(&req, 1), "ip:203.0.113.7");
    }

    /// Spoof attempt: the client prepends junk hoping to be attributed to
    /// it. Counting from the right ignores everything it prepended.
    #[test]
    fn prepended_entries_cannot_shift_attribution() {
        let honest = request(Some("203.0.113.7"), Some("10.0.0.5:5555"));
        let spoofed = request(Some("1.1.1.1, 2.2.2.2, 203.0.113.7"), Some("10.0.0.5:5555"));
        assert_eq!(
            caller_key(&honest, 1),
            caller_key(&spoofed, 1),
            "a client must not be able to move itself to a fresh bucket"
        );
    }

    /// Two proxies deep, take two from the right.
    #[test]
    fn multiple_hops_are_honoured() {
        let req = request(Some("203.0.113.7, 10.1.1.1"), Some("10.0.0.5:5555"));
        assert_eq!(caller_key(&req, 2), "ip:203.0.113.7");
    }

    /// Misconfigured (more hops than entries): fall back to the peer
    /// rather than to whatever the client sent.
    #[test]
    fn too_many_hops_falls_back_to_the_peer() {
        let req = request(Some("1.1.1.1"), Some("10.0.0.5:5555"));
        assert_eq!(caller_key(&req, 5), "ip:10.0.0.5");
    }

    /// A credential identifies a caller better than an address does.
    #[test]
    fn a_credential_outranks_the_address() {
        let mut req = request(None, Some("10.0.0.5:5555"));
        req.headers_mut()
            .insert("x-api-key", "llk_abc".parse().unwrap());
        assert!(caller_key(&req, 0).starts_with("key:"));
    }
}
