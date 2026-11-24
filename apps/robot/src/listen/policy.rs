use super::State;
use crate::input::Control;
use std::time::{Duration, Instant};

const FOLLOW: Duration = Duration::from_secs(4);
const ECHO_TAIL: Duration = Duration::from_millis(350);

#[derive(Clone, Copy, Default)]
pub(super) struct Policy {
    window: Option<(Instant, Instant)>,
    pub face_deadline: Option<Instant>,
    stopped: bool,
    acoustic_window: bool,
}

fn is_sep(c: char) -> bool {
    c.is_whitespace() || ",.!?:;-–—…\"'“”‘’".contains(c)
}

fn trim_lead(text: &str) -> &str {
    text.trim_start_matches(is_sep)
}

fn token(text: &str) -> (&str, &str) {
    let text = trim_lead(text);
    let mut end = text.len();
    for (idx, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            continue;
        }
        if matches!(ch, '\'' | '’')
            && text[idx + ch.len_utf8()..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        end = idx;
        break;
    }
    (&text[..end], trim_lead(&text[end..]))
}

/// Prefix only: hearing a third party mention Pip in conversation isn't a command.
fn addressed(text: &str, busy: bool) -> Option<&str> {
    let (first, rest) = token(text);
    if first.eq_ignore_ascii_case("hey") || first.eq_ignore_ascii_case("hi") {
        let (name, body) = token(rest);
        return name.eq_ignore_ascii_case("pip").then_some(body);
    }
    (!busy && first.eq_ignore_ascii_case("pip")).then_some(rest)
}

fn strip_possessive(word: &str) -> &str {
    word.strip_suffix("'s")
        .or_else(|| word.strip_suffix("’s"))
        .or_else(|| word.strip_suffix("'S"))
        .or_else(|| word.strip_suffix("’S"))
        .unwrap_or(word)
}

fn is_bare_pip_variant(word: &str) -> bool {
    let w = strip_possessive(word).to_ascii_lowercase();
    matches!(
        w.as_str(),
        "pip"
            | "pipp"
            | "pips"
            | "pippa"
            | "pippy"
            | "pippin"
            | "pipi"
            | "peep"
            | "peeps"
            | "pep"
            | "pepp"
            | "peps"
            | "bip"
            | "bipp"
            | "bips"
            | "bib"
            | "bep"
            | "pib"
            | "piff"
            | "kip"
            | "fip"
            | "yip"
    )
}

fn is_greeted_pip_variant(word: &str) -> bool {
    if is_bare_pip_variant(word) {
        return true;
    }
    let w = strip_possessive(word).to_ascii_lowercase();
    matches!(
        w.as_str(),
        "beep"
            | "pit"
            | "pitt"
            | "pic"
            | "pig"
            | "pete"
            | "pup"
            | "pop"
            | "tip"
            | "dip"
            | "hip"
            | "sip"
            | "rip"
            | "lip"
            | "nip"
            | "zip"
            | "vip"
    )
}

/// After neural wake already fired, strip Whisper's common mishearings of "Hey Pip".
fn acoustic_addressed(text: &str) -> Option<&str> {
    let (first, rest) = token(text);
    if first.eq_ignore_ascii_case("hey")
        || first.eq_ignore_ascii_case("hi")
        || first.eq_ignore_ascii_case("hay")
    {
        let (name, body) = token(rest);
        return is_greeted_pip_variant(name).then_some(body);
    }
    is_bare_pip_variant(first).then_some(rest)
}

fn is_non_speech_artifact(words: &str) -> bool {
    words.starts_with(['[', '(', '*'])
        || words.chars().any(|c| matches!(c, '♪' | '♫' | '♬' | '♩'))
        || (!words.is_empty() && !words.chars().any(char::is_alphanumeric))
}

fn normalize_artifact(text: &str) -> String {
    let trimmed = text.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '\'');
    trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Known whisper.cpp silence/outro/caption hallucinations; checked only on unaddressed clips
