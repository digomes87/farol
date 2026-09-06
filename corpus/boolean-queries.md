# Boolean queries

A query is a combination of clauses. Bare terms are optional and contribute to
the score. A required term filters the collection: documents without it are
discarded before ranking. An excluded term removes the documents that contain
it.

Evaluation order matters for performance. When a required clause exists, the
candidate set comes from the intersection of the corresponding lists, which is
always smaller than the union. Scoring few documents is cheaper than scoring all
of them and throwing most away afterwards.

A query made only of exclusions makes no sense: it describes what is unwanted
without saying what is being looked for.
