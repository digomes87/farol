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
                                      ↳ optional terms → WAND → top-k
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

### 5. Dynamic pruning is opt-in by query shape

The top-k of a query is decided by a handful of documents, yet exhaustive
scoring pays for all of them. WAND avoids that: the cursors are kept sorted by
the document they point at, their upper bounds are accumulated in that order,
and the first term whose running sum exceeds the current threshold marks the
*pivot*. No document before the pivot's current document can beat the threshold,
because the only terms positioned there are the ones whose bounds have not added
up yet — so the loop either scores the pivot document or skips the lagging
cursors straight to it.

Three details make the implementation honest:

- **The bounds live in the index, not in the query.** Each term stores the
  highest frequency and the shortest document length among its postings. BM25 is
  monotonic in both, so the pair is a valid bound for any `k1`/`b`, and runtime
  tuning of the ranking cannot invalidate it.
- **The skip is a galloping search**, not a linear scan or a binary search over
  the whole tail. A query mixes rare terms (long skips) with frequent ones
  (short skips) and galloping is good at both.
- **The fallback is explicit.** Required, excluded and phrase clauses take the
  exhaustive path, because they constrain the candidate set more aggressively
  than pruning would. `Searcher::with_pruning(false)` forces that path for any
  query, which is what lets the benchmark compare the two over an identical
  candidate set.

Equivalence is a test, not a claim: for a range of queries and `k` values, the
pruned ranking is compared document by document against exhaustive scoring.

### 6. Snippets re-analyze the document

The alternative would be storing per-term offsets in the index, growing the
index to benefit only the ten documents actually displayed. Re-analyzing costs
one pass over a handful of documents, at the moment they have already been
chosen.

The window is picked by number of **distinct** terms, not by total occurrences:
a window repeating the same word ten times explains the match less than one
showing three different query words.

### 7. Parallel indexing without losing reproducibility

Analyzing text is CPU work over independent inputs — the ideal case for data
parallelism. Files are split into chunks, each chunk builds its own index, and
the shards are merged at the end.

Order matters: paths are sorted before the split and shards are merged in that
order, so document ids are identical to a sequential run. Without that, score
ties would break differently on every run and no output test would be stable.

### 8. The index file describes itself

Magic bytes and a format version sit in front of the payload. The former tell
"this is not an index" apart from "this index is corrupt"; the latter turns a
layout change into a clear error instead of a silent misread.

Writing goes through a temporary file and ends in a `rename`, which is atomic
within one filesystem. An interruption mid-write leaves the old index intact.

The analyzer is **not** serialised. A stopword list is configuration, not data:
freezing it into the file would make it impossible to change without reindexing
everything.

### 9. An unreadable file aborts indexing

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
| Skip to a document | O(log distance) — galloping then binary search |
| Intersection of n required terms | O(Σ df) with hash sets |
| Phrase of n terms | O(df_rarest × n × log(tf)) — binary search per position |
| Snippet | O(t) per displayed document |

## What was left out, and why

- **Posting compression** (delta + varint): would shrink the index considerably,
  but would require decoding on every read. Without a genuinely large corpus to
  measure against, it would be speculative optimisation.
- **Incremental updates**: require immutable segments with background merging,
  tombstones for deletion and a commit manager. That is another project.
- **Block-max WAND**: refines the bounds per block of postings instead of per
  term, which prunes harder. It needs the compressed block layout above to be
  worth it, so the two go together or not at all.
- **Prefix search and typo tolerance**: need a different structure (an FST or a
  Levenshtein automaton) alongside the inverted index.
