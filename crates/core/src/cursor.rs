//! Block cursors: the iteration primitive dynamic pruning is built on.
//!
//! An ordinary iterator can only say "give me the next posting". Pruning needs
//! a stronger operation — *"skip ahead to document 900"* — because its whole
//! point is to never look at what lies in between. And since posting lists are
//! stored compressed, "never look at" should mean literally that: a skip walks
//! the block index, comparing one integer per block, and decodes only the block
//! it finally lands in.
//!
//! ```text
//! blocks:   [ …last_doc 120 ] [ …last_doc 480 ] [ …last_doc 900 ]
//! skip_to(700):   skipped         skipped         decoded, searched
//! ```
//!
//! Inside the landing block the search is a galloping (exponential) one rather
//! than a plain binary search: when the target is a few postings away galloping
//! finds it in a couple of comparisons, and when it is far it degrades to a
//! binary search over the range it has bracketed. A query mixes rare terms
//! (long skips) with frequent ones (short skips), and galloping is good at both.

use crate::bm25::Bm25;
use crate::index::{DocId, Posting, TermRef};

/// A position inside one term's compressed posting list.
#[derive(Debug, Clone)]
pub struct BlockCursor<'a> {
    term: TermRef<'a>,
    /// Index of the block currently decoded.
    block: usize,
    /// Postings of that block. Only one block is ever decoded at a time.
    decoded: Vec<Posting>,
    /// Position inside `decoded`.
    at: usize,
    idf: f32,
    bm25: Bm25,
    avg_doc_len: f32,
}

impl<'a> BlockCursor<'a> {
    /// Creates a cursor on the first posting of `term`.
    pub fn new(term: TermRef<'a>, idf: f32, bm25: Bm25, avg_doc_len: f32) -> Self {
        let mut cursor = Self {
            term,
            block: 0,
            decoded: Vec::new(),
            at: 0,
            idf,
            bm25,
            avg_doc_len,
        };
        cursor.load_block(0);
        cursor
    }

    fn load_block(&mut self, block: usize) {
        self.block = block;
        self.at = 0;
        self.decoded = if block < self.term.blocks().len() {
            self.term.decode_block(block)
        } else {
            Vec::new()
        };
    }

    /// Document the cursor points at, or `None` once exhausted.
    pub fn doc(&self) -> Option<DocId> {
        self.decoded.get(self.at).map(|p| p.doc)
    }

    /// Term frequency in the current document.
    pub fn tf(&self) -> u32 {
        self.decoded.get(self.at).map_or(0, Posting::tf)
    }

    pub fn idf(&self) -> f32 {
        self.idf
    }

    pub fn is_exhausted(&self) -> bool {
        self.doc().is_none()
    }

    /// Highest score this term can contribute to *any* document.
    pub fn term_upper_bound(&self) -> f32 {
        self.bm25.score(
            self.term.max_tf(),
            self.term.min_len(),
            self.avg_doc_len,
            self.idf,
        )
    }

    /// Highest score this term can contribute to a document *in the current
    /// block*.
    ///
    /// Tighter than the term-wide bound, because a block covers a narrow slice
    /// of the collection — which is exactly what makes block-max pruning skip
    /// more than plain WAND.
    pub fn block_upper_bound(&self) -> f32 {
        self.term.blocks().get(self.block).map_or(0.0, |meta| {
            self.bm25
                .score(meta.max_tf, meta.min_len, self.avg_doc_len, self.idf)
        })
    }

    /// First block at or after the current one that can contain `doc`.
    ///
    /// Walks block metadata only — no decoding — which is what lets the pruning
    /// loop ask about a document the cursor has not reached yet.
    fn block_covering(&self, doc: DocId) -> Option<usize> {
        let blocks = self.term.blocks();
        (self.block..blocks.len()).find(|&idx| blocks[idx].last_doc >= doc)
    }

    /// Bound on this term's contribution to `doc`, taken from the block that
    /// would contain it.
    ///
    /// Summing the *current* block's bound instead would be wrong whenever a
    /// cursor still sits behind `doc`: the document lives in a later block,
    /// whose bound may be higher, and the pruning decision would rest on an
    /// underestimate.
    pub fn block_upper_bound_at(&self, doc: DocId) -> f32 {
        match self.block_covering(doc) {
            Some(idx) => {
                let meta = &self.term.blocks()[idx];
                self.bm25
                    .score(meta.max_tf, meta.min_len, self.avg_doc_len, self.idf)
            }
            // The term has nothing left at or after `doc`.
            None => 0.0,
        }
    }

