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

/// A term's posting list plus the statistics needed to bound its score.
///
/// `max_tf` and `min_len` are the extremes observed across the postings. They
/// give an upper bound on what this term can contribute to *any* document:
/// BM25 grows with term frequency and shrinks with document length, so the best
/// case is the highest frequency in the shortest document. The bound holds for
/// every `k1`/`b`, which is why it can be computed once at index time and reused
/// by queries that tune the ranking at runtime.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TermIndex {
    postings: Vec<Posting>,
    max_tf: u32,
    min_len: u32,
}

impl TermIndex {
    pub fn postings(&self) -> &[Posting] {
        &self.postings
    }

    /// Number of documents containing the term.
    pub fn doc_freq(&self) -> u32 {
        self.postings.len() as u32
    }

    /// Highest term frequency observed for this term.
    pub fn max_tf(&self) -> u32 {
        self.max_tf
    }

    /// Length of the shortest document containing the term.
    pub fn min_len(&self) -> u32 {
        self.min_len
    }
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
                .postings
                .push(Posting { doc: id, positions });
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
            entry
                .postings
                .extend(term_index.postings.into_iter().map(|mut p| {
                    p.doc += offset;
                    p
                }));
        }
        for mut doc in other.docs {
            doc.id += offset;
            self.docs.push(doc);
        }
        self.total_length += other.total_length;
    }

    /// Sorts every posting list and refreshes the per-term score bounds.
    ///
    /// Callers must run this after [`merge`](Index::merge): intersection and
    /// phrase matching assume ascending document ids, and dynamic pruning
    /// assumes the bounds describe the postings as they currently are.
    pub fn finish(&mut self) {
        for term_index in self.postings.values_mut() {
            term_index.postings.sort_unstable_by_key(|p| p.doc);
            term_index.max_tf = term_index
                .postings
                .iter()
                .map(Posting::tf)
                .max()
                .unwrap_or(0);
            term_index.min_len = term_index
                .postings
                .iter()
                .filter_map(|p| self.docs.get(p.doc as usize))
                .map(|doc| doc.length)
                .min()
                .unwrap_or(0);
        }
    }

    /// Posting list for an already analyzed `term`, or `None` if unseen.
    pub fn postings(&self, term: &str) -> Option<&[Posting]> {
        self.postings.get(term).map(TermIndex::postings)
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
        self.postings.values().map(|t| t.postings.len()).sum()
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
