//! `farol-core` — a full-text search engine built from first principles.
//!
//! The crate is organised as a pipeline, and each module owns one stage:
//!
//! | Module | Responsibility |
//! |--------|----------------|
//! | [`analyzer`] | raw text → normalized, stemmed [`Token`]s |
//! | [`index`] | terms → posting lists (the inverted index) |
//! | [`error`] | the crate wide error type |

pub mod analyzer;
pub mod error;
pub mod index;

pub use analyzer::{Analyzer, Token};
pub use error::{Error, Result};
pub use index::{DocId, Document, Index, Posting};