/// so legitimate addressed speech ("Hey Pip, thank you") is never blacklisted.
fn is_unaddressed_silence_artifact(text: &str) -> bool {
    let norm = normalize_artifact(text);
    matches!(
        norm.as_str(),
        "blank_audio"
            | "blank audio"
            | "no_speech"
            | "no speech"
            | "you"
            | "thank you"
            | "thanks"
            | "thank you very much"
            | "thanks for watching"
            | "thank you for watching"
            | "thanks for watching everyone"
            | "thank you for watching everyone"
            | "thanks for watching this video"
            | "thank you for watching this video"
            | "thanks for listening"
            | "thank you for listening"
            | "please subscribe"
            | "like and subscribe"
            | "please like and subscribe"
            | "don't forget to subscribe"
            | "subscribe to my channel"
            | "see you next time"
            | "see you in the next video"
            | "bye"
            | "bye-bye"
            | "bye bye"
            | "the end"
    ) || norm.starts_with("subtitles by ")
        || norm.starts_with("captions by ")
        || norm.starts_with("transcribed by ")
        || norm.contains("amara.org")
}

fn code(text: &str) -> Option<String> {
    let mut digits = String::new();
    for part in text
        .trim_end_matches(['.', '!', '?'])
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | ','))
        .filter(|s| !s.is_empty())
    {
        if part.bytes().all(|b| b.is_ascii_digit()) {
            digits.push_str(part);
        } else {
            let digit = [
                "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
            ]
            .iter()
            .position(|n| part.eq_ignore_ascii_case(n))?;
            digits.push(char::from(b"0123456789"[digit]));
        }
        if digits.len() > 6 {
            return None;
        }
    }
    (digits.len() == 6).then_some(digits)
}

impl Policy {
    pub fn acoustic_wake(&mut self, at: Instant) {
        self.stopped = false;
        // The utterance containing the name usually started before the score peaked.
        self.window = Some((
            at.checked_sub(Duration::from_secs(2)).unwrap_or(at),
            at + FOLLOW,
        ));
        self.face_deadline = Some(at + FOLLOW);
        self.acoustic_window = true;
    }

    pub fn acoustic_clip(&self, onset: Instant) -> bool {
        self.window.is_some_and(|(a, b)| (a..=b).contains(&onset))
    }
    pub fn changed(&mut self, old: State, state: State, now: Instant) {
        if old.busy
            && !state.busy
            && !old.muted
            && !state.muted
            && !old.confirming
            && !state.confirming
            && self.face_deadline.is_some_and(|until| until >= now)
        {
            // Our own Wake stopped playback. A new capture generation fences out its
            // echo; keep the listener open for the person already starting to speak.
            return;
        }
        self.window = None;
        self.acoustic_window = false;
        if state.busy || state.muted || state.confirming || old.confirming || old.muted {
            self.face_deadline = None;
            self.stopped = false;
        } else if old.busy && !self.stopped {
            self.window = Some((now + ECHO_TAIL, now + ECHO_TAIL + FOLLOW));
        }
    }

