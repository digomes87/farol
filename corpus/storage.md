# On-disk storage

Building the index is the expensive part. Reading it back should be cheap, so
the whole index is serialised and kept in a single file.

Writing goes through a temporary file and ends with an atomic rename. Without
that, an interruption in the middle of the write would leave half an index where
a valid one used to be.

In front of the payload sit magic bytes and a format version number. The former
tell "this is not an index" apart from "this index is corrupt"; the latter turns
a layout change into a clear error rather than a silent misread.
