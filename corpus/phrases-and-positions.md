# Phrases and positions

Exact phrase search verifies adjacency. Each term of the phrase carries an
offset relative to its beginning, and a document only matches when the positions
stored in the index reproduce those offsets exactly.

Storing offsets instead of a plain sequence solves an awkward case: when a
stopword is removed inside the quotes, it leaves a hole. Since the same hole
exists in the indexed document, the phrase still matches.

The check starts from the posting list of the rarest term in the phrase. It
bounds how many documents can possibly match, so the positional test, which is
the expensive part, runs as few times as possible.
