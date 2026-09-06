//! `farol` — command line interface for the search engine.
//!
//! ```text
//! farol index ./docs          build an index from a directory
//! farol search "+rust engine" query it
//! farol repl                  keep the index hot and query interactively
//! farol stats                 inspect the index
//! ```

mod render;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use farol_core::{Analyzer, Bm25, Engine, Highlighter};

use crate::render::Style;

/// Writes `text` to stdout, treating a closed pipe as a normal end.
///
/// Rust ignores `SIGPIPE`, so `farol search … | head -3` would otherwise make
/// the process panic when `head` exits — a crash report for something the user
/// asked for.
fn emit(text: &str) -> Result<()> {
    let mut stdout = io::stdout();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

/// Default location of the index file, relative to the working directory.
const DEFAULT_INDEX: &str = "farol.idx";

#[derive(Parser)]
#[command(
    name = "farol",
    version,
    about = "Full-text search engine for collections of text files",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Indexes a directory recursively and writes the index to disk.
    Index {
        /// Root directory to crawl.
        path: PathBuf,
        #[command(flatten)]
        index: IndexPath,
        /// File extensions to index, comma separated (e.g. md,txt).
        #[arg(long, value_delimiter = ',')]
        ext: Option<Vec<String>>,
        /// Disables stemming, indexing words as written.
        #[arg(long)]
        no_stemming: bool,
        /// Keeps stopwords in the index.
        #[arg(long)]
        keep_stopwords: bool,
        /// Writes the serialised format instead of the memory-mapped one.
        #[arg(long)]
        serialized: bool,
    },
    /// Searches the index and prints the best results.
    Search {
        /// The query. Accepts +required, -excluded and "exact phrase".
        query: Vec<String>,
        #[command(flatten)]
        index: IndexPath,
        #[command(flatten)]
        ranking: Ranking,
        /// Maximum number of results.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Prints JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Reports which evaluation strategy ran and how much work it avoided.
        #[arg(long)]
        explain: bool,
    },
    /// Interactive session: loads the index once and answers many queries.
    Repl {
        #[command(flatten)]
        index: IndexPath,
        #[command(flatten)]
        ranking: Ranking,
        #[arg(short = 'n', long, default_value_t = 5)]
        limit: usize,
    },
    /// Shows the index counters.
    Stats {
        #[command(flatten)]
        index: IndexPath,
    },
}

#[derive(Args, Clone)]
struct IndexPath {
    /// Path of the index file.
    #[arg(short = 'i', long = "index", default_value = DEFAULT_INDEX, global = true)]
    file: PathBuf,
}

#[derive(Args, Clone, Copy)]
struct Ranking {
    /// BM25 term frequency saturation.
    #[arg(long, default_value_t = 1.2)]
    k1: f32,
    /// Document length normalisation, from 0.0 to 1.0.
    #[arg(long, default_value_t = 0.75)]
    b: f32,
    /// Maximum snippet length, in characters.
    #[arg(long, default_value_t = 200)]
    snippet: usize,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Index {
            path,
            index,
            ext,
            no_stemming,
            keep_stopwords,
            serialized,
        } => build_index(
            path,
            index.file,
            ext,
            no_stemming,
            keep_stopwords,
            serialized,
        ),
        Command::Search {
            query,
            index,
            ranking,
            limit,
            json,
            explain,
        } => search(query.join(" "), index.file, ranking, limit, json, explain),
        Command::Repl {
            index,
            ranking,
            limit,
        } => repl(index.file, ranking, limit),
        Command::Stats { index } => stats(index.file),
    }
}

fn analyzer(no_stemming: bool, keep_stopwords: bool) -> Analyzer {
    let analyzer = Analyzer::default().with_stemming(!no_stemming);
    if keep_stopwords {
        analyzer.with_stopwords(Vec::<String>::new())
    } else {
        analyzer
    }
}

fn build_index(
    path: PathBuf,
    index_path: PathBuf,
    ext: Option<Vec<String>>,
    no_stemming: bool,
    keep_stopwords: bool,
    serialized: bool,
) -> Result<()> {
    let mut engine = Engine::new(analyzer(no_stemming, keep_stopwords));
    if let Some(ext) = ext {
        engine = engine.with_extensions(ext);
    }

    let started = Instant::now();
    let count = engine
        .index_dir(&path)
        .with_context(|| format!("indexing `{}`", path.display()))?;
    if serialized {
        engine.save_serialized(&index_path)
    } else {
        engine.save(&index_path)
    }
    .with_context(|| format!("writing `{}`", index_path.display()))?;

    let style = Style::detect(None);
    emit(&format!(
        "{} document(s) indexed in {:.2}s → {}\n",
        count,
        started.elapsed().as_secs_f64(),
        style.cyan(&index_path.display().to_string())
    ))?;
    emit(&render::stats(
        &engine.stats(),
        &index_path.display().to_string(),
        &style,
    ))
}

/// Loads the index and applies the ranking flags to the engine.
fn open(index_path: &PathBuf, ranking: Ranking, style: &Style) -> Result<Engine> {
    let (open_marker, close_marker) = style.markers();
    let mut engine = Engine::default()
        .with_bm25(Bm25::new(ranking.k1, ranking.b))
        .with_highlighter(
            Highlighter::default()
                .with_max_chars(ranking.snippet)
                .with_markers(open_marker, close_marker),
        );
    engine.load(index_path).with_context(|| {
        format!(
            "opening `{}` (run `farol index` first)",
            index_path.display()
        )
    })?;
    Ok(engine)
}

fn search(
    query: String,
    index_path: PathBuf,
    ranking: Ranking,
    limit: usize,
    json: bool,
    explain: bool,
) -> Result<()> {
    let style = Style::detect(if json { Some(false) } else { None });
    let engine = open(&index_path, ranking, &style)?;

    let started = Instant::now();
    let (results, stats) = engine.search_with_stats(&query, limit)?;
    let elapsed = started.elapsed().as_secs_f64() * 1_000.0;

    if json {
        emit(&render::results_json(&results, &query, elapsed, &stats))?;
        emit("\n")?;
    } else {
        if explain {
            emit(&render::explain(&stats, &style))?;
        }
        emit(&render::results(&results, &query, elapsed, &style))?;
    }
    Ok(())
}

fn repl(index_path: PathBuf, ranking: Ranking, limit: usize) -> Result<()> {
    let style = Style::detect(None);
    let engine = open(&index_path, ranking, &style)?;
    emit(&format!(
        "{}\n{}\n",
        style.bold("farol repl"),
        style.dim("type a query, or Ctrl-D to quit")
    ))?;

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{} ", style.cyan("›"));
        io::stdout().flush()?;
        let Some(line) = lines.next().transpose()? else {
            break;
        };
        let query = line.trim();
        if query.is_empty() {
            continue;
        }

        let started = Instant::now();
        // A malformed query must not end the session — report and keep going.
        match engine.search(query, limit) {
            Ok(results) => {
                let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
                emit(&render::results(&results, query, elapsed, &style))?;
            }
            Err(err) => emit(&format!("{}\n", style.yellow(&format!("error: {err}"))))?,
        }
    }
    emit("\n")
}

fn stats(index_path: PathBuf) -> Result<()> {
    let style = Style::detect(None);
    let mut engine = Engine::default();
    engine
        .load(&index_path)
        .with_context(|| format!("opening `{}`", index_path.display()))?;
    emit(&render::stats(
        &engine.stats(),
        &index_path.display().to_string(),
        &style,
    ))
}
