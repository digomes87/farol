# The inverted index

A forward index answers "which terms does document 7 contain?". Search needs the
opposite question: "which documents contain the word rust?". That is why the
index is inverted — every term points at the sorted list of documents where it
occurs.

The posting list also stores the position of each occurrence. Without positions
there is no way to answer an exact phrase query, because knowing that two words
share a document says nothing about them sitting side by side.

The length of every document is kept next to its metadata. Ranking uses that
number to penalise long texts, which are more likely to contain any given word
by accident.
