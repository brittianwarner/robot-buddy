//! Curated public voices from Fish Audio's Drama 3 documentation.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Voice {
    #[default]
    Adrian,
    Delia,
    Sheila,
}
impl Voice {
    pub fn id(self) -> &'static str {
        match self {
            Self::Adrian => "bf322df2096a46f18c579d0baa36f41d",
            Self::Delia => "c70cfb98867a4bfb9ae20d2401257b92",
            Self::Sheila => "f3a2b90078d54a65af4b95f63bc7798e",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Adrian => "Adrian",
            Self::Delia => "Delia",
            Self::Sheila => "Sheila",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceConfig {
    pub voice: Voice,
    pub delivery: String,
    #[serde(default)]
    pub synthesis: Synthesis,
    #[serde(default)]
    pub speech_rules: Vec<SpeechRule>,
}
impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            voice: Voice::default(),
            synthesis: Synthesis::default(),
            speech_rules: Vec::new(),
            delivery: include_str!("prompts/voice.txt").trim().into(),
        }
    }
}
impl VoiceConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.delivery.trim().is_empty() && self.delivery.len() <= 1500,
            "voice delivery must be 1-1500 bytes"
        );
        ensure!(self.speech_rules.len() <= 32, "at most 32 speech rules");
        for rule in &self.speech_rules {
            rule.validate()?;
        }
        self.synthesis.validate()?;
        Ok(())
    }
}
pub fn catalog() -> Value {
    json!(
        [Voice::Adrian, Voice::Delia, Voice::Sheila]
            .into_iter()
            .map(|voice| json!({"key":voice,"name":voice.name(),"voice_id":voice.id()}))
            .collect::<Vec<_>>()
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synthesis_rejects_out_of_bounds_controls() {
        let invalid = Synthesis {
            speed: 4.0,
            ..Default::default()
        };
        assert!(invalid.validate().is_err());
        let valid = Synthesis::default();
        valid.validate().unwrap();
        assert_eq!(valid.provider_options()["prosody"]["speed"], 1.0);
    }
    #[test]
    fn curated_voice_rejects_arbitrary_provider_ids() {
        assert!(
            serde_json::from_value::<VoiceConfig>(
                json!({"voice":"unapproved-id","delivery":"calm"})
            )
            .is_err()
        );
        VoiceConfig::default().validate().unwrap();
    }
}

/// Bounded Fish provider controls; transport format remains the client's PCM contract.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Synthesis {
    pub temperature: f64,
    pub top_p: f64,
    pub speed: f64,
    pub volume: f64,
    pub normalize_loudness: bool,
    pub normalize: bool,
    pub language: Option<String>,
    pub latency: Latency,
    pub chunk_length: u16,
    pub min_chunk_length: u16,
    pub condition_on_previous_chunks: bool,
    pub max_new_tokens: u16,
    pub repetition_penalty: f64,
    pub early_stop_threshold: f64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Latency {
    Low,
    Balanced,
    #[default]
    Normal,
}
impl Default for Synthesis {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.7,
            speed: 1.0,
            volume: 0.0,
            normalize_loudness: true,
            normalize: true,
            language: None,
            latency: Latency::Normal,
            chunk_length: 300,
            min_chunk_length: 50,
            condition_on_previous_chunks: true,
            max_new_tokens: 1024,
            repetition_penalty: 1.2,
            early_stop_threshold: 1.0,
        }
    }
}
impl Synthesis {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (0.0..=1.0).contains(&self.temperature)
                && (0.0..=1.0).contains(&self.top_p)
                && (0.0..=1.0).contains(&self.early_stop_threshold),
            "sampling values must be 0-1"
        );
        ensure!(
            (0.5..=2.0).contains(&self.speed) && (-20.0..=10.0).contains(&self.volume),
            "speed must be 0.5-2 and volume -20 to 10 dB"
        );
        ensure!(
            (100..=300).contains(&self.chunk_length)
                && self.min_chunk_length <= 100
                && (256..=2048).contains(&self.max_new_tokens)
                && (1.0..=2.0).contains(&self.repetition_penalty),
            "synthesis chunk/token settings out of bounds"
        );
        ensure!(
            self.language.as_ref().is_none_or(|v| v.len() <= 35
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')),
            "invalid normalization language"
        );
        Ok(())
    }
    pub fn provider_options(&self) -> Value {
        json!({"temperature":self.temperature,"top_p":self.top_p,"prosody":{"speed":self.speed,"volume":self.volume,"normalize_loudness":self.normalize_loudness},"normalize":self.normalize,"language":self.language,"latency":self.latency,"chunk_length":self.chunk_length,"min_chunk_length":self.min_chunk_length,"condition_on_previous_chunks":self.condition_on_previous_chunks,"max_new_tokens":self.max_new_tokens,"repetition_penalty":self.repetition_penalty,"early_stop_threshold":self.early_stop_threshold})
    }
}
pub fn schema() -> Value {
    json!({"type":"object","properties":{"voice":{"type":"string","enum":["adrian","delia","sheila"]},"delivery":{"type":"string","maxLength":1500},"speech_rules":{"type":"array","maxItems":32,"items":{"type":"object","properties":{"text":{"type":"string","maxLength":80},"style":{"type":"string","enum":["whisper","emphasis","soft","fast","slow","stress","forceful","phoneme"]},"phonemes":{"type":["string","null"]}},"required":["text","style"],"additionalProperties":false}},"synthesis":{"type":"object","properties":{"temperature":{"type":"number","minimum":0,"maximum":1},"top_p":{"type":"number","minimum":0,"maximum":1},"speed":{"type":"number","minimum":0.5,"maximum":2},"volume":{"type":"number","minimum":-20,"maximum":10},"normalize_loudness":{"type":"boolean"},"normalize":{"type":"boolean"},"language":{"type":["string","null"]},"latency":{"type":"string","enum":["normal","balanced","low"]},"chunk_length":{"type":"integer","minimum":100,"maximum":300},"min_chunk_length":{"type":"integer","minimum":0,"maximum":100},"condition_on_previous_chunks":{"type":"boolean"},"max_new_tokens":{"type":"integer","minimum":256,"maximum":2048},"repetition_penalty":{"type":"number","minimum":1,"maximum":2},"early_stop_threshold":{"type":"number","minimum":0,"maximum":1}},"additionalProperties":false}},"required":["voice","delivery"],"additionalProperties":false})
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechRule {
    pub text: String,
    pub style: WordStyle,
    #[serde(default)]
    pub phonemes: Option<String>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordStyle {
    Whisper,
    Emphasis,
    Soft,
    Fast,
    Slow,
    Stress,
    Forceful,
    Phoneme,
}
impl SpeechRule {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.text.trim().is_empty()
                && self.text == self.text.trim()
                && self.text.len() <= 80
                && !self
                    .text
                    .chars()
                    .any(|c| c.is_control() || "<>[]".contains(c)),
            "invalid speech rule text"
        );
        if matches!(self.style, WordStyle::Phoneme) {
            ensure!(
                !self.text.chars().any(char::is_whitespace),
                "English phoneme rules replace one word"
            );
            let phones = self.phonemes.as_deref().unwrap_or("");
            const SYMBOLS: &str = "AA AE AH AO AW AY B CH D DH EH ER EY F G HH IH IY JH K L M N NG OW OY P R S SH T TH UH UW V W Y Z ZH";
            ensure!(
                !phones.trim().is_empty()
                    && phones.len() <= 200
                    && phones.split_whitespace().all(|t| {
                        let base = t.trim_end_matches(['0', '1', '2']);
                        SYMBOLS.split_whitespace().any(|p| p == base)
                            && (t.len() == base.len()
                                || (t.len() == base.len() + 1
                                    && "AA AE AH AO AW AY EH ER EY IH IY OW OY UH UW"
                                        .split_whitespace()
                                        .any(|v| v == base)))
                    }),
                "use CMU Arpabet phonemes, with optional 0/1/2 stress digits"
            );
        } else {
            ensure!(
                self.phonemes.is_none(),
                "phonemes only apply to phoneme rules"
            );
        }
        Ok(())
    }
    fn annotate(&self, text: &str) -> String {
        let tag = match self.style {
            WordStyle::Whisper => "whisper",
            WordStyle::Emphasis => "emphasis",
            WordStyle::Soft => "soft",
            WordStyle::Fast => "fast",
            WordStyle::Slow => "slow",
            WordStyle::Stress => "stress",
            WordStyle::Forceful => "forceful",
            WordStyle::Phoneme => {
                return format!(
                    "<|phoneme_start|>{}<|phoneme_end|>",
                    self.phonemes.as_deref().unwrap_or("")
                );
            }
        };
        format!("<{tag}>{text}</{tag}>")
    }
}
impl VoiceConfig {
    /// Derive provider-only markup from a completed answer; never change the transcript.
    pub fn speech_input(&self, text: &str) -> Result<String> {
        self.validate()?;
        let mut hits = Vec::new();
        for rule in &self.speech_rules {
            for (start, matched) in text.match_indices(&rule.text) {
                let end = start + matched.len();
                if text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
                    || text[end..]
                        .chars()
                        .next()
                        .is_some_and(char::is_alphanumeric)
                {
                    continue;
                }
                hits.push((start, end, rule));
            }
        }
        hits.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end, rule) in hits {
            if start < cursor {
                continue;
            }
            out.push_str(&text[cursor..start]);
            out.push_str(&rule.annotate(&text[start..end]));
            cursor = end;
            ensure!(out.len() <= 24576, "annotated speech exceeds budget");
        }
        out.push_str(&text[cursor..]);
        ensure!(out.len() <= 24576, "annotated speech exceeds budget");
        Ok(out)
    }
}
#[cfg(test)]
mod speech_rule_tests {
    use super::*;
    #[test]
    fn annotations_preserve_punctuation_and_word_boundaries() {
        let config = VoiceConfig {
            speech_rules: vec![
                SpeechRule {
                    text: "robot".into(),
                    style: WordStyle::Emphasis,
                    phonemes: None,
                },
                SpeechRule {
                    text: "read".into(),
                    style: WordStyle::Phoneme,
                    phonemes: Some("R IY1 D".into()),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            config
                .speech_input("A robot can read, unlike robotics.")
                .unwrap(),
            "A <emphasis>robot</emphasis> can <|phoneme_start|>R IY1 D<|phoneme_end|>, unlike robotics."
        );
    }
    #[test]
    fn rejects_markup_in_phonemes() {
        let rule = SpeechRule {
            text: "hello".into(),
            style: WordStyle::Phoneme,
            phonemes: Some("<script>".into()),
        };
        assert!(rule.validate().is_err());
    }
}
