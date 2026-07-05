//! Property tests for the span/mask and pool occupancy algebra — the
//! arithmetic the zero-double-sell guarantee rests on.

use lulan_engine::domain::{PoolOccupancy, SegmentSpan};
use proptest::prelude::*;

/// Strategy: a valid span on a trip with `segments` segments.
fn span_strategy(segments: u8) -> impl Strategy<Value = SegmentSpan> {
    (0..segments).prop_flat_map(move |from| {
        ((from + 1)..=segments).prop_map(move |to| SegmentSpan::new(from, to).unwrap())
    })
}

proptest! {
    /// Greedily reserving random spans on one seat via bitmasks must accept
    /// exactly the spans an interval model would accept, and the final mask
    /// must equal the union of accepted spans.
    #[test]
    fn seat_mask_reservation_matches_interval_model(
        spans in proptest::collection::vec(span_strategy(64), 1..40)
    ) {
        let mut occupied: u64 = 0;
        let mut accepted: Vec<SegmentSpan> = Vec::new();

        for span in &spans {
            let mask_says_free = span.is_available(occupied);
            let interval_says_free = accepted.iter().all(|a| {
                a.from_index().max(span.from_index()) >= a.to_index().min(span.to_index())
            });
            prop_assert_eq!(mask_says_free, interval_says_free);

            if mask_says_free {
                occupied |= span.mask();
                accepted.push(*span);
            }
        }

        let union = accepted.iter().fold(0u64, |m, s| m | s.mask());
        prop_assert_eq!(occupied, union);
    }

    /// Pool reserve/release must agree with a naive per-segment counter
    /// simulation, and never drive any counter negative or above capacity.
    #[test]
    fn pool_matches_naive_per_segment_model(
        capacity in 1i32..50,
        segments in 1u8..16,
        ops in proptest::collection::vec((any::<bool>(), 0u8..16, 1u8..17, 1i32..10), 1..60)
    ) {
        let mut pool = PoolOccupancy::new(capacity, segments);
        let mut model = vec![capacity; segments as usize];

        for (is_reserve, from, to, qty) in ops {
            let (from, to) = (from % segments, (to % segments) + 1);
            if from >= to {
                continue;
            }
            let span = SegmentSpan::new(from, to).unwrap();
            let range = from as usize..to as usize;

            if is_reserve {
                let model_ok = model[range.clone()].iter().all(|r| *r >= qty);
                let result = pool.reserve(span, qty);
                prop_assert_eq!(result.is_ok(), model_ok);
                if model_ok {
                    model[range].iter_mut().for_each(|r| *r -= qty);
                }
            } else {
                let model_ok = model[range.clone()].iter().all(|r| r + qty <= capacity);
                let result = pool.release(span, qty);
                prop_assert_eq!(result.is_ok(), model_ok);
                if model_ok {
                    model[range].iter_mut().for_each(|r| *r += qty);
                }
            }

            prop_assert_eq!(pool.remaining(), model.as_slice());
            prop_assert!(pool.remaining().iter().all(|r| (0..=capacity).contains(r)));
        }
    }

    /// A successful reserve followed by the same release is a no-op.
    #[test]
    fn pool_reserve_release_roundtrip(
        capacity in 1i32..50,
        segments in 1u8..16,
        qty in 1i32..10,
    ) {
        let mut pool = PoolOccupancy::new(capacity, segments);
        let span = SegmentSpan::new(0, segments).unwrap();
        let fresh = pool.clone();
        if pool.reserve(span, qty).is_ok() {
            pool.release(span, qty).unwrap();
            prop_assert_eq!(pool, fresh);
        }
    }
}
