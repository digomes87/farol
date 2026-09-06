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
use crate::index::{DocId, Index, Posting};
use crate::query::{ClauseKind, Occur, Query};

/// One ranked document.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub doc: DocId,
    /// BM25 score, summed over all matching clauses. Comparable within one
    /// result set only — it is not a normalised probability.
    pub score: f32,
}

/// Executes queries against an [`Index`].
///
/// Borrowing the index keeps the searcher cheap to create: one per query is
/// fine, and several can run concurrently over the same index.
#[derive(Debug, Clone)]
pub struct Searcher<'a> {
    index: &'a Index,
    bm25: Bm25,
}

impl<'a> Searcher<'a> {
    pub fn new(index: &'a Index) -> Self {
        Self {
            index,
            bm25: Bm25::default(),
        }
    }

    /// Overrides the ranking parameters.
    pub fn with_bm25(mut self, bm25: Bm25) -> Self {
        self.bm25 = bm25;
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
        if self.index.is_empty() || limit == 0 {
            return Vec::new();
        }

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
        if candidates.is_empty() {
            return Vec::new();
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

        let mut hits: Vec<Hit> = scores
            .into_iter()
            .map(|(doc, score)| Hit { doc, score })
            .collect();
        hits.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.doc.cmp(&b.doc))
        });
        hits.truncate(limit);
        hits
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
        let mut lists: Vec<(&[Posting], u32)> = Vec::with_capacity(parts.len());
        for (term, offset) in parts {
            match self.index.postings(term) {
                // One missing term is enough to rule out the whole phrase.
                None => return Vec::new(),
                Some(postings) => lists.push((postings, *offset)),
            }
        }

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
            .map(|hit| (index.document(hit.doc).unwrap().uri.clone(), hit.score))
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

    #[test]
    fn searching_an_empty_index_is_not_an_error() {
        let index = Index::new(Analyzer::raw());
        let query = Query::parse("rust", &Analyzer::raw()).unwrap();
        assert!(Searcher::new(&index).search(&query, 10).is_empty());
    }
}
