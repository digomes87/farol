//! Query parsing: a small, predictable syntax on top of the analyzer.
//!
//! | Syntax | Meaning |
//! |--------|---------|
//! | `rust search` | either term may match; documents with both rank higher |
//! | `+rust` | the document **must** contain the term |
//! | `-java` | the document **must not** contain the term |
//! | `"motor de busca"` | the words must appear adjacent, in this order |
//! | `+"motor de busca"` | …and the phrase is mandatory |
//!
//! Every fragment goes through the same [`Analyzer`] used at index time, so a
//! query for `"Migrações"` finds a document that says `migracao`.

use crate::analyzer::Analyzer;
use crate::error::{Error, Result};

/// How a clause participates in matching, mirroring boolean query semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occur {
    /// Optional, but contributes to the score.
    Should,
    /// Required. Documents without it are discarded.
    Must,
    /// Forbidden. Documents with it are discarded.
    MustNot,
}

/// What a clause matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClauseKind {
    /// A single analyzed term.
    Term(String),
    /// Consecutive terms with their offsets relative to the start of the
    /// phrase. Offsets — rather than a plain list — are what allows a stopword
    /// dropped inside the quotes to line up with the same gap in the document.
    Phrase(Vec<(String, u32)>),
}

/// One parsed unit of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    pub occur: Occur,
    pub kind: ClauseKind,
}

/// A parsed query: a flat list of clauses, evaluated as a boolean combination.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub clauses: Vec<Clause>,
}

impl Query {
    /// Parses `input`, analyzing every fragment with `analyzer`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Query`] when a quote is left open, or when nothing
    /// searchable survives analysis (an empty string, or only stopwords).
    ///
    /// # Example
    ///
    /// ```
    /// use farol_core::{Analyzer, Occur, Query};
    ///
    /// let query = Query::parse(r#"+rust "motor de busca" -java"#, &Analyzer::default())?;
    /// assert_eq!(query.clauses.len(), 3);
    /// assert_eq!(query.clauses[0].occur, Occur::Must);
    /// # Ok::<(), farol_core::Error>(())
    /// ```
    pub fn parse(input: &str, analyzer: &Analyzer) -> Result<Self> {
        let mut clauses = Vec::new();

        for fragment in split_fragments(input)? {
            let kind = if fragment.phrase {
                let terms: Vec<(String, u32)> = analyzer
                    .analyze(&fragment.text)
                    .into_iter()
                    .map(|t| (t.term, t.position))
                    .collect();
                match terms.len() {
                    0 => continue,
                    // A one-word phrase is just a term; keeping it as a phrase
                    // would pay the positional check for nothing.
                    1 => ClauseKind::Term(terms.into_iter().next().expect("len == 1").0),
                    _ => ClauseKind::Phrase(terms),
                }
            } else {
                match analyzer.terms(&fragment.text).into_iter().next() {
                    Some(term) => ClauseKind::Term(term),
                    None => continue,
                }
            };
            clauses.push(Clause {
                occur: fragment.occur,
                kind,
            });
        }

        if clauses.is_empty() {
            return Err(Error::Query(format!(
                "`{input}` has no searchable term left after analysis"
            )));
        }
        if clauses.iter().all(|c| c.occur == Occur::MustNot) {
            return Err(Error::Query(
                "a query made only of exclusions matches nothing".into(),
            ));
        }
        Ok(Self { clauses })
    }

    /// Every term mentioned by the query, excluding the forbidden ones.
    ///
    /// Used for snippet highlighting, which should mark what the user was
    /// looking for and not what they ruled out.
    pub fn positive_terms(&self) -> Vec<&str> {
        let mut terms = Vec::new();
        for clause in &self.clauses {
            if clause.occur == Occur::MustNot {
                continue;
            }
            match &clause.kind {
                ClauseKind::Term(term) => terms.push(term.as_str()),
                ClauseKind::Phrase(parts) => {
                    terms.extend(parts.iter().map(|(term, _)| term.as_str()))
                }
            }
        }
        terms
    }
}

/// A raw fragment of the query, before analysis.
struct Fragment {
    text: String,
    occur: Occur,
    phrase: bool,
}

