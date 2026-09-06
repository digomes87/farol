# Ranking with BM25

Finding documents is the easy part. Ordering them is the real problem.

BM25 combines three signals. Term frequency counts, but with diminishing
returns: ten occurrences of rust do not make a text ten times more relevant. The
rarity of the term across the collection counts even more, because a word
present in every document distinguishes nothing. And document length enters as a
penalty, since one occurrence inside a note is stronger evidence than the same
occurrence inside a book.

The k1 parameter controls frequency saturation. The b parameter controls how
much length normalisation weighs: at zero, long and short documents compete on
equal footing.
