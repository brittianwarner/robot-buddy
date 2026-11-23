//! BASELINE ONLY: frozen copy of the pre-rewrite splitter (commit 983e90f), kept so
//! `cargo bench --bench sentences` compares the current splitter against it in one run.
//! Not used by the application.
#[derive(Default)]
pub struct LegacySentences {
    pending: String,
}
impl LegacySentences {
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.pending.push_str(text);
        self.drain(false)
    }
    pub fn finish(&mut self) -> Vec<String> {
        self.drain(true)
    }
    fn drain(&mut self, flush: bool) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let mut end = None;
            for (i, c) in self.pending.char_indices() {
                if "!?。！？\n".contains(c) {
                    end = Some(i + c.len_utf8());
                    break;
                }
                if c == '.' {
                    let before = &self.pending[..i];
                    let word = before.split_whitespace().next_back().unwrap_or("");
                    let next = self.pending[i + 1..].chars().next();
                    let abbreviation = [
                        "Mr", "Mrs", "Ms", "Dr", "Prof", "Sr", "Jr", "St", "vs", "etc", "e.g",
                        "i.e",
                    ]
                    .contains(&word)
                        || word.len() == 1
                        || word.contains('.');
                    if !abbreviation
                        && !before.chars().next_back().is_some_and(|c| c.is_numeric())
                        && next.is_none_or(|c| c.is_whitespace() || "\"'”’".contains(c))
                    {
                        end = Some(i + 1);
                        break;
                    }
                }
                if i >= 240 && c.is_whitespace() {
                    end = Some(i + c.len_utf8());
                    break;
                }
                if i >= 600 {
                    end = Some(i);
                    break;
                }
            }
            let Some(end) = end.or_else(|| flush.then_some(self.pending.len())) else {
                break;
            };
            if end == 0 {
                break;
            }
            let tail = self.pending.split_off(end);
            let piece = std::mem::replace(&mut self.pending, tail);
            if !piece.trim().is_empty() {
                out.push(piece);
            }
        }
        out
    }
}
