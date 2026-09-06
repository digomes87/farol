//! The inverted index: the data structure that makes search sublinear.
//!
//! A forward index answers "which terms does document 7 contain?". Search needs
//! the opposite question — "which documents contain `rust`?" — so the index is
//! inverted: every term points at the sorted list of documents where it occurs.
//!
//! ```text
//! "rust"   -> [ (doc 0, positions [3, 11]), (doc 4, positions [0]) ]
//! "search" -> [ (doc 4, positions [1]) ]
//! ```
//!
//! Positions are kept per posting because phrase queries need adjacency, and
//! document lengths are kept because BM25 penalises long documents.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::analyzer::Analyzer;
use crate::codec;

/// Dense document identifier, assigned in insertion order.
pub type DocId = u32;

/// Everything the engine remembers about one indexed document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: DocId,
    /// Where the document came from: a file path, a URL, a database key.
    pub uri: String,
    /// Human readable title, shown in results.
    pub title: String,
    /// Number of indexed terms — the BM25 document length.
    pub length: u32,
    /// Original text, retained so snippets can quote the source verbatim.
    pub text: String,
}

/// Occurrences of a single term inside a single document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Posting {
    pub doc: DocId,
    /// Token positions, ascending. Its length is the term frequency.
    pub positions: Vec<u32>,
}

impl Posting {
    /// Term frequency: how many times the term occurs in the document.
    pub fn tf(&self) -> u32 {
        self.positions.len() as u32
    }
}

/// Postings per block.
///
/// Small enough that decoding one block to inspect a single document is cheap,
/// large enough that the per-block metadata stays a rounding error next to the
/// data it describes. 128 is what most production engines settle on, for the
/// same reason.
pub const BLOCK_SIZE: usize = 128;

/// What is known about one block without decoding it.
///
/// These four numbers are why blocks exist. `last_doc` lets a search skip an
/// entire block by comparing one integer, and `max_tf`/`min_len` bound the score
/// of everything inside it — a much tighter bound than the term-wide one,
/// because a block covers a narrow slice of the collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMeta {
    /// Highest document id in the block.
    pub last_doc: DocId,
    /// Number of postings in the block.
    pub count: u32,
    /// Where the block starts inside the term's byte buffer.
    pub offset: u32,
    /// Highest term frequency inside the block.
    pub max_tf: u32,
    /// Shortest document inside the block.
    pub min_len: u32,
}

/// A term's compressed posting list, sliced into blocks.
///
/// Postings are staged uncompressed while documents are being added and encoded
/// by [`Index::finish`]. Nothing reads the compressed form until then, which
/// keeps indexing a plain append and confines the encoding to one place.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermIndex {
    blocks: Vec<BlockMeta>,
    data: Vec<u8>,
    doc_freq: u32,
    max_tf: u32,
    min_len: u32,
    /// Postings not yet encoded. Never serialised: an index is always finished
    /// before it is written.
    #[serde(skip)]
    staging: Vec<Posting>,
}

impl TermIndex {
    /// Number of documents containing the term.
    pub fn doc_freq(&self) -> u32 {
        self.doc_freq
    }

    /// Highest term frequency observed for this term.
    pub fn max_tf(&self) -> u32 {
        self.max_tf
    }

    /// Length of the shortest document containing the term.
    pub fn min_len(&self) -> u32 {
        self.min_len
    }

    /// Block index, for cursors that skip without decoding.
    pub fn blocks(&self) -> &[BlockMeta] {
        &self.blocks
    }

    /// The compressed bytes the blocks point into.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Bytes occupied by this term's postings.
    pub fn size_bytes(&self) -> usize {
        self.data.len()
    }

    /// Decodes one block.
    pub fn decode_block(&self, block: usize) -> Vec<Posting> {
        let Some(meta) = self.blocks.get(block) else {
            return Vec::new();
        };
        decode_block(&self.data[meta.offset as usize..], meta.count as usize)
    }

