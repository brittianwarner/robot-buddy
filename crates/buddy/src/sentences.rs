//! Bounded, UTF-8-safe segmentation of the spoken-only model channel.
//!
//! The splitter scans each byte once. It keeps a cursor into the pending text and a
//! small state machine, so a push never rescans text it has already classified.
//! A boundary is only decided once the character after it has arrived, which makes
//! the output independent of how the model fragments its tokens. `finish` decides
//! whatever is left. The concatenation of every chunk always equals the input.
//!
//! Rules, in order:
//! - Runs of terminal punctuation and the closing quotes or brackets after them stay
//!   together: `Really?!` and `"Go!"` end one chunk. Whitespace after a boundary
//!   belongs to the chunk it ends. A quoted `!` or `?` before a lower-case word
//!   (`"Go!" he said`) does not end the sentence.
//! - `!` and `?` end a sentence before whitespace. CJK terminators always do.
//! - `.` and ellipses end a sentence only before whitespace, and not when the next
//!   word starts in lower case. A period also does not end a sentence after a listed
//!   abbreviation, an initial, a dotted abbreviation (`U.S.`) or a list number at the
//!   start of a line. Periods inside words (`example.com`, `3.14`) never split.
//! - A newline ends a chunk.
//! - Before the first chunk of a stream, a clause mark (`,` `;` `:` `—`) followed by
//!   whitespace ends a chunk once it holds at least [`FIRST_CLAUSE_BYTES`], so first
//!   audio starts sooner.
//! - Unpunctuated text breaks at whitespace after [`SOFT_LIMIT`] bytes and anywhere
//!   after [`HARD_LIMIT`] bytes, so a chunk is at most `HARD_LIMIT + 3` bytes.

/// Bytes after which unpunctuated text breaks at the next whitespace.
pub const SOFT_LIMIT: usize = 240;
/// Bytes after which a chunk breaks at the next character boundary.
pub const HARD_LIMIT: usize = 600;
/// Minimum size of an early first chunk that ends at a clause mark.
pub const FIRST_CLAUSE_BYTES: usize = 24;

const ABBREVIATIONS: [&str; 12] = [
    "Mr", "Mrs", "Ms", "Dr", "Prof", "Sr", "Jr", "St", "vs", "etc", "e.g", "i.e",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    /// `!` or `?`: a sentence end before whitespace.
    Strong,
    /// A CJK terminator, or a CJK clause mark in the first chunk: always a boundary.
    Cjk,
    /// One `.`: a sentence end before whitespace, unless the next word is lower case.
    Period,
    /// `...` or `…`: like a period.
    Ellipsis,
    /// A clause mark in the first chunk: a boundary before whitespace.
    Clause,
}

#[derive(Clone, Copy)]
enum State {
    /// Ordinary text.
    Text,
    /// Inside a run of terminators and closers that started a candidate boundary.
    Run { kind: Run, closed: bool },
    /// After a decided boundary, absorbing whitespace until the next visible character.
    Space {
        /// The next word must not start in lower case for the boundary to hold.
        lowercase_continues: bool,
        newline: bool,
    },
}

pub struct Sentences {
    pending: String,
    /// Bytes of `pending` already classified.
    cursor: usize,
    state: State,
    /// Start of the current word in `pending`.
    word_start: usize,
    /// The current word is the first on its line.
    word_first_on_line: bool,
    /// A word has started on the current line.
    line_has_word: bool,
    /// A chunk has been emitted from this stream.
    emitted: bool,
}

impl Default for Sentences {
    fn default() -> Self {
        Self {
            pending: String::new(),
            cursor: 0,
            state: State::Text,
            word_start: 0,
            word_first_on_line: true,
            line_has_word: false,
            emitted: false,
        }
    }
}

fn is_cjk_terminator(c: char) -> bool {
    matches!(c, '。' | '！' | '？')
}
fn is_closer(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '”' | '’' | ')' | ']' | '}' | '»' | '」' | '』' | '）'
    )
}
fn is_clause(c: char) -> bool {
    matches!(c, ',' | ';' | ':' | '—')
}
fn is_cjk_clause(c: char) -> bool {
    matches!(c, '，' | '；' | '：' | '、')
}