    /// Last document of the block that would contain `doc`.
    pub fn block_last_doc_at(&self, doc: DocId) -> Option<DocId> {
        self.block_covering(doc)
            .map(|idx| self.term.blocks()[idx].last_doc)
    }

    /// Index of the block the cursor is currently in.
    pub fn block_index(&self) -> usize {
        self.block
    }

    /// Last document id of the current block, for deciding where to skip to.
    pub fn block_last_doc(&self) -> Option<DocId> {
        self.term.blocks().get(self.block).map(|meta| meta.last_doc)
    }

    /// Postings left, including the current one. Used to report pruning.
    pub fn remaining(&self) -> usize {
        let in_block = self.decoded.len().saturating_sub(self.at);
        let later: usize = self
            .term
            .blocks()
            .iter()
            .skip(self.block + 1)
            .map(|meta| meta.count as usize)
            .sum();
        in_block + later
    }

    /// Moves to the first document `>= target`, returning it.
    ///
    /// Never moves backwards, so calling it with a target already behind the
    /// cursor is a no-op — the pruning loop relies on that to push several
    /// cursors at the same pivot without tracking which are already there.
    pub fn advance_to(&mut self, target: DocId) -> Option<DocId> {
        if self.doc().is_some_and(|doc| doc >= target) {
            return self.doc();
        }

        // Walk the block index. Nothing here is decoded: one integer per block.
        if self.block_last_doc().is_some_and(|last| last < target) {
            let mut block = self.block + 1;
            let blocks = self.term.blocks();
            while block < blocks.len() && blocks[block].last_doc < target {
                block += 1;
            }
            if block >= blocks.len() {
                self.load_block(blocks.len());
                return None;
            }
            self.load_block(block);
        }

        self.at = gallop(&self.decoded, self.at, target);
        if self.at >= self.decoded.len() {
            let next = self.block + 1;
            self.load_block(next);
        }
        self.doc()
    }

    /// Moves one posting forward, crossing into the next block if needed.
    pub fn advance(&mut self) -> Option<DocId> {
        self.at += 1;
        if self.at >= self.decoded.len() {
            let next = self.block + 1;
            self.load_block(next);
        }
        self.doc()
    }
}

