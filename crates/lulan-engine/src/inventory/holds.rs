//! Soft holds in Redis (ADR 0002): the fast-path "seat is being held for
//! you" layer. Holds are span-aware — two holds on the same seat coexist iff
//! their spans don't overlap — and expire via TTL. Losing Redis loses holds,
//! never sold inventory: the Postgres claim is the source of truth.
//!
//! Layout per hold:
//! - `lulan:holds:{trip}:{unit}` — hash: hold_id → `expires_ms:mask_hi:mask_lo`.
//!   The acquire script prunes expired entries, ORs live masks, and admits
//!   the new hold only if it doesn't overlap. Masks are split into two
//!   32-bit halves because Redis Lua's bitops are 32-bit.
//! - `lulan:hold:{hold_id}` — registry string `trip:unit`, same TTL, so a
//!   hold can be released or verified by id alone.

use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Script};
use uuid::Uuid;

use crate::domain::SegmentSpan;

const ACQUIRE_SCRIPT: &str = r#"
local key = KEYS[1]
local now = tonumber(ARGV[1])
local ttl = tonumber(ARGV[2])
local hold_id = ARGV[3]
local hi = tonumber(ARGV[4])
local lo = tonumber(ARGV[5])

local union_hi, union_lo = 0, 0
local max_exp = now + ttl
local fields = redis.call('HGETALL', key)
for i = 1, #fields, 2 do
    local exp, fhi, flo = string.match(fields[i + 1], '^(%d+):(%d+):(%d+)$')
    exp = tonumber(exp)
    if exp == nil or exp <= now then
        redis.call('HDEL', key, fields[i])
    else
        union_hi = bit.bor(union_hi, tonumber(fhi))
        union_lo = bit.bor(union_lo, tonumber(flo))
        if exp > max_exp then max_exp = exp end
    end
end

if bit.band(union_hi, hi) ~= 0 or bit.band(union_lo, lo) ~= 0 then
    return 0
end

redis.call('HSET', key, hold_id, tostring(now + ttl) .. ':' .. ARGV[4] .. ':' .. ARGV[5])
redis.call('PEXPIRE', key, max_exp - now)
return 1
"#;

#[derive(Debug, thiserror::Error)]
#[error("hold store error: {0}")]
pub struct HoldError(#[from] redis::RedisError);

#[derive(Debug, Clone, Copy)]
pub struct Hold {
    pub hold_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct HoldStore {
    conn: ConnectionManager,
    acquire: Script,
}

impl HoldStore {
    pub fn new(conn: ConnectionManager) -> Self {
        Self {
            conn,
            acquire: Script::new(ACQUIRE_SCRIPT),
        }
    }

    fn unit_key(trip_id: Uuid, unit_id: Uuid) -> String {
        format!("lulan:holds:{trip_id}:{unit_id}")
    }

    fn registry_key(hold_id: Uuid) -> String {
        format!("lulan:hold:{hold_id}")
    }

    /// Try to hold `span` on one seat for `ttl`. `Ok(None)` = span is held
    /// by someone else.
    pub async fn acquire(
        &self,
        trip_id: Uuid,
        unit_id: Uuid,
        span: SegmentSpan,
        ttl: std::time::Duration,
    ) -> Result<Option<Hold>, HoldError> {
        let hold_id = Uuid::new_v4();
        let now = Utc::now();
        let ttl_ms = ttl.as_millis() as i64;
        let mask = span.mask();
        let hi = (mask >> 32) as u32;
        let lo = (mask & 0xFFFF_FFFF) as u32;

        let mut conn = self.conn.clone();
        let admitted: i64 = self
            .acquire
            .key(Self::unit_key(trip_id, unit_id))
            .arg(now.timestamp_millis())
            .arg(ttl_ms)
            .arg(hold_id.to_string())
            .arg(hi)
            .arg(lo)
            .invoke_async(&mut conn)
            .await?;

        if admitted != 1 {
            return Ok(None);
        }

        let _: () = redis::cmd("SET")
            .arg(Self::registry_key(hold_id))
            .arg(format!("{trip_id}:{unit_id}"))
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await?;

        Ok(Some(Hold {
            hold_id,
            expires_at: now + chrono::Duration::milliseconds(ttl_ms),
        }))
    }

    /// True if `hold_id` is live and belongs to this trip + unit.
    pub async fn verify(
        &self,
        hold_id: Uuid,
        trip_id: Uuid,
        unit_id: Uuid,
    ) -> Result<bool, HoldError> {
        let mut conn = self.conn.clone();
        let value: Option<String> = conn.get(Self::registry_key(hold_id)).await?;
        Ok(value.as_deref() == Some(&format!("{trip_id}:{unit_id}")))
    }

    /// Release a hold by id. Returns false if it had already expired.
    pub async fn release(&self, hold_id: Uuid) -> Result<bool, HoldError> {
        let mut conn = self.conn.clone();
        let registry = Self::registry_key(hold_id);
        let value: Option<String> = conn.get(&registry).await?;
        let Some(value) = value else {
            return Ok(false);
        };
        if let Some((trip, unit)) = value.split_once(':') {
            let _: () = conn
                .hdel(format!("lulan:holds:{trip}:{unit}"), hold_id.to_string())
                .await?;
        }
        let _: () = conn.del(&registry).await?;
        Ok(true)
    }
}