/// Splits the query into fragments, honouring quotes and the `+`/`-` prefixes.
fn split_fragments(input: &str) -> Result<Vec<Fragment>> {
    let mut fragments = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }

        let occur = match ch {
            '+' => {
                chars.next();
                Occur::Must
            }
            '-' => {
                chars.next();
                Occur::MustNot
            }
            _ => Occur::Should,
        };

        let (text, phrase) = if chars.peek() == Some(&'"') {
            chars.next();
            let mut text = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '"' {
                    closed = true;
                    break;
                }
                text.push(c);
            }
            if !closed {
                return Err(Error::Query(format!("unterminated quote in `{input}`")));
            }
            (text, true)
        } else {
            let mut text = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                text.push(c);
                chars.next();
            }
            (text, false)
        };

        if !text.trim().is_empty() {
            fragments.push(Fragment {
                text,
                occur,
                phrase,
            });
        }
    }

    Ok(fragments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<Query> {
        Query::parse(input, &Analyzer::raw())
    }

    #[test]
    fn bare_terms_are_optional() {
        let query = parse("rust search").unwrap();
        assert_eq!(query.clauses.len(), 2);
        assert!(query.clauses.iter().all(|c| c.occur == Occur::Should));
    }

    #[test]
    fn prefixes_set_the_boolean_role() {
        let query = parse("+rust -java maybe").unwrap();
        let occurs: Vec<_> = query.clauses.iter().map(|c| c.occur).collect();
        assert_eq!(occurs, [Occur::Must, Occur::MustNot, Occur::Should]);
    }

    #[test]
    fn quoted_text_becomes_a_phrase_with_relative_offsets() {
        let query = parse("\"motor de busca\"").unwrap();
        match &query.clauses[0].kind {
            ClauseKind::Phrase(parts) => {
                assert_eq!(parts[0], ("motor".to_string(), 0));
                assert_eq!(parts[2], ("busca".to_string(), 2));
            }
            other => panic!("expected a phrase, got {other:?}"),
        }
    }

    #[test]
    fn phrase_offsets_keep_the_gap_left_by_a_stopword() {
        let query = Query::parse("\"canto de sereia\"", &Analyzer::default()).unwrap();
        match &query.clauses[0].kind {
            ClauseKind::Phrase(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].1, 0);
                assert_eq!(parts[1].1, 2, "the dropped `de` must still occupy a slot");
            }
            other => panic!("expected a phrase, got {other:?}"),
        }
    }

    #[test]
    fn a_single_word_phrase_degrades_to_a_term() {
        let query = parse("\"rust\"").unwrap();
        assert_eq!(query.clauses[0].kind, ClauseKind::Term("rust".into()));
    }

    #[test]
    fn a_phrase_can_be_required_or_forbidden() {
        let query = parse("+\"motor busca\" -\"maquina virtual\"").unwrap();
        assert_eq!(query.clauses[0].occur, Occur::Must);
        assert_eq!(query.clauses[1].occur, Occur::MustNot);
    }

    #[test]
    fn unterminated_quote_is_rejected() {
        let err = parse("\"motor de busca").unwrap_err();
        assert!(err.to_string().contains("unterminated quote"));
    }

    #[test]
    fn a_query_of_only_stopwords_is_rejected() {
        let err = Query::parse("o de a", &Analyzer::default()).unwrap_err();
        assert!(err.to_string().contains("no searchable term"));
    }

    #[test]
    fn a_query_of_only_exclusions_is_rejected() {
        let err = parse("-java -php").unwrap_err();
        assert!(err.to_string().contains("matches nothing"));
    }

    #[test]
    fn positive_terms_ignore_exclusions() {
        let query = parse("+rust \"motor busca\" -java").unwrap();
        assert_eq!(query.positive_terms(), ["rust", "motor", "busca"]);
    }

    #[test]
    fn queries_are_analyzed_like_documents() {
        let query = Query::parse("MIGRAÇÕES", &Analyzer::default()).unwrap();
        assert_eq!(query.clauses[0].kind, ClauseKind::Term("migr".into()));
    }
}
