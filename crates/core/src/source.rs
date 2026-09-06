//! Where an index lives: in memory, or in a mapped file.
//!
//! Both forms answer the same questions, so everything above this module — the
//! searcher, the pruning loop, the engine — is written against [`IndexSource`]
//! and never learns which one it is talking to. The distinction is deliberately
//! an enum rather than a trait: there are exactly two cases, they are known
//! here, and dispatch stays a match instead of a virtual call on the hot path.

use std::ops::{Deref, DerefMut};

use crate::analyzer::Analyzer;
use crate::error::{Error, Result};
use crate::index::{Building, DocId, DocumentRef, Index, Posting, TermRef};
use crate::mmap::MappedIndex;

/// An index the engine can search.
#[derive(Debug)]
pub enum IndexSource {
    /// Built in this process, or deserialised from a [`store`](crate::store)
    /// file. Writable.
    Memory(Index),
    /// Read in place from a mapped file. Read-only, and cheap to open.
    Mapped(Box<MappedIndex>),
}

impl Default for IndexSource {
    fn default() -> Self {
        Self::Memory(Index::default())
    }
}

impl From<Index> for IndexSource {
    fn from(index: Index) -> Self {
        Self::Memory(index)
    }
}

impl From<MappedIndex> for IndexSource {
    fn from(index: MappedIndex) -> Self {
        Self::Mapped(Box::new(index))
    }
}

impl IndexSource {
    /// The analyzer terms were produced with. Queries must use the same one.
    pub fn analyzer(&self) -> &Analyzer {
        match self {
            Self::Memory(index) => index.analyzer(),
            Self::Mapped(index) => index.analyzer(),
        }
    }

    /// Opens an editing session over the in-memory index.
    ///
    /// A mapped index is read-only: adding to it would mean writing through the
    /// mapping, and the whole point of that format is that it is laid out for
    /// reading. Callers that need to index get an error naming the reason —
    /// raised *before* anything is moved out of `self`, so a failed edit leaves
    /// the source exactly as it was.
    ///
    /// The returned [`Edit`] seals the index and puts it back when it is
    /// dropped, including on an early return through `?`. There is no code path
    /// that leaves a source holding a half-built index, because there is no
    /// code path that skips a destructor.
    pub fn edit(&mut self) -> Result<Edit<'_>> {
        if let Self::Mapped(index) = self {
            return Err(Error::Query(format!(
                "`{}` is a memory-mapped index and is read-only; rebuild it with `farol index`",
                index.path().display()
            )));
        }
        let Self::Memory(index) = std::mem::take(self) else {
            unreachable!("the mapped case returned above");
        };
        Ok(Edit {
            source: self,
            index: Some(index.edit()),
        })
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self, Self::Mapped(_))
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Memory(index) => index.len(),
            Self::Mapped(index) => index.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn avg_doc_len(&self) -> f32 {
        match self {
            Self::Memory(index) => index.avg_doc_len(),
            Self::Mapped(index) => index.avg_doc_len(),
        }
    }

    pub fn vocabulary_size(&self) -> usize {
        match self {
            Self::Memory(index) => index.vocabulary_size(),
            Self::Mapped(index) => index.vocabulary_size(),
        }
    }

    pub fn total_postings(&self) -> usize {
        match self {
            Self::Memory(index) => index.total_postings(),
            Self::Mapped(index) => index.total_postings(),
        }
    }

    pub fn postings_bytes(&self) -> usize {
        match self {
            Self::Memory(index) => index.postings_bytes(),
            Self::Mapped(index) => index.postings_bytes(),
        }
    }

    pub fn term(&self, term: &str) -> Option<TermRef<'_>> {
        match self {
            Self::Memory(index) => index.term(term),
            Self::Mapped(index) => index.term(term),
        }
    }

    pub fn postings(&self, term: &str) -> Option<Vec<Posting>> {
        match self {
            Self::Memory(index) => index.postings(term),
            Self::Mapped(index) => index.postings(term),
        }
    }

    pub fn doc_freq(&self, term: &str) -> u32 {
        match self {
            Self::Memory(index) => index.doc_freq(term),
            Self::Mapped(index) => index.doc_freq(term),
        }
    }

    /// Every document, in id order.
    pub fn documents(&self) -> impl Iterator<Item = DocumentRef<'_>> {
        (0..self.len() as DocId).filter_map(|id| self.document(id))
    }

    pub fn document(&self, id: DocId) -> Option<DocumentRef<'_>> {
        match self {
            Self::Memory(index) => index.document(id),
            Self::Mapped(index) => index.document(id),
        }
    }
}