/// Index of the first posting `>= target` at or after `from`, found by
/// exponentially widening a window and then bisecting it.
fn gallop(postings: &[Posting], from: usize, target: DocId) -> usize {
    if from >= postings.len() {
        return postings.len();
    }
    let mut step = 1;
    let mut low = from;
    while low + step < postings.len() && postings[low + step].doc < target {
        low += step;
        step *= 2;
    }
    let high = (low + step).min(postings.len());

    low + postings[low..high]
        .binary_search_by(|p| {
            if p.doc < target {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        })
        .unwrap_or_else(|insertion| insertion)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::Analyzer;
    use crate::index::{Index, BLOCK_SIZE};

    /// An index where `rust` appears in every `step`-th document.
    fn index_with(docs: usize, step: usize) -> Index {
        let mut index = Index::new(Analyzer::raw());
        for id in 0..docs {
            let text = if id % step == 0 { "rust" } else { "other" };
            index.add(format!("d{id}"), "D", text);
        }
        index.seal()
    }

    fn cursor(index: &Index) -> BlockCursor<'_> {
        BlockCursor::new(index.term("rust").unwrap(), 1.0, Bm25::default(), 1.0)
    }

    #[test]
    fn a_new_cursor_points_at_the_first_document() {
        let index = index_with(10, 3);
        let cursor = cursor(&index);
        assert_eq!(cursor.doc(), Some(0));
        assert_eq!(cursor.tf(), 1);
    }

    #[test]
    fn advance_walks_every_posting_across_block_boundaries() {
        let docs = BLOCK_SIZE * 2 + 7;
        let index = index_with(docs, 1);
        let mut cursor = cursor(&index);

        let mut seen = vec![cursor.doc().unwrap()];
        while let Some(doc) = cursor.advance() {
            seen.push(doc);
        }
        assert_eq!(seen.len(), docs);
        assert_eq!(seen.last(), Some(&(docs as DocId - 1)));
    }

    #[test]
    fn advance_to_matches_a_linear_scan_on_every_target() {
        // The skip is an optimisation, not a different answer: check it against
        // the obvious implementation, across and inside blocks.
        let index = index_with(BLOCK_SIZE * 3, 2);
        let all: Vec<DocId> = index
            .postings("rust")
            .unwrap()
            .iter()
            .map(|p| p.doc)
            .collect();

        for target in 0..(BLOCK_SIZE as DocId * 3 + 10) {
            let mut cursor = cursor(&index);
            let expected = all.iter().copied().find(|&doc| doc >= target);
            assert_eq!(cursor.advance_to(target), expected, "target {target}");
        }
    }

    #[test]
    fn advance_to_never_moves_backwards() {
        let index = index_with(BLOCK_SIZE * 2, 1);
        let mut cursor = cursor(&index);
        cursor.advance_to(200);
        assert_eq!(cursor.advance_to(1), Some(200), "cursor moved back");
    }

    #[test]
    fn skipping_past_the_end_exhausts_the_cursor() {
        let index = index_with(10, 1);
        let mut cursor = cursor(&index);
        assert_eq!(cursor.advance_to(1_000), None);
        assert!(cursor.is_exhausted());
        assert_eq!(cursor.tf(), 0);
        assert_eq!(cursor.remaining(), 0);
    }

    #[test]
    fn a_long_skip_decodes_only_the_landing_block() {
        let index = index_with(BLOCK_SIZE * 4, 1);
        let mut cursor = cursor(&index);

        let target = BLOCK_SIZE as DocId * 3 + 5;
        cursor.advance_to(target);

        assert_eq!(cursor.doc(), Some(target));
        assert_eq!(cursor.block, 3, "landed directly on the fourth block");
        assert_eq!(
            cursor.decoded.len(),
            BLOCK_SIZE,
            "only the landing block was decoded"
        );
    }

    #[test]
    fn remaining_counts_the_postings_still_ahead() {
        let index = index_with(BLOCK_SIZE * 2, 1);
        let mut cursor = cursor(&index);
        assert_eq!(cursor.remaining(), BLOCK_SIZE * 2);
        cursor.advance_to(BLOCK_SIZE as DocId);
        assert_eq!(cursor.remaining(), BLOCK_SIZE);
    }

    #[test]
    fn block_covering_finds_the_block_a_document_lives_in() {
        let index = index_with(BLOCK_SIZE * 3, 1);
        let cursor = cursor(&index);

        // Documents map to blocks of BLOCK_SIZE, so the arithmetic is known
        // independently of the implementation.
        for doc in [0, 5, BLOCK_SIZE - 1, BLOCK_SIZE, BLOCK_SIZE * 2 + 3] {
            let expected = doc / BLOCK_SIZE;
            assert_eq!(
                cursor.block_covering(doc as DocId),
                Some(expected),
                "document {doc} should be in block {expected}"
            );
            assert_eq!(
                cursor.block_last_doc_at(doc as DocId),
                Some((expected * BLOCK_SIZE + BLOCK_SIZE - 1) as DocId),
                "wrong block end for document {doc}"
            );
        }

        // Past the last posting there is no covering block at all.
        assert_eq!(cursor.block_covering((BLOCK_SIZE * 3) as DocId), None);
        assert_eq!(cursor.block_last_doc_at((BLOCK_SIZE * 3) as DocId), None);
    }

    #[test]
    fn block_covering_never_looks_behind_the_cursor() {
        let index = index_with(BLOCK_SIZE * 3, 1);
        let mut cursor = cursor(&index);
        cursor.advance_to((BLOCK_SIZE * 2) as DocId);

        // The cursor is in block 2; a document from block 0 is behind it and
        // has no covering block ahead.
        assert_eq!(cursor.block_covering(0), Some(2));
        assert_eq!(cursor.block_index(), 2);
    }

    #[test]
    fn a_block_bound_is_never_looser_than_the_term_bound() {
        let mut index = Index::new(Analyzer::raw());
        for id in 0..BLOCK_SIZE {
            index.add(format!("d{id}"), "D", "rust padding padding padding");
        }
        index.add("hot", "Hot", "rust rust rust");
        let index = index.seal();

        let mut cursor = cursor(&index);
        let term_bound = cursor.term_upper_bound();
        assert!(cursor.block_upper_bound() <= term_bound);

        // The second block holds the document where the term is strongest, so
        // its bound should be the one that matches the term-wide bound.
        cursor.advance_to(BLOCK_SIZE as DocId);
        assert!(cursor.block_upper_bound() > 0.0);
        assert!(cursor.block_upper_bound() <= term_bound);
    }
}
