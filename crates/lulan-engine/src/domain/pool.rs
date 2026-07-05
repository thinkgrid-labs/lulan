//! Count-based pool occupancy: per-segment remaining counters with the same
//! half-open span semantics as seat bitmasks. A span is available for `qty`
//! iff every segment it covers has at least `qty` remaining.

use serde::{Deserialize, Serialize};

use super::segment::SegmentSpan;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("span end {span_to} exceeds the pool's {segments} segments")]
    SpanOutOfRange { span_to: u8, segments: u8 },
    #[error("insufficient capacity: requested {requested}, minimum remaining {remaining}")]
    Insufficient { requested: i32, remaining: i32 },
    #[error("release of {requested} would exceed capacity {capacity}")]
    ReleaseExceedsCapacity { requested: i32, capacity: i32 },
    #[error("quantity must be positive, got {0}")]
    NonPositiveQuantity(i32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolOccupancy {
    capacity: i32,
    remaining: Vec<i32>,
}

impl PoolOccupancy {
    /// A fresh pool: every segment starts at full capacity.
    pub fn new(capacity: i32, segment_count: u8) -> Self {
        Self {
            capacity: capacity.max(0),
            remaining: vec![capacity.max(0); segment_count as usize],
        }
    }

    /// Rebuild from stored per-segment counters.
    pub fn from_parts(capacity: i32, remaining: Vec<i32>) -> Self {
        Self {
            capacity,
            remaining,
        }
    }

    pub fn capacity(&self) -> i32 {
        self.capacity
    }

    pub fn segment_count(&self) -> u8 {
        self.remaining.len() as u8
    }

    pub fn remaining(&self) -> &[i32] {
        &self.remaining
    }

    fn check_span(&self, span: SegmentSpan) -> Result<(), PoolError> {
        if span.to_index() > self.segment_count() {
            return Err(PoolError::SpanOutOfRange {
                span_to: span.to_index(),
                segments: self.segment_count(),
            });
        }
        Ok(())
    }

    fn segments(&self, span: SegmentSpan) -> std::ops::Range<usize> {
        span.from_index() as usize..span.to_index() as usize
    }

    /// Minimum remaining across the span — how many can still be reserved.
    pub fn remaining_for(&self, span: SegmentSpan) -> Result<i32, PoolError> {
        self.check_span(span)?;
        Ok(self.remaining[self.segments(span)]
            .iter()
            .copied()
            .min()
            .unwrap_or(0))
    }

    pub fn is_available(&self, span: SegmentSpan, qty: i32) -> Result<bool, PoolError> {
        if qty <= 0 {
            return Err(PoolError::NonPositiveQuantity(qty));
        }
        Ok(self.remaining_for(span)? >= qty)
    }

    /// Take `qty` from every segment in the span, or fail atomically.
    pub fn reserve(&mut self, span: SegmentSpan, qty: i32) -> Result<(), PoolError> {
        if qty <= 0 {
            return Err(PoolError::NonPositiveQuantity(qty));
        }
        let remaining = self.remaining_for(span)?;
        if remaining < qty {
            return Err(PoolError::Insufficient {
                requested: qty,
                remaining,
            });
        }
        for i in self.segments(span) {
            self.remaining[i] -= qty;
        }
        Ok(())
    }

    /// Return `qty` to every segment in the span, or fail atomically if any
    /// segment would exceed capacity (releasing something never reserved).
    pub fn release(&mut self, span: SegmentSpan, qty: i32) -> Result<(), PoolError> {
        if qty <= 0 {
            return Err(PoolError::NonPositiveQuantity(qty));
        }
        self.check_span(span)?;
        let range = self.segments(span);
        if self.remaining[range.clone()]
            .iter()
            .any(|r| r + qty > self.capacity)
        {
            return Err(PoolError::ReleaseExceedsCapacity {
                requested: qty,
                capacity: self.capacity,
            });
        }
        for i in range {
            self.remaining[i] += qty;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(from: u8, to: u8) -> SegmentSpan {
        SegmentSpan::new(from, to).unwrap()
    }

    #[test]
    fn reserve_takes_from_every_covered_segment_only() {
        let mut pool = PoolOccupancy::new(10, 3);
        pool.reserve(span(0, 2), 4).unwrap();
        assert_eq!(pool.remaining(), &[6, 6, 10]);
        assert_eq!(pool.remaining_for(span(0, 3)).unwrap(), 6);
        assert_eq!(pool.remaining_for(span(2, 3)).unwrap(), 10);
    }

    #[test]
    fn availability_is_bounded_by_the_tightest_segment() {
        let mut pool = PoolOccupancy::new(10, 3);
        pool.reserve(span(1, 2), 9).unwrap();
        // Only 1 left on the middle segment: a spanning journey is capped.
        assert!(pool.is_available(span(0, 3), 1).unwrap());
        assert!(!pool.is_available(span(0, 3), 2).unwrap());
        // The outer segments individually still have room.
        assert!(pool.is_available(span(0, 1), 10).unwrap());
    }

    #[test]
    fn failed_reserve_changes_nothing() {
        let mut pool = PoolOccupancy::new(5, 4);
        pool.reserve(span(2, 3), 5).unwrap();
        let before = pool.clone();
        assert_eq!(
            pool.reserve(span(0, 4), 1),
            Err(PoolError::Insufficient {
                requested: 1,
                remaining: 0
            })
        );
        assert_eq!(pool, before);
    }

    #[test]
    fn release_rejects_overflow_and_reserve_release_roundtrips() {
        let mut pool = PoolOccupancy::new(8, 2);
        assert_eq!(
            pool.release(span(0, 1), 1),
            Err(PoolError::ReleaseExceedsCapacity {
                requested: 1,
                capacity: 8
            })
        );
        let fresh = pool.clone();
        pool.reserve(span(0, 2), 3).unwrap();
        pool.release(span(0, 2), 3).unwrap();
        assert_eq!(pool, fresh);
    }

    #[test]
    fn rejects_span_past_pool_end_and_non_positive_quantities() {
        let mut pool = PoolOccupancy::new(5, 2);
        assert_eq!(
            pool.remaining_for(span(0, 3)),
            Err(PoolError::SpanOutOfRange {
                span_to: 3,
                segments: 2
            })
        );
        assert_eq!(
            pool.reserve(span(0, 1), 0),
            Err(PoolError::NonPositiveQuantity(0))
        );
    }
}
