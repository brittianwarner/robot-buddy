//! Hot primitives on the speech and auth paths.
//! Run: `cargo bench -p robot-buddy --bench primitives -- --warm-up-time 1 --measurement-time 3`
use criterion::{Criterion, criterion_group, criterion_main};
use robot_buddy::{
    audio::PcmFormat,
    auth::{self, Session},
    voice::{SPEECH_MODEL, SpeechRule, VoiceConfig, WordStyle},
};
use std::hint::black_box;

const KEY: &[u8] = b"bench-signing-key-not-for-production-0123456789";

fn session() -> Session {
    Session {
        bot_id: "a".repeat(64),
        role: "device".into(),
        expires: auth::now() + 600,
    }
}

fn voice_with_rules() -> (VoiceConfig, String) {
    let styles = [
        WordStyle::Whisper,
        WordStyle::Emphasis,
        WordStyle::Soft,
        WordStyle::Fast,
        WordStyle::Slow,
        WordStyle::Stress,
        WordStyle::Forceful,
    ];
    let words: Vec<String> = (0..32).map(|i| format!("word{i}")).collect();
    let rules = words
        .iter()
        .enumerate()
        .map(|(i, word)| SpeechRule {
            text: word.clone(),
            style: styles[i % styles.len()],
            phonemes: None,
        })
        .collect();
    let config = VoiceConfig {
        speech_rules: rules,
        ..Default::default()
    };
    // A realistic sentence chunk that hits a handful of the rules.
    let text = format!(
        "Sure, {} and {} are on the list, but {} is not. Let's try {} next time.",
        words[3], words[17], words[30], words[8]
    );
    (config, text)
}

fn bench(c: &mut Criterion) {
    let token = auth::sign(KEY, "session", &session()).expect("sign");
    c.bench_function("auth/sign_session", |b| {
        let value = session();
        b.iter(|| auth::sign(black_box(KEY), "session", black_box(&value)).expect("sign"));
    });
    c.bench_function("auth/verify_session", |b| {
        b.iter(|| {
            auth::verify::<Session>(black_box(KEY), "session", black_box(&token)).expect("verify")
        });
    });
    let (config, text) = voice_with_rules();
    c.bench_function("voice/speech_body_32_rules", |b| {
        b.iter(|| {
            config
                .speech_body(black_box(SPEECH_MODEL), black_box(&text))
                .expect("body")
        });
    });
    c.bench_function("audio/pcm_format_parse", |b| {
        b.iter(|| PcmFormat::parse(black_box("audio/pcm;rate=24000;channels=1")).expect("parse"));
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
