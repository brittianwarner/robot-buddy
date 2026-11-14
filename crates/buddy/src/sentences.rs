//! Bounded, UTF-8-safe segmentation of the spoken-only model channel.
#[derive(Default)]
pub struct Sentences {
    pending: String,
}
impl Sentences {
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn splits_before_completion_and_preserves_text() {
        let mut s = Sentences::default();
        assert!(s.push("Dr. Ada has 3.").is_empty());
        let mut pieces = s.push("14 apples. Next sentence!");
        pieces.extend(s.finish());
        assert_eq!(pieces.concat(), "Dr. Ada has 3.14 apples. Next sentence!");
        assert!(pieces.len() >= 2);
    }
    #[test]
    fn bounds_unpunctuated_unicode() {
        let text = "机器人".repeat(300);
        let mut s = Sentences::default();
        let mut chunks = s.push(&text);
        chunks.extend(s.finish());
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|c| c.len() <= 604));
    }
}
