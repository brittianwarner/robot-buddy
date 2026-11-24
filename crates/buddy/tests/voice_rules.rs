//! Speech rule evaluation through the public `VoiceConfig` API.
//!
//! The corpus in `fixtures/voice_rules.json` is table-driven: each case gives saved rules,
//! a completed answer, and either the exact provider input or an error substring. The
//! generated cases below cover the caps, which are too large to write by hand.
use robot_buddy::voice::{SPEECH_MODEL, SpeechRule, VoiceConfig, WordStyle};
use serde::Deserialize;
use serde_json::json;

/// Byte budget for the rendered provider input (docs/architecture/voice.md: 24 KiB).
const BUDGET: usize = 24 * 1024;
const WHISPER: &str = "[whispering] ";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    rules: Vec<SpeechRule>,
    input: String,
    #[serde(default)]
    expect: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn corpus() -> Vec<Case> {
    let raw = include_str!("fixtures/voice_rules.json");
    serde_json::from_str(raw).expect("voice rule corpus parses")
}

fn config(rules: Vec<SpeechRule>) -> VoiceConfig {
    VoiceConfig {
        speech_rules: rules,
        ..VoiceConfig::default()
    }
}

fn rule(text: &str, style: WordStyle) -> SpeechRule {
    SpeechRule {
        text: text.into(),
        style,
        phonemes: None,
    }
}

/// What the provider input must look like apart from the one allowed tag.
fn without_whisper_tags(rendered: &str) -> String {
    rendered.replace(WHISPER, "")
}