    /// Decodes the whole posting list.
    ///
    /// Convenient for the paths that walk every posting anyway — phrase
    /// matching and exhaustive scoring — and deliberately *not* what dynamic
    /// pruning uses, since the point there is to skip most blocks.
    pub fn decode_all(&self) -> Vec<Posting> {
        let mut out = Vec::with_capacity(self.doc_freq as usize);
        for block in 0..self.blocks.len() {
            out.append(&mut self.decode_block(block));
        }
        out
    }

    /// Appends a posting to the staging area.
    fn stage(&mut self, posting: Posting) {
        self.staging.push(posting);
    }

    /// Sorts, compresses and indexes every staged posting.
    ///
    /// Already encoded blocks are decoded back into the staging area first, so
    /// finishing an index twice — or adding documents to a finished one — stays
    /// correct rather than silently dropping the earlier postings.
    fn compress(&mut self, lengths: &[u32]) {
        if !self.data.is_empty() {
            let mut previous = self.decode_all();
            previous.append(&mut self.staging);
            self.staging = previous;
        }
        let mut postings = std::mem::take(&mut self.staging);
        postings.sort_unstable_by_key(|p| p.doc);

        self.blocks.clear();
        self.data.clear();
        self.doc_freq = postings.len() as u32;
        self.max_tf = 0;
        self.min_len = u32::MAX;

        for chunk in postings.chunks(BLOCK_SIZE) {
            let offset = self.data.len() as u32;
            let max_tf = chunk.iter().map(Posting::tf).max().unwrap_or(0);
            let min_len = chunk
                .iter()
                .filter_map(|p| lengths.get(p.doc as usize).copied())
                .min()
                .unwrap_or(0);

            encode_block(chunk, &mut self.data);
            self.blocks.push(BlockMeta {
                last_doc: chunk.last().map_or(0, |p| p.doc),
                count: chunk.len() as u32,
                offset,
                max_tf,
                min_len,
            });

            self.max_tf = self.max_tf.max(max_tf);
            self.min_len = self.min_len.min(min_len);
        }

        if self.blocks.is_empty() {
            self.min_len = 0;
        }
    }
}

/// Encodes one block: document gaps, then frequencies, then positions.
///
/// The three are kept in separate runs rather than interleaved per posting so
/// each run compresses against its own kind of data — gaps are small and
/// similar, frequencies are tiny, positions restart per document.
fn encode_block(postings: &[Posting], out: &mut Vec<u8>) {
    let docs: Vec<DocId> = postings.iter().map(|p| p.doc).collect();
    codec::encode_sorted(&docs, out);

    let tfs: Vec<u32> = postings.iter().map(Posting::tf).collect();
    codec::encode_plain(&tfs, out);

    for posting in postings {
        codec::encode_sorted(&posting.positions, out);
    }
}

/// Reverses [`encode_block`].
fn decode_block(bytes: &[u8], count: usize) -> Vec<Posting> {
    let (docs, mut consumed) = codec::decode_sorted(bytes, count);
    let (tfs, used) = codec::decode_plain(&bytes[consumed.min(bytes.len())..], docs.len());
    consumed += used;

    let mut out = Vec::with_capacity(docs.len());
    for (doc, tf) in docs.into_iter().zip(tfs) {
        let (positions, used) =
            codec::decode_sorted(&bytes[consumed.min(bytes.len())..], tf as usize);
        consumed += used;
        out.push(Posting { doc, positions });
    }
    out
}

/// An in-memory inverted index.
///
/// # Example
///
/// ```
/// use farol_core::{Analyzer, Index};
///
/// let mut index = Index::new(Analyzer::default());
/// index.add("doc-1", "Ferris the crab", "The crab named Ferris learns Rust");
/// index.finish(); // compresses the postings and computes the score bounds
///
/// assert_eq!(index.len(), 1);
/// assert_eq!(index.doc_freq("rust"), 1);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    postings: HashMap<String, TermIndex>,
    docs: Vec<Document>,
    total_length: u64,
    #[serde(skip)]
    analyzer: Option<Analyzer>,
}

