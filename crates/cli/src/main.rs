//! `farol` — command line interface for the search engine.
//!
//! ```text
//! farol index ./docs          build an index from a directory
//! farol search "+rust busca"  query it
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

/// Default location of the index file, relative to the working directory.
const DEFAULT_INDEX: &str = "farol.idx";

#[derive(Parser)]
#[command(
    name = "farol",
    version,
    about = "Motor de busca full-text para coleções de arquivos de texto",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Indexa um diretório recursivamente e grava o índice em disco.
    Index {
        /// Diretório raiz a percorrer.
        path: PathBuf,
        #[command(flatten)]
        index: IndexPath,
        /// Extensões consideradas, separadas por vírgula (ex.: md,txt).
        #[arg(long, value_delimiter = ',')]
        ext: Option<Vec<String>>,
        /// Desliga o stemming (indexa as palavras como escritas).
        #[arg(long)]
        no_stemming: bool,
        /// Mantém as stopwords no índice.
        #[arg(long)]
        keep_stopwords: bool,
    },
    /// Busca no índice e imprime os melhores resultados.
    Search {
        /// Consulta. Aceita +obrigatório, -excluído e "frase exata".
        query: Vec<String>,
        #[command(flatten)]
        index: IndexPath,
        #[command(flatten)]
        ranking: Ranking,
        /// Número máximo de resultados.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Sai em JSON em vez de texto.
        #[arg(long)]
        json: bool,
    },
    /// Sessão interativa: carrega o índice uma vez e responde várias consultas.
    Repl {
        #[command(flatten)]
        index: IndexPath,
        #[command(flatten)]
        ranking: Ranking,
        #[arg(short = 'n', long, default_value_t = 5)]
        limit: usize,
    },
    /// Mostra os contadores do índice.
    Stats {
        #[command(flatten)]
        index: IndexPath,
    },
}

#[derive(Args, Clone)]
struct IndexPath {
    /// Caminho do arquivo de índice.
    #[arg(short = 'i', long = "index", default_value = DEFAULT_INDEX, global = true)]
    file: PathBuf,
}

#[derive(Args, Clone, Copy)]
struct Ranking {
    /// Saturação de frequência do BM25.
    #[arg(long, default_value_t = 1.2)]
    k1: f32,
    /// Normalização por tamanho do documento (0.0 a 1.0).
    #[arg(long, default_value_t = 0.75)]
    b: f32,
    /// Tamanho máximo do trecho exibido, em caracteres.
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
        } => build_index(path, index.file, ext, no_stemming, keep_stopwords),
        Command::Search {
            query,
            index,
            ranking,
            limit,
            json,
        } => search(query.join(" "), index.file, ranking, limit, json),
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
) -> Result<()> {
    let mut engine = Engine::new(analyzer(no_stemming, keep_stopwords));
    if let Some(ext) = ext {
        engine = engine.with_extensions(ext);
    }

    let started = Instant::now();
    let count = engine
        .index_dir(&path)
        .with_context(|| format!("indexando `{}`", path.display()))?;
    engine
        .save(&index_path)
        .with_context(|| format!("gravando `{}`", index_path.display()))?;

    let style = Style::detect(None);
    println!(
        "{} documento(s) indexado(s) em {:.2}s → {}",
        count,
        started.elapsed().as_secs_f64(),
        style.cyan(&index_path.display().to_string())
    );
    print!(
        "{}",
        render::stats(&engine.stats(), &index_path.display().to_string(), &style)
    );
    Ok(())
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
            "abrindo `{}` (rode `farol index` antes)",
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
) -> Result<()> {
    let style = Style::detect(if json { Some(false) } else { None });
    let engine = open(&index_path, ranking, &style)?;

    let started = Instant::now();
    let results = engine.search(&query, limit)?;
    let elapsed = started.elapsed().as_secs_f64() * 1_000.0;

    if json {
        println!("{}", render::results_json(&results, &query, elapsed));
    } else {
        print!("{}", render::results(&results, &query, elapsed, &style));
    }
    Ok(())
}

fn repl(index_path: PathBuf, ranking: Ranking, limit: usize) -> Result<()> {
    let style = Style::detect(None);
    let engine = open(&index_path, ranking, &style)?;
    println!(
        "{}\n{}",
        style.bold("farol repl"),
        style.dim("digite uma consulta, ou Ctrl-D para sair")
    );

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
                print!("{}", render::results(&results, query, elapsed, &style));
            }
            Err(err) => println!("{}", style.yellow(&format!("erro: {err}"))),
        }
    }
    println!();
    Ok(())
}

fn stats(index_path: PathBuf) -> Result<()> {
    let style = Style::detect(None);
    let mut engine = Engine::default();
    engine
        .load(&index_path)
        .with_context(|| format!("abrindo `{}`", index_path.display()))?;
    print!(
        "{}",
        render::stats(&engine.stats(), &index_path.display().to_string(), &style)
    );
    Ok(())
}
