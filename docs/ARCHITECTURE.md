# Architecture

This document records the design decisions behind `farol` and the reason for
each one. The README shows what the program does; this is why it does it that
way.

## Overview

The system is a pipeline. No stage knows about the previous one, and all of them
are independent enough to be tested alone.

```text
indexing:  files  → analyzer → shards → merge → index → disk
querying:  string → parser   → clauses → filter → BM25 → snippet
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

### 5. Snippets re-analyze the document

The alternative would be storing per-term offsets in the index, growing the
index to benefit only the ten documents actually displayed. Re-analyzing costs
one pass over a handful of documents, at the moment they have already been
chosen.

The window is picked by number of **distinct** terms, not by total occurrences:
a window repeating the same word ten times explains the match less than one
showing three different query words.

### 6. Parallel indexing without losing reproducibility

Analyzing text is CPU work over independent inputs — the ideal case for data
parallelism. Files are split into chunks, each chunk builds its own index, and
the shards are merged at the end.

Order matters: paths are sorted before the split and shards are merged in that
order, so document ids are identical to a sequential run. Without that, score
ties would break differently on every run and no output test would be stable.

### 7. The index file describes itself

Magic bytes and a format version sit in front of the payload. The former tell
"this is not an index" apart from "this index is corrupt"; the latter turns a
layout change into a clear error instead of a silent misread.

Writing goes through a temporary file and ends in a `rename`, which is atomic
within one filesystem. An interruption mid-write leaves the old index intact.

The analyzer is **not** serialised. A stopword list is configuration, not data:
freezing it into the file would make it impossible to change without reindexing
everything.

### 8. An unreadable file aborts indexing

The opposite — skip and carry on — would produce a silently incomplete index. A
search engine that lies about its coverage is worse than one that refuses to
start.

## Complexity

| Operation | Cost |
|-----------|------|
| Index one document | O(t), t = number of terms |
| Merge one shard | O(p), p = postings in the shard |
| Term query | O(df) to collect + O(k) for the output ranking |
| Intersection of n required terms | O(Σ df) with hash sets |
| Phrase of n terms | O(df_rarest × n × log(tf)) — binary search per position |
| Snippet | O(t) per displayed document |

## What was left out, and why

- **Posting compression** (delta + varint): would shrink the index considerably,
  but would require decoding on every read. Without a genuinely large corpus to
  measure against, it would be speculative optimisation.
- **Incremental updates**: require immutable segments with background merging,
  tombstones for deletion and a commit manager. That is another project.
- **WAND / block-max**: only pays off when `k ≪ number of candidates`, a regime
  this corpus never reaches.
- **Prefix search and typo tolerance**: need a different structure (an FST or a
  Levenshtein automaton) alongside the inverted index.
