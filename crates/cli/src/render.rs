//! Terminal and JSON rendering of search results.

use std::io::IsTerminal;

use farol_core::{SearchResult, Stats};

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
        return format!(
            "{}\n",
            style.dim(&format!("nenhum resultado para `{query}`"))
        );
    }

    let mut out = String::new();
    out.push_str(&style.dim(&format!(
        "{} resultado(s) em {elapsed_ms:.1} ms\n\n",
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

/// Renders results as JSON, for piping into `jq` or another program.
pub fn results_json(results: &[SearchResult], query: &str, elapsed_ms: f64) -> String {
    let payload = serde_json::json!({
        "query": query,
        "elapsed_ms": elapsed_ms,
        "count": results.len(),
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
        ("índice", path.to_string()),
        ("documentos", stats.documents.to_string()),
        ("vocabulário", format!("{} termos", stats.vocabulary)),
        ("postings", stats.postings.to_string()),
        (
            "tamanho médio",
            format!("{:.1} termos/doc", stats.avg_doc_len),
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
        assert!(text.contains("nenhum resultado"));
    }

    #[test]
    fn json_output_is_valid_and_complete() {
        let text = results_json(&sample(), "safety", 2.0);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["results"][0]["title"], "Rust");
    }
}
