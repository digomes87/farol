//! Text analysis: the pipeline that turns raw text into indexable terms.
//!
//! ```text
//! "Strong Hearts!"  ->  normalize  ->  "strong hearts!"
//!                   ->  tokenize   ->  ["strong", "hearts"]
//!                   ->  stopwords  ->  ["strong", "hearts"]
//!                   ->  stem       ->  ["strong", "heart"]
//! ```
//!
//! The same [`Analyzer`] instance must be used for indexing and querying,
//! otherwise the terms produced on each side will never match.

use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;

/// A single term extracted from a document, with the metadata needed later for
/// phrase queries (`position`) and snippet highlighting (`start`/`end`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Normalized and stemmed form, as stored in the index.
    pub term: String,
    /// Ordinal position of the token inside the document, starting at zero.
    pub position: u32,
    /// Byte offset where the original word starts in the source text.
    pub start: usize,
    /// Byte offset just past the end of the original word.
    pub end: usize,
}

/// Configurable analysis pipeline shared by the indexer and the query parser.
#[derive(Debug, Clone)]
pub struct Analyzer {
    stopwords: HashSet<String>,
    min_token_len: usize,
    stemming: bool,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self {
            stopwords: default_stopwords(),
            min_token_len: 2,
            stemming: true,
        }
    }
}

impl Analyzer {
    /// Analyzer with no stopword list and no stemming — useful for tests and
    /// for corpora where every token is meaningful (code, identifiers, logs).
    pub fn raw() -> Self {
        Self {
            stopwords: HashSet::new(),
            min_token_len: 1,
            stemming: false,
        }
    }

    /// Replaces the stopword list. Words are normalized before being stored, so
    /// callers may pass them accented and in any case.
    pub fn with_stopwords<I, S>(mut self, words: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.stopwords = words
            .into_iter()
            .map(|w| normalize(w.as_ref()))
            .filter(|w| !w.is_empty())
            .collect();
        self
    }

    /// Enables or disables the suffix stripping step.
    pub fn with_stemming(mut self, stemming: bool) -> Self {
        self.stemming = stemming;
        self
    }

    /// Drops tokens shorter than `len` characters (after normalization).
    pub fn with_min_token_len(mut self, len: usize) -> Self {
        self.min_token_len = len;
        self
    }

    /// Runs the full pipeline over `text`.
    ///
    /// Positions are assigned *before* stopword removal so that phrase queries
    /// keep working across a removed word: in `"canto de sereia"` the terms
    /// `canto` and `sereia` stay two positions apart, which is exactly what the
    /// same query text produces at search time.
    pub fn analyze(&self, text: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        for (position, (start, end, word)) in split_words(text).enumerate() {
            let normalized = normalize(word);
            if normalized.chars().count() < self.min_token_len {
                continue;
            }
            if self.stopwords.contains(&normalized) {
                continue;
            }
            let term = if self.stemming {
                stem(&normalized)
            } else {
                normalized
            };
            tokens.push(Token {
                term,
                position: position as u32,
                start,
                end,
            });
        }
        tokens
    }

    /// Convenience wrapper returning only the terms.
    pub fn terms(&self, text: &str) -> Vec<String> {
        self.analyze(text).into_iter().map(|t| t.term).collect()
    }
}

/// Splits `text` into word spans: maximal runs of alphanumeric characters.
///
/// Yields `(start_byte, end_byte, word)` so the caller can map a term back to
/// the exact slice of the original text.
fn split_words(text: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    let mut start: Option<usize> = None;
    let mut out = Vec::new();
    for (idx, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            start.get_or_insert(idx);
        } else if let Some(begin) = start.take() {
            out.push((begin, idx, &text[begin..idx]));
        }
    }
    if let Some(begin) = start {
        out.push((begin, text.len(), &text[begin..]));
    }
    out.into_iter()
}

/// Lowercases and strips diacritics so that `"Ação"`, `"acao"` and `"AÇÃO"`
/// collapse into the same term.
///
/// Uses NFD decomposition and then drops the combining marks, which handles
/// every Latin accent without a hand written character table.
pub fn normalize(word: &str) -> String {
    word.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Unicode combining diacritical marks, the blocks produced by NFD for Latin.
fn is_combining_mark(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x20D0..=0x20FF)
}

/// Suffix rules applied by [`stem`], ordered from most to least specific.
///
/// Each entry is `(suffix, replacement, min_stem_chars)`. `min_stem_chars`
/// guards against over stemming short words: `"mente"` is stripped from
/// `"rapidamente"` but not from `"mente"` itself.
const SUFFIXES: &[(&str, &str, usize)] = &[
    // Portuguese plurals that change the stem.
    ("oes", "ao", 3),
    ("aes", "ao", 3),
    ("ais", "al", 3),
    ("eis", "el", 3),
    ("ois", "ol", 3),
    ("ns", "m", 3),
    // Portuguese derivational suffixes.
    ("mente", "", 4),
    ("amento", "", 4),
    ("imento", "", 4),
    ("idade", "", 4),
    ("ancia", "", 4),
    ("encia", "", 4),
    ("avel", "", 4),
    ("ivel", "", 4),
    ("ista", "", 4),
    ("ismo", "", 4),
    ("acao", "", 4),
    ("icao", "", 4),
    ("ador", "", 4),
    ("adora", "", 4),
    // English derivational suffixes.
    ("ational", "ate", 4),
    ("iveness", "ive", 4),
    ("fulness", "ful", 4),
    ("tional", "tion", 4),
    ("ization", "ize", 4),
    ("ness", "", 4),
    ("ing", "", 4),
    ("edly", "", 4),
    ("ies", "y", 4),
    ("ed", "", 4),
    ("ly", "", 4),
    // Generic plurals, last so the specific rules above win.
    ("es", "", 4),
    ("s", "", 4),
];