impl Sentences {
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.pending.push_str(text);
        let mut out = Vec::new();
        self.scan(&mut out);
        out
    }

    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        self.scan(&mut out);
        if !self.pending.is_empty() {
            out.push(std::mem::take(&mut self.pending));
        }
        *self = Self {
            emitted: self.emitted || !out.is_empty(),
            ..Self::default()
        };
        out
    }

    /// Decides a boundary that waits on the next character as if a new sentence followed.
    /// For a stream that has stalled: the caller decides when, so output then depends on
    /// timing. Plain text and pending clause marks are kept.
    pub fn flush_boundary(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        self.scan(&mut out);
        let candidate = match self.state {
            State::Run { kind, .. } => kind != Run::Clause,
            State::Space { .. } => true,
            State::Text => false,
        };
        let visible = self.pending.chars().any(|c| !c.is_whitespace());
        if candidate && visible {
            self.emit(self.pending.len(), &mut out);
            self.state = State::Space {
                lowercase_continues: false,
                newline: false,
            };
        }
        out
    }

    fn scan(&mut self, out: &mut Vec<String>) {
        while let Some(c) = self.pending[self.cursor..].chars().next() {
            let at = self.cursor;
            if at >= HARD_LIMIT {
                self.emit(at, out);
                continue;
            }
            if self.step(c, at, out) {
                self.cursor += c.len_utf8();
            }
        }
    }

    /// Classifies `c` at byte `at`. Returns false when `c` must be classified again
    /// in the new state (the cursor does not advance).
    fn step(&mut self, c: char, at: usize, out: &mut Vec<String>) -> bool {
        match self.state {
            State::Text => self.text(c, at),
            State::Run { kind, closed } => self.run(kind, closed, c, at, out),
            State::Space {
                lowercase_continues,
                newline,
            } => self.space(lowercase_continues, newline, c, at, out),
        }
    }

    fn text(&mut self, c: char, at: usize) -> bool {
        if c.is_whitespace() {
            self.word_start = at + c.len_utf8();
            if c == '\n' {
                self.line_has_word = false;
                self.state = State::Space {
                    lowercase_continues: false,
                    newline: true,
                };
            } else if at >= SOFT_LIMIT {
                self.state = State::Space {
                    lowercase_continues: false,
                    newline: false,
                };
            }
            return true;
        }
        if at == self.word_start {
            self.word_first_on_line = !self.line_has_word;
            self.line_has_word = true;
        }
        let first_clause = !self.emitted && at + c.len_utf8() >= FIRST_CLAUSE_BYTES;
        let kind = match c {
            '!' | '?' => Some(Run::Strong),
            '.' => Some(Run::Period),
            '…' => Some(Run::Ellipsis),
            c if is_cjk_terminator(c) => Some(Run::Cjk),
            c if first_clause && is_clause(c) => Some(Run::Clause),
            // CJK clause marks are not followed by spaces.
            c if first_clause && is_cjk_clause(c) => Some(Run::Cjk),
            _ => None,
        };
        if let Some(kind) = kind {
            if kind == Run::Period && !self.word_can_end(at) {
                return true;
            }
            self.state = State::Run {
                kind,
                closed: false,
            };
        }
        true
    }

    fn run(&mut self, kind: Run, closed: bool, c: char, at: usize, out: &mut Vec<String>) -> bool {
        if kind == Run::Clause {
            return self.end_run(kind, false, c, at, out);
        }
        if !closed {
            let merged = match c {
                c if is_cjk_terminator(c) => Some(Run::Cjk),
                '!' | '?' if kind == Run::Cjk => Some(Run::Cjk),
                '!' | '?' => Some(Run::Strong),
                '.' | '…' => Some(match kind {
                    Run::Strong | Run::Cjk => kind,
                    _ => Run::Ellipsis,
                }),
                _ => None,
            };
            if let Some(kind) = merged {
                self.state = State::Run {
                    kind,
                    closed: false,
                };
                return true;
            }
        }
        if is_closer(c) {
            self.state = State::Run { kind, closed: true };
            return true;
        }
        self.end_run(kind, closed, c, at, out)
    }

    /// The run ended before `c`. Decides whether it is a boundary; `c` is classified again.
    fn end_run(
        &mut self,
        kind: Run,
        closed: bool,
        c: char,
        at: usize,
        out: &mut Vec<String>,
    ) -> bool {
        if c.is_whitespace() {
            // `"Go!" he said` continues the sentence; so does `Dr. Lee... then`.
            let lowercase_continues =
                matches!(kind, Run::Period | Run::Ellipsis) || (kind == Run::Strong && closed);
            self.state = State::Space {
                lowercase_continues,
                newline: false,
            };
            return false;
        }
        self.state = State::Text;
        if kind == Run::Cjk {
            self.emit(at, out);
        }
        false
    }

    fn space(
        &mut self,
        lowercase_continues: bool,
        newline: bool,
        c: char,
        at: usize,
        out: &mut Vec<String>,
    ) -> bool {
        if c.is_whitespace() {
            if c == '\n' {
                self.line_has_word = false;
                self.state = State::Space {
                    lowercase_continues,
                    newline: true,
                };
            }
            self.word_start = at + c.len_utf8();
            return true;
        }
        self.state = State::Text;
        self.word_start = at;
        let continues = lowercase_continues && !newline && c.is_lowercase();
        if !continues && self.pending[..at].chars().any(|c| !c.is_whitespace()) {
            self.emit(at, out);
        }
        false
    }

    /// Whether a single period after the current word can end a sentence.
    fn word_can_end(&self, at: usize) -> bool {
        let word =
            self.pending[self.word_start..at].trim_start_matches(|c: char| !c.is_alphanumeric());
        if word.is_empty() {
            return true;
        }
        if ABBREVIATIONS.contains(&word) {
            return false;
        }
        // Initials and dotted abbreviations: every dot-separated piece is one letter.
        if word
            .split('.')
            .all(|piece| piece.chars().count() == 1 && piece.chars().all(char::is_alphabetic))
        {
            return false;
        }
        // A list number at the start of a line: "1. Apples".
        let list_number = word.len() <= 3 && word.bytes().all(|b| b.is_ascii_digit());
        !(list_number && self.word_first_on_line)
    }

    fn emit(&mut self, end: usize, out: &mut Vec<String>) {
        if end == 0 {
            return;
        }
        let rest = self.pending.split_off(end);
        out.push(std::mem::replace(&mut self.pending, rest));
        self.cursor -= end;
        self.word_start = self.word_start.saturating_sub(end);
        self.emitted = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn split(text: &str) -> Vec<String> {
        let mut s = Sentences::default();
        let mut chunks = s.push(text);
        chunks.extend(s.finish());
        chunks
    }
    #[test]
    fn splits_before_completion_and_preserves_text() {
        let mut s = Sentences::default();
        assert!(s.push("Dr. Ada has 3.").is_empty());
        let mut pieces = s.push("14 apples. Next sentence!");
        pieces.extend(s.finish());
        assert_eq!(pieces, ["Dr. Ada has 3.14 apples. ", "Next sentence!"]);
    }
    #[test]
    fn bounds_unpunctuated_unicode() {
        let text = "机器人".repeat(300);
        let chunks = split(&text);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= HARD_LIMIT + 3));
    }
    #[test]
    fn coalesces_terminal_runs_and_closers() {
        assert_eq!(split("Really?! Yes."), ["Really?! ", "Yes."]);
        assert_eq!(split("\"Go!\" He ran."), ["\"Go!\" ", "He ran."]);
        assert_eq!(split("\"Go!\" he said. Ok."), ["\"Go!\" he said. ", "Ok."]);
        assert_eq!(split("Wait... What?"), ["Wait... ", "What?"]);
        assert_eq!(split("Well… maybe not."), ["Well… maybe not."]);
    }
    #[test]
    fn digits_and_domains() {
        assert_eq!(
            split("I have 3. You have 4."),
            ["I have 3. ", "You have 4."]
        );
        assert_eq!(split("Pi is 3.14 today."), ["Pi is 3.14 today."]);
        assert_eq!(
            split("Visit example.com. Then rest."),
            ["Visit example.com. ", "Then rest."]
        );
        assert_eq!(split("The U.S. team won."), ["The U.S. team won."]);
    }
    #[test]
    fn first_chunk_breaks_early_at_a_clause() {
        assert_eq!(
            split("Honestly, the best way to do this, I think, is to wait."),
            [
                "Honestly, the best way to do this, ",
                "I think, is to wait."
            ]
        );
        assert_eq!(split("Well, sure."), ["Well, sure."]);
    }
    #[test]
    fn flush_boundary_decides_a_stalled_sentence_end() {
        let mut s = Sentences::default();
        assert!(s.push("Partial output.").is_empty());
        assert_eq!(s.flush_boundary(), ["Partial output."]);
        assert!(s.push(" More text, still going").is_empty());
        assert!(s.flush_boundary().is_empty());
        assert_eq!(s.push(" on. Done"), [" More text, still going on. "]);
        assert_eq!(s.finish(), ["Done"]);
        let mut s = Sentences::default();
        assert!(s.push("Wait, ").is_empty());
        assert!(s.flush_boundary().is_empty());
    }
    #[test]
    fn whitespace_stays_with_the_chunk_it_ends() {
        assert_eq!(split("One.\n\nTwo."), ["One.\n\n", "Two."]);
        assert_eq!(split("  \n"), ["  \n"]);
    }
}
