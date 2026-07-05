//! Segment spans and occupancy bitmasks.
//!
//! A trip with stops `A ─ B ─ C ─ D` has 3 segments, indexed 0..3.
//! A passenger journey is a half-open span of segment indices `[from, to)`.
//! Occupancy of one capacity unit across a trip is a bitmask where bit `i`
//! set means segment `i` is occupied. Availability for a journey is then a
//! single AND: the span is free iff `occupied & span.mask() == 0`.

use serde::{Deserialize, Serialize};

/// Maximum number of segments per trip. Occupancy masks are `u64`, so trips
/// are limited to 64 consecutive segments — beyond any real-world route.
pub const MAX_SEGMENTS: u8 = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpanError {
    #[error("span is empty: from {from} must be less than to {to}")]
    Empty { from: u8, to: u8 },
    #[error("span end {to} exceeds the {MAX_SEGMENTS}-segment limit")]
    TooLong { to: u8 },
}

/// A half-open range of segment indices `[from, to)` within a single trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentSpan {
    from: u8,
    to: u8,
}

impl SegmentSpan {
    pub fn new(from: u8, to: u8) -> Result<Self, SpanError> {
        if from >= to {
            return Err(SpanError::Empty { from, to });
        }
        if to > MAX_SEGMENTS {
            return Err(SpanError::TooLong { to });
        }
        Ok(Self { from, to })
    }

    pub fn from_index(&self) -> u8 {
        self.from
    }

    pub fn to_index(&self) -> u8 {
        self.to
    }

    /// Number of segments covered. Never zero: the constructor rejects
    /// empty spans.
    pub fn segment_count(&self) -> u8 {
        self.to - self.from
    }

    /// Bitmask with bits `from..to` set.
    pub fn mask(&self) -> u64 {
        let width = self.to - self.from;
        if width == 64 {
            u64::MAX
        } else {
            ((1u64 << width) - 1) << self.from
        }
    }

    /// True if the span does not touch any occupied segment.
    pub fn is_available(&self, occupied: u64) -> bool {
        occupied & self.mask() == 0
    }

    /// True if the two spans share at least one segment.
    pub fn overlaps(&self, other: &SegmentSpan) -> bool {
        self.mask() & other.mask() != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(from: u8, to: u8) -> SegmentSpan {
        SegmentSpan::new(from, to).unwrap()
    }

    #[test]
    fn rejects_empty_and_reversed_spans() {
        assert_eq!(
            SegmentSpan::new(2, 2),
            Err(SpanError::Empty { from: 2, to: 2 })
        );
        assert_eq!(
            SegmentSpan::new(3, 1),
            Err(SpanError::Empty { from: 3, to: 1 })
        );
    }

    #[test]
    fn rejects_spans_past_the_segment_limit() {
        assert_eq!(SegmentSpan::new(0, 65), Err(SpanError::TooLong { to: 65 }));
        assert!(SegmentSpan::new(0, 64).is_ok());
    }

    #[test]
    fn mask_covers_exactly_the_span() {
        assert_eq!(span(0, 1).mask(), 0b001);
        assert_eq!(span(0, 3).mask(), 0b111);
        assert_eq!(span(1, 3).mask(), 0b110);
        assert_eq!(span(0, 64).mask(), u64::MAX);
        assert_eq!(span(63, 64).mask(), 1u64 << 63);
    }

    #[test]
    fn prd_example_seat_12a() {
        // A─B─C─D: occupied A→B and C→D, free B→C.
        let occupied = span(0, 1).mask() | span(2, 3).mask();
        assert!(span(1, 2).is_available(occupied));
        assert!(!span(0, 1).is_available(occupied));
        assert!(!span(0, 2).is_available(occupied));
        assert!(!span(1, 3).is_available(occupied));
    }

    #[test]
    fn adjacent_spans_do_not_overlap() {
        assert!(!span(0, 2).overlaps(&span(2, 4)));
        assert!(span(0, 2).overlaps(&span(1, 4)));
    }

    #[test]
    fn exhaustive_availability_matches_interval_arithmetic() {
        // For every pair of spans on an 8-segment trip, bitmask overlap must
        // agree with plain interval overlap: max(from) < min(to).
        for a_from in 0..8u8 {
            for a_to in (a_from + 1)..=8 {
                for b_from in 0..8u8 {
                    for b_to in (b_from + 1)..=8 {
                        let a = span(a_from, a_to);
                        let b = span(b_from, b_to);
                        let interval_overlap = a_from.max(b_from) < a_to.min(b_to);
                        assert_eq!(a.overlaps(&b), interval_overlap);
                        assert_eq!(a.is_available(b.mask()), !interval_overlap);
                    }
                }
            }
        }
    }
}