#[test]
fn corpus_cases_render_exactly() {
    let cases = corpus();
    assert!(cases.len() >= 30, "corpus shrank to {}", cases.len());
    let mut failures = Vec::new();
    for case in cases {
        let rendered = config(case.rules).speech_input(&case.input);
        match (&case.expect, &case.error, rendered) {
            (Some(expect), None, Ok(out)) if &out == expect => {}
            (None, Some(error), Err(e)) if format!("{e:#}").contains(error.as_str()) => {}
            (expect, error, got) => failures.push(format!(
                "{}: expected {expect:?} / error {error:?}, got {:?}",
                case.name,
                got.map_err(|e| format!("{e:#}"))
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn rendering_never_changes_the_spoken_words() {
    for case in corpus() {
        let Some(expect) = case.expect else { continue };
        assert_eq!(
            without_whisper_tags(&expect),
            case.input,
            "{}: markup may only add [whispering]",
            case.name
        );
    }
}

#[test]
fn provider_input_has_no_markup_except_whispering() {
    for case in corpus() {
        if case.error.is_some() {
            continue;
        }
        let config = config(case.rules);
        let body = config
            .speech_body(SPEECH_MODEL, &case.input)
            .unwrap_or_else(|e| panic!("{}: {e:#}", case.name));
        let input = body["input"].as_str().expect("input is text");
        assert_eq!(input, config.speech_input(&case.input).expect("same input"));
        let plain = without_whisper_tags(input);
        assert!(
            !plain.contains(['[', ']', '<', '>']),
            "{}: provider input carries markup: {input}",
            case.name
        );
        let text = body.to_string();
        assert!(
            !text.contains(&config.delivery),
            "{}: delivery direction must not be spoken",
            case.name
        );
    }
}

#[test]
fn every_style_renders_whisper_or_plain() {
    let styles = [
        (WordStyle::Whisper, true),
        (WordStyle::Soft, true),
        (WordStyle::Emphasis, false),
        (WordStyle::Stress, false),
        (WordStyle::Forceful, false),
        (WordStyle::Fast, false),
        (WordStyle::Slow, false),
    ];
    for (style, whispers) in styles {
        let out = config(vec![rule("word", style)])
            .speech_input("One word here.")
            .expect("render");
        let expected = if whispers {
            "One [whispering] word here."
        } else {
            "One word here."
        };
        assert_eq!(out, expected);
    }
    let phoneme = SpeechRule {
        text: "word".into(),
        style: WordStyle::Phoneme,
        phonemes: Some("W ER1 D".into()),
    };
    assert_eq!(
        config(vec![phoneme])
            .speech_input("One word here.")
            .expect("render"),
        "One word here."
    );
}

#[test]
fn thirty_two_rules_are_allowed_and_the_thirty_third_is_not() {
    let rules = |n: usize| {
        (0..n)
            .map(|i| rule(&format!("word{i}"), WordStyle::Whisper))
            .collect::<Vec<_>>()
    };
    let text = (0..32)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let out = config(rules(32)).speech_input(&text).expect("32 rules");
    assert_eq!(out.matches(WHISPER).count(), 32);
    let err = config(rules(33))
        .speech_input(&text)
        .expect_err("33 rules must be rejected");
    assert!(format!("{err:#}").contains("at most 32"), "{err:#}");
    assert!(config(rules(33)).validate().is_err());
}

#[test]
fn rendered_input_is_capped_at_24_kib() {
    // Without rules the cap applies to the text itself.
    let exact = "a".repeat(BUDGET);
    assert_eq!(
        config(vec![]).speech_input(&exact).expect("at budget"),
        exact
    );
    assert!(config(vec![]).speech_input(&format!("{exact}a")).is_err());

    // Markup counts: text under the cap can render over it.
    let words = "hush ".repeat(BUDGET / 5);
    assert!(words.len() <= BUDGET);
    let whisper = config(vec![rule("hush", WordStyle::Whisper)]);
    let err = whisper
        .speech_input(&words)
        .expect_err("markup pushes past 24 KiB");
    assert!(format!("{err:#}").contains("budget"), "{err:#}");

    // Exactly at the cap with markup is allowed.
    let tagged = WHISPER.len() + "hush".len();
    let fill = BUDGET - tagged;
    let text = format!("{} hush", "b".repeat(fill - 1));
    let out = whisper.speech_input(&text).expect("exactly at budget");
    assert_eq!(out.len(), BUDGET);
    assert!(out.ends_with("[whispering] hush"));
}

#[test]
fn rules_do_not_change_the_canonical_transcript() {
    let config = config(vec![rule("secret", WordStyle::Whisper)]);
    let answer = String::from("The secret is safe.");
    let rendered = config.speech_input(&answer).expect("render");
    assert_eq!(answer, "The secret is safe.");
    assert_ne!(rendered, answer);
    let body = config.speech_body(SPEECH_MODEL, &answer).expect("body");
    assert_eq!(body["voice"], json!(config.voice.id()));
    assert_eq!(body["response_format"], "pcm");
}

/// docs/architecture/voice.md: "Longest overlapping matches win", across start positions,
/// so a longer rule that starts inside a shorter one still wins.
#[test]
fn longest_overlapping_match_wins_across_start_positions() {
    let config = config(vec![
        rule("New York", WordStyle::Emphasis),
        rule("York City Hall", WordStyle::Whisper),
    ]);
    assert_eq!(
        config
            .speech_input("At New York City Hall.")
            .expect("render"),
        "At New [whispering] York City Hall."
    );
}

/// Only rule-derived markup reaches Eleven v4, where `[...]` is an audio tag: a stray
/// "[laughs]" or "[whispering]" from the model is removed, not performed.
#[test]
fn model_authored_tags_do_not_reach_the_provider() {
    let config = config(vec![]);
    for answer in [
        "Sure [laughs] that's funny.",
        "Fine. [sighs]",
        "Okay <break time=\"1s\"/> next.",
    ] {
        let input = config.speech_input(answer).expect("render");
        let plain = without_whisper_tags(&input);
        assert!(
            !plain.contains(['[', ']', '<', '>']),
            "provider input carries model markup: {input}"
        );
    }
}

#[test]
fn equal_length_overlaps_go_to_the_leftmost() {
    // Two 8-byte rules overlapping on "York": the leftmost wins.
    let tie = config(vec![
        rule("York Cit", WordStyle::Emphasis),
        rule("New York", WordStyle::Whisper),
    ]);
    assert_eq!(
        tie.speech_input("At New York Cit.").expect("render"),
        "At [whispering] New York Cit."
    );
    // A 9-byte rule starting later beats the 8-byte leftmost one.
    let longer = config(vec![
        rule("York City", WordStyle::Emphasis),
        rule("New York", WordStyle::Whisper),
    ]);
    assert_eq!(
        longer.speech_input("At New York City.").expect("render"),
        "At New York City."
    );
}

#[test]
fn model_markup_is_removed_with_tidy_spacing() {
    let whisper = config(vec![rule("secret", WordStyle::Whisper)]);
    for (answer, expect) in [
        ("Sure [laughs] that's funny.", "Sure that's funny."),
        ("Fine. [sighs]", "Fine."),
        ("[whispering] The secret.", "The [whispering] secret."),
        ("Okay <break time=\"1s\"/> next.", "Okay next."),
        ("Ha[laughs]ha", "Ha ha"),
        ("Done[laughs].", "Done."),
        ("A [secret] word", "A word"),
        ("x < y and z > w", "x y and z w"),
        ("[laughs]", ""),
    ] {
        assert_eq!(
            whisper.speech_input(answer).expect("render"),
            expect,
            "{answer}"
        );
    }
    // Long bracketed text is not a tag: its words are kept, the brackets dropped.
    let long = "See [this is a long aside that is clearly not an audio tag at all] ok.";
    assert_eq!(
        whisper.speech_input(long).expect("render"),
        "See this is a long aside that is clearly not an audio tag at all ok."
    );
}
