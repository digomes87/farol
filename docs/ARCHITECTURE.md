# Architecture

This document records the design decisions behind `farol` and the reason for
each one. The README shows what the program does; this is why it does it that
way.

## Overview

The system is a pipeline. No stage knows about the previous one, and all of them
are independent enough to be tested alone.

```text
indexing:  files  → analyzer → shards → merge → compress → index → disk
querying:  string → parser   → clauses → filter → BM25 → snippet
                                      ↳ optional terms → block-max WAND → top-k
reading:   file   → mmap     → offsets → borrowed terms and documents
```

The `farol-core` crate holds the entire pipeline and does not depend on the CLI.
The `farol-cli` crate only deals with arguments, formatting and colors.

## Decisions

### 1. The analyzer is the contract between indexing and querying

If indexing produces `coracao` while querying produces `coracoes`, search finds
nothing — and the failure is silent, which is the worst category of bug in
search. That is why there is a single `Analyzer` type, used on both sides, and
the index holds a reference to it.

Stemming runs in two passes. The first collapses the inflection
(`migracoes → migracao`), the second the derivation (`migracao → migr`). With a
single pass, plural and singular land on different terms: the plural comes out as
`migracao` and the singular as `migr`. An idempotence test
(`stem(stem(x)) == stem(x)`) protects that property.

### 2. Positions are stored per posting

They cost space, and they are what separates "both words are in this document"
from "both words are side by side". Without them, exact phrase search is
impossible.

Positions are assigned **before** stopword removal. So in `"song of a siren"`
the terms `song` and `siren` stay three positions apart in the document and in
the query alike, and the phrase still matches even though the function words are
dropped on both sides.

### 3. Smoothed probabilistic IDF

```text
idf = ln(1 + (N - df + 0.5) / (df + 0.5))
```

The `+1` inside the logarithm is not decoration. The raw probabilistic formula
goes negative for terms present in more than half of the collection, and a
negative IDF lets a common term *subtract* points from a document containing it.
With the smoothing, the worst case is contributing zero.

### 4. Candidates come from the intersection, not the union

When the query has a required clause, the candidate set is the intersection of
the corresponding lists. Scoring only the survivors is substantially cheaper than
scoring the union and discarding afterwards — the benchmark shows 10 µs against
43 µs for the same query.

Phrase matching follows the same logic: it starts from the posting list of the
rarest term, which bounds how many documents can match, and verifies the rest by
binary search over positions.

### 5. Posting lists are compressed, in blocks

Document ids are sorted, so storing the gaps turns large numbers into small
ones, and LEB128 then makes small numbers cheap. Dense lists fall from four
bytes per id to roughly one; the index file for a 5,000 document corpus goes
from 15.4 MB to 7.9 MB.

Two decisions inside the block format are worth stating:

- **Runs are grouped by kind, not interleaved per posting.** All document gaps
  first, then all frequencies, then the positions. Each run then compresses
  against data of the same shape — gaps are small and similar, frequencies are
  tiny, positions restart per document.
- **Blocks are 128 postings.** Small enough that decoding one to inspect a
  single document is cheap, large enough that the 20 bytes of per-block metadata
  are a rounding error. That metadata is not overhead paid for compression: it
  is what makes skipping possible at all.

Compression is a trade, and the honest version of it is that queries got about
30% slower per posting decoded while the index halved. On a corpus that does not
fit in memory that trade is the whole point; on a small one it is a wash.

### 6. Dynamic pruning is opt-in by query shape

The top-k of a query is decided by a handful of documents, yet exhaustive
scoring pays for all of them. WAND avoids that: the cursors are kept sorted by
the document they point at, their upper bounds are accumulated in that order,
and the first term whose running sum exceeds the current threshold marks the
*pivot*. No document before the pivot's current document can beat the threshold,
because the only terms positioned there are the ones whose bounds have not added
up yet — so the loop either scores the pivot document or skips the lagging
cursors straight to it.

