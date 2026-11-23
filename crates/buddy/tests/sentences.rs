//! Sentence splitter corpus and fragmentation invariance.
use rand::{Rng, SeedableRng, rngs::StdRng};
use robot_buddy::sentences::{HARD_LIMIT, Sentences};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    name: String,
    input: String,
    chunks: Vec<String>,
}

fn corpus() -> Vec<Case> {
    serde_json::from_str(include_str!("fixtures/sentences.json")).expect("fixture corpus")
}

fn split<'a>(pieces: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut splitter = Sentences::default();
    let mut chunks = Vec::new();
    for piece in pieces {
        chunks.extend(splitter.push(piece));
    }
    chunks.extend(splitter.finish());
    chunks
}

/// Pieces of 1-`max` characters, always on char boundaries.
fn random_pieces<'a>(text: &'a str, rng: &mut StdRng, max: usize) -> Vec<&'a str> {
    let bounds: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at + 1 < bounds.len() {
        let next = (at + rng.gen_range(1..=max)).min(bounds.len() - 1);
        out.push(&text[bounds[at]..bounds[next]]);
        at = next;
    }
    out
}

fn chars(text: &str) -> Vec<&str> {
    text.char_indices()
        .map(|(i, c)| &text[i..i + c.len_utf8()])
        .collect()
}

#[test]
fn corpus_has_realistic_coverage() {
    assert!(corpus().len() >= 40);
}

#[test]
fn corpus_matches_whole_input() {
    for case in corpus() {
        assert_eq!(split([case.input.as_str()]), case.chunks, "{}", case.name);
        assert_eq!(case.chunks.concat(), case.input, "{}", case.name);
    }
}

#[test]
fn corpus_is_fragmentation_invariant() {
    let mut rng = StdRng::seed_from_u64(0x5eed_b0d1);
    for case in corpus() {
        assert_eq!(
            split(chars(&case.input)),
            case.chunks,
            "{} by char",
            case.name
        );
        for max in [2, 4, 8, 32] {
            let pieces = random_pieces(&case.input, &mut rng, max);
            assert_eq!(split(pieces), case.chunks, "{} by {max}", case.name);
        }
    }
}

#[test]
fn random_text_concatenates_bounds_and_ignores_fragmentation() {
    const PARTS: [&str; 24] = [
        "word",
        " ",
        " ",
        "Dr.",
        "3.",
        "14",
        "!",
        "?",
        ".",
        "\"",
        "…",
        "...",
        "\n",
        ",",
        ";",
        " —",
        "example.com",
        "机器人",
        "。",
        "🎉",
        "e.g.",
        "U.S.",
        "1. ",
        ") ",
    ];
    let mut rng = StdRng::seed_from_u64(42);
    for _ in 0..500 {
        let len = rng.gen_range(0..400);
        let text: String = (0..len)
            .map(|_| PARTS[rng.gen_range(0..PARTS.len())])
            .collect();
        let whole = split([text.as_str()]);
        assert_eq!(whole.concat(), text);
        assert!(whole.iter().all(|c| c.len() <= HARD_LIMIT + 3));
        assert_eq!(split(chars(&text)), whole);
        assert_eq!(split(random_pieces(&text, &mut rng, 6)), whole);
    }
}
