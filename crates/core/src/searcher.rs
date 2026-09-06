//! Query execution: turning a parsed [`Query`] into a ranked list of documents.
//!
//! The evaluation order is chosen to touch as few documents as possible:
//!
//! 1. every clause is resolved into `(document, frequency)` pairs by walking
//!    its posting list — phrases additionally verify positional adjacency;
//! 2. the candidate set is built from the `Must` clauses when there are any
//!    (an intersection, always smaller than the union) and from the `Should`
//!    clauses otherwise;
//! 3. `MustNot` clauses subtract from the candidates;
//! 4. only the survivors are scored with BM25 and ranked.

use std::collections::{HashMap, HashSet};

use crate::bm25::Bm25;
use crate::cursor::BlockCursor;
use crate::index::{DocId, Index, Posting};
use crate::query::{ClauseKind, Occur, Query};
use crate::topk::TopK;

/// One ranked document.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub doc: DocId,
    /// BM25 score, summed over all matching clauses. Comparable within one
    /// result set only — it is not a normalised probability.
    pub score: f32,
}

/// What a query actually did, for benchmarking and for `--explain` style output.
///
/// `scored` versus `candidates` is the whole story of dynamic pruning: both
/// strategies return the same ranking, but WAND reaches it while fully scoring
/// a fraction of the documents that match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchStats {
    /// Documents that were fully scored with BM25.
    pub scored: usize,
    /// Postings the query would have had to visit without pruning.
    pub candidates: usize,
    /// Postings skipped because their score bound could not reach the
    /// threshold. Always zero for the exhaustive strategy.
    pub pruned: usize,
    /// Blocks skipped whole, without decoding them, because the block's own
    /// bound could not reach the threshold.
    pub blocks_skipped: usize,
    /// Which evaluation strategy ran.
    pub strategy: Strategy,
}

/// How a query was evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Every matching document is scored, then ranked.
    #[default]
    Exhaustive,
    /// Documents whose maximum possible score cannot reach the current
    /// threshold are skipped without ever being scored, and whole blocks of
    /// postings are skipped without being decoded.
    Wand,
}

/// Executes queries against an [`Index`].
///
/// Borrowing the index keeps the searcher cheap to create: one per query is
/// fine, and several can run concurrently over the same index.
#[derive(Debug, Clone)]
pub struct Searcher<'a> {
    index: &'a Index,
    bm25: Bm25,
    pruning: bool,
}

impl<'a> Searcher<'a> {
    pub fn new(index: &'a Index) -> Self {
        Self {
            index,
            bm25: Bm25::default(),
            pruning: true,
        }
    }

    /// Overrides the ranking parameters.
    pub fn with_bm25(mut self, bm25: Bm25) -> Self {
        self.bm25 = bm25;
        self
    }

    /// Enables or disables dynamic pruning.
    ///
    /// Both strategies return the same ranking, so this is not a quality knob:
    /// it exists to A/B the two implementations in benchmarks and to isolate
    /// pruning when a ranking bug is being tracked down.
    pub fn with_pruning(mut self, pruning: bool) -> Self {
        self.pruning = pruning;
        self
    }

    pub fn index(&self) -> &Index {
        self.index
    }

    /// Returns the `limit` best documents for `query`, best first.
    ///
    /// Ties are broken by document id so results are stable across runs — a
    /// property worth having whenever output is diffed or snapshot tested.
    pub fn search(&self, query: &Query, limit: usize) -> Vec<Hit> {
        self.search_with_stats(query, limit).0
    }

    /// Same as [`search`](Searcher::search), also reporting how the query was
    /// evaluated.
    ///
    /// The strategy is picked from the query shape. A query of purely optional
    /// terms is the case dynamic pruning was designed for, and gets WAND.
    /// Anything with a required, excluded or phrase clause falls back to
    /// exhaustive evaluation: those clauses constrain the candidate set by
    /// themselves, and the intersection they produce is usually smaller than
    /// what pruning would save.
    pub fn search_with_stats(&self, query: &Query, limit: usize) -> (Vec<Hit>, SearchStats) {
        if self.index.is_empty() || limit == 0 {
            return (Vec::new(), SearchStats::default());
        }
        if self.is_prunable(query) {
            return self.search_wand(query, limit);
        }
        self.search_exhaustive(query, limit)
    }