/// Light suffix stripping for Portuguese and English.
///
/// This is deliberately *not* a full Porter/RSLP implementation: the goal is to
/// collapse the most common inflections with rules that are easy to audit and
/// that never touch short words. Stemming is heuristic by nature — the index
/// only requires that indexing and querying agree on the output.
pub fn stem(word: &str) -> String {
    // Two rounds: the first collapses the inflection (`migracoes` -> `migracao`)
    // and the second the derivation (`migracao` -> `migr`). Without the second
    // round the plural and the singular of the same word would land on
    // different terms, which silently breaks recall.
    let once = strip_one_suffix(word);
    strip_one_suffix(&once)
}

/// Applies the first matching rule of [`SUFFIXES`], or returns `word` unchanged.
fn strip_one_suffix(word: &str) -> String {
    let len = word.chars().count();
    for (suffix, replacement, min_stem) in SUFFIXES {
        if let Some(base) = word.strip_suffix(suffix) {
            if base.chars().count() >= *min_stem && len > suffix.chars().count() {
                return format!("{base}{replacement}");
            }
        }
    }
    word.to_string()
}

/// Frequent Portuguese and English function words, filtered out by default.
///
/// They appear in nearly every document, so they carry almost no ranking signal
/// while inflating the postings lists.
pub fn default_stopwords() -> HashSet<String> {
    const WORDS: &str = "\
a as o os um uma uns umas de do da dos das em no na nos nas por para pelo pela com sem sob sobre \
e ou mas que se ao aos à às entre ate até como quando onde qual quais quem cujo cuja é sao são \
foi era ser estar tem têm ha há isso isto aquilo esse essa este esta aquele aquela seu sua seus suas \
the a an and or but if of in on at to for from by with without as is are was were be been being \
this that these those it its they them their he she his her you your we our i me my not no";
    WORDS.split_whitespace().map(normalize).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_accents_and_case() {
        assert_eq!(normalize("Ação"), "acao");
        assert_eq!(normalize("CORAÇÕES"), "coracoes");
        assert_eq!(normalize("naïve"), "naive");
    }

    #[test]
    fn split_words_yields_byte_spans_of_the_original_text() {
        let text = "olá, mundo!";
        let spans: Vec<_> = split_words(text).collect();
        assert_eq!(spans.len(), 2);
        let (start, end, word) = spans[1];
        assert_eq!(word, "mundo");
        assert_eq!(&text[start..end], "mundo");
    }

    #[test]
    fn stem_is_idempotent() {
        for word in [
            "coracoes",
            "migracoes",
            "carros",
            "animais",
            "running",
            "casas",
        ] {
            let once = stem(word);
            assert_eq!(stem(&once), once, "stemming {word} twice changed the term");
        }
    }

    #[test]
    fn stem_collapses_portuguese_plurals() {
        assert_eq!(stem("coracoes"), "coracao");
        assert_eq!(stem("animais"), "animal");
        assert_eq!(stem("carros"), "carro");
    }

    #[test]
    fn stem_leaves_short_words_untouched() {
        assert_eq!(stem("mente"), "mente");
        assert_eq!(stem("mais"), "mais");
        assert_eq!(stem("as"), "as");
    }

    #[test]
    fn analyze_keeps_positions_across_removed_stopwords() {
        let analyzer = Analyzer::default();
        let tokens = analyzer.analyze("the song of a siren");
        let terms: Vec<_> = tokens.iter().map(|t| t.term.as_str()).collect();
        assert_eq!(terms, ["song", "siren"]);
        assert_eq!(tokens[0].position, 1);
        assert_eq!(tokens[1].position, 4);
    }

    #[test]
    fn indexing_and_query_produce_the_same_terms() {
        // Portuguese support: the plural in the document and the singular in
        // the query must converge on the same terms.
        let analyzer = Analyzer::default();
        let indexed = analyzer.terms("As MIGRAÇÕES dos pássaros");
        let queried = analyzer.terms("migração de pássaro");
        assert!(indexed.contains(&queried[0]));
        assert!(indexed.contains(&queried[1]));
    }

    #[test]
    fn raw_analyzer_preserves_every_token() {
        let analyzer = Analyzer::raw();
        assert_eq!(
            analyzer.terms("the cats are running"),
            ["the", "cats", "are", "running"]
        );
    }

    #[test]
    fn offsets_point_at_the_original_word() {
        let text = "Résumé parsing";
        let analyzer = Analyzer::default();
        let tokens = analyzer.analyze(text);
        assert_eq!(&text[tokens[0].start..tokens[0].end], "Résumé");
    }
}
