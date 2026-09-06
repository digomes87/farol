//! BM25 ranking — deciding which of the matching documents is the best answer.
//!
//! Matching is a yes/no question; ranking is the hard part. BM25 scores a
//! `(term, document)` pair with three signals:
//!
//! * **term frequency** — more occurrences is better, but with diminishing
//!   returns. Ten mentions of `rust` do not make a document ten times more
//!   relevant, so `tf` is passed through a saturating curve controlled by `k1`.
//! * **inverse document frequency** — a term appearing in every document says
//!   nothing; a rare term says a lot. `idf` weights rarity.
//! * **document length** — a match inside a tweet is stronger evidence than a
//!   match inside a book. `b` controls how hard long documents are penalised.
//!
//! The score of a document for a multi-term query is the sum of its per-term
//! scores.

use serde::{Deserialize, Serialize};

/// BM25 free parameters.
///
/// The defaults (`k1 = 1.2`, `b = 0.75`) are the values used by the TREC
/// experiments the model comes from and are a sane starting point for prose.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bm25 {
    /// Term frequency saturation. `0.0` makes term frequency binary; larger
    /// values let repetition keep mattering for longer.
    pub k1: f32,
    /// Length normalisation, in `0.0..=1.0`. `0.0` ignores document length,
    /// `1.0` normalises fully by the ratio to the average length.
    pub b: f32,
}

impl Default for Bm25 {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Bm25 {
    pub fn new(k1: f32, b: f32) -> Self {
        Self {
            k1: k1.max(0.0),
            b: b.clamp(0.0, 1.0),
        }
    }

    /// Inverse document frequency, in the smoothed probabilistic form
    ///
    /// ```text
    /// idf = ln(1 + (N - df + 0.5) / (df + 0.5))
    /// ```
    ///
    /// The `+1` inside the logarithm is what keeps the result non-negative: the
    /// raw probabilistic formula goes negative for terms present in more than
    /// half of the collection, which would let a common term *subtract* from a
    /// document's score.
    pub fn idf(&self, doc_freq: u32, total_docs: usize) -> f32 {
        let n = total_docs as f32;
        let df = doc_freq as f32;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// Score contributed by one term occurring `tf` times in a document of
    /// `doc_len` terms.
    ///
    /// ```text
    ///                    tf * (k1 + 1)
    /// score = idf * ---------------------------------
    ///                tf + k1 * (1 - b + b * len/avg)
    /// ```
    pub fn score(&self, tf: u32, doc_len: u32, avg_doc_len: f32, idf: f32) -> f32 {
        if tf == 0 || avg_doc_len <= 0.0 {
            return 0.0;
        }
        let tf = tf as f32;
        let norm = 1.0 - self.b + self.b * (doc_len as f32 / avg_doc_len);
        idf * (tf * (self.k1 + 1.0)) / (tf + self.k1 * norm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AVG: f32 = 100.0;

    #[test]
    fn idf_falls_as_the_term_becomes_common() {
        let bm25 = Bm25::default();
        let rare = bm25.idf(1, 1_000);
        let frequent = bm25.idf(500, 1_000);
        assert!(rare > frequent, "{rare} should beat {frequent}");
    }

    #[test]
    fn idf_never_goes_negative_for_ubiquitous_terms() {
        let bm25 = Bm25::default();
        assert!(bm25.idf(1_000, 1_000) >= 0.0);
    }

    #[test]
    fn term_frequency_saturates() {
        let bm25 = Bm25::default();
        let idf = 1.0;
        let one = bm25.score(1, 100, AVG, idf);
        let two = bm25.score(2, 100, AVG, idf);
        let ten = bm25.score(10, 100, AVG, idf);

        assert!(two > one);
        assert!(ten > two);
        // Doubling the occurrences must not double the score.
        assert!(two < 2.0 * one, "tf grew linearly: {one} -> {two}");
        // And the curve is bounded by idf * (k1 + 1).
        assert!(ten < idf * (bm25.k1 + 1.0));
    }

    #[test]
    fn short_documents_outrank_long_ones_for_the_same_match() {
        let bm25 = Bm25::default();
        let short = bm25.score(3, 20, AVG, 1.0);
        let long = bm25.score(3, 500, AVG, 1.0);
        assert!(short > long);
    }

    #[test]
    fn b_zero_disables_length_normalisation() {
        let bm25 = Bm25::new(1.2, 0.0);
        let short = bm25.score(3, 20, AVG, 1.0);
        let long = bm25.score(3, 500, AVG, 1.0);
        assert_eq!(short, long);
    }

    #[test]
    fn k1_zero_makes_term_frequency_binary() {
        let bm25 = Bm25::new(0.0, 0.75);
        let once = bm25.score(1, 100, AVG, 2.0);
        let many = bm25.score(50, 100, AVG, 2.0);
        assert_eq!(once, many);
        assert_eq!(once, 2.0);
    }

    #[test]
    fn absent_terms_and_empty_indexes_score_zero() {
        let bm25 = Bm25::default();
        assert_eq!(bm25.score(0, 100, AVG, 3.0), 0.0);
        assert_eq!(bm25.score(5, 100, 0.0, 3.0), 0.0);
    }

    #[test]
    fn parameters_are_clamped_to_a_valid_range() {
        let bm25 = Bm25::new(-1.0, 5.0);
        assert_eq!(bm25.k1, 0.0);
        assert_eq!(bm25.b, 1.0);
    }
}