    /// WAND is only applicable when every clause is an optional single term.
    fn is_prunable(&self, query: &Query) -> bool {
        self.pruning
            && query
                .clauses
                .iter()
                .all(|c| c.occur == Occur::Should && matches!(c.kind, ClauseKind::Term(_)))
    }

    /// Weak AND: scores a document only when the terms known to be positioned
    /// on it *could* together beat the current threshold.
    ///
    /// The loop keeps one cursor per term, sorted by the document each points
    /// at, and adds up their upper bounds in that order. The first term whose
    /// running sum exceeds the threshold is the *pivot*: no document before the
    /// pivot's current document can possibly score high enough, because the
    /// terms positioned there are exactly the ones whose bounds have not yet
    /// added up. So the loop either scores the pivot document — when every
    /// preceding cursor is already on it — or skips those cursors straight to
    /// it, never touching what lies in between.
    fn search_wand(&self, query: &Query, limit: usize) -> (Vec<Hit>, SearchStats) {
        let avg = self.index.avg_doc_len();
        let mut cursors: Vec<BlockCursor<'_>> = Vec::new();
        let mut candidates = 0usize;

        for clause in &query.clauses {
            let ClauseKind::Term(term) = &clause.kind else {
                continue;
            };
            let Some(term_index) = self.index.term(term) else {
                continue;
            };
            let idf = self.bm25.idf(term_index.doc_freq(), self.index.len());
            candidates += term_index.doc_freq() as usize;
            cursors.push(BlockCursor::new(term_index, idf, self.bm25, avg));
        }

        let mut stats = SearchStats {
            candidates,
            strategy: Strategy::Wand,
            ..SearchStats::default()
        };
        if cursors.is_empty() {
            return (Vec::new(), stats);
        }

        let mut topk = TopK::new(limit);

        loop {
            // Exhausted cursors sort to the end and are then dropped.
            cursors.sort_unstable_by_key(|c| c.doc().unwrap_or(DocId::MAX));
            cursors.retain(|c| !c.is_exhausted());
            if cursors.is_empty() {
                break;
            }

            let threshold = topk.threshold();
            let Some(mut pivot) = find_pivot(&cursors, threshold) else {
                // Not even every remaining term together can beat the
                // threshold: nothing left in the index can enter the top k.
                stats.pruned += cursors.iter().map(BlockCursor::remaining).sum::<usize>();
                break;
            };
            let pivot_doc = cursors[pivot].doc().expect("pivot cursor is not exhausted");

            // Cursors sitting on the same document as the pivot contribute to
            // it too. Leaving them out of the bound below would underestimate
            // the document's score and prune a document that belongs in the
            // results.
            while pivot + 1 < cursors.len() && cursors[pivot + 1].doc() == Some(pivot_doc) {
                pivot += 1;
            }

            // Block-max refinement. The term-wide bounds said this pivot is
            // worth considering; the bounds of the blocks the cursors actually
            // sit on are tighter and often say otherwise.
            let block_sum: f32 = cursors[..=pivot]
                .iter()
                .map(|c| c.block_upper_bound_at(pivot_doc))
                .sum();
            if block_sum <= threshold {
                // No document up to the end of the shallowest block can beat
                // the threshold: jump past it. Documents below the pivot were
                // already ruled out by the pivot itself.
                let block_end = cursors[..=pivot]
                    .iter()
                    .filter_map(|c| c.block_last_doc_at(pivot_doc))
                    .min()
                    .unwrap_or(pivot_doc);
                // Stop at the next cursor's document: beyond it, terms that
                // were not part of the bound above start matching again, so
                // nothing has been proven about those documents.
                let next_cursor_doc = cursors
                    .get(pivot + 1)
                    .and_then(BlockCursor::doc)
                    .unwrap_or(DocId::MAX);
                let target = block_end
                    .saturating_add(1)
                    .min(next_cursor_doc)
                    .max(pivot_doc + 1);

                for cursor in cursors[..=pivot].iter_mut() {
                    let postings_before = cursor.remaining();
                    let block_before = cursor.block_index();
                    cursor.advance_to(target);
                    stats.pruned += postings_before - cursor.remaining();
                    stats.blocks_skipped += cursor.block_index() - block_before;
                }
                continue;
            }

            if cursors[0].doc() == Some(pivot_doc) {
                // Every cursor up to the pivot is on this document: score it.
                let len = self.index.document(pivot_doc).map_or(0, |d| d.length);
                let score: f32 = cursors
                    .iter()
                    .take_while(|c| c.doc() == Some(pivot_doc))
                    .map(|c| self.bm25.score(c.tf(), len, avg, c.idf()))
                    .sum();
                stats.scored += 1;
                topk.offer(pivot_doc, score);

                for cursor in cursors.iter_mut() {
                    if cursor.doc() == Some(pivot_doc) {
                        cursor.advance();
                    }
                }
            } else {
                // Skip the lagging cursors straight to the pivot document.
                for cursor in cursors[..pivot].iter_mut() {
                    if cursor.doc().is_some_and(|doc| doc < pivot_doc) {
                        let before = cursor.remaining();
                        cursor.advance_to(pivot_doc);
                        stats.pruned += before - cursor.remaining();
                    }
                }
            }
        }

        (topk.into_sorted(), stats)
    }

