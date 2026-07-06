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

/// One seat held under an itinerary hold.
#[derive(Debug, Clone, Copy)]
pub struct HeldSeat {
    pub trip_id: Uuid,
    pub unit_id: Uuid,
}

/// A soft reservation of several seats across one or more trips, addressed
/// by a single id — the itinerary-level hold (round-trip / multi-city).
#[derive(Debug, Clone)]
pub struct ItineraryHold {
    pub hold_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub members: Vec<HeldSeat>,
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

    /// Registry for an itinerary hold: `seat_hold_id:trip:unit` triples
    /// joined by `;`, so the group can be verified/released by one id.
    fn itinerary_key(hold_id: Uuid) -> String {
        format!("lulan:itinerary:{hold_id}")
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

    /// Hold several seats — across one or more trips — as ONE itinerary
    /// hold. All-or-nothing: if any span is already held, the ones taken so
    /// far are rolled back and `Ok(None)` is returned. This is what a
    /// round-trip (2 legs) or multi-city selection uses; a single seat is
    /// just a one-member itinerary.
    pub async fn acquire_itinerary(
        &self,
        seats: &[(Uuid, Uuid, SegmentSpan)],
        ttl: std::time::Duration,
    ) -> Result<Option<ItineraryHold>, HoldError> {
        let group_id = Uuid::new_v4();
        let ttl_ms = ttl.as_millis() as i64;
        // (seat_hold_id, trip, unit)
        let mut acquired: Vec<(Uuid, Uuid, Uuid)> = Vec::with_capacity(seats.len());
        for (trip, unit, span) in seats {
            match self.acquire(*trip, *unit, *span, ttl).await? {
                Some(hold) => acquired.push((hold.hold_id, *trip, *unit)),
                None => {
                    for (seat_hold_id, _, _) in &acquired {
                        let _ = self.release(*seat_hold_id).await;
                    }
                    return Ok(None);
                }
            }
        }

        let value = acquired
            .iter()
            .map(|(hid, t, u)| format!("{hid}:{t}:{u}"))
            .collect::<Vec<_>>()
            .join(";");
        let mut conn = self.conn.clone();
        let _: () = redis::cmd("SET")
            .arg(Self::itinerary_key(group_id))
            .arg(value)
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await?;

        Ok(Some(ItineraryHold {
            hold_id: group_id,
            expires_at: Utc::now() + chrono::Duration::milliseconds(ttl_ms),
            members: acquired
                .iter()
                .map(|(_, trip_id, unit_id)| HeldSeat {
                    trip_id: *trip_id,
                    unit_id: *unit_id,
                })
                .collect(),
        }))
    }

    async fn itinerary_raw(
        &self,
        hold_id: Uuid,
    ) -> Result<Option<Vec<(Uuid, Uuid, Uuid)>>, HoldError> {
        let mut conn = self.conn.clone();
        let value: Option<String> = conn.get(Self::itinerary_key(hold_id)).await?;
        let Some(value) = value else {
            return Ok(None);
        };
        let mut members = Vec::new();
        for member in value.split(';') {
            let mut parts = member.split(':');
            if let (Some(h), Some(t), Some(u), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
                && let (Ok(h), Ok(t), Ok(u)) =
                    (Uuid::parse_str(h), Uuid::parse_str(t), Uuid::parse_str(u))
            {
                members.push((h, t, u));
            }
        }
        Ok(Some(members))
    }

    /// The seats an itinerary hold covers, or `None` if it has expired /
    /// never existed. Used to verify a presented hold covers an order's
    /// items.
    pub async fn itinerary_members(
        &self,
        hold_id: Uuid,
    ) -> Result<Option<Vec<HeldSeat>>, HoldError> {
        Ok(self.itinerary_raw(hold_id).await?.map(|members| {
            members
                .into_iter()
                .map(|(_, trip_id, unit_id)| HeldSeat { trip_id, unit_id })
                .collect()
        }))
    }

    /// Release every seat of an itinerary hold. Returns false if it had
    /// already expired.
    pub async fn release_itinerary(&self, hold_id: Uuid) -> Result<bool, HoldError> {
        let Some(members) = self.itinerary_raw(hold_id).await? else {
            return Ok(false);
        };
        for (seat_hold_id, _, _) in &members {
            let _ = self.release(*seat_hold_id).await?;
        }
        let mut conn = self.conn.clone();
        let _: () = conn.del(Self::itinerary_key(hold_id)).await?;
        Ok(true)
    }
}
