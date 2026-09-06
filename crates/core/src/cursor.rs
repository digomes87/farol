//! Posting list cursors: the iteration primitive dynamic pruning is built on.
//!
//! An ordinary iterator can only say "give me the next posting". Pruning needs
//! a stronger operation — *"skip ahead to document 900, and tell me if it is
//! there"* — because its whole point is to never look at the documents in
//! between.
//!
//! [`PostingCursor`] provides exactly that, and does the skip with a galloping
//! (exponential) search rather than a plain binary search over the remaining
//! postings. When the target is a few entries away, galloping finds it in a
//! couple of comparisons; when it is far away, it degrades to a binary search
//! over the range it has bracketed. A linear scan would be fast in the first
//! case and terrible in the second; a binary search over the whole tail is the
//! opposite. Galloping is good at both, which matters because a query mixes
//! rare terms (long skips) and frequent ones (short skips).

use crate::index::{DocId, Posting};

/// A position inside one term's posting list.
#[derive(Debug, Clone)]
pub struct PostingCursor<'a> {
    postings: &'a [Posting],
    at: usize,
    idf: f32,
    /// Highest score this term can contribute to any document.
    upper_bound: f32,
}

impl<'a> PostingCursor<'a> {
    /// Creates a cursor over `postings`, positioned at the first document.
    pub fn new(postings: &'a [Posting], idf: f32, upper_bound: f32) -> Self {
        Self {
            postings,
            at: 0,
            idf,
            upper_bound,
        }
    }

    /// Document the cursor currently points at, or `None` once exhausted.
    pub fn doc(&self) -> Option<DocId> {
        self.postings.get(self.at).map(|p| p.doc)
    }

    /// Term frequency in the current document.
    pub fn tf(&self) -> u32 {
        self.postings.get(self.at).map_or(0, Posting::tf)
    }

    pub fn idf(&self) -> f32 {
        self.idf
    }

    /// Upper bound on this term's contribution, used to decide whether a
    /// document is worth scoring at all.
    pub fn upper_bound(&self) -> f32 {
        self.upper_bound
    }

    pub fn is_exhausted(&self) -> bool {
        self.at >= self.postings.len()
    }

    /// Number of postings left, including the current one.
    pub fn remaining(&self) -> usize {
        self.postings.len().saturating_sub(self.at)
    }

    /// Moves to the first document `>= target`, and returns it.
    ///
    /// Never moves backwards, so calling it with a target already behind the
    /// cursor is a no-op — the pruning loop relies on that to advance several
    /// cursors to the same pivot without tracking which ones are already there.
    pub fn advance_to(&mut self, target: DocId) -> Option<DocId> {
        if self.is_exhausted() {
            return None;
        }
        if self.postings[self.at].doc >= target {
            return Some(self.postings[self.at].doc);
        }

        // Gallop: double the step until the target is bracketed.
        let mut step = 1;
        let mut low = self.at;
        while low + step < self.postings.len() && self.postings[low + step].doc < target {
            low += step;
            step *= 2;
        }
        let high = (low + step).min(self.postings.len());

        // Then binary search inside the bracket.
        self.at = low
            + self.postings[low..high]
                .binary_search_by(|p| {
                    if p.doc < target {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    }
                })
                .unwrap_or_else(|insertion| insertion);
        self.doc()
    }

    /// Moves one posting forward.
    pub fn advance(&mut self) -> Option<DocId> {
        self.at += 1;
        self.doc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn postings(docs: &[DocId]) -> Vec<Posting> {
        docs.iter()
            .map(|&doc| Posting {
                doc,
                positions: vec![0],
            })
            .collect()
    }

    #[test]
    fn a_new_cursor_points_at_the_first_document() {
        let postings = postings(&[3, 9, 20]);
        let cursor = PostingCursor::new(&postings, 1.0, 2.0);
        assert_eq!(cursor.doc(), Some(3));
        assert_eq!(cursor.remaining(), 3);
    }

    #[test]
    fn advance_to_lands_on_the_first_document_at_or_after_the_target() {
        let postings = postings(&[3, 9, 20, 51]);
        let mut cursor = PostingCursor::new(&postings, 1.0, 2.0);
        assert_eq!(cursor.advance_to(10), Some(20));
        assert_eq!(cursor.advance_to(51), Some(51));
    }

    #[test]
    fn advance_to_never_moves_backwards() {
        let postings = postings(&[3, 9, 20]);
        let mut cursor = PostingCursor::new(&postings, 1.0, 2.0);
        cursor.advance_to(20);
        assert_eq!(cursor.advance_to(1), Some(20), "cursor moved back");
    }

    #[test]
    fn advancing_past_the_end_exhausts_the_cursor() {
        let postings = postings(&[3, 9]);
        let mut cursor = PostingCursor::new(&postings, 1.0, 2.0);
        assert_eq!(cursor.advance_to(100), None);
        assert!(cursor.is_exhausted());
        assert_eq!(cursor.tf(), 0);
    }

    #[test]
    fn galloping_matches_a_linear_scan_on_every_target() {
        // The whole point of the gallop is to be an optimisation, not a
        // different answer: check it against the obvious implementation.
        let docs: Vec<DocId> = (0..500).map(|i| i * 3).collect();
        let postings = postings(&docs);

        for target in 0..1_500 {
            let mut cursor = PostingCursor::new(&postings, 1.0, 1.0);
            let expected = docs.iter().copied().find(|&doc| doc >= target);
            assert_eq!(cursor.advance_to(target), expected, "target {target}");
        }
    }

    #[test]
    fn repeated_advances_walk_the_whole_list() {
        let postings = postings(&[1, 2, 3]);
        let mut cursor = PostingCursor::new(&postings, 1.0, 1.0);
        let mut seen = vec![cursor.doc().unwrap()];
        while let Some(doc) = cursor.advance() {
            seen.push(doc);
        }
        assert_eq!(seen, [1, 2, 3]);
    }
}