    /// Scores every document that matches at least one clause.
    fn search_exhaustive(&self, query: &Query, limit: usize) -> (Vec<Hit>, SearchStats) {
        let mut required: Option<HashSet<DocId>> = None;
        let mut optional: HashSet<DocId> = HashSet::new();
        let mut excluded: HashSet<DocId> = HashSet::new();
        // (matches, idf) per scoring clause.
        let mut scoring: Vec<(Vec<(DocId, u32)>, f32)> = Vec::new();

        for clause in &query.clauses {
            let matches = self.resolve(&clause.kind);
            let docs: HashSet<DocId> = matches.iter().map(|(doc, _)| *doc).collect();

            match clause.occur {
                Occur::MustNot => {
                    excluded.extend(docs);
                    continue;
                }
                Occur::Must => {
                    required = Some(match required {
                        Some(current) => current.intersection(&docs).copied().collect(),
                        None => docs,
                    });
                }
                Occur::Should => optional.extend(docs),
            }

            let idf = self.bm25.idf(matches.len() as u32, self.index.len());
            scoring.push((matches, idf));
        }

        let candidates = match required {
            Some(required) => required,
            None => optional,
        };
        let mut stats = SearchStats {
            candidates: candidates.len(),
            strategy: Strategy::Exhaustive,
            ..SearchStats::default()
        };
        if candidates.is_empty() {
            return (Vec::new(), stats);
        }

        let avg = self.index.avg_doc_len();
        let mut scores: HashMap<DocId, f32> = HashMap::new();
        for (matches, idf) in scoring {
            for (doc, freq) in matches {
                if !candidates.contains(&doc) || excluded.contains(&doc) {
                    continue;
                }
                let len = self.index.document(doc).map_or(0, |d| d.length);
                *scores.entry(doc).or_insert(0.0) += self.bm25.score(freq, len, avg, idf);
            }
        }

        stats.scored = scores.len();
        let mut topk = TopK::new(limit);
        // Offer in ascending id order so ties resolve the same way WAND
        // resolves them.
        let mut scored: Vec<(DocId, f32)> = scores.into_iter().collect();
        scored.sort_unstable_by_key(|(doc, _)| *doc);
        for (doc, score) in scored {
            topk.offer(doc, score);
        }
        (topk.into_sorted(), stats)
    }

    /// Resolves a clause into the documents it matches and how often.
    fn resolve(&self, kind: &ClauseKind) -> Vec<(DocId, u32)> {
        match kind {
            ClauseKind::Term(term) => self
                .index
                .postings(term)
                .map(|postings| postings.iter().map(|p| (p.doc, p.tf())).collect())
                .unwrap_or_default(),
            ClauseKind::Phrase(parts) => self.resolve_phrase(parts),
        }
    }

