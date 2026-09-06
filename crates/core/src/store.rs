//! On-disk index format.
//!
//! Building an index is the expensive part of search; reading it back should be
//! cheap. The index is serialised with `bincode` behind a small header:
//!
//! ```text
//! +--------+---------+------------------------+
//! | magic  | version | bincode(Index)         |
//! | FAROL1 | u32     | postings + documents   |
//! +--------+---------+------------------------+
//! ```
//!
//! The magic bytes distinguish "this is not an index" from "this is a corrupt
//! index", and the version turns a silent misparse into a clear error whenever
//! the layout changes.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::analyzer::Analyzer;
use crate::error::{Error, Result};
use crate::index::Index;

/// Bumped whenever the serialised layout stops being backward compatible.
pub const FORMAT_VERSION: u32 = 1;

const MAGIC: [u8; 6] = *b"FAROL1";

#[derive(Serialize, Deserialize)]
struct Envelope {
    magic: [u8; 6],
    version: u32,
    index: Index,
}

/// Writes `index` to `path`, replacing any previous file.
///
/// The write goes to a sibling temporary file first and is then renamed, so an
/// interrupted save can never leave a half written index where a valid one was.
pub fn save(index: &Index, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let envelope = Envelope {
        magic: MAGIC,
        version: FORMAT_VERSION,
        index: index.clone(),
    };
    let bytes = bincode::serialize(&envelope).map_err(Error::Encode)?;

    let tmp = path.with_extension("farol.tmp");
    let mut file = fs::File::create(&tmp).map_err(|source| Error::Io {
        path: tmp.clone(),
        source,
    })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| Error::Io {
            path: tmp.clone(),
            source,
        })?;
    fs::rename(&tmp, path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Reads an index from `path` and rebinds `analyzer` to it.
///
/// The analyzer is not serialised: stopword lists are configuration, not data,
/// and pinning them into the file would make them impossible to change without
/// rebuilding the index. The caller is responsible for passing the same
/// analyzer that was used to build it.
pub fn load(path: impl AsRef<Path>, analyzer: Analyzer) -> Result<Index> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;

    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::Query(format!(
            "`{}` is not a farol index",
            path.display()
        )));
    }

    let envelope: Envelope = bincode::deserialize(&bytes).map_err(|source| Error::Decode {
        path: path.to_path_buf(),
        source,
    })?;
    if envelope.version != FORMAT_VERSION {
        return Err(Error::Version {
            found: envelope.version,
            expected: FORMAT_VERSION,
        });
    }

    let mut index = envelope.index;
    index.set_analyzer(analyzer);
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;
    use crate::searcher::Searcher;

    fn sample() -> Index {
        let mut index = Index::new(Analyzer::default());
        index.add(
            "a.txt",
            "Rust",
            "rust gives memory safety without a collector",
        );
        index.add("b.txt", "Go", "go gives goroutines and a garbage collector");
        index.finish();
        index
    }

    #[test]
    fn an_index_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.farol");

        save(&sample(), &path).unwrap();
        let loaded = load(&path, Analyzer::default()).unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.avg_doc_len(), sample().avg_doc_len());
        assert_eq!(loaded.document(1).unwrap().title, "Go");
    }

    #[test]
    fn a_reloaded_index_still_answers_queries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.farol");
        save(&sample(), &path).unwrap();

        let index = load(&path, Analyzer::default()).unwrap();
        let query = Query::parse("collector", index.analyzer()).unwrap();
        assert_eq!(Searcher::new(&index).search(&query, 10).len(), 2);
    }

    #[test]
    fn saving_twice_replaces_the_file_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.farol");

        save(&sample(), &path).unwrap();
        save(&Index::new(Analyzer::default()), &path).unwrap();

        assert!(load(&path, Analyzer::default()).unwrap().is_empty());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary file was not cleaned up");
    }

    #[test]
    fn a_foreign_file_is_rejected_before_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "just some text").unwrap();

        let err = load(&path, Analyzer::default()).unwrap_err();
        assert!(err.to_string().contains("is not a farol index"));
    }

    #[test]
    fn a_future_format_version_is_reported_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.farol");
        let envelope = Envelope {
            magic: MAGIC,
            version: FORMAT_VERSION + 1,
            index: sample(),
        };
        fs::write(&path, bincode::serialize(&envelope).unwrap()).unwrap();

        let err = load(&path, Analyzer::default()).unwrap_err();
        assert!(matches!(err, Error::Version { .. }), "{err}");
    }

    #[test]
    fn a_truncated_index_is_a_decode_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.farol");
        save(&sample(), &path).unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        assert!(matches!(
            load(&path, Analyzer::default()).unwrap_err(),
            Error::Decode { .. }
        ));
    }
}
