//! A search service that can be rebuilt while it is answering queries.
//!
//! Reindexing is minutes of work; queries are microseconds. Blocking the second
//! on the first is the usual way search engines acquire a maintenance window,
//! and it is unnecessary: the new index can be built beside the old one and
//! published in a single atomic step.
//!
//! ```text
//! queries ──────────────────────────────────────────────────────►
//!            served by generation 3      │  served by generation 4
//!                                        │
//! rebuild  ├── crawl ── analyze ── seal ─┘ publish (one pointer swap)
//! ```
//!
//! Readers never take a lock a writer can hold, so a rebuild is invisible to
//! them except for the moment the answers change. A query that started before
//! the swap finishes against the index it started with — it holds an `Arc` to
//! that generation, so the old index stays alive exactly as long as someone is
//! still reading it, and not one moment longer.

use std::path::Path;
use std::sync::Mutex;

use crate::analyzer::Analyzer;
use crate::engine::{Engine, SearchResult, Stats};
use crate::error::{Error, Result};
use crate::searcher::SearchStats;
use crate::snapshot::{Shared, SnapshotCell};

/// One published version of the index, and the number identifying it.
///
/// The number lives *inside* the shared value rather than in a separate atomic,
/// which is what makes "which generation answered this query?" a question with
/// an exact answer: readers see a version and its number together or not at all.
#[derive(Debug)]
pub struct Generation {
    pub number: u64,
    pub engine: Engine,
}

/// A search engine that can be replaced under load.
#[derive(Debug)]
pub struct SearchService {
    current: SnapshotCell<Generation>,
    /// Serialises rebuilds. Readers never touch it, so holding it for minutes
    /// costs queries nothing; it only stops two rebuilds from doing the same
    /// work twice and racing to publish.
    rebuild: Mutex<()>,
}

impl SearchService {
    /// Starts a service serving `engine` as generation 1.
    pub fn new(engine: Engine) -> Self {
        Self {
            current: SnapshotCell::new(Generation { number: 1, engine }),
            rebuild: Mutex::new(()),
        }
    }

    /// The generation currently being served.
    ///
    /// Holding the returned handle pins that version: it keeps answering
    /// consistently even after newer ones are published.
    pub fn snapshot(&self) -> Shared<Generation> {
        self.current.load()
    }

    /// Number of the generation currently being served.
    pub fn generation(&self) -> u64 {
        self.snapshot().number
    }

    /// Runs a query against the current generation.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        self.snapshot().engine.search(query, limit)
    }

    /// Runs a query, also reporting the generation that answered it.
    pub fn search_with_stats(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<(Vec<SearchResult>, SearchStats, u64)> {
        let generation = self.snapshot();
        let (results, stats) = generation.engine.search_with_stats(query, limit)?;
        Ok((results, stats, generation.number))
    }

    /// Statistics of the current generation.
    pub fn stats(&self) -> Stats {
        self.snapshot().engine.stats()
    }

    /// Publishes `engine` as the next generation, returning its number.
    ///
    /// Queries in flight keep running against the previous one.
    pub fn publish(&self, engine: Engine) -> u64 {
        // The rebuild lock also linearises publishes, so the numbering cannot
        // skip or repeat even when two rebuilds finish at the same instant.
        let _serialised = self.rebuild.lock();
        let number = self.current.load().number + 1;
        self.current.store(Generation { number, engine });
        number
    }

    /// Rebuilds the index from `root` and publishes the result.
    ///
    /// The crawl happens on the calling thread, against a fresh engine that
    /// inherits the analysis and ranking configuration of the one in service.
    /// Queries are answered from the current generation throughout, including
    /// while this runs.
    pub fn reindex(&self, root: impl AsRef<Path>) -> Result<u64> {
        let _serialised = self.rebuild.lock().map_err(|_| {
            Error::Query("a previous rebuild panicked; the service needs restarting".into())
        })?;

        let mut fresh = self.current.load().engine.fork_config();
        fresh.index_dir(root)?;

        let number = self.current.load().number + 1;
        self.current.store(Generation {
            number,
            engine: fresh,
        });
        Ok(number)
    }

    /// Loads an index from `path` and publishes it.
    ///
    /// Used to pick up an index another process wrote — the mapped format makes
    /// this nearly free, so a service can adopt a freshly built index without
    /// rebuilding it itself.
    pub fn reload(&self, path: impl AsRef<Path>) -> Result<u64> {
        let _serialised = self.rebuild.lock().map_err(|_| {
            Error::Query("a previous rebuild panicked; the service needs restarting".into())
        })?;

        let mut fresh = self.current.load().engine.fork_config();
        fresh.load(path)?;

        let number = self.current.load().number + 1;
        self.current.store(Generation {
            number,
            engine: fresh,
        });
        Ok(number)
    }

    /// Drops superseded generations that no reader is using any more.
    ///
    /// Called automatically on every publish; exposed for tests and for callers
    /// that want to reclaim at a quiet moment.
    pub fn collect(&self) {
        self.current.collect();
    }

    /// The analyzer every generation shares.
    pub fn analyzer(&self) -> Analyzer {
        self.snapshot().engine.index().analyzer().clone()
    }
}

