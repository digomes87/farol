//! Snippets: the two lines of context shown under each result.
//!
//! A ranked list of file names is nearly useless — the reader needs to see
//! *why* a document matched. The highlighter re-analyzes the stored text,
//! finds the densest window of query matches and quotes it verbatim, marking
//! the matched words.
//!
//! Re-analyzing at display time is a deliberate trade: it costs one pass over
//! the few documents actually shown, and in exchange the index does not have to
//! store per-document offsets for every term.

use crate::analyzer::Analyzer;

/// A highlighted excerpt of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The excerpt, with matched words wrapped in the configured markers and
    /// ellipses where text was cut.
    pub text: String,
    /// How many query terms were found in the excerpt. Zero means the document
    /// matched through a term that does not appear in the chosen window.
    pub matched: usize,
}

/// Builds [`Snippet`]s from document text.
#[derive(Debug, Clone)]
pub struct Highlighter {
    max_chars: usize,
    open: String,
    close: String,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self {
            max_chars: 200,
            open: "**".into(),
            close: "**".into(),
        }
    }
}

impl Highlighter {
    /// Sets the excerpt budget in characters.
    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars.max(16);
        self
    }

    /// Sets the markers wrapped around matched words — `**` for Markdown, ANSI
    /// escapes for a terminal, `<mark>`/`</mark>` for HTML.
    pub fn with_markers(mut self, open: impl Into<String>, close: impl Into<String>) -> Self {
        self.open = open.into();
        self.close = close.into();
        self
    }

    /// Extracts the best window of `text` for `terms`, which must already be
    /// analyzed (they come from [`Query::positive_terms`](crate::Query::positive_terms)).
    ///
    /// # Example
    ///
    /// ```
    /// use farol_core::{Analyzer, Highlighter};
    ///
    /// let analyzer = Analyzer::raw();
    /// let snippet = Highlighter::default()
    ///     .with_max_chars(40)
    ///     .snippet("rust is a fast and safe language", &analyzer, &["safe"]);
    ///
    /// assert!(snippet.text.contains("**safe**"));
    /// assert_eq!(snippet.matched, 1);
    /// ```
    pub fn snippet(&self, text: &str, analyzer: &Analyzer, terms: &[&str]) -> Snippet {
        let tokens = analyzer.analyze(text);
        let hits: Vec<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, token)| terms.contains(&token.term.as_str()))
            .map(|(idx, _)| idx)
            .collect();

        if hits.is_empty() {
            return Snippet {
                text: self.head(text),
                matched: 0,
            };
        }

        let (start, end) = self.best_window(text, &tokens, &hits);
        let mut out = String::with_capacity(end - start + 16);
        if start > 0 {
            out.push('…');
        }

        // Walk the window once, copying plain text and wrapping the matches.
        let mut cursor = start;
        let mut matched = 0;
        for &idx in &hits {
            let token = &tokens[idx];
            if token.start < cursor || token.end > end {
                continue;
            }
            out.push_str(&text[cursor..token.start]);
            out.push_str(&self.open);
            out.push_str(&text[token.start..token.end]);
            out.push_str(&self.close);
            cursor = token.end;
            matched += 1;
        }
        out.push_str(&text[cursor..end]);
        if end < text.len() {
            out.push('…');
        }

        Snippet {
            text: collapse_whitespace(&out),
            matched,
        }
    }

    /// Picks the window that contains the most *distinct* query terms.
    ///
    /// Distinct rather than total: a window repeating one term ten times is a
    /// worse explanation of the match than one showing three different terms.
    fn best_window(
        &self,
        text: &str,
        tokens: &[crate::analyzer::Token],
        hits: &[usize],
    ) -> (usize, usize) {
        let mut best = (hits[0], 0usize);
        for (position, &first) in hits.iter().enumerate() {
            let limit = tokens[first].start + self.max_chars;
            let mut seen: Vec<&str> = Vec::new();
            for &idx in &hits[position..] {
                if tokens[idx].end > limit {
                    break;
                }
                let term = tokens[idx].term.as_str();
                if !seen.contains(&term) {
                    seen.push(term);
                }
            }
            if seen.len() > best.1 {
                best = (first, seen.len());
            }
        }

        // Give the first match a little context on its left instead of starting
        // the excerpt exactly on the matched word.
        let anchor = tokens[best.0].start;
        let lead = self.max_chars / 4;
        let start = floor_boundary(text, anchor.saturating_sub(lead));
        let end = ceil_boundary(text, (start + self.max_chars).min(text.len()));
        (start, end)
    }

    /// Fallback excerpt when nothing matched: the head of the document.
    fn head(&self, text: &str) -> String {
        let end = ceil_boundary(text, self.max_chars.min(text.len()));
        let mut out = collapse_whitespace(&text[..end]);
        if end < text.len() {
            out.push('…');
        }
        out
    }
}

