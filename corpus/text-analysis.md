# Text analysis

The analyzer is the contract between indexing and querying. If the two sides
produce different terms for the same word, search simply finds nothing, and the
failure is silent.

Normalization folds accents and case, so that acao, AÇÃO and ação collapse into
the same term. Stopwords — articles, prepositions, conjunctions — are dropped
because they appear in almost every document and inflate the posting lists
without adding signal.

Stemming strips suffixes to bring inflections of the same root together. It is a
heuristic, not a morphological analysis: the only goal is that plural and
singular land on the same term on both sides of the index.