// Under `--cfg loom` the snapshot cell is built on loom's primitives, which
// panic outside a `loom::model` block. These tests drive it from ordinary
// threads, so they are compiled out of loom builds; the model checking lives in
// `snapshot::loom_tests`.
#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn corpus(dir: &Path, docs: &[(&str, &str)]) {
        for (name, text) in docs {
            fs::write(dir.join(name), text).unwrap();
        }
    }

    fn service_over(dir: &Path) -> SearchService {
        let mut engine = Engine::default();
        engine.index_dir(dir).unwrap();
        SearchService::new(engine)
    }

    #[test]
    fn a_new_service_serves_generation_one() {
        let dir = tempfile::tempdir().unwrap();
        corpus(dir.path(), &[("a.md", "rust is a systems language")]);

        let service = service_over(dir.path());
        assert_eq!(service.generation(), 1);
        assert_eq!(service.search("rust", 5).unwrap().len(), 1);
    }

    #[test]
    fn reindexing_publishes_the_next_generation() {
        let dir = tempfile::tempdir().unwrap();
        corpus(dir.path(), &[("a.md", "rust is a systems language")]);
        let service = service_over(dir.path());

        corpus(dir.path(), &[("b.md", "rust powers this search engine")]);
        assert_eq!(service.reindex(dir.path()).unwrap(), 2);
        assert_eq!(service.search("rust", 5).unwrap().len(), 2);
    }

    #[test]
    fn a_snapshot_keeps_answering_from_the_generation_it_took() {
        let dir = tempfile::tempdir().unwrap();
        corpus(dir.path(), &[("a.md", "rust is a systems language")]);
        let service = service_over(dir.path());

        let held = service.snapshot();
        corpus(dir.path(), &[("b.md", "rust powers this search engine")]);
        service.reindex(dir.path()).unwrap();
        service.collect();

        assert_eq!(held.number, 1);
        assert_eq!(held.engine.search("rust", 5).unwrap().len(), 1);
        assert_eq!(service.search("rust", 5).unwrap().len(), 2);
    }

    #[test]
    fn a_rebuild_inherits_the_configuration_in_service() {
        let dir = tempfile::tempdir().unwrap();
        corpus(
            dir.path(),
            &[("a.md", "the RUNNING birds"), ("b.txt", "ignored")],
        );

        let mut engine = Engine::default()
            .with_extensions(["md"])
            .with_bm25(crate::bm25::Bm25::new(2.0, 0.1));
        engine.index_dir(dir.path()).unwrap();
        let service = SearchService::new(engine);
        assert_eq!(service.stats().documents, 1);

        service.reindex(dir.path()).unwrap();
        assert_eq!(
            service.stats().documents,
            1,
            "the rebuild must keep the extension filter"
        );
    }

    #[test]
    fn queries_keep_being_answered_while_the_index_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        corpus(dir.path(), &[("a.md", "rust is a systems language")]);
        let service = Arc::new(service_over(dir.path()));
        let stop = Arc::new(AtomicBool::new(false));
        let answered = Arc::new(AtomicU64::new(0));

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let service = Arc::clone(&service);
                let stop = Arc::clone(&stop);
                let answered = Arc::clone(&answered);
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let (results, _, generation) =
                            service.search_with_stats("rust", 5).expect("query failed");
                        // Every generation contains the word, so an empty
                        // result would mean a query hit a half-published index.
                        assert!(
                            !results.is_empty(),
                            "generation {generation} lost a document"
                        );
                        answered.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for round in 0..5 {
            corpus(dir.path(), &[(&format!("gen{round}.md"), "rust again")]);
            service.reindex(dir.path()).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }

        assert_eq!(service.generation(), 6);
        assert!(
            answered.load(Ordering::Relaxed) > 0,
            "readers never got to run"
        );
    }

    #[test]
    fn concurrent_rebuilds_are_serialised_and_numbered_without_gaps() {
        let dir = tempfile::tempdir().unwrap();
        corpus(dir.path(), &[("a.md", "rust")]);
        let service = Arc::new(service_over(dir.path()));

        let rebuilds: Vec<_> = (0..4)
            .map(|_| {
                let service = Arc::clone(&service);
                let root = dir.path().to_path_buf();
                thread::spawn(move || service.reindex(&root).unwrap())
            })
            .collect();

        let mut numbers: Vec<u64> = rebuilds.into_iter().map(|h| h.join().unwrap()).collect();
        numbers.sort_unstable();
        assert_eq!(numbers, [2, 3, 4, 5], "generations must not skip or repeat");
        assert_eq!(service.generation(), 5);
    }

    #[test]
    fn a_mapped_index_can_be_adopted_without_rebuilding_it() {
        let dir = tempfile::tempdir().unwrap();
        corpus(dir.path(), &[("a.md", "rust is a systems language")]);

        // One process builds and writes the index…
        let mut builder = Engine::default();
        builder.index_dir(dir.path()).unwrap();
        corpus(dir.path(), &[("b.md", "rust powers this search engine")]);
        let mut builder = Engine::default();
        builder.index_dir(dir.path()).unwrap();
        let path = dir.path().join("index.farol");
        builder.save(&path).unwrap();

        // …and the one serving queries picks it up.
        let service = service_over(dir.path());
        let generation = service.reload(&path).unwrap();
        assert_eq!(generation, 2);
        assert!(service.stats().mapped, "the adopted index should be mapped");
        assert_eq!(service.search("rust", 5).unwrap().len(), 2);
    }

    #[test]
    fn publishing_an_engine_directly_bumps_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        corpus(dir.path(), &[("a.md", "rust")]);
        let service = service_over(dir.path());

        let mut replacement = Engine::default();
        replacement
            .add_document("x", "X", "something else entirely")
            .unwrap();
        assert_eq!(service.publish(replacement), 2);
        assert!(service.search("rust", 5).unwrap().is_empty());
    }
}
