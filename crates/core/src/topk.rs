//! A bounded top-k collector.
//!
//! Ranking does not need the documents sorted — it needs the best `k` of them.
//! Sorting a million scored documents to show ten is wasted work, so hits go
//! through a min-heap of capacity `k`: the cheapest thing to look at is always
//! the *worst* result currently held, which is exactly the value a new
//! candidate must beat.
//!
//! That worst score is also what dynamic pruning uses as its threshold, which
//! is why [`TopK::threshold`] is part of the public surface.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::index::DocId;
use crate::searcher::Hit;

/// A scored document ordered so that `BinaryHeap` yields the *worst* one first.
///
/// `f32` is only `PartialOrd` — NaN has no place in an ordering — so the
/// comparison is defined explicitly, treating a NaN score as the worst possible
/// and breaking ties by document id.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    doc: DocId,
    score: f32,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed on score so the heap root is the lowest scoring candidate.
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or_else(|| match (self.score.is_nan(), other.score.is_nan()) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => Ordering::Equal,
            })
            // On a tie the *larger* document id sorts first, so it is the one
            // evicted: ties resolve towards the lower id, matching the order a
            // full sort produces.
            .then(self.doc.cmp(&other.doc))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Keeps the best `k` documents seen so far.
#[derive(Debug)]
pub struct TopK {
    heap: BinaryHeap<Candidate>,
    k: usize,
}

impl TopK {
    pub fn new(k: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(k.saturating_add(1)),
            k,
        }
    }

    /// Score a new candidate must beat to enter the result set.
    ///
    /// Zero until the collector is full: before that, every scoring document is
    /// worth keeping.
    pub fn threshold(&self) -> f32 {
        if self.heap.len() < self.k {
            0.0
        } else {
            self.heap.peek().map_or(0.0, |worst| worst.score)
        }
    }

    /// Returns whether a document scoring `score` could enter the result set.
    pub fn would_accept(&self, score: f32) -> bool {
        self.heap.len() < self.k || score > self.threshold()
    }

    /// Offers a scored document, keeping it only if it belongs in the top `k`.
    pub fn offer(&mut self, doc: DocId, score: f32) {
        if self.k == 0 {
            return;
        }
        let candidate = Candidate { doc, score };
        if self.heap.len() < self.k {
            self.heap.push(candidate);
            return;
        }
        // `Less` means "better" under this reversed ordering. Equality keeps
        // the incumbent, so ties resolve towards the document seen first —
        // which, since documents arrive in ascending id order, is the lower id.
        if let Some(worst) = self.heap.peek() {
            if candidate.cmp(worst) == Ordering::Less {
                self.heap.pop();
                self.heap.push(candidate);
            }
        }
    }

    /// Consumes the collector, returning the hits best first.
    pub fn into_sorted(self) -> Vec<Hit> {
        let mut hits: Vec<Hit> = self
            .heap
            .into_iter()
            .map(|c| Hit {
                doc: c.doc,
                score: c.score,
            })
            .collect();
        hits.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then(a.doc.cmp(&b.doc))
        });
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(k: usize, scored: &[(DocId, f32)]) -> Vec<(DocId, f32)> {
        let mut topk = TopK::new(k);
        for &(doc, score) in scored {
            topk.offer(doc, score);
        }
        topk.into_sorted()
            .into_iter()
            .map(|hit| (hit.doc, hit.score))
            .collect()
    }

    #[test]
    fn keeps_only_the_best_k() {
        let got = collect(2, &[(0, 1.0), (1, 5.0), (2, 3.0), (3, 0.5)]);
        assert_eq!(got, [(1, 5.0), (2, 3.0)]);
    }

    #[test]
    fn ties_resolve_towards_the_lower_document_id() {
        let got = collect(2, &[(7, 2.0), (1, 2.0), (9, 2.0)]);
        assert_eq!(got, [(1, 2.0), (7, 2.0)]);
    }

    #[test]
    fn the_threshold_is_zero_until_it_fills_up() {
        let mut topk = TopK::new(3);
        topk.offer(0, 4.0);
        assert_eq!(topk.threshold(), 0.0);
        topk.offer(1, 2.0);
        topk.offer(2, 9.0);
        assert_eq!(topk.threshold(), 2.0, "worst of the three");
    }

    #[test]
    fn the_threshold_rises_as_better_documents_arrive() {
        let mut topk = TopK::new(2);
        topk.offer(0, 1.0);
        topk.offer(1, 2.0);
        assert_eq!(topk.threshold(), 1.0);
        topk.offer(2, 5.0);
        assert_eq!(topk.threshold(), 2.0);
    }

    #[test]
    fn would_accept_agrees_with_offer() {
        let mut topk = TopK::new(1);
        topk.offer(0, 3.0);
        assert!(
            !topk.would_accept(3.0),
            "a tie does not displace the incumbent"
        );
        assert!(topk.would_accept(3.5));
    }

    #[test]
    fn a_zero_sized_collector_keeps_nothing() {
        assert!(collect(0, &[(0, 1.0)]).is_empty());
    }

    #[test]
    fn fewer_documents_than_k_is_fine() {
        assert_eq!(collect(10, &[(0, 1.0)]), [(0, 1.0)]);
    }
}