    /// Finds documents where the phrase terms occur at the expected distances.
    ///
    /// Starts from the rarest term's posting list: it bounds the number of
    /// documents that can possibly match, so the positional check — the
    /// expensive part — runs as few times as possible.
    fn resolve_phrase(&self, parts: &[(String, u32)]) -> Vec<(DocId, u32)> {
        let mut owned: Vec<(Vec<Posting>, u32)> = Vec::with_capacity(parts.len());
        for (term, offset) in parts {
            match self.index.postings(term) {
                // One missing term is enough to rule out the whole phrase.
                None => return Vec::new(),
                Some(postings) => owned.push((postings, *offset)),
            }
        }
        let lists: Vec<(&[Posting], u32)> = owned
            .iter()
            .map(|(postings, offset)| (postings.as_slice(), *offset))
            .collect();

        let pivot = lists
            .iter()
            .enumerate()
            .min_by_key(|(_, (postings, _))| postings.len())
            .map(|(idx, _)| idx)
            .expect("a phrase has at least two terms");

        let mut out = Vec::new();
        for posting in lists[pivot].0 {
            let doc = posting.doc;
            let Some(aligned) = collect_positions(&lists, doc) else {
                continue;
            };
            let count = count_phrase_occurrences(&aligned);
            if count > 0 {
                out.push((doc, count));
            }
        }
        out.sort_unstable_by_key(|(doc, _)| *doc);
        out
    }
}