    pub fn heard(
        &mut self,
        text: &str,
        state: State,
        started: Instant,
        now: Instant,
    ) -> Vec<Control> {
        if state.muted
            || text.is_empty()
            || text.len() > crate::input::MAX_LINE
            || text.chars().any(char::is_control)
        {
            return vec![];
        }
        let clean = trim_lead(text);
        if clean.is_empty() {
            return vec![];
        }
        let in_window = !state.busy && self.window.is_some_and(|(a, b)| (a..=b).contains(&started));
        let body = addressed(clean, state.busy).or_else(|| {
            (in_window && self.acoustic_window)
                .then(|| acoustic_addressed(clean))
                .flatten()
        });
        let words = body.unwrap_or(clean);
        let (first, tail) = token(words);
        let confirm = first.eq_ignore_ascii_case("confirm");
        if state.confirming {
            return code(if confirm { tail } else { words })
                .map(Control::Confirm)
                .into_iter()
                .collect();
        }
        if confirm && code(tail).is_some() {
            return vec![];
        } // explicit claim command is never dialogue
        if body.is_none() && !in_window {
            return vec![];
        }
        if is_non_speech_artifact(words)
            || (body.is_none() && is_unaddressed_silence_artifact(words))
        {
            return vec![];
        }
        let mut commands = Vec::new();
        let intent = words
            .trim_end_matches(|c: char| ".!?\"'”’".contains(c))
            .to_ascii_lowercase();
        if intent == "go to sleep" {
            self.stopped = true;
            self.window = None;
            self.face_deadline = None;
            self.acoustic_window = false;
            return vec![Control::Sleep];
        }
        let stop = matches!(
            intent.as_str(),
            "stop" | "stop talking" | "never mind" | "nevermind"
        );
        if body.is_some() || stop {
            commands.push(Control::Wake);
        }
        self.stopped = stop;
        self.window = None;
        self.face_deadline = None;
        self.acoustic_window = false;
        if stop {
            commands.push(Control::Listening(false));
        } else if words.is_empty() {
            // A second clip may have already begun while the name was being decoded.
            self.window = Some((started, now + FOLLOW));
            self.face_deadline = Some(now + FOLLOW);
        } else {
            commands.push(Control::Say(words.to_owned()));
        }
        commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_clears_the_window_without_a_wake_or_chat_turn() {
        let now = Instant::now();
        let idle = State::default();
        let mut p = Policy::default();
        assert_eq!(p.heard("Hey Pip", idle, now, now), [Control::Wake]);
        assert_eq!(
            p.heard("Pip, go to sleep.", idle, now, now),
            [Control::Sleep]
        );
        assert!(p.heard("go to sleep", idle, now, now).is_empty());
        assert!(p.face_deadline.is_none());
    }

    #[test]
    fn barge_requires_address_and_stop_never_starts_a_turn() {
        let busy = State {
            busy: true,
            ..State::default()
        };
        let now = Instant::now();
        let mut p = Policy::default();
        for text in [
            "my name is Pip",
            "stop talking",
            "pipeline",
            "Pip, what time is it?",
        ] {
            assert!(p.heard(text, busy, now, now).is_empty());
        }
        assert_eq!(
            p.heard("Hey, Pip! Stop talking.", busy, now, now),
            [Control::Wake, Control::Listening(false)]
        );
        p.changed(busy, State::default(), now);
        assert!(
            p.heard(
                "still there?",
                State::default(),
                now + FOLLOW / 2,
                now + FOLLOW / 2
            )
            .is_empty()
        );
    }

    #[test]
    fn followup_is_scoped_to_voice_onset_not_decoder_finish_time() {
        let now = Instant::now();
        let idle = State::default();
        let busy = State { busy: true, ..idle };
        let mut p = Policy::default();
        p.changed(busy, idle, now);
        assert!(p.heard("playback echo", idle, now, now + FOLLOW).is_empty());
        assert!(
            p.heard("[BLANK_AUDIO]", idle, now + ECHO_TAIL, now + FOLLOW)
                .is_empty()
        );
        assert_eq!(
            p.heard(
                "what about tomorrow?",
                idle,
                now + ECHO_TAIL,
                now + FOLLOW * 3
            ),
            [Control::Say("what about tomorrow?".into())]
        );
        assert!(
            p.heard("stray speech", idle, now + FOLLOW, now + FOLLOW)
                .is_empty()
        );
    }

    #[test]
    fn only_confirming_state_interprets_digits_as_claim_input() {
        let now = Instant::now();
        let idle = State::default();
        let claiming = State {
            confirming: true,
            ..idle
        };
        let mut p = Policy::default();
        assert_eq!(
            p.heard("012 345.", claiming, now, now),
            [Control::Confirm("012345".into())]
        );
        assert_eq!(
            p.heard(
                "Pip, confirm zero one two three four five",
                claiming,
                now,
                now
            ),
            [Control::Confirm("012345".into())]
        );
        assert!(p.heard("Pip, confirm 123456", idle, now, now).is_empty());
        assert!(
            p.heard("Pip, tell a story 123456", claiming, now, now)
                .is_empty()
        );
        assert_eq!(
            p.heard("Pip, 123456", idle, now, now),
            [Control::Wake, Control::Say("123456".into())]
        );
    }

    #[test]
    fn leading_punctuation_and_misheard_name_work_after_acoustic_wake() {
        let now = Instant::now();
        let idle = State::default();
        let busy = State { busy: true, ..idle };

        // Leading punctuation works for direct transcript addressing too.
        let mut p = Policy::default();
        assert_eq!(
            p.heard("- Hey Pip, what time is it?", idle, now, now),
            [Control::Wake, Control::Say("what time is it?".into())]
        );
        assert_eq!(
            p.heard("...Pip, go to sleep.", idle, now, now),
            [Control::Sleep]
        );
        // Without acoustic wake, misheard names never wake Pip.
        for stray in [
            "Hey, Bip, what time is it?",
            "Hey, Pep.",
            "Kip, go to sleep.",
            "Pip's favorite color",
        ] {
            assert!(p.heard(stray, idle, now, now).is_empty(), "{stray}");
        }

        // After acoustic wake, leading punctuation and misheard wake names strip cleanly.
        p.acoustic_wake(now);
        assert_eq!(
            p.heard("- Hey, Bip, what time is it?", idle, now, now),
            [Control::Wake, Control::Say("what time is it?".into())]
        );

        // Misheard wake-only clip holds the follow-up window instead of sending a bogus turn.
        p.acoustic_wake(now);
        assert_eq!(p.heard("— Hey, Pep.", idle, now, now), [Control::Wake]);
        assert!(p.face_deadline.is_some());
        assert_eq!(
            p.heard(
                "what about tomorrow?",
                idle,
                now,
                now + Duration::from_secs(1)
            ),
            [Control::Say("what about tomorrow?".into())]
        );

        // Bare misheard name + sleep/stop works after acoustic wake (including post-barge-in).
        p.acoustic_wake(now);
        p.changed(busy, idle, now);
        assert_eq!(
            p.heard("...Bip, stop talking!", idle, now, now),
            [Control::Wake, Control::Listening(false)]
        );
        p.acoustic_wake(now);
        assert_eq!(
            p.heard("Hey, Pip's, go to sleep.", idle, now, now),
            [Control::Sleep]
        );

        // Ordinary English starters after acoustic wake are not stripped as bare names.
        p.acoustic_wake(now);
        assert_eq!(
            p.heard("Pick a number", idle, now, now),
            [Control::Say("Pick a number".into())]
        );
    }

    #[test]
    fn silence_and_song_artifacts_are_ignored_without_blocking_addressed_speech() {
        let now = Instant::now();
        let idle = State::default();
        let mut p = Policy::default();
        p.acoustic_wake(now);

        for artifact in [
            "- [BLANK_AUDIO]",
            "...(upbeat music)",
            "*music*",
            "♪",
            "♪ Music ♪",
            "♫ La la la ♫",
            "...",
            "Thank you.",
            "Thanks for watching!",
            "Thank you for watching.",
            "You.",
            "Subtitles by the Amara.org community",
            "Please subscribe.",
            "Bye.",
        ] {
            assert!(p.heard(artifact, idle, now, now).is_empty(), "{artifact}");
        }
        // Artifacts leave the open wake window intact for the actual utterance.
        assert_eq!(
            p.heard("what time is it?", idle, now, now),
            [Control::Say("what time is it?".into())]
        );

        // Legitimate addressed human speech is never blacklisted.
        for (input, expected) in [
            ("Hey Pip, thank you!", "thank you!"),
            ("Pip, thanks for watching.", "thanks for watching."),
            ("Hey Pip, you.", "you."),
            ("- Pip, bye!", "bye!"),
            ("Hey Pip, sing a song.", "sing a song."),
        ] {
            assert_eq!(
                p.heard(input, idle, now, now),
                [Control::Wake, Control::Say(expected.into())],
                "{input}"
            );
        }

        // Addressed speech after a misheard acoustic wake is also preserved.
        p.acoustic_wake(now);
        assert_eq!(
            p.heard("Hey, Bip, thank you!", idle, now, now),
            [Control::Wake, Control::Say("thank you!".into())]
        );

        // Longer unaddressed follow-up sentences containing those words still pass.
        p.acoustic_wake(now);
        assert_eq!(
            p.heard("Thank you, what about tomorrow?", idle, now, now),
            [Control::Say("Thank you, what about tomorrow?".into())]
        );
    }
}
