//! A memory-mapped index format.
//!
//! [`store`](crate::store) serialises the whole index and reads it back the
//! same way: opening a 100 MB index means allocating 100 MB and decoding every
//! posting list before answering the first query. That is fine for a corpus of
//! notes and wrong for anything larger, because a query touches a handful of
//! terms and the rest of that work is thrown away.
//!
//! This module writes a layout designed to be *read in place*. The file is
//! mapped into the address space, the operating system pages in only the bytes
//! actually touched, and every lookup is an offset computation over those
//! bytes — no allocation, no decoding of anything but the blocks a query walks.
//!
//! ```text
//! ┌────────┬───────────┬────────┬──────────┬───────────┬─────────┐
//! │ header │ term dict │ blocks │ postings │ documents │ strings │
//! └────────┴───────────┴────────┴──────────┴───────────┴─────────┘
//!      ▲         ▲          ▲        ▲           ▲          ▲
//!      │         │          │        │           │          └ utf8 blob
//!      │         │          │        │           └ fixed 28-byte records
//!      │         │          │        └ delta+varint bytes, as in memory
//!      │         │          └ fixed 20-byte block metadata
//!      │         └ fixed 32-byte entries, sorted by term for binary search
//!      └ magic, version, counts and section offsets
//! ```
//!
//! Every section holds fixed size records or is addressed by an offset stored
//! elsewhere, which is what makes random access possible without parsing what
//! comes before. Integers are little-endian and read through `from_le_bytes`,
//! so the format does not depend on the host's alignment or endianness, and no
//! `unsafe` is involved beyond the mapping itself.

use std::borrow::Cow;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::analyzer::Analyzer;
use crate::error::{Error, Result};
use crate::index::{BlockMeta, DocId, DocumentRef, Index, Posting, TermRef};

const MAGIC: [u8; 8] = *b"FAROLMM\x01";
/// Bumped whenever the mapped layout stops being backward compatible.
pub const MAPPED_VERSION: u32 = 1;

const HEADER_LEN: usize = 76;
const TERM_ENTRY: usize = 32;
const BLOCK_ENTRY: usize = 20;
const DOC_ENTRY: usize = 28;

