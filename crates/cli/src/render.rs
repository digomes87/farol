//! Terminal and JSON rendering of search results.

use std::io::IsTerminal;

use farol_core::{SearchResult, SearchStats, Stats, Strategy};

/// ANSI styling, disabled when stdout is not a terminal so piped output stays
/// clean and greppable.
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn detect(force: Option<bool>) -> Self {
        Self {
            enabled: force.unwrap_or_else(|| std::io::stdout().is_terminal()),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn cyan(&self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn yellow(&self, text: &str) -> String {
        self.paint("33", text)
    }

    /// Markers handed to the highlighter: bold yellow on a terminal, Markdown
    /// emphasis everywhere else.
    pub fn markers(&self) -> (String, String) {
        if self.enabled {
            ("\x1b[1;33m".into(), "\x1b[0m".into())
        } else {
            ("**".into(), "**".into())
        }
    }
}

/// Renders results as a human readable list.
pub fn results(results: &[SearchResult], query: &str, elapsed_ms: f64, style: &Style) -> String {
    if results.is_empty() {
        return format!("{}\n", style.dim(&format!("no results for `{query}`")));
    }

    let mut out = String::new();
    out.push_str(&style.dim(&format!(
        "{} result(s) in {elapsed_ms:.1} ms\n\n",
        results.len()
    )));

    for (position, result) in results.iter().enumerate() {
        out.push_str(&format!(
            "{} {}  {}\n",
            style.dim(&format!("{:>2}.", position + 1)),
            style.bold(&result.title),
            style.yellow(&format!("{:.3}", result.score)),
        ));
        out.push_str(&format!("    {}\n", style.cyan(&result.uri)));
        out.push_str(&format!("    {}\n\n", result.snippet));
    }
    out
}

/// Renders how the query was evaluated: which strategy ran, how many documents
/// matched, and how many of them actually had to be scored.
pub fn explain(stats: &SearchStats, style: &Style) -> String {
    let strategy = match stats.strategy {
        Strategy::Wand => "wand (dynamic pruning)",
        Strategy::Exhaustive => "exhaustive",
    };
    let saved = if stats.candidates > 0 {
        100.0 * (1.0 - stats.scored as f64 / stats.candidates as f64)
    } else {
        0.0
    };
    style.dim(&format!(
        "strategy: {strategy} · postings: {} · scored: {} · skipped: {} · blocks skipped: {} ({saved:.0}% avoided)\n\n",
        stats.candidates, stats.scored, stats.pruned, stats.blocks_skipped
    ))
}

/// Renders results as JSON, for piping into `jq` or another program.
pub fn results_json(
    results: &[SearchResult],
    query: &str,
    elapsed_ms: f64,
    stats: &SearchStats,
) -> String {
    let payload = serde_json::json!({
        "query": query,
        "elapsed_ms": elapsed_ms,
        "count": results.len(),
        "blocks_skipped": stats.blocks_skipped,
        "strategy": match stats.strategy {
            Strategy::Wand => "wand",
            Strategy::Exhaustive => "exhaustive",
        },
        "candidates": stats.candidates,
        "scored": stats.scored,
        "results": results
            .iter()
            .map(|r| serde_json::json!({
                "uri": r.uri,
                "title": r.title,
                "score": r.score,
                "snippet": r.snippet,
            }))
            .collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&payload).expect("serializing owned data cannot fail")
}

/// Renders index level counters.
pub fn stats(stats: &Stats, path: &str, style: &Style) -> String {
    let rows = [
        ("index", path.to_string()),
        ("documents", stats.documents.to_string()),
        ("vocabulary", format!("{} terms", stats.vocabulary)),
        ("postings", stats.postings.to_string()),
        ("avg length", format!("{:.1} terms/doc", stats.avg_doc_len)),
        (
            "postings size",
            format!(
                "{:.1} KiB ({:.2} bytes/posting)",
                stats.postings_bytes as f64 / 1024.0,
                if stats.postings > 0 {
                    stats.postings_bytes as f64 / stats.postings as f64
                } else {
                    0.0
                }
            ),
        ),
    ];
    rows.iter()
        .map(|(label, value)| format!("{:<16}{}\n", style.dim(label), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<SearchResult> {
        vec![SearchResult {
            uri: "corpus/a.md".into(),
            title: "Rust".into(),
            score: 1.2345,
            snippet: "…memory **safety**…".into(),
        }]
    }

    #[test]
    fn plain_output_carries_no_escape_codes() {
        let style = Style::detect(Some(false));
        let text = results(&sample(), "safety", 1.0, &style);
        assert!(!text.contains('\x1b'));
        assert!(text.contains("corpus/a.md"));
    }

    #[test]
    fn styled_output_is_colored() {
        let style = Style::detect(Some(true));
        assert!(results(&sample(), "safety", 1.0, &style).contains('\x1b'));
    }

    #[test]
    fn an_empty_result_set_says_so() {
        let style = Style::detect(Some(false));
        let text = results(&[], "kubernetes", 0.4, &style);
        assert!(text.contains("no results"));
    }

    fn stats_sample() -> SearchStats {
        SearchStats {
            scored: 3,
            candidates: 12,
            pruned: 9,
            blocks_skipped: 2,
            strategy: Strategy::Wand,
        }
    }

    #[test]
    fn json_output_is_valid_and_complete() {
        let text = results_json(&sample(), "safety", 2.0, &stats_sample());
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["results"][0]["title"], "Rust");
        assert_eq!(parsed["strategy"], "wand");
        assert_eq!(parsed["scored"], 3);
    }

    #[test]
    fn explain_reports_the_share_of_work_avoided() {
        let style = Style::detect(Some(false));
        let text = explain(&stats_sample(), &style);
        assert!(text.contains("wand"), "{text}");
        assert!(text.contains("75% avoided"), "{text}");
        assert!(text.contains("blocks skipped: 2"), "{text}");
    }
}
