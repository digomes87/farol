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

    /// The formula under test, written out independently of the implementation.
    ///
    /// Asserting only that IDF falls as a term gets commoner leaves most of the
    /// expression free to be wrong — mutation testing showed that swapping the
    /// operators inside it kept every directional test passing. Pinning the
    /// value closes that.
    fn expected_idf(doc_freq: u32, total_docs: usize) -> f32 {
        let n = total_docs as f64;
        let df = doc_freq as f64;
        ((1.0 + (n - df + 0.5) / (df + 0.5)).ln()) as f32
    }

    #[test]
    fn idf_matches_the_probabilistic_formula() {
        let bm25 = Bm25::default();
        for (df, n) in [
            (1, 10),
            (1, 1_000),
            (5, 10),
            (9, 10),
            (500, 1_000),
            (1_000, 1_000),
        ] {
            let expected = expected_idf(df, n);
            let got = bm25.idf(df, n);
            assert!(
                (got - expected).abs() < 1e-5,
                "idf(df={df}, n={n}) = {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn idf_falls_monotonically_across_the_whole_range() {
        let bm25 = Bm25::default();
        let curve: Vec<f32> = (1..=1_000).map(|df| bm25.idf(df, 1_000)).collect();
        assert!(
            curve.windows(2).all(|w| w[0] > w[1]),
            "idf must strictly decrease as the term becomes commoner"
        );
        // And the ends are pinned, so a rescaled curve is not mistaken for the
        // right one: ln(1 + 999.5/1.5) for the rarest term, and almost nothing
        // for one present in every document.
        assert!(
            (curve[0] - 6.503_29).abs() < 1e-4,
            "idf(1, 1000) = {}",
            curve[0]
        );
        assert!(curve[999] < 0.001, "idf(1000, 1000) = {}", curve[999]);
    }

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

    /// The scoring formula, written out independently of the implementation.
    fn expected_score(bm25: Bm25, tf: u32, doc_len: u32, avg: f32, idf: f32) -> f32 {
        let tf = f64::from(tf);
        let k1 = f64::from(bm25.k1);
        let b = f64::from(bm25.b);
        let norm = 1.0 - b + b * (f64::from(doc_len) / f64::from(avg));
        (f64::from(idf) * (tf * (k1 + 1.0)) / (tf + k1 * norm)) as f32
    }

    #[test]
    fn score_matches_the_bm25_formula() {
        // Directional tests — more occurrences score higher, longer documents
        // score lower — leave the arithmetic itself free to be wrong: mutation
        // testing showed that turning the length ratio into a product kept them
        // all passing. Pinning the value is what closes that.
        let cases = [
            (Bm25::default(), 1, 100, 100.0, 1.0),
            (Bm25::default(), 7, 30, 100.0, 2.5),
            (Bm25::default(), 3, 400, 100.0, 0.8),
            (Bm25::new(2.0, 0.3), 4, 50, 120.0, 1.7),
            (Bm25::new(0.4, 1.0), 9, 250, 80.0, 3.1),
        ];
        for (bm25, tf, len, avg, idf) in cases {
            let expected = expected_score(bm25, tf, len, avg, idf);
            let got = bm25.score(tf, len, avg, idf);
            assert!(
                (got - expected).abs() < 1e-4,
                "score(tf={tf}, len={len}, avg={avg}, idf={idf}) = {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn length_normalisation_uses_the_ratio_to_the_average() {
        let bm25 = Bm25::default();
        // A document of exactly average length is the fixed point: the
        // normalisation factor is 1, so the score depends only on tf and idf.
        let average = bm25.score(3, 100, 100.0, 1.0);
        let unnormalised = Bm25::new(1.2, 0.0).score(3, 100, 100.0, 1.0);
        assert!(
            (average - unnormalised).abs() < 1e-6,
            "at the average length, b should not matter: {average} vs {unnormalised}"
        );

        // And doubling the length must move the score the same way doubling the
        // ratio does, which a product instead of a division would not.
        let doubled_length = bm25.score(3, 200, 100.0, 1.0);
        let halved_average = bm25.score(3, 100, 50.0, 1.0);
        assert!(
            (doubled_length - halved_average).abs() < 1e-6,
            "{doubled_length} vs {halved_average}"
        );
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
        // Each guard alone must be enough: a term that does not occur scores
        // zero even in a healthy index, and an index with no documents scores
        // zero even for a term that does occur.
        assert_eq!(bm25.score(0, 100, AVG, 3.0), 0.0, "tf = 0");
        assert_eq!(bm25.score(5, 100, 0.0, 3.0), 0.0, "empty index");
        assert_eq!(bm25.score(0, 100, 0.0, 3.0), 0.0, "both");
        // …and with neither condition present, the score must not be zero,
        // otherwise the guard is swallowing real matches.
        assert!(bm25.score(5, 100, AVG, 3.0) > 0.0, "healthy match");

        // The guard also keeps the arithmetic away from NaN. With length
        // normalisation disabled the ratio to the average is multiplied by
        // zero, and `0.0 * inf` is NaN — which would sort unpredictably against
        // real scores instead of ranking last. Each condition therefore has to
        // stand on its own: it is `tf == 0 || avg <= 0`, never `&&`.
        for bm25 in [Bm25::default(), Bm25::new(1.2, 0.0)] {
            for (tf, len, avg) in [(0, 0, 0.0), (5, 100, 0.0), (0, 100, AVG)] {
                let score = bm25.score(tf, len, avg, 3.0);
                assert!(
                    !score.is_nan(),
                    "score(tf={tf}, len={len}, avg={avg}) with b={} is NaN",
                    bm25.b
                );
                assert_eq!(score, 0.0);
            }
        }
    }

    #[test]
    fn parameters_are_clamped_to_a_valid_range() {
        let bm25 = Bm25::new(-1.0, 5.0);
        assert_eq!(bm25.k1, 0.0);
        assert_eq!(bm25.b, 1.0);
    }
}
