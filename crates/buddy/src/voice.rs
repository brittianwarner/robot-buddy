//! Premade `ElevenLabs` voices for `OpenRouter` `elevenlabs/eleven-v4-turbo`.
//! Legacy Fish names deserialize onto the nearest catalog speaker.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SPEECH_MODEL: &str = "elevenlabs/eleven-v4-turbo";
/// The English phoneme symbols a phoneme speech rule may use.
const PHONEME_SYMBOLS: &str = "AA AE AH AO AW AY B CH D DH EH ER EY F G HH IH IY JH K L M N \
    NG OW OY P R S SH T TH UH UW V W Y Z ZH";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Voice {
    George,
    Sarah,
    Adam,
    Alice,
    Bella,
    Bill,
    Brian,
    Callum,
    Charlie,
    Chris,
    Daniel,
    Eric,
    Harry,
    Jessica,
    Laura,
    Liam,
    Lily,
    Matilda,
    River,
    Roger,
    #[default]
    Will,
}
impl Voice {
    pub const ALL: [Voice; 21] = [
        Self::George,
        Self::Sarah,
        Self::Adam,
        Self::Alice,
        Self::Bella,
        Self::Bill,
        Self::Brian,
        Self::Callum,
        Self::Charlie,
        Self::Chris,
        Self::Daniel,
        Self::Eric,
        Self::Harry,
        Self::Jessica,
        Self::Laura,
        Self::Liam,
        Self::Lily,
        Self::Matilda,
        Self::River,
        Self::Roger,
        Self::Will,
    ];
    /// `OpenRouter` voice name. Premade names are not `ElevenLabs` voice IDs.
    pub fn id(self) -> &'static str {
        match self {
            Self::George => "george",
            Self::Sarah => "sarah",
            Self::Adam => "adam",
            Self::Alice => "alice",
            Self::Bella => "bella",
            Self::Bill => "bill",
            Self::Brian => "brian",
            Self::Callum => "callum",
            Self::Charlie => "charlie",
            Self::Chris => "chris",
            Self::Daniel => "daniel",
            Self::Eric => "eric",
            Self::Harry => "harry",
            Self::Jessica => "jessica",
            Self::Laura => "laura",
            Self::Liam => "liam",
            Self::Lily => "lily",
            Self::Matilda => "matilda",
            Self::River => "river",
            Self::Roger => "roger",
            Self::Will => "will",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::George => "George",
            Self::Sarah => "Sarah",
            Self::Adam => "Adam",
            Self::Alice => "Alice",
            Self::Bella => "Bella",
            Self::Bill => "Bill",
            Self::Brian => "Brian",
            Self::Callum => "Callum",
            Self::Charlie => "Charlie",
            Self::Chris => "Chris",
            Self::Daniel => "Daniel",
            Self::Eric => "Eric",
            Self::Harry => "Harry",
            Self::Jessica => "Jessica",
            Self::Laura => "Laura",
            Self::Liam => "Liam",
            Self::Lily => "Lily",
            Self::Matilda => "Matilda",
            Self::River => "River",
            Self::Roger => "Roger",
            Self::Will => "Will",
        }
    }
    pub fn from_stored(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            // Fish Drama 3 names from actors created before Eleven v4 Turbo.
            "adrian" | "will" => Some(Self::Will),
            "delia" | "sarah" => Some(Self::Sarah),
            "sheila" | "jessica" => Some(Self::Jessica),
            "george" => Some(Self::George),
            "adam" => Some(Self::Adam),
            "alice" => Some(Self::Alice),
            "bella" => Some(Self::Bella),
            "bill" => Some(Self::Bill),
            "brian" => Some(Self::Brian),
            "callum" => Some(Self::Callum),
            "charlie" => Some(Self::Charlie),
            "chris" => Some(Self::Chris),
            "daniel" => Some(Self::Daniel),
            "eric" => Some(Self::Eric),
            "harry" => Some(Self::Harry),
            "laura" => Some(Self::Laura),
            "liam" => Some(Self::Liam),
            "lily" => Some(Self::Lily),
            "matilda" => Some(Self::Matilda),
            "river" => Some(Self::River),
            "roger" => Some(Self::Roger),
            _ => None,
        }
    }
}
impl<'de> Deserialize<'de> for Voice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::from_stored(&raw).ok_or_else(|| serde::de::Error::custom("unknown voice"))
    }
}
/// Tolerates unknown fields on read (stored rows, clients); writes decode
/// through `protocol::strict`.
#[derive(Clone, Serialize, Deserialize)]
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
            synthesis: Synthesis {
                // Stored with the profile. Eleven v4 Turbo does not receive these fields.
                // Pace stays at the provider default; character is the speaker and the wording.
                temperature: 0.55,
                top_p: 0.65,
                language: Some("en".into()),
                ..Synthesis::default()
            },
            speech_rules: Vec::new(),
            delivery: include_str!("prompts/voice.txt").trim().into(),
        }
    }
}
impl VoiceConfig {
    /// True for the pre-J.P. factory seed, including a stored Fish `adrian` row.
    pub fn is_factory_v1(&self) -> bool {
        self.voice == Voice::Will
            && self.speech_rules.is_empty()
            && self.delivery == include_str!("prompts/voice.factory-v1.txt").trim()
            && self.synthesis == Synthesis::default()
    }
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
    /// Eleven v4 Turbo body. Delivery is not spoken. Fish controls and speed are omitted.
    pub fn speech_body(&self, model: &str, text: &str) -> Result<Value> {
        let input = self.speech_input(text)?;
        Ok(json!({
            "model": model,
            "input": input,
            "voice": self.voice.id(),
            "response_format": "pcm",
            "provider": {
                "zdr": true,
                "options": {
                    "elevenlabs": {
                        "seed": 7,
                        "voice_settings": {
                            "stability": 0.5,
                            "similarity_boost": 0.75,
                            "style": 0.0,
                            "use_speaker_boost": true
                        }
                    }
                }
            }
        }))
    }
}
pub fn catalog() -> Value {
    json!(
        Voice::ALL
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
        assert!((valid.speed - 1.0).abs() < f64::EPSILON);
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
    #[test]
    fn legacy_fish_names_load_as_premade_speakers() {
        assert_eq!(
            serde_json::from_value::<Voice>(json!("adrian")).unwrap(),
            Voice::Will
        );
        assert_eq!(
            serde_json::from_value::<Voice>(json!("delia")).unwrap(),
            Voice::Sarah
        );
        assert_eq!(
            serde_json::from_value::<Voice>(json!("sheila")).unwrap(),
            Voice::Jessica
        );
        assert_eq!(serde_json::to_value(Voice::Will).unwrap(), json!("will"));
    }
    #[test]
    fn default_voice_is_will_and_factory_adrian_still_matches() {
        let config = VoiceConfig::default();
        assert_eq!(config.voice, Voice::Will);
        assert_eq!(config.voice.id(), "will");
        assert!((config.synthesis.speed - 1.0).abs() < f64::EPSILON);
        assert!(config.synthesis.volume.abs() < f64::EPSILON);
        assert!((config.synthesis.temperature - 0.55).abs() < f64::EPSILON);
        assert_eq!(config.synthesis.language.as_deref(), Some("en"));
        assert!(config.delivery.contains("Grandma's Boy"));
        assert!(!config.delivery.contains('['));
        assert!(config.delivery.len() <= 1500);
        assert!(!config.is_factory_v1());
        let stored = json!({
            "voice": "adrian",
            "delivery": include_str!("prompts/voice.factory-v1.txt").trim(),
            "synthesis": Synthesis::default(),
            "speech_rules": []
        });
        let previous: VoiceConfig = serde_json::from_value(stored).unwrap();
        assert_eq!(previous.voice, Voice::Will);
        assert!(previous.is_factory_v1());
        let mut changed = previous.clone();
        changed.synthesis.speed = 0.9;
        assert!(!changed.is_factory_v1());
        assert_eq!(catalog().as_array().unwrap().len(), 21);
    }
    #[test]
    fn eleven_request_speaks_the_line_only() {
        let body = VoiceConfig::default()
            .speech_body(SPEECH_MODEL, "It's three.")
            .unwrap();
        assert_eq!(body["model"], SPEECH_MODEL);
        assert_eq!(body["input"], "It's three.");
        assert_eq!(body["voice"], "will");
        assert_eq!(body["response_format"], "pcm");
        assert_eq!(body["provider"]["zdr"], true);
        assert_eq!(body["provider"]["options"]["elevenlabs"]["seed"], 7);
        let settings = &body["provider"]["options"]["elevenlabs"]["voice_settings"];
        assert_eq!(settings["stability"], 0.5);
        assert_eq!(settings["similarity_boost"], 0.75);
        assert_eq!(settings["style"], 0.0);
        assert_eq!(settings["use_speaker_boost"], true);
        assert!(settings.get("speed").is_none());
        assert!(body.get("instructions").is_none());
        assert!(body.get("speed").is_none());
        assert!(body["provider"]["options"].get("fish-audio").is_none());
        assert!(!body.to_string().contains("Grandma"));
    }
}

/// Bounded Fish provider controls. They stay on saved profiles and are not sent to Eleven.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
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
}
pub fn schema() -> Value {
    let voices: Vec<&str> = Voice::ALL.into_iter().map(Voice::id).collect();
    json!({
        "type": "object",
        "properties": {
            "voice": { "type": "string", "enum": voices },
            "delivery": { "type": "string", "maxLength": 1500 },
            "speech_rules": {
                "type": "array",
                "maxItems": 32,
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "maxLength": 80 },
                        "style": {
                            "type": "string",
                            "enum": [
                                "whisper",
                                "emphasis",
                                "soft",
                                "fast",
                                "slow",
                                "stress",
                                "forceful",
                                "phoneme",
                            ],
                        },
                        "phonemes": { "type": ["string", "null"] },
                    },
                    "required": ["text", "style"],
                    "additionalProperties": false,
                },
            },
            "synthesis": {
                "type": "object",
                "properties": {
                    "temperature": { "type": "number", "minimum": 0, "maximum": 1 },
                    "top_p": { "type": "number", "minimum": 0, "maximum": 1 },
                    "speed": {
                        "type": "number",
                        "minimum": 0.5,
                        "maximum": 2,
                    },
                    "volume": {
                        "type": "number",
                        "minimum": -20,
                        "maximum": 10,
                    },
                    "normalize_loudness": { "type": "boolean" },
                    "normalize": { "type": "boolean" },
                    "language": { "type": ["string", "null"] },
                    "latency": {
                        "type": "string",
                        "enum": ["normal", "balanced", "low"],
                    },
                    "chunk_length": {
                        "type": "integer",
                        "minimum": 100,
                        "maximum": 300,
                    },
                    "min_chunk_length": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 100,
                    },
                    "condition_on_previous_chunks": { "type": "boolean" },
                    "max_new_tokens": {
                        "type": "integer",
                        "minimum": 256,
                        "maximum": 2048,
                    },
                    "repetition_penalty": { "type": "number", "minimum": 1, "maximum": 2 },
                    "early_stop_threshold": { "type": "number", "minimum": 0, "maximum": 1 },
                },
                "additionalProperties": false,
            },
        },
        "required": ["voice", "delivery"],
        "additionalProperties": false,
    })
}