/// Positions of every phrase term inside `doc`, shifted so the first term of
/// the phrase sits at offset zero. Returns `None` if some term is absent.
fn collect_positions<'p>(
    lists: &[(&'p [Posting], u32)],
    doc: DocId,
) -> Option<Vec<(&'p [u32], u32)>> {
    let base = lists[0].1;
    lists
        .iter()
        .map(|(postings, offset)| {
            let idx = postings.binary_search_by_key(&doc, |p| p.doc).ok()?;
            Some((postings[idx].positions.as_slice(), offset - base))
        })
        .collect()
}

/// Counts how many times the terms line up at their expected offsets.
fn count_phrase_occurrences(aligned: &[(&[u32], u32)]) -> u32 {
    let (first_positions, _) = aligned[0];
    let mut count = 0;
    for &start in first_positions {
        let hit = aligned[1..]
            .iter()
            .all(|(positions, offset)| positions.binary_search(&(start + offset)).is_ok());
        if hit {
            count += 1;
        }
    }
    count
}

/// Finds the first cursor whose accumulated upper bound exceeds `threshold`.
///
/// `cursors` must be sorted by current document. Returns `None` when even the
/// sum of every remaining bound cannot beat the threshold, which means the
/// search is finished.
fn find_pivot(cursors: &[BlockCursor<'_>], threshold: f32) -> Option<usize> {
    let mut accumulated = 0.0;
    for (idx, cursor) in cursors.iter().enumerate() {
        accumulated += cursor.term_upper_bound();
        if accumulated > threshold {
            return Some(idx);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::Analyzer;

    fn index() -> Index {
        let mut index = Index::new(Analyzer::raw());
        index.add("d0", "Rust", "rust is a fast and safe systems language");
        index.add(
            "d1",
            "Search",
            "a search engine ranks documents by relevance",
        );
        index.add(
            "d2",
            "Both",
            "rust is a great language to write a search engine",
        );
        index.add("d3", "Java", "java is a language with a virtual machine");
        index.finish();
        index
    }

    fn run(input: &str, limit: usize) -> Vec<(String, f32)> {
        let index = index();
        let analyzer = Analyzer::raw();
        let query = Query::parse(input, &analyzer).unwrap();
        Searcher::new(&index)
            .search(&query, limit)
            .into_iter()
            .map(|hit| (index.document(hit.doc).unwrap().uri.to_string(), hit.score))
            .collect()
    }

    fn uris(input: &str) -> Vec<String> {
        run(input, 10).into_iter().map(|(uri, _)| uri).collect()
    }

    #[test]
    fn optional_terms_match_any_document() {
        let mut found = uris("rust java");
        found.sort();
        assert_eq!(found, ["d0", "d2", "d3"]);
    }

    #[test]
    fn documents_matching_more_clauses_rank_higher() {
        let ranked = uris("rust search");
        assert_eq!(ranked[0], "d2", "d2 has both terms and must come first");
    }

    #[test]
    fn required_terms_intersect() {
        assert_eq!(uris("+rust +search"), ["d2"]);
    }

    #[test]
    fn excluded_terms_are_removed_from_the_results() {
        let mut found = uris("language -java");
        found.sort();
        assert_eq!(found, ["d0", "d2"]);
    }

    #[test]
    fn phrases_require_adjacency_in_the_right_order() {
        let mut found = uris("\"search engine\"");
        found.sort();
        assert_eq!(found, ["d1", "d2"]);
        assert!(
            uris("\"engine search\"").is_empty(),
            "reversed phrase must not match"
        );
    }

    #[test]
    fn phrases_do_not_match_terms_that_are_merely_present() {
        // Both words occur in d0, far apart, so the phrase must not match it.
        assert!(!uris("\"rust engine\"").contains(&"d0".to_string()));
    }

    #[test]
    fn limit_truncates_the_ranking() {
        assert_eq!(run("a", 2).len(), 2);
    }

    #[test]
    fn unknown_terms_yield_no_results() {
        assert!(uris("kubernetes").is_empty());
        assert!(uris("+kubernetes rust").is_empty());
    }

    #[test]
    fn scores_are_positive_and_ordered() {
        let ranked = run("rust search engine", 10);
        assert!(ranked.iter().all(|(_, score)| *score > 0.0));
        assert!(ranked.windows(2).all(|w| w[0].1 >= w[1].1));
    }

    /// A corpus shaped like natural language rather than like a uniform grid.
    ///
    /// Term frequencies and document lengths both vary, which matters for more
    /// than realism: with uniform values every block of a posting list would
    /// have the same bound as the term itself, and block-max pruning would have
    /// nothing tighter to work with.
    fn skewed_index(docs: usize) -> Index {
        let mut index = Index::new(Analyzer::raw());
        for id in 0..docs {
            let mut text = String::new();
            // Terms cluster: they are prominent in the first tenth of the
            // collection and incidental afterwards. Real corpora behave this
            // way — documents arrive in batches that share a subject — and it
            // is what gives blocks bounds that differ from the term's own.
            let repeats = if id < docs / 10 { 6 } else { 1 };
            for term in 0..8 {
                // Term `t` appears in every `2^t`-th document.
                if id % (1 << term) == 0 {
                    for _ in 0..repeats {
                        text.push_str(&format!("term{term} "));
                    }
                }
            }
            // Documents differ in length by an order of magnitude.
            for filler in 0..(1 + id % 40) {
                text.push_str(&format!("filler{filler} "));
            }
            index.add(format!("d{id}"), format!("Doc {id}"), &text);
        }
        index.finish();
        index
    }

    #[test]
    fn pruning_and_exhaustive_scoring_return_the_same_ranking() {
        let index = skewed_index(500);
        let analyzer = Analyzer::raw();
        let searcher = Searcher::new(&index);

        let queries = [
            "term0",
            "term0 term1",
            "term3 term7",
            "term0 term2 term5",
            "term1 term2 term3 term4 term5 term6 term7",
            "filler3 term0",
            "missing term0",
        ];

        for input in queries {
            let query = Query::parse(input, &analyzer).unwrap();
            for limit in [1, 3, 10, 50] {
                let (pruned, stats) = searcher.search_with_stats(&query, limit);
                let (exhaustive, _) = searcher.search_exhaustive(&query, limit);

                assert_eq!(stats.strategy, Strategy::Wand, "`{input}` should prune");
                assert_eq!(
                    pruned.len(),
                    exhaustive.len(),
                    "`{input}` limit {limit}: different result count"
                );
                for (a, b) in pruned.iter().zip(&exhaustive) {
                    assert_eq!(a.doc, b.doc, "`{input}` limit {limit}: different ranking");
                    assert!(
                        (a.score - b.score).abs() < 1e-4,
                        "`{input}` limit {limit}: doc {} scored {} vs {}",
                        a.doc,
                        a.score,
                        b.score
                    );
                }
            }
        }
    }

    #[test]
    fn pruning_holds_under_retuned_ranking_parameters() {
        // The score bounds are stored as (max_tf, min_len) precisely so they
        // stay valid when k1 and b change at query time. If that reasoning were
        // wrong, pruning would drop documents here.
        let index = skewed_index(800);
        let analyzer = Analyzer::raw();
        let query = Query::parse("term0 term1 term4", &analyzer).unwrap();

        for bm25 in [
            Bm25::new(2.0, 0.3),
            Bm25::new(0.5, 1.0),
            Bm25::new(0.0, 0.0),
        ] {
            let pruned = Searcher::new(&index).with_bm25(bm25).search(&query, 10);
            let exhaustive = Searcher::new(&index)
                .with_bm25(bm25)
                .with_pruning(false)
                .search(&query, 10);
            assert_eq!(
                pruned.iter().map(|h| h.doc).collect::<Vec<_>>(),
                exhaustive.iter().map(|h| h.doc).collect::<Vec<_>>(),
                "k1={} b={} changed the ranking",
                bm25.k1,
                bm25.b
            );
        }
    }

    #[test]
    fn pruning_scores_far_fewer_documents_than_it_matches() {
        let index = skewed_index(2_000);
        let analyzer = Analyzer::raw();
        let query = Query::parse("term0 term4 term6", &analyzer).unwrap();

        let (_, stats) = Searcher::new(&index).search_with_stats(&query, 10);

        assert_eq!(stats.strategy, Strategy::Wand);
        assert!(
            stats.scored < stats.candidates / 2,
            "scored {} of {} candidates — pruning is not paying off",
            stats.scored,
            stats.candidates
        );
        assert!(stats.pruned > 0);
    }

    #[test]
    fn pruning_can_be_turned_off_without_changing_the_ranking() {
        let index = skewed_index(300);
        let analyzer = Analyzer::raw();
        let query = Query::parse("term0 term2 term5", &analyzer).unwrap();

        let (pruned, wand) = Searcher::new(&index).search_with_stats(&query, 10);
        let (plain, exhaustive) = Searcher::new(&index)
            .with_pruning(false)
            .search_with_stats(&query, 10);

        assert_eq!(wand.strategy, Strategy::Wand);
        assert_eq!(exhaustive.strategy, Strategy::Exhaustive);
        assert_eq!(
            pruned.iter().map(|h| h.doc).collect::<Vec<_>>(),
            plain.iter().map(|h| h.doc).collect::<Vec<_>>()
        );
        assert!(wand.scored < exhaustive.scored);
    }

    #[test]
    fn constrained_queries_fall_back_to_exhaustive_evaluation() {
        let index = index();
        let analyzer = Analyzer::raw();
        for input in ["+rust search", "rust -java", "\"search engine\""] {
            let query = Query::parse(input, &analyzer).unwrap();
            let (_, stats) = Searcher::new(&index).search_with_stats(&query, 10);
            assert_eq!(
                stats.strategy,
                Strategy::Exhaustive,
                "`{input}` cannot be pruned safely"
            );
        }
    }

    #[test]
    fn block_max_bounds_skip_whole_blocks() {
        // Long posting lists are where block-max pays off: with only a couple
        // of blocks per term there is nothing to skip over.
        let index = skewed_index(6_000);
        let analyzer = Analyzer::raw();
        let query = Query::parse("term0 term5 term7", &analyzer).unwrap();

        let (_, stats) = Searcher::new(&index).search_with_stats(&query, 10);
        assert!(stats.blocks_skipped > 0, "no block was skipped: {stats:?}");
        assert!(stats.scored < stats.candidates / 4);
    }

    #[test]
    fn a_smaller_limit_prunes_more() {
        let index = skewed_index(2_000);
        let analyzer = Analyzer::raw();
        let query = Query::parse("term0 term1 term2", &analyzer).unwrap();
        let searcher = Searcher::new(&index);

        let (_, tight) = searcher.search_with_stats(&query, 1);
        let (_, loose) = searcher.search_with_stats(&query, 100);
        assert!(
            tight.scored < loose.scored,
            "asking for fewer results scored {} vs {}",
            tight.scored,
            loose.scored
        );
    }

    #[test]
    fn searching_an_empty_index_is_not_an_error() {
        let index = Index::new(Analyzer::raw());
        let query = Query::parse("rust", &Analyzer::raw()).unwrap();
        assert!(Searcher::new(&index).search(&query, 10).is_empty());
    }
}
