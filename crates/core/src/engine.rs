//! The façade: one type that ties analysis, indexing, ranking and snippets
//! together, so callers never have to wire the stages by hand.
//!
//! ```no_run
//! use farol_core::Engine;
//!
//! let mut engine = Engine::default();
//! engine.index_dir("./corpus")?;
//! for hit in engine.search("+rust \"search engine\"", 5)? {
//!     println!("{:.3}  {}", hit.score, hit.title);
//! }
//! # Ok::<(), farol_core::Error>(())
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::analyzer::Analyzer;
use crate::bm25::Bm25;
use crate::error::{Error, Result};
use crate::index::Index;
use crate::query::Query;
use crate::searcher::{SearchStats, Searcher};
use crate::snippet::Highlighter;
use crate::store;

/// File extensions crawled by [`Engine::index_dir`] unless overridden.
pub const DEFAULT_EXTENSIONS: &[&str] = &["txt", "md", "markdown", "rst", "text"];

/// Files handed to each parallel worker.
///
/// Large enough that the per-shard `HashMap` is amortised over several
/// documents, small enough that a directory of a few dozen files still uses
/// every core.
const INDEX_CHUNK: usize = 16;

/// A search result, ready to be displayed.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub uri: String,
    pub title: String,
    pub score: f32,
    /// Highlighted excerpt explaining the match.
    pub snippet: String,
}

/// Index level counters, reported by `farol stats`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub documents: usize,
    pub vocabulary: usize,
    pub postings: usize,
    pub avg_doc_len: f32,
}

/// A complete search engine over a document collection.
#[derive(Debug, Clone)]
pub struct Engine {
    index: Index,
    bm25: Bm25,
    highlighter: Highlighter,
    extensions: Vec<String>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(Analyzer::default())
    }
}

impl Engine {
    /// Creates an empty engine using `analyzer` for both indexing and querying.
    pub fn new(analyzer: Analyzer) -> Self {
        Self {
            index: Index::new(analyzer),
            bm25: Bm25::default(),
            highlighter: Highlighter::default(),
            extensions: DEFAULT_EXTENSIONS.iter().map(|e| e.to_string()).collect(),
        }
    }

    /// Overrides the BM25 parameters used for ranking.
    pub fn with_bm25(mut self, bm25: Bm25) -> Self {
        self.bm25 = bm25;
        self
    }

    /// Overrides how snippets are cut and marked.
    pub fn with_highlighter(mut self, highlighter: Highlighter) -> Self {
        self.highlighter = highlighter;
        self
    }

