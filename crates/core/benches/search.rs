//! Benchmarks for the two operations that matter: building the index and
//! answering a query.
//!
//! The corpus is synthetic but shaped like natural language: a Zipf-ish
//! vocabulary where a few terms are very frequent and most are rare. A uniform
//! vocabulary would make every posting list the same length and hide exactly
//! the cost the query planner is designed to avoid.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use farol_core::{Analyzer, Index, Query, Searcher};

const DOCS: usize = 5_000;
const TERMS_PER_DOC: usize = 120;
const VOCABULARY: usize = 4_000;

/// Deterministic pseudo-random generator, so every run indexes the same corpus.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Skewed towards low indexes, approximating a Zipf distribution.
    fn zipf(&mut self, n: usize) -> usize {
        let a = (self.next() % n as u64) as f64;
        let b = (self.next() % n as u64) as f64;
        (a.min(b) as usize).min(n - 1)
    }
}

fn corpus() -> Vec<String> {
    let mut rng = Rng(0x5EED);
    (0..DOCS)
        .map(|_| {
            let mut doc = String::with_capacity(TERMS_PER_DOC * 7);
            for _ in 0..TERMS_PER_DOC {
                doc.push_str(&format!("term{} ", rng.zipf(VOCABULARY)));
            }
            doc
        })
        .collect()
}

fn build(corpus: &[String]) -> Index {
    let mut index = Index::new(Analyzer::raw());
    for (id, text) in corpus.iter().enumerate() {
        index.add(format!("doc-{id}"), format!("Document {id}"), text);
    }
    index.finish();
    index
}

fn indexing(c: &mut Criterion) {
    let corpus = corpus();
    let mut group = c.benchmark_group("indexing");
    group.sample_size(10);
    group.bench_function("5k docs x 120 terms", |b| {
        b.iter_batched(|| corpus.clone(), |c| build(&c), BatchSize::LargeInput)
    });
    group.finish();
}

fn querying(c: &mut Criterion) {
    let index = build(&corpus());
    let analyzer = Analyzer::raw();
    let searcher = Searcher::new(&index);

    let cases = [
        ("single frequent term", "term0"),
        ("single rare term", "term3999"),
        ("two optional terms", "term0 term7"),
        ("required intersection", "+term0 +term7"),
        ("phrase", "\"term0 term1\""),
        ("with exclusion", "term0 -term1"),
    ];

    let mut group = c.benchmark_group("querying");
    for (name, input) in cases {
        let query = Query::parse(input, &analyzer).expect("valid query");
        group.bench_function(name, |b| b.iter(|| black_box(searcher.search(&query, 10))));
    }
    group.finish();
}

criterion_group!(benches, indexing, querying);
criterion_main!(benches);