impl Default for Index {
    fn default() -> Self {
        Self::new(Analyzer::default())
    }
}

impl Index {
    /// Creates an empty index that will analyze text with `analyzer`.
    pub fn new(analyzer: Analyzer) -> Self {
        Self {
            postings: HashMap::new(),
            docs: Vec::new(),
            total_length: 0,
            analyzer: Some(analyzer),
        }
    }

    /// The analyzer used for indexing; queries must go through the same one.
    pub fn analyzer(&self) -> &Analyzer {
        self.analyzer.as_ref().expect("analyzer is always set")
    }

    /// Rebinds an analyzer after deserialization, which cannot carry the
    /// stopword configuration (see [`crate::store`]).
    pub(crate) fn set_analyzer(&mut self, analyzer: Analyzer) {
        self.analyzer = Some(analyzer);
    }

    /// Analyzes `text` and adds it as a new document, returning its id.
    pub fn add(&mut self, uri: impl Into<String>, title: impl Into<String>, text: &str) -> DocId {
        let tokens = self.analyzer().analyze(text);
        let id = self.docs.len() as DocId;

        // Group the token positions by term first, so each posting list is
        // touched once per document instead of once per token.
        let mut per_term: HashMap<&str, Vec<u32>> = HashMap::new();
        for token in &tokens {
            per_term
                .entry(&token.term)
                .or_default()
                .push(token.position);
        }
        for (term, positions) in per_term {
            self.postings
                .entry(term.to_string())
                .or_default()
                .stage(Posting { doc: id, positions });
        }

        let length = tokens.len() as u32;
        self.total_length += u64::from(length);
        self.docs.push(Document {
            id,
            uri: uri.into(),
            title: title.into(),
            length,
            text: text.to_string(),
        });
        id
    }

    /// Merges `other` into `self`, renumbering its documents.
    ///
    /// This is what makes parallel indexing possible: each worker builds an
    /// independent shard and the shards are folded together at the end.
    pub fn merge(&mut self, other: Index) {
        let offset = self.docs.len() as DocId;
        for (term, term_index) in other.postings {
            let entry = self.postings.entry(term).or_default();
            // A shard may or may not have been finished before merging, so take
            // both its staged and its encoded postings.
            let mut postings = term_index.decode_all();
            postings.extend(term_index.staging);
            for mut posting in postings {
                posting.doc += offset;
                entry.stage(posting);
            }
        }
        for mut doc in other.docs {
            doc.id += offset;
            self.docs.push(doc);
        }
        self.total_length += other.total_length;
    }

    /// Compresses every posting list and refreshes the score bounds.
    ///
    /// Callers must run this after [`merge`](Index::merge) and before any
    /// search: postings only exist in their compressed, sorted form afterwards,
    /// and both phrase matching and pruning depend on that order.
    pub fn finish(&mut self) {
        let lengths: Vec<u32> = self.docs.iter().map(|d| d.length).collect();
        for term_index in self.postings.values_mut() {
            term_index.compress(&lengths);
        }
    }

    /// Decoded posting list for an already analyzed `term`, or `None` if unseen.
    ///
    /// Returns owned postings because the stored form is compressed. Callers
    /// that only need to skip through the list should use
    /// [`term`](Index::term) and walk the blocks instead.
    pub fn postings(&self, term: &str) -> Option<Vec<Posting>> {
        self.postings.get(term).map(TermIndex::decode_all)
    }

    /// Posting list and score bounds for an already analyzed `term`.
    pub fn term(&self, term: &str) -> Option<&TermIndex> {
        self.postings.get(term)
    }