Three details make the implementation honest:

- **The bounds live in the index, not in the query.** Each term — and each block
  within it — stores the highest frequency and the shortest document length
  among its postings. BM25 is monotonic in both, so the pair is a valid bound
  for any `k1`/`b`, and runtime tuning of the ranking cannot invalidate it.
  Block bounds are much tighter than term bounds, which is what turns WAND into
  block-max WAND: a block whose own bound cannot reach the threshold is skipped
  without being decoded.
- **The skip is a galloping search**, not a linear scan or a binary search over
  the whole tail. A query mixes rare terms (long skips) with frequent ones
  (short skips) and galloping is good at both.
- **The fallback is explicit.** Required, excluded and phrase clauses take the
  exhaustive path, because they constrain the candidate set more aggressively
  than pruning would. `Searcher::with_pruning(false)` forces that path for any
  query, which is what lets the benchmark compare the two over an identical
  candidate set.

Equivalence is a test, not a claim: for a range of queries, `k` values and BM25
parameters, the pruned ranking is compared document by document against
exhaustive scoring. That test earned its place — it caught two bugs that each
silently dropped correct results:

- the block bound must come from the block that *would contain* the pivot
  document, not from the block a lagging cursor currently sits on, whose bound
  may be lower;
- the pivot must cover every cursor positioned on the same document, or their
  contribution is missing from the bound being compared.

### 7. Snippets re-analyze the document

The alternative would be storing per-term offsets in the index, growing the
index to benefit only the ten documents actually displayed. Re-analyzing costs
one pass over a handful of documents, at the moment they have already been
chosen.

The window is picked by number of **distinct** terms, not by total occurrences:
a window repeating the same word ten times explains the match less than one
showing three different query words.

### 8. Parallel indexing without losing reproducibility

Analyzing text is CPU work over independent inputs — the ideal case for data
parallelism. Files are split into chunks, each chunk builds its own index, and
the shards are merged at the end.

Order matters: paths are sorted before the split and shards are merged in that
order, so document ids are identical to a sequential run. Without that, score
ties would break differently on every run and no output test would be stable.

### 9. The index file describes itself

Magic bytes and a format version sit in front of the payload. The former tell
"this is not an index" apart from "this index is corrupt"; the latter turns a
layout change into a clear error instead of a silent misread.

Writing goes through a temporary file and ends in a `rename`, which is atomic
within one filesystem. An interruption mid-write leaves the old index intact.

The analyzer is **not** serialised. A stopword list is configuration, not data:
freezing it into the file would make it impossible to change without reindexing
everything.

### 10. Indexes are read in place

Deserialising an index means allocating all of it and decoding every posting
list before answering the first query. A query touches a handful of terms, so
most of that work is discarded — and the cost grows with the corpus, which is
exactly the wrong direction.

The mapped layout is designed for random access instead: fixed-size records, a
term dictionary sorted for binary search, and section offsets in the header.
Nothing needs to be parsed to reach anything else. On an 80 MB index the
difference is 2.8 ms and 6.7 MB of memory against 39.9 ms and 190 MB.

Three choices keep it defensible:

- **No `unsafe` beyond the mapping call.** Integers are read with
  `from_le_bytes` over byte slices, so the format does not depend on host
  alignment or endianness, and no struct is ever transmuted from bytes.
- **The soundness contract is written down.** A mapping is only sound while the
  file does not change underneath it. Index writes go through a temporary file
  and a rename, which leaves an open mapping pointing at the old inode, so the
  normal path is safe; editing an index in place while it is mapped is
  documented as unsupported.
- **The two storages meet behind an enum, not a trait.** There are exactly two
  cases and both are known here, so `IndexSource` dispatches with a match and
  the searcher never becomes generic over storage. `TermRef` carries its block
  table in a `Cow`, which is what lets an in-memory index lend a slice while a
  mapped one parses and owns the same few entries.

### 11. Illegal states are unrepresentable, not merely undocumented