/// Writes `index` to `path` in the mapped layout.
///
/// Like [`store::save`](crate::store::save), the write lands on a temporary
/// file and is renamed into place, so a crash cannot replace a working index
/// with a truncated one.
pub fn write(index: &Index, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();

    let mut terms: Vec<&str> = index.terms().collect();
    // Sorted so the reader can binary search the dictionary.
    terms.sort_unstable();

    let mut dict = Vec::with_capacity(terms.len() * TERM_ENTRY);
    let mut blocks = Vec::new();
    let mut postings = Vec::new();
    let mut docs = Vec::new();
    let mut strings: Vec<u8> = Vec::new();

    for term in &terms {
        let entry = index.term(term).expect("term came from this index");
        let term_off = push_string(&mut strings, term);
        let block_start = (blocks.len() / BLOCK_ENTRY) as u32;
        let data_off = postings.len() as u32;

        for meta in entry.blocks() {
            blocks.extend_from_slice(&meta.last_doc.to_le_bytes());
            blocks.extend_from_slice(&meta.count.to_le_bytes());
            blocks.extend_from_slice(&meta.offset.to_le_bytes());
            blocks.extend_from_slice(&meta.max_tf.to_le_bytes());
            blocks.extend_from_slice(&meta.min_len.to_le_bytes());
        }
        postings.extend_from_slice(entry.data());

        dict.extend_from_slice(&term_off.to_le_bytes());
        dict.extend_from_slice(&(term.len() as u32).to_le_bytes());
        dict.extend_from_slice(&entry.doc_freq().to_le_bytes());
        dict.extend_from_slice(&entry.max_tf().to_le_bytes());
        dict.extend_from_slice(&entry.min_len().to_le_bytes());
        dict.extend_from_slice(&block_start.to_le_bytes());
        dict.extend_from_slice(&(entry.blocks().len() as u32).to_le_bytes());
        dict.extend_from_slice(&data_off.to_le_bytes());
    }

    for document in index.documents() {
        let uri = push_string(&mut strings, &document.uri);
        let title = push_string(&mut strings, &document.title);
        let text = push_string(&mut strings, &document.text);
        docs.extend_from_slice(&uri.to_le_bytes());
        docs.extend_from_slice(&(document.uri.len() as u32).to_le_bytes());
        docs.extend_from_slice(&title.to_le_bytes());
        docs.extend_from_slice(&(document.title.len() as u32).to_le_bytes());
        docs.extend_from_slice(&text.to_le_bytes());
        docs.extend_from_slice(&(document.text.len() as u32).to_le_bytes());
        docs.extend_from_slice(&document.length.to_le_bytes());
    }

    let off_terms = HEADER_LEN as u64;
    let off_blocks = off_terms + dict.len() as u64;
    let off_postings = off_blocks + blocks.len() as u64;
    let off_docs = off_postings + postings.len() as u64;
    let off_strings = off_docs + docs.len() as u64;
    let end = off_strings + strings.len() as u64;

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&MAPPED_VERSION.to_le_bytes());
    header.extend_from_slice(&(index.len() as u32).to_le_bytes());
    header.extend_from_slice(&(terms.len() as u32).to_le_bytes());
    header.extend_from_slice(&index.total_length().to_le_bytes());
    for offset in [
        off_terms,
        off_blocks,
        off_postings,
        off_docs,
        off_strings,
        end,
    ] {
        header.extend_from_slice(&offset.to_le_bytes());
    }
    debug_assert_eq!(header.len(), HEADER_LEN);

    let tmp = path.with_extension("farolmm.tmp");
    let mut file = fs::File::create(&tmp).map_err(|source| Error::Io {
        path: tmp.clone(),
        source,
    })?;
    let mut write_sections = || -> std::io::Result<()> {
        for section in [&header, &dict, &blocks, &postings, &docs, &strings] {
            file.write_all(section)?;
        }
        file.sync_all()
    };
    write_sections().map_err(|source| Error::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Appends `text` to the string blob, returning its offset.
fn push_string(strings: &mut Vec<u8>, text: &str) -> u32 {
    let offset = strings.len() as u32;
    strings.extend_from_slice(text.as_bytes());
    offset
}

/// An index read directly out of a mapped file.
///
/// Opening one is a `mmap` call and a header check: no allocation proportional
/// to the index, no decoding. Everything else happens lazily, as queries touch
/// pages.
#[derive(Debug)]
pub struct MappedIndex {
    map: Mmap,
    path: PathBuf,
    analyzer: Analyzer,
    doc_count: u32,
    term_count: u32,
    total_length: u64,
    off_terms: usize,
    off_blocks: usize,
    off_postings: usize,
    off_docs: usize,
    off_strings: usize,
}

impl MappedIndex {
    /// Maps the index at `path`, checking the header before trusting anything.
    ///
    /// # Safety and expectations
    ///
    /// Mapping a file is only sound while the file does not change underneath
    /// the mapping: another process truncating it turns reads into `SIGBUS`.
    /// The engine writes indexes through a temporary file and a rename, which
    /// leaves this mapping pointing at the old inode instead of a mutating one,
    /// so the normal write path is safe. Editing an index file in place while
    /// it is mapped is not supported.
    pub fn open(path: impl AsRef<Path>, analyzer: Analyzer) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = fs::File::open(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        // SAFETY: the file is opened read-only and, per the contract above, is
        // never modified in place while mapped.
        let map = unsafe { Mmap::map(&file) }.map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;

        if map.len() < HEADER_LEN || map[..8] != MAGIC {
            return Err(Error::Query(format!(
                "`{}` is not a mapped farol index",
                path.display()
            )));
        }
        let version = read_u32(&map, 8);
        if version != MAPPED_VERSION {
            return Err(Error::Version {
                found: version,
                expected: MAPPED_VERSION,
            });
        }

        let index = Self {
            doc_count: read_u32(&map, 12),
            term_count: read_u32(&map, 16),
            total_length: read_u64(&map, 20),
            off_terms: read_u64(&map, 28) as usize,
            off_blocks: read_u64(&map, 36) as usize,
            off_postings: read_u64(&map, 44) as usize,
            off_docs: read_u64(&map, 52) as usize,
            off_strings: read_u64(&map, 60) as usize,
            path,
            analyzer,
            map,
        };

        let end = read_u64(&index.map, 68) as usize;
        if end > index.map.len() || index.off_strings > index.map.len() {
            return Err(Error::Query(format!(
                "`{}` is truncated: header describes {end} bytes, file has {}",
                index.path.display(),
                index.map.len()
            )));
        }
        Ok(index)
    }

    pub fn analyzer(&self) -> &Analyzer {
        &self.analyzer
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.doc_count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.doc_count == 0
    }

    pub fn vocabulary_size(&self) -> usize {
        self.term_count as usize
    }

    pub fn avg_doc_len(&self) -> f32 {
        if self.doc_count == 0 {
            return 0.0;
        }
        self.total_length as f32 / self.doc_count as f32
    }

    /// Bytes of compressed posting data, straight from the section boundaries.
    pub fn postings_bytes(&self) -> usize {
        self.off_docs - self.off_postings
    }

    pub fn total_postings(&self) -> usize {
        (0..self.term_count)
            .map(|entry| {
                read_u32(&self.map, self.off_terms + entry as usize * TERM_ENTRY + 8) as usize
            })
            .sum()
    }

    /// Reads the term at dictionary slot `slot`.
    fn term_at(&self, slot: usize) -> &str {
        let base = self.off_terms + slot * TERM_ENTRY;
        let offset = read_u32(&self.map, base) as usize;
        let len = read_u32(&self.map, base + 4) as usize;
        self.string(offset, len)
    }

    fn string(&self, offset: usize, len: usize) -> &str {
        let start = self.off_strings + offset;
        let end = (start + len).min(self.map.len());
        std::str::from_utf8(&self.map[start.min(end)..end]).unwrap_or_default()
    }

    /// Finds a term by binary search over the sorted dictionary.
    pub fn term(&self, term: &str) -> Option<TermRef<'_>> {
        let mut low = 0usize;
        let mut high = self.term_count as usize;
        while low < high {
            let mid = (low + high) / 2;
            match self.term_at(mid).cmp(term) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Some(self.term_ref(mid)),
            }
        }
        None
    }

    /// Builds a [`TermRef`] over the mapped bytes of dictionary slot `slot`.
    ///
    /// The block table is parsed into an owned vector — a handful of entries
    /// for a typical term — while the postings themselves stay borrowed from
    /// the mapping and are never copied.
    fn term_ref(&self, slot: usize) -> TermRef<'_> {
        let base = self.off_terms + slot * TERM_ENTRY;
        let doc_freq = read_u32(&self.map, base + 8);
        let max_tf = read_u32(&self.map, base + 12);
        let min_len = read_u32(&self.map, base + 16);
        let block_start = read_u32(&self.map, base + 20) as usize;
        let block_count = read_u32(&self.map, base + 24) as usize;
        let data_off = read_u32(&self.map, base + 28) as usize;

        let blocks: Vec<BlockMeta> = (0..block_count)
            .map(|idx| {
                let at = self.off_blocks + (block_start + idx) * BLOCK_ENTRY;
                BlockMeta {
                    last_doc: read_u32(&self.map, at),
                    count: read_u32(&self.map, at + 4),
                    offset: read_u32(&self.map, at + 8),
                    max_tf: read_u32(&self.map, at + 12),
                    min_len: read_u32(&self.map, at + 16),
                }
            })
            .collect();

        let data_start = self.off_postings + data_off;
        let data = &self.map[data_start.min(self.off_docs)..self.off_docs];
        TermRef::new(Cow::Owned(blocks), data, doc_freq, max_tf, min_len)
    }

    /// Decoded posting list for `term`.
    pub fn postings(&self, term: &str) -> Option<Vec<Posting>> {
        self.term(term).map(|entry| entry.decode_all())
    }

    pub fn doc_freq(&self, term: &str) -> u32 {
        self.term(term).map_or(0, |entry| entry.doc_freq())
    }

    /// Reads one document record. Strings point straight into the mapping.
    pub fn document(&self, id: DocId) -> Option<DocumentRef<'_>> {
        if id >= self.doc_count {
            return None;
        }
        let base = self.off_docs + id as usize * DOC_ENTRY;
        Some(DocumentRef {
            id,
            uri: self.string(
                read_u32(&self.map, base) as usize,
                read_u32(&self.map, base + 4) as usize,
            ),
            title: self.string(
                read_u32(&self.map, base + 8) as usize,
                read_u32(&self.map, base + 12) as usize,
            ),
            text: self.string(
                read_u32(&self.map, base + 16) as usize,
                read_u32(&self.map, base + 20) as usize,
            ),
            length: read_u32(&self.map, base + 24),
        })
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .and_then(|slice| slice.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    bytes
        .get(at..at + 8)
        .and_then(|slice| slice.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;

    fn sample() -> Index {
        let mut index = Index::new(Analyzer::raw());
        index.add(
            "a.txt",
            "Rust",
            "rust gives memory safety without a collector",
        );
        index.add("b.txt", "Go", "go gives goroutines and a garbage collector");
        index.add("c.txt", "Zig", "zig gives manual memory management");
        index.finish();
        index
    }

    fn mapped(index: &Index, dir: &Path) -> MappedIndex {
        let path = dir.join("corpus.farolmm");
        write(index, &path).unwrap();
        MappedIndex::open(&path, Analyzer::raw()).unwrap()
    }

    #[test]
    fn a_mapped_index_answers_like_the_one_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let index = sample();
        let mapped = mapped(&index, dir.path());

        assert_eq!(mapped.len(), index.len());
        assert_eq!(mapped.vocabulary_size(), index.vocabulary_size());
        assert_eq!(mapped.avg_doc_len(), index.avg_doc_len());
        assert_eq!(mapped.total_postings(), index.total_postings());
        assert_eq!(mapped.postings_bytes(), index.postings_bytes());
    }

    #[test]
    fn documents_are_read_straight_from_the_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let index = sample();
        let mapped = mapped(&index, dir.path());

        for id in 0..index.len() as DocId {
            let expected = index.document(id).unwrap();
            let got = mapped.document(id).unwrap();
            assert_eq!(got, expected, "document {id}");
        }
        assert!(mapped.document(99).is_none());
    }

    #[test]
    fn posting_lists_decode_to_the_same_values() {
        let dir = tempfile::tempdir().unwrap();
        let index = sample();
        let mapped = mapped(&index, dir.path());

        for term in ["rust", "gives", "collector", "memory"] {
            let expected = index.postings(term).unwrap();
            let got = mapped.postings(term).unwrap();
            assert_eq!(got.len(), expected.len(), "term {term}");
            for (a, b) in got.iter().zip(&expected) {
                assert_eq!(a.doc, b.doc);
                assert_eq!(a.positions, b.positions);
            }
        }
        assert!(mapped.postings("kubernetes").is_none());
    }

    #[test]
    fn score_bounds_survive_the_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let index = sample();
        let mapped = mapped(&index, dir.path());

        let expected = index.term("gives").unwrap();
        let got = mapped.term("gives").unwrap();
        assert_eq!(got.doc_freq(), expected.doc_freq());
        assert_eq!(got.max_tf(), expected.max_tf());
        assert_eq!(got.min_len(), expected.min_len());
        assert_eq!(got.blocks().len(), expected.blocks().len());
    }

    #[test]
    fn the_dictionary_binary_search_finds_every_term() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = Index::new(Analyzer::raw());
        for id in 0..500 {
            index.add(format!("d{id}"), "D", &format!("term{id} shared"));
        }
        index.finish();
        let mapped = mapped(&index, dir.path());

        for id in 0..500 {
            assert!(
                mapped.term(&format!("term{id}")).is_some(),
                "term{id} not found"
            );
        }
        assert!(mapped.term("term500").is_none());
        assert_eq!(mapped.doc_freq("shared"), 500);
    }

    #[test]
    fn queries_run_against_a_mapped_index() {
        let dir = tempfile::tempdir().unwrap();
        let mapped = mapped(&sample(), dir.path());
        let query = Query::parse("collector", mapped.analyzer()).unwrap();
        let terms = query.positive_terms();
        assert_eq!(mapped.doc_freq(terms[0]), 2);
    }

    #[test]
    fn a_foreign_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "not an index at all, just some prose").unwrap();

        let err = MappedIndex::open(&path, Analyzer::raw()).unwrap_err();
        assert!(err.to_string().contains("not a mapped farol index"));
    }

    #[test]
    fn a_truncated_file_is_rejected_instead_of_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.farolmm");
        write(&sample(), &path).unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        let err = MappedIndex::open(&path, Analyzer::raw()).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }

    #[test]
    fn an_empty_index_maps_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let mapped = mapped(&Index::new(Analyzer::raw()), dir.path());
        assert!(mapped.is_empty());
        assert_eq!(mapped.avg_doc_len(), 0.0);
        assert!(mapped.term("anything").is_none());
    }
}
