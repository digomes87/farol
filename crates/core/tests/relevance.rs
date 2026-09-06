//! Relevance regression tests over the sample corpus in `corpus/`.
//!
//! Unit tests prove each stage does what it claims in isolation. These tests
//! assert something harder and more valuable: that the *whole* pipeline still
//! returns the right document at the top for a handful of realistic queries.
//! Any change to analysis, ranking or query planning that quietly hurts quality
//! shows up here.

use std::path::PathBuf;

use farol_core::Engine;

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpus")
}

fn engine() -> Engine {
    let mut engine = Engine::default();
    engine.index_dir(corpus()).expect("corpus is readable");
    engine
}

fn top(engine: &Engine, query: &str) -> String {
    let results = engine.search(query, 5).expect("valid query");
    assert!(!results.is_empty(), "`{query}` returned nothing");
    results[0].uri.clone()
}

#[test]
fn every_corpus_document_is_indexed() {
    let stats = engine().stats();
    assert_eq!(stats.documents, 7);
    assert!(stats.vocabulary > 100, "vocabulary looks too small");
}

#[test]
fn queries_land_on_the_document_that_covers_the_topic() {
    let engine = engine();
    let expectations = [
        ("saturação da frequência", "ranqueamento-bm25.md"),
        ("stopwords stemming", "analise-de-texto.md"),
        ("renomeação atômica", "armazenamento.md"),
        ("deslocamento relativo", "frases-e-posicoes.md"),
        ("janela de termos distintos", "trechos-destacados.md"),
    ];
    for (query, expected) in expectations {
        let uri = top(&engine, query);
        assert!(
            uri.ends_with(expected),
            "`{query}` should rank {expected} first, got {uri}"
        );
    }
}

#[test]
fn accents_and_inflections_do_not_change_the_answer() {
    let engine = engine();
    // Same question, four spellings: accented, unaccented, plural, uppercase.
    let variants = ["posições", "posicoes", "posição", "POSIÇÕES"];
    let answers: Vec<_> = variants.iter().map(|q| top(&engine, q)).collect();
    assert!(
        answers.windows(2).all(|w| w[0] == w[1]),
        "spelling changed the ranking: {answers:?}"
    );
}

#[test]
fn a_phrase_is_stricter_than_the_same_words_loose() {
    let engine = engine();
    let loose = engine.search("lista de postings", 10).unwrap();
    let phrase = engine.search("\"lista de postings\"", 10).unwrap();
    assert!(
        phrase.len() < loose.len(),
        "the phrase matched as many documents as the loose terms"
    );
    assert!(!phrase.is_empty());
}

#[test]
fn required_and_excluded_clauses_reshape_the_result_set() {
    let engine = engine();
    let all = engine.search("indice", 10).unwrap().len();
    let excluded = engine.search("indice -postings", 10).unwrap().len();
    assert!(excluded < all, "the exclusion removed nothing");

    let required = engine.search("+postings +frase", 10).unwrap();
    assert!(required
        .iter()
        .all(|r| r.uri.ends_with(".md") && r.score > 0.0));
}

#[test]
fn results_carry_a_snippet_that_shows_the_match() {
    let engine = engine();
    let results = engine.search("stemming", 3).unwrap();
    assert!(results[0].snippet.contains("**stemming**"));
}

#[test]
fn ranking_is_stable_across_runs() {
    let first: Vec<_> = engine()
        .search("indice termos documento", 5)
        .unwrap()
        .into_iter()
        .map(|r| r.uri)
        .collect();
    let second: Vec<_> = engine()
        .search("indice termos documento", 5)
        .unwrap()
        .into_iter()
        .map(|r| r.uri)
        .collect();
    assert_eq!(first, second);
}