#[derive(Clone, Serialize, Deserialize)]
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
            ensure!(
                !phones.trim().is_empty()
                    && phones.len() <= 200
                    && phones.split_whitespace().all(|t| {
                        let base = t.trim_end_matches(['0', '1', '2']);
                        PHONEME_SYMBOLS.split_whitespace().any(|p| p == base)
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
        match self.style {
            // Eleven documents [whispering]. Other Fish tags would be spoken, so the word
            // stays plain.
            WordStyle::Whisper | WordStyle::Soft => format!("[whispering] {text}"),
            WordStyle::Emphasis
            | WordStyle::Stress
            | WordStyle::Forceful
            | WordStyle::Fast
            | WordStyle::Slow
            | WordStyle::Phoneme => text.to_owned(),
        }
    }
}
/// Byte cap on the rendered provider input.
const SPEECH_INPUT_BUDGET: usize = 24 * 1024;
/// Bracketed spans up to this many bytes are treated as tags and dropped whole.
/// Longer spans, and an opener followed by a space (`x < y`), keep their words
/// and lose only the brackets.
const MAX_TAG_BYTES: usize = 48;
const MARKUP: [char; 4] = ['[', ']', '<', '>'];

/// Remove markup the provider would perform instead of speak: model-authored
/// `[...]` audio tags, `<...>` SSML-like tags and stray brackets. Borrowed when
/// the text has none.
fn strip_markup(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(MARKUP) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(MARKUP) {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let close = match rest.as_bytes()[at] {
            b'[' => Some(']'),
            b'<' => Some('>'),
            _ => None,
        };
        let tag = close
            .filter(|_| !after.starts_with(char::is_whitespace))
            .and_then(|c| after.find(c));
        rest = match tag {
            Some(end) if end <= MAX_TAG_BYTES => &after[end + 1..],
            _ => after,
        };
        // Keep exactly one space where the markup was, and none at the edges.
        if out.is_empty() || out.ends_with(char::is_whitespace) {
            rest = rest.trim_start();
        } else if rest.starts_with(char::is_alphanumeric) {
            out.push(' ');
        }
    }
    out.push_str(rest);
    out.truncate(out.trim_end().len());
    std::borrow::Cow::Owned(out)
}

impl VoiceConfig {
    /// Derive provider-only markup from a completed answer; never change the transcript.
    ///
    /// Markup in the answer itself is removed first, so the only tag that can reach
    /// the provider is `[whispering]` from a saved rule. Where rule matches overlap,
    /// the longest wins; equal lengths go to the leftmost.
    pub fn speech_input(&self, text: &str) -> Result<String> {
        self.validate()?;
        let text = strip_markup(text);
        let text = text.as_ref();
        let mut hits = Vec::new();
        for (index, rule) in self.speech_rules.iter().enumerate() {
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
                hits.push((start, end, index));
            }
        }
        if hits.len() > 1 {
            // Claim bytes longest first (ties leftmost); a hit touching a claimed
            // byte loses. One bit per input byte.
            hits.sort_unstable_by_key(|&(start, end, _)| (std::cmp::Reverse(end - start), start));
            let mut claimed = vec![0u64; text.len().div_ceil(64)];
            let is_claimed = |claimed: &[u64], i: usize| claimed[i / 64] >> (i % 64) & 1 == 1;
            hits.retain(|&(start, end, _)| {
                if (start..end).any(|i| is_claimed(&claimed, i)) {
                    return false;
                }
                for i in start..end {
                    claimed[i / 64] |= 1 << (i % 64);
                }
                true
            });
            hits.sort_unstable_by_key(|&(start, ..)| start);
        }
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end, index) in hits {
            out.push_str(&text[cursor..start]);
            out.push_str(&self.speech_rules[index].annotate(&text[start..end]));
            cursor = end;
            ensure!(
                out.len() <= SPEECH_INPUT_BUDGET,
                "annotated speech exceeds budget"
            );
        }
        out.push_str(&text[cursor..]);
        ensure!(
            out.len() <= SPEECH_INPUT_BUDGET,
            "annotated speech exceeds budget"
        );
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
            "A robot can read, unlike robotics."
        );
        let whisper = VoiceConfig {
            speech_rules: vec![SpeechRule {
                text: "robot".into(),
                style: WordStyle::Whisper,
                phonemes: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            whisper.speech_input("A robot can read.").unwrap(),
            "A [whispering] robot can read."
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
