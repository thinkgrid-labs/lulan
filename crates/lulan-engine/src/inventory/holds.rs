//! Soft holds in Redis (ADR 0002): the fast-path "seat is being held for
//! you" layer. Holds are span-aware — two holds on the same seat coexist iff
//! their spans don't overlap — and expire via TTL. Losing Redis loses holds,
//! never sold inventory: the Postgres claim is the source of truth.
//!
//! Layout per hold. Every key carries `{trip}` as a Redis Cluster hash tag
//! so a trip's keys share a slot and the multi-key acquire script is
//! cluster-safe:
//!
//! - `lulan:holds:{trip}:unit:{unit}` — hash: hold_id →
//!   `expires_ms:mask_hi:mask_lo`. The acquire script prunes expired
//!   entries, ORs live masks, and admits the new hold only if it doesn't
//!   overlap. Masks are split into two 32-bit halves because Redis Lua's
//!   bitops are 32-bit.
//! - `lulan:holds:{trip}:index` — set of unit ids with at least one hold.
//!   Written by the same script that creates the hold, so it can never
//!   miss one. It CAN over-report: TTL expiry removes a unit's hash
//!   without touching the set, so readers prune lazily (see
//!   [`HoldStore::live_held_units`]).
//! - `lulan:hold:{hold_id}` — registry string `trip:unit`, same TTL, so a
//!   hold can be released or verified by id alone.
//!
//! The index exists because the alternative was `SCAN MATCH` across the
//! whole keyspace on every seat-map request: O(all keys in Redis) to
//! answer "which seats on THIS trip are held", on an endpoint anyone can
//! call. It also gives the per-trip hold ceiling an O(1) fast path.

use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Script};
use uuid::Uuid;

use crate::domain::SegmentSpan;

const ACQUIRE_SCRIPT: &str = r#"
local key = KEYS[1]
local index = KEYS[2]
local now = tonumber(ARGV[1])
local ttl = tonumber(ARGV[2])
local hold_id = ARGV[3]
local hi = tonumber(ARGV[4])
local lo = tonumber(ARGV[5])
local unit = ARGV[6]
local index_ttl = tonumber(ARGV[7])

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
-- Index the unit in the same atomic step as the hold itself, so a live
-- hold is never invisible to the seat map. The index TTL outlives any
-- hold and is refreshed here; stale members are pruned by readers.
redis.call('SADD', index, unit)
redis.call('PEXPIRE', index, index_ttl)
return 1
"#;

/// How long the per-trip index outlives the longest hold it can contain.
/// Only a bound on how long an all-stale index lingers — readers prune.
const INDEX_TTL_MULTIPLIER: u32 = 4;

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
        format!("lulan:holds:{{{trip_id}}}:unit:{unit_id}")
    }

    /// Set of units on this trip that have at least one hold. Shares the
    /// trip's hash tag with every unit key, so the acquire script's two
    /// keys land in the same cluster slot.
    fn index_key(trip_id: Uuid) -> String {
        format!("lulan:holds:{{{trip_id}}}:index")
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
            .key(Self::index_key(trip_id))
            .arg(now.timestamp_millis())
            .arg(ttl_ms)
            .arg(hold_id.to_string())
            .arg(hi)
            .arg(lo)
            .arg(unit_id.to_string())
            .arg(ttl_ms * i64::from(INDEX_TTL_MULTIPLIER))
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
        if let Some((trip, unit)) = value.split_once(':')
            && let (Ok(trip_id), Ok(unit_id)) = (Uuid::parse_str(trip), Uuid::parse_str(unit))
        {
            let key = Self::unit_key(trip_id, unit_id);
            let _: () = conn.hdel(&key, hold_id.to_string()).await?;
            // Last hold on this seat: take it out of the index now rather
            // than leaving it for a reader to prune.
            let remaining: i64 = conn.hlen(&key).await?;
            if remaining == 0 {
                let _: () = conn
                    .srem(Self::index_key(trip_id), unit_id.to_string())
                    .await?;
            }
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

    /// Every unit on `trip_id` with a live hold, mapped to the union of
    /// its held spans — and the index pruned of units whose holds have
    /// since expired.
    ///
    /// Reads the per-trip index instead of scanning the keyspace. The
    /// index only ever over-reports (TTL expiry removes a unit's hash but
    /// not its index entry), so a member with no live holds is dropped
    /// here: the cost of self-healing lands on reads, which are cheap and
    /// frequent, rather than on a sweeper.
    pub async fn live_held_units(
        &self,
        trip_id: Uuid,
    ) -> Result<std::collections::HashMap<Uuid, u64>, HoldError> {
        let mut conn = self.conn.clone();
        let members: Vec<String> = conn.smembers(Self::index_key(trip_id)).await?;
        let now = Utc::now().timestamp_millis();

        let mut live = std::collections::HashMap::new();
        let mut stale: Vec<String> = Vec::new();
        for member in members {
            let Ok(unit_id) = Uuid::parse_str(&member) else {
                stale.push(member);
                continue;
            };
            let fields: std::collections::HashMap<String, String> =
                conn.hgetall(Self::unit_key(trip_id, unit_id)).await?;
            let mut union: u64 = 0;
            for value in fields.values() {
                let mut parts = value.split(':');
                if let (Some(exp), Some(hi), Some(lo)) = (parts.next(), parts.next(), parts.next())
                    && let (Ok(exp), Ok(hi), Ok(lo)) =
                        (exp.parse::<i64>(), hi.parse::<u64>(), lo.parse::<u64>())
                    && exp > now
                {
                    union |= (hi << 32) | lo;
                }
            }
            if union == 0 {
                stale.push(member);
            } else {
                live.insert(unit_id, union);
            }
        }
        if !stale.is_empty() {
            let _: () = conn.srem(Self::index_key(trip_id), stale).await?;
        }
        Ok(live)
    }

    /// Units on `trip_id` with a live hold overlapping `span` — what lets
    /// the seat map grey out seats other sessions are holding. Advisory:
    /// a hold never blocks the authoritative claim.
    pub async fn held_units(
        &self,
        trip_id: Uuid,
        span: SegmentSpan,
    ) -> Result<std::collections::HashSet<Uuid>, HoldError> {
        let span_mask = span.mask();
        Ok(self
            .live_held_units(trip_id)
            .await?
            .into_iter()
            .filter(|(_, union)| union & span_mask != 0)
            .map(|(unit_id, _)| unit_id)
            .collect())
    }

    /// How many distinct seats currently carry a hold on `trip_id` — the
    /// input to the per-trip hold ceiling.
    ///
    /// `SCARD` is the fast path and can over-count stale members, so an
    /// answer at or above `accurate_above` is recomputed exactly (which
    /// also prunes). Holds are refused only on a number that has been
    /// verified: over-refusing is the one outcome worth paying for.
    pub async fn held_unit_count(
        &self,
        trip_id: Uuid,
        accurate_above: usize,
    ) -> Result<usize, HoldError> {
        let mut conn = self.conn.clone();
        let approximate: usize = conn.scard(Self::index_key(trip_id)).await?;
        if approximate < accurate_above {
            return Ok(approximate);
        }
        Ok(self.live_held_units(trip_id).await?.len())
    }
}
