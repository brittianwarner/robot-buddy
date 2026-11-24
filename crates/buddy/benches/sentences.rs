//! Sentence splitter throughput: the current splitter against the frozen legacy baseline.
//! Run: `cargo bench -p robot-buddy --bench sentences -- --warm-up-time 1 --measurement-time 3`
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use robot_buddy::sentences::Sentences;
use std::hint::black_box;

mod legacy_sentences;
use legacy_sentences::LegacySentences;

const REPLY: &str = "Hey, good to see you. Dr. Lee said the meeting moved to 3:30, so you've got \
about forty minutes. It's 72 degrees out and sunny, which is pretty nice for November. \
Want me to set a reminder? I can also read you the agenda, it's short. First, the budget \
review. Second, the 2.5 percent raise question. Third, whatever Sam brings up, as usual. \
Honestly, I'd grab a coffee first. That's my advice!";

/// Splits on char boundaries into pieces of about `size` bytes, like model tokens.
fn fragments(text: &str, size: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + size).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        out.push(&text[start..end]);
        start = end;
    }
    out
}

trait Splitter: Default {
    fn push(&mut self, text: &str) -> Vec<String>;
    fn finish(&mut self) -> Vec<String>;
}
impl Splitter for Sentences {
    fn push(&mut self, text: &str) -> Vec<String> {
        Sentences::push(self, text)
    }
    fn finish(&mut self) -> Vec<String> {
        Sentences::finish(self)
    }
}
impl Splitter for LegacySentences {
    fn push(&mut self, text: &str) -> Vec<String> {
        LegacySentences::push(self, text)
    }
    fn finish(&mut self) -> Vec<String> {
        LegacySentences::finish(self)
    }
}

fn run<S: Splitter>(pieces: &[&str]) -> usize {
    let mut splitter = S::default();
    let mut chunks = 0;
    for piece in pieces {
        chunks += splitter.push(black_box(piece)).len();
    }
    chunks + splitter.finish().len()
}

fn bench(c: &mut Criterion) {
    let long = "the robot keeps talking without any punctuation at all ".repeat(80);
    let cases: Vec<(&str, Vec<&str>, usize)> = vec![
        ("whole_reply", vec![REPLY], REPLY.len()),
        ("token_fragments", fragments(REPLY, 4), REPLY.len()),
        ("char_fragments", fragments(REPLY, 1), REPLY.len()),
        ("long_unpunctuated", fragments(&long, 4), long.len()),
    ];
    let mut group = c.benchmark_group("sentences");
    for (name, pieces, bytes) in &cases {
        group.throughput(Throughput::Bytes(*bytes as u64));
        group.bench_with_input(BenchmarkId::new("current", name), pieces, |b, p| {
            b.iter(|| run::<Sentences>(p));
        });
        group.bench_with_input(BenchmarkId::new("legacy", name), pieces, |b, p| {
            b.iter(|| run::<LegacySentences>(p));
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