/// An editing session over an [`IndexSource`].
///
/// Derefs to the index being built, and on drop seals it and writes it back to
/// the source it came from.
#[derive(Debug)]
pub struct Edit<'a> {
    source: &'a mut IndexSource,
    /// `Some` until the session ends; the `Option` exists only so `Drop` can
    /// move the index out without leaving anything invalid behind.
    index: Option<Index<Building>>,
}

impl Edit<'_> {
    /// Ends the session explicitly. Equivalent to dropping it, and clearer at a
    /// call site where the sealing matters.
    pub fn commit(self) {}
}

impl Deref for Edit<'_> {
    type Target = Index<Building>;

    fn deref(&self) -> &Self::Target {
        self.index.as_ref().expect("index is taken only on drop")
    }
}

impl DerefMut for Edit<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.index.as_mut().expect("index is taken only on drop")
    }
}

impl Drop for Edit<'_> {
    fn drop(&mut self) {
        if let Some(index) = self.index.take() {
            *self.source = IndexSource::Memory(index.seal());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Sealed;
    use crate::mmap;

    fn built() -> Index<Sealed> {
        let mut index = Index::new(Analyzer::raw());
        index.add("a", "A", "rust is fast and safe");
        index.add("b", "B", "rust ranks documents by relevance");
        index.seal()
    }

    #[test]
    fn both_sources_answer_identically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.farolmm");
        let index = built();
        mmap::write(&index, &path).unwrap();

        let memory = IndexSource::from(built());
        let mapped = IndexSource::from(MappedIndex::open(&path, Analyzer::raw()).unwrap());

        assert_eq!(memory.len(), mapped.len());
        assert_eq!(memory.avg_doc_len(), mapped.avg_doc_len());
        assert_eq!(memory.vocabulary_size(), mapped.vocabulary_size());
        assert_eq!(memory.doc_freq("rust"), mapped.doc_freq("rust"));
        assert_eq!(
            memory.document(1).unwrap().title,
            mapped.document(1).unwrap().title
        );
        assert_eq!(
            memory.term("rust").unwrap().blocks().len(),
            mapped.term("rust").unwrap().blocks().len()
        );
    }

    #[test]
    fn a_mapped_source_refuses_to_be_written_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.farolmm");
        mmap::write(&built(), &path).unwrap();

        let mut mapped = IndexSource::from(MappedIndex::open(&path, Analyzer::raw()).unwrap());
        let err = mapped.edit().unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
        assert!(
            mapped.is_mapped(),
            "a failed edit must leave the source intact"
        );
        assert_eq!(mapped.len(), 2);
    }

    #[test]
    fn a_memory_source_can_still_be_extended() {
        let mut memory = IndexSource::from(built());
        memory.edit().unwrap().add("c", "C", "more rust");
        assert_eq!(memory.len(), 3, "dropping the session seals it back");
        assert_eq!(memory.doc_freq("rust"), 3);
    }

    #[test]
    fn an_edit_is_sealed_back_even_when_the_caller_returns_early() {
        fn edit_then_fail(source: &mut IndexSource) -> Result<()> {
            let mut edit = source.edit()?;
            edit.add("c", "C", "more rust");
            // Something goes wrong halfway through the update.
            Err(Error::Query("boom".into()))
        }

        let mut memory = IndexSource::from(built());
        assert!(edit_then_fail(&mut memory).is_err());
        assert_eq!(memory.len(), 3, "the session must still have been sealed");
        assert!(memory.term("rust").is_some(), "and the index is searchable");
    }
}
