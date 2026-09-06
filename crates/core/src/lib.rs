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
//! | [`searcher`] | clauses + index → ranked hits |
//! | [`snippet`] | matched document → highlighted excerpt |
//! | [`error`] | the crate wide error type |

pub mod analyzer;
pub mod bm25;
pub mod error;
pub mod index;
pub mod query;
pub mod searcher;
pub mod snippet;

pub use analyzer::{Analyzer, Token};
pub use bm25::Bm25;
pub use error::{Error, Result};
pub use index::{DocId, Document, Index, Posting};
pub use query::{Clause, ClauseKind, Occur, Query};
pub use searcher::{Hit, Searcher};
pub use snippet::{Highlighter, Snippet};
