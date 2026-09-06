//! `farol-core` — a full-text search engine built from first principles.
//!
//! The crate is organised as a pipeline, and each module owns one stage:
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`analyzer`] | raw text → normalized, stemmed [`Token`]s |
//! | [`index`] | terms → posting lists (the inverted index) |
//! | [`bm25`] | posting lists → relevance scores |
//! | [`query`] | query string → boolean clauses |
//! | [`codec`] | delta + varint compression of posting lists |
//! | [`cursor`] | skip-capable iteration over a posting list |
//! | [`topk`] | bounded collector for the best k hits |
//! | [`searcher`] | clauses + index → ranked hits |
//! | [`snippet`] | matched document → highlighted excerpt |
//! | [`store`] | index ↔ a single self-describing file |
//! | [`mmap`] | index ↔ a file read in place, without loading it |
//! | [`snapshot`] | lock-free publication of a new index to live readers |
//! | [`service`] | a search engine that can be rebuilt while it serves |
//! | [`source`] | one interface over in-memory and mapped indexes |
//! | [`engine`] | the façade tying every stage together |
//! | [`error`] | the crate wide error type |

pub mod analyzer;
pub mod bm25;
pub mod codec;
pub mod cursor;
pub mod engine;
pub mod error;
pub mod index;
pub mod mmap;
pub mod query;
pub mod searcher;
pub mod service;
pub mod snapshot;
pub mod snippet;
pub mod source;
pub mod store;
pub mod topk;

pub use analyzer::{Analyzer, Token};
pub use bm25::Bm25;
pub use engine::{Engine, SearchResult, Stats};
pub use error::{Error, Result};
pub use index::{DocId, Document, Index, Posting};
pub use mmap::MappedIndex;
pub use query::{Clause, ClauseKind, Occur, Query};
pub use searcher::{Hit, SearchStats, Searcher, Strategy};
pub use service::{Generation, SearchService};
pub use snapshot::{Shared, SnapshotCell};
pub use snippet::{Highlighter, Snippet};
pub use source::{Edit, IndexSource};