    /// Number of documents containing `term` — the BM25 document frequency.
    pub fn doc_freq(&self, term: &str) -> u32 {
        self.postings.get(term).map_or(0, TermIndex::doc_freq)
    }

    /// Looks up document metadata by id.
    pub fn document(&self, id: DocId) -> Option<&Document> {
        self.docs.get(id as usize)
    }

    /// All indexed documents, in insertion order.
    pub fn documents(&self) -> &[Document] {
        &self.docs
    }

    /// Number of indexed documents.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Number of distinct terms in the vocabulary.
    pub fn vocabulary_size(&self) -> usize {
        self.postings.len()
    }

    /// Mean document length in terms, used by the BM25 length normalisation.
    pub fn avg_doc_len(&self) -> f32 {
        if self.docs.is_empty() {
            return 0.0;
        }
        self.total_length as f32 / self.docs.len() as f32
    }

    /// Total number of postings, i.e. the size of the index in entries.
    pub fn total_postings(&self) -> usize {
        self.postings.values().map(|t| t.doc_freq() as usize).sum()
    }

    /// Bytes occupied by the compressed posting lists.
    ///
    /// Reported by `farol stats`: it is the number compression is meant to
    /// move, so it should be visible.
    pub fn postings_bytes(&self) -> usize {
        self.postings.values().map(TermIndex::size_bytes).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut index = Index::new(Analyzer::raw());
        index.add("a", "A", "rust is fast rust is safe");
        index.add("b", "B", "search engines rank documents");
        index.finish();
        index
    }

