use super::State;
use crate::input::Control;
use std::time::{Duration, Instant};

const FOLLOW: Duration = Duration::from_secs(4);
const ECHO_TAIL: Duration = Duration::from_millis(350);

#[derive(Default)]
pub(super) struct Policy {
    window: Option<(Instant, Instant)>,
    pub face_deadline: Option<Instant>,
    stopped: bool,
}

fn token(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(text.len());
    (
        &text[..end],
        text[end..].trim_start_matches(|c: char| c.is_whitespace() || ",.!?:;-".contains(c)),
    )
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
        let body = addressed(text, state.busy);
        let words = body.unwrap_or(text);
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
        let in_window = !state.busy && self.window.is_some_and(|(a, b)| (a..=b).contains(&started));
        if body.is_none() && !in_window {
            return vec![];
        }
        if words.starts_with(['[', '(', '*'])
            || (!words.is_empty() && !words.chars().any(char::is_alphanumeric))
        {
            return vec![];
        }
        let mut commands = Vec::new();
        let intent = words.trim_end_matches(['.', '!', '?']).to_ascii_lowercase();
        if intent == "go to sleep" {
            self.stopped = true;
            self.window = None;
            self.face_deadline = None;
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
}