/// Largest char boundary `<= idx`.
fn floor_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Smallest char boundary `>= idx`.
fn ceil_boundary(text: &str, mut idx: usize) -> usize {
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Flattens newlines and runs of spaces so a snippet always renders on one line.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.trim().chars() {
        if ch.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "A first paragraph with nothing of interest at all here. \
The second paragraph explains that rust gives memory safety without a garbage \
collector, and that rust does it at compile time. A third paragraph closes.";

    fn snippet(terms: &[&str], max_chars: usize) -> Snippet {
        Highlighter::default()
            .with_max_chars(max_chars)
            .snippet(TEXT, &Analyzer::raw(), terms)
    }

    #[test]
    fn matched_words_are_wrapped_in_the_markers() {
        let snippet = snippet(&["safety"], 120);
        assert!(snippet.text.contains("**safety**"), "{}", snippet.text);
        assert_eq!(snippet.matched, 1);
    }

    #[test]
    fn the_window_moves_to_where_the_matches_are() {
        let snippet = snippet(&["rust"], 100);
        assert!(!snippet.text.contains("first paragraph"));
        assert!(snippet.text.starts_with('…'));
    }

    #[test]
    fn the_window_prefers_the_most_distinct_terms() {
        let snippet = snippet(&["memory", "safety", "collector"], 90);
        assert!(snippet.matched >= 2, "{}", snippet.text);
    }

    #[test]
    fn documents_without_a_visible_match_fall_back_to_the_head() {
        let snippet = snippet(&["kubernetes"], 60);
        assert_eq!(snippet.matched, 0);
        assert!(snippet.text.starts_with("A first paragraph"));
        assert!(snippet.text.ends_with('…'));
    }

    #[test]
    fn snippets_are_single_line() {
        let text = "line one\n\n  line   two with rust\n";
        let snippet = Highlighter::default().snippet(text, &Analyzer::raw(), &["rust"]);
        assert!(!snippet.text.contains('\n'));
        assert!(!snippet.text.contains("  "));
    }

    #[test]
    fn markers_are_configurable() {
        let snippet = Highlighter::default()
            .with_markers("<mark>", "</mark>")
            .snippet("safe rust", &Analyzer::raw(), &["rust"]);
        assert!(snippet.text.contains("<mark>rust</mark>"));
    }

    #[test]
    fn windows_never_split_a_multibyte_character() {
        let text = "ação é acentuação repetida ação é acentuação repetida ação";
        for max in 16..48 {
            let snippet = Highlighter::default().with_max_chars(max).snippet(
                text,
                &Analyzer::raw(),
                &["acentuacao"],
            );
            assert!(!snippet.text.is_empty());
        }
    }

    #[test]
    fn short_documents_are_returned_whole() {
        let snippet = Highlighter::default().snippet("rust", &Analyzer::raw(), &["rust"]);
        assert_eq!(snippet.text, "**rust**");
    }
}