    #[test]
    fn postings_record_every_position() {
        let index = sample();
        let postings = index.postings("rust").unwrap();
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].positions, [0, 3]);
        assert_eq!(postings[0].tf(), 2);
    }

    #[test]
    fn doc_freq_counts_documents_not_occurrences() {
        let index = sample();
        assert_eq!(index.doc_freq("is"), 1);
        assert_eq!(index.doc_freq("rust"), 1);
        assert_eq!(index.doc_freq("missing"), 0);
    }

    #[test]
    fn avg_doc_len_is_the_mean_term_count() {
        let index = sample();
        assert_eq!(index.documents()[0].length, 6);
        assert_eq!(index.documents()[1].length, 4);
        assert_eq!(index.avg_doc_len(), 5.0);
    }

    #[test]
    fn compressed_postings_decode_back_to_what_was_indexed() {
        let mut index = Index::new(Analyzer::raw());
        index.add("a", "A", "rust is fast rust is safe");
        index.add("b", "B", "rust ranks documents");
        index.finish();

        let postings = index.postings("rust").unwrap();
        assert_eq!(postings.len(), 2);
        assert_eq!(postings[0].doc, 0);
        assert_eq!(postings[0].positions, [0, 3]);
        assert_eq!(postings[1].doc, 1);
        assert_eq!(postings[1].positions, [0]);
    }

    #[test]
    fn posting_lists_are_split_into_blocks() {
        let mut index = Index::new(Analyzer::raw());
        for id in 0..(BLOCK_SIZE * 2 + 5) {
            index.add(format!("d{id}"), "D", "rust");
        }
        index.finish();

        let term = index.term("rust").unwrap();
        assert_eq!(term.blocks().len(), 3, "two full blocks and a partial one");
        assert_eq!(term.blocks()[0].count as usize, BLOCK_SIZE);
        assert_eq!(term.blocks()[2].count, 5);
        assert_eq!(
            term.blocks()[0].last_doc,
            BLOCK_SIZE as DocId - 1,
            "last_doc must allow skipping the block without decoding it"
        );
        assert_eq!(term.decode_all().len(), BLOCK_SIZE * 2 + 5);
    }

    #[test]
    fn every_block_carries_its_own_score_bounds() {
        let mut index = Index::new(Analyzer::raw());
        for id in 0..BLOCK_SIZE {
            index.add(format!("d{id}"), "D", "rust padding padding padding");
        }
        // A second block where the term is much stronger: high frequency in a
        // short document.
        index.add("hot", "Hot", "rust rust rust");
        index.finish();

        let term = index.term("rust").unwrap();
        assert_eq!(term.blocks().len(), 2);
        assert!(
            term.blocks()[1].max_tf > term.blocks()[0].max_tf,
            "per-block bounds must be tighter than the term-wide one"
        );
        assert_eq!(term.max_tf(), 3, "term bound is the maximum over blocks");
    }

    #[test]
    fn compression_shrinks_a_dense_posting_list() {
        let mut index = Index::new(Analyzer::raw());
        for id in 0..2_000 {
            index.add(format!("d{id}"), "D", "rust");
        }
        index.finish();

        let uncompressed = 2_000 * (std::mem::size_of::<DocId>() + std::mem::size_of::<u32>() * 2);
        assert!(
            index.postings_bytes() < uncompressed / 3,
            "{} bytes is not much better than {uncompressed} uncompressed",
            index.postings_bytes()
        );
    }

    #[test]
    fn adding_documents_to_a_finished_index_keeps_the_earlier_postings() {
        let mut index = Index::new(Analyzer::raw());
        index.add("a", "A", "rust");
        index.finish();
        index.add("b", "B", "rust");
        index.finish();

        assert_eq!(index.doc_freq("rust"), 2);
        let docs: Vec<_> = index
            .postings("rust")
            .unwrap()
            .iter()
            .map(|p| p.doc)
            .collect();
        assert_eq!(docs, [0, 1]);
    }

    #[test]
    fn finish_records_the_score_bounds_of_every_term() {
        let mut index = Index::new(Analyzer::raw());
        index.add("a", "A", "rust rust rust padding padding padding");
        index.add("b", "B", "rust");
        index.finish();

        let term = index.term("rust").unwrap();
        assert_eq!(term.max_tf(), 3, "highest frequency across postings");
        assert_eq!(term.min_len(), 1, "shortest document containing the term");
    }

    #[test]
    fn score_bounds_are_refreshed_after_a_merge() {
        let mut left = Index::new(Analyzer::raw());
        left.add("a", "A", "rust padding padding");
        left.finish();
        assert_eq!(left.term("rust").unwrap().max_tf(), 1);

        let mut right = Index::new(Analyzer::raw());
        right.add("b", "B", "rust rust");
        left.merge(right);
        left.finish();

        assert_eq!(left.term("rust").unwrap().max_tf(), 2);
        assert_eq!(left.term("rust").unwrap().min_len(), 2);
    }

    #[test]
    fn merge_renumbers_documents_and_keeps_postings_aligned() {
        let mut left = Index::new(Analyzer::raw());
        left.add("a", "A", "rust");
        let mut right = Index::new(Analyzer::raw());
        right.add("b", "B", "rust");

        left.merge(right);
        left.finish();

        assert_eq!(left.len(), 2);
        assert_eq!(left.document(1).unwrap().uri, "b");
        let docs: Vec<_> = left
            .postings("rust")
            .unwrap()
            .iter()
            .map(|p| p.doc)
            .collect();
        assert_eq!(docs, [0, 1]);
    }

    #[test]
    fn finish_sorts_posting_lists_by_doc_id() {
        let mut index = Index::new(Analyzer::raw());
        index.add("a", "A", "x");
        let mut other = Index::new(Analyzer::raw());
        other.add("b", "B", "x");
        // Merge in reverse: the shard's document lands before the local one.
        let mut merged = other;
        merged.merge(index);
        merged.finish();
        let docs: Vec<_> = merged
            .postings("x")
            .unwrap()
            .iter()
            .map(|p| p.doc)
            .collect();
        assert!(docs.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn empty_index_reports_zero_average_length() {
        let index = Index::new(Analyzer::raw());
        assert!(index.is_empty());
        assert_eq!(index.avg_doc_len(), 0.0);
    }
}