`finish()` used to be a rule: build the index, remember to call it, then search.
The rule was forgotten in this repository's own doc example, which asserted a
document frequency of zero on a perfectly good index.

The state is now a type parameter — `Index<Building>` and `Index<Sealed>` — with
`seal()`/`edit()` consuming the index to move between them, so a writable handle
never coexists with a searchable one. The `State` trait is sealed through a
private supertrait, so no downstream crate can add a third state and void the
guarantee, and two `compile_fail` doc tests assert that the wrong programs do not
build.

The same reasoning produced `IndexSource::edit`, an RAII session that seals the
index back into the source when it drops — including on an early return through
the question-mark operator. There is no path that leaves a source holding a
half-built index, because there is no path that skips a destructor.

### 12. Publishing a new index must not stop the old one

A rebuild takes minutes and a query takes microseconds, so the two cannot share
a lock. `SearchService` builds the replacement beside the live index and
publishes it with one pointer swap; in-flight queries hold an `Arc` to the
generation they started with and finish against it.

The primitive underneath, `SnapshotCell`, is where the real care went. Its read
path is a pointer load followed by a reference-count bump, and the window
between those two steps is the classic hole in every hand-rolled atomic `Arc`: a
writer can swap and drop the last reference in between, turning the reader's
increment into a write to freed memory.

The first implementation closed that window with an atomic reader counter —
announce, load, count, leave; reclaim when the writer observes zero. `loom`
rejected it. The proof obligation is Dekker's, and it holds only if the pointer
swap and the counter read participate in a single total order: true of `SeqCst`
on real hardware, false in loom's model, which deliberately does not implement
full `SeqCst` semantics, and — more to the point — not something a reviewer can
verify by reading four lines.

The response was to change the algorithm rather than argue for it. Readers now
hold a shared guard across load-and-increment, and superseded values are dropped
only by a thread holding that guard exclusively, acquired with `try_write`. The
safety argument became one sentence: nothing is freed except under exclusive
access. Readers still never block each other, and because reclamation only ever
*tries*, a rebuild never blocks a query — a failed attempt just leaves the value
retired until the next quiet moment.

Both properties are checked rather than asserted: loom explores every
interleaving of readers and writers, and the same code runs under Miri with
strict provenance to catch undefined behaviour in the raw reference-count
manipulation. Both run in CI.

### 13. An unreadable file aborts indexing

The opposite — skip and carry on — would produce a silently incomplete index. A
search engine that lies about its coverage is worse than one that refuses to
start.

## Complexity

| Operation | Cost |
|-----------|------|
| Index one document | O(t), t = number of terms |
| Merge one shard | O(p), p = postings in the shard |
| Term query, exhaustive | O(df) to collect + O(k) for the ranking |
| Term query, pruned | O(scored × terms × log df) — `scored ≪ df` in practice |
| Skip to a document | O(blocks skipped) + O(log 128) inside the landing block |
| Term lookup, mapped | O(log vocabulary) — binary search over the dictionary |
| Opening an index, mapped | O(1) — one `mmap` and a header check |
| Reading the live index | one uncontended shared guard, no writer can block it |
| Publishing a new index | one pointer swap, independent of index size |
| Intersection of n required terms | O(Σ df) with hash sets |
| Phrase of n terms | O(df_rarest × n × log(tf)) — binary search per position |
| Snippet | O(t) per displayed document |

## What was left out, and why

- **Incremental updates**: require immutable segments with background merging,
  tombstones for deletion and a commit manager. That is another project.
- **SIMD block decoding** (PFOR-delta and friends): the usual next step for
  posting compression, and the point where the code stops being readable without
  a benchmark to justify every line.
- **Incremental updates**: require immutable segments with background merging,
  tombstones for deletion and a commit manager. That is another project.
- **Prefix search and typo tolerance**: need a different structure (an FST or a
  Levenshtein automaton) alongside the inverted index.