    /// Restricts the crawl to the given file extensions, without the dot.
    pub fn with_extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.extensions = extensions
            .into_iter()
            .map(|e| e.as_ref().trim_start_matches('.').to_lowercase())
            .collect();
        self
    }

    /// Indexes a single in-memory document.
    pub fn add_document(&mut self, uri: impl Into<String>, title: impl Into<String>, text: &str) {
        self.index.add(uri, title, text);
        self.index.finish();
    }

    /// Recursively indexes every matching file under `root`.
    ///
    /// Returns how many documents were added. Unreadable files abort the crawl:
    /// silently skipping them would produce an index that is quietly missing
    /// documents, and a search engine that lies about its coverage is worse
    /// than one that refuses to start.
    ///
    /// Files are read and analyzed in parallel. Analysis is pure CPU work over
    /// independent inputs, so the crawl is split into chunks, each chunk builds
    /// a private index shard, and the shards are merged in path order — which
    /// keeps document ids identical to a sequential run no matter how the work
    /// was scheduled.
    pub fn index_dir(&mut self, root: impl AsRef<Path>) -> Result<usize> {
        let files = self.collect_files(root.as_ref())?;
        let analyzer = self.index.analyzer().clone();

        let shards: Vec<Index> = files
            .par_chunks(INDEX_CHUNK)
            .map(|chunk| {
                let mut shard = Index::new(analyzer.clone());
                for path in chunk {
                    let text = fs::read_to_string(path).map_err(|source| Error::Io {
                        path: path.clone(),
                        source,
                    })?;
                    let title = title_of(path, &text);
                    shard.add(path.display().to_string(), title, &text);
                }
                Ok(shard)
            })
            .collect::<Result<Vec<_>>>()?;

        for shard in shards {
            self.index.merge(shard);
        }
        self.index.finish();
        Ok(files.len())
    }

    /// Parses and runs `input`, returning at most `limit` results.
    pub fn search(&self, input: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.search_with_stats(input, limit).map(|(hits, _)| hits)
    }

    /// Same as [`search`](Engine::search), also reporting how the query was
    /// evaluated — which strategy ran, and how many documents it had to score.
    pub fn search_with_stats(
        &self,
        input: &str,
        limit: usize,
    ) -> Result<(Vec<SearchResult>, SearchStats)> {
        let query = Query::parse(input, self.index.analyzer())?;
        let terms = query.positive_terms();
        let (hits, stats) = Searcher::new(&self.index)
            .with_bm25(self.bm25)
            .search_with_stats(&query, limit);

        let results = hits
            .into_iter()
            .filter_map(|hit| {
                let doc = self.index.document(hit.doc)?;
                Some(SearchResult {
                    uri: doc.uri.clone(),
                    title: doc.title.clone(),
                    score: hit.score,
                    snippet: self
                        .highlighter
                        .snippet(&doc.text, self.index.analyzer(), &terms)
                        .text,
                })
            })
            .collect();
        Ok((results, stats))
    }

    /// Counters describing the current index.
    pub fn stats(&self) -> Stats {
        Stats {
            documents: self.index.len(),
            vocabulary: self.index.vocabulary_size(),
            postings: self.index.total_postings(),
            avg_doc_len: self.index.avg_doc_len(),
        }
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Persists the index to `path`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        store::save(&self.index, path)
    }

    /// Loads an index previously written by [`save`](Engine::save), keeping the
    /// analyzer and ranking configuration of `self`.
    pub fn load(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.index = store::load(path, self.index.analyzer().clone())?;
        Ok(())
    }

    /// Walks `root` collecting the files this engine is willing to index.
    ///
    /// Hidden entries are skipped: a crawl that descends into `.git` spends
    /// most of its time indexing object files nobody will ever search for.
    fn collect_files(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];

        while let Some(dir) = stack.pop() {
            let entries = fs::read_dir(&dir).map_err(|source| Error::Io {
                path: dir.clone(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| Error::Io {
                    path: dir.clone(),
                    source,
                })?;
                let path = entry.path();
                if is_hidden(&path) {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else if self.accepts(&path) {
                    out.push(path);
                }
            }
        }

        // Directory iteration order is filesystem dependent; sorting keeps
        // document ids — and therefore score ties — reproducible.
        out.sort();
        Ok(out)
    }

    fn accepts(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| self.extensions.iter().any(|e| e == &ext.to_lowercase()))
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

/// Uses the document's first meaningful line as its title, falling back to the
/// file name. Markdown heading markers are stripped.
fn title_of(path: &Path, text: &str) -> String {
    text.lines()
        .map(|line| line.trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(80).collect::<String>())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("untitled")
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("rust.md"),
            "# Rust\n\nRust gives memory safety without a garbage collector.",
        )
        .unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(
            dir.path().join("nested/go.txt"),
            "Go ships goroutines and a garbage collector.",
        )
        .unwrap();
        fs::write(dir.path().join("photo.png"), [0u8, 1, 2]).unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/HEAD.txt"), "ref: refs/heads/main").unwrap();
        dir
    }

    #[test]
    fn parallel_indexing_produces_the_same_ids_as_a_sequential_run() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            fs::write(
                dir.path().join(format!("doc-{i:03}.txt")),
                format!("document number {i} about rust and search"),
            )
            .unwrap();
        }

        let mut engine = Engine::default();
        engine.index_dir(dir.path()).unwrap();

        let uris: Vec<_> = engine
            .index()
            .documents()
            .iter()
            .map(|d| d.uri.clone())
            .collect();
        let mut sorted = uris.clone();
        sorted.sort();
        assert_eq!(uris, sorted, "document ids must follow path order");
        assert_eq!(engine.stats().documents, 200);
    }

    #[test]
    fn crawling_picks_text_files_recursively_and_skips_the_rest() {
        let dir = corpus();
        let mut engine = Engine::default();
        assert_eq!(engine.index_dir(dir.path()).unwrap(), 2);
        assert_eq!(engine.stats().documents, 2);
    }

    #[test]
    fn markdown_headings_become_titles() {
        let dir = corpus();
        let mut engine = Engine::default();
        engine.index_dir(dir.path()).unwrap();
        let titles: Vec<_> = engine
            .index()
            .documents()
            .iter()
            .map(|d| d.title.clone())
            .collect();
        assert!(titles.contains(&"Rust".to_string()));
    }

    #[test]
    fn search_returns_titles_scores_and_snippets() {
        let dir = corpus();
        let mut engine = Engine::default();
        engine.index_dir(dir.path()).unwrap();

        let results = engine.search("goroutines", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.contains("**goroutines**"));
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn a_shared_term_ranks_both_documents() {
        let dir = corpus();
        let mut engine = Engine::default();
        engine.index_dir(dir.path()).unwrap();
        assert_eq!(engine.search("collector", 5).unwrap().len(), 2);
    }

    #[test]
    fn search_reports_the_strategy_it_used() {
        let dir = corpus();
        let mut engine = Engine::default();
        engine.index_dir(dir.path()).unwrap();

        let (_, optional) = engine.search_with_stats("garbage collector", 5).unwrap();
        assert_eq!(optional.strategy, crate::searcher::Strategy::Wand);

        let (_, required) = engine.search_with_stats("+garbage collector", 5).unwrap();
        assert_eq!(required.strategy, crate::searcher::Strategy::Exhaustive);
    }

    #[test]
    fn an_unparsable_query_is_an_error_not_an_empty_result() {
        let mut engine = Engine::default();
        engine.add_document("a", "A", "text");
        assert!(engine.search("\"unterminated", 5).is_err());
    }

    #[test]
    fn extensions_can_be_restricted() {
        let dir = corpus();
        let mut engine = Engine::default().with_extensions(["md"]);
        assert_eq!(engine.index_dir(dir.path()).unwrap(), 1);
    }

    #[test]
    fn indexing_a_missing_directory_reports_the_path() {
        let mut engine = Engine::default();
        let err = engine.index_dir("/does/not/exist").unwrap_err();
        assert!(err.to_string().contains("/does/not/exist"));
    }

    #[test]
    fn an_engine_round_trips_through_disk() {
        let dir = corpus();
        let mut engine = Engine::default();
        engine.index_dir(dir.path()).unwrap();
        let path = dir.path().join("index.farol");
        engine.save(&path).unwrap();

        let mut reopened = Engine::default();
        reopened.load(&path).unwrap();
        assert_eq!(reopened.stats(), engine.stats());
        assert_eq!(reopened.search("goroutines", 5).unwrap().len(), 1);
    }
}
