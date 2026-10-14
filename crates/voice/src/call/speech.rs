//! A streamed reply becomes speakable sentences: each one is voiced while the
//! model is still writing the next, so the assistant starts talking early.

/// Below this a sentence end is too likely an abbreviation ("e.g. ") to split on.
const MIN_SENTENCE: usize = 12;
/// A run-on longer than this is split at its last pause.
const MAX_SENTENCE: usize = 240;
/// Words whose period does not end a sentence.
const ABBREVIATIONS: &[&str] = &["mr", "mrs", "ms", "dr", "st", "vs", "etc", "approx"];

#[derive(Default)]
pub(crate) struct Sentences {
    buffer: String,
}

impl Sentences {
    /// Add streamed text; returns the sentences it completed.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        let mut out = Vec::new();
        while let Some(end) = self.boundary() {
            let rest = self.buffer.split_off(end);
            let sentence = std::mem::replace(&mut self.buffer, rest);
            if let Some(sentence) = speakable(&sentence) {
                out.push(sentence);
            }
        }
        out
    }

    /// Whatever is left once the reply ends.
    pub fn finish(&mut self) -> Option<String> {
        speakable(&std::mem::take(&mut self.buffer))
    }

    /// Byte index just past the first complete sentence, if any.
    fn boundary(&self) -> Option<usize> {
        let text = &self.buffer;
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let next = chars.peek().map(|(_, c)| *c);
            let end = i + c.len_utf8();
            if c == '\n' && !text[..i].trim().is_empty() {
                return Some(end);
            }
            if matches!(c, '.' | '!' | '?' | '…' | '।')
                && next.is_some_and(char::is_whitespace)
                && text[..end].trim().len() >= MIN_SENTENCE
                && !(c == '.' && abbreviation(&text[..i]))
            {
                return Some(end);
            }
        }
        if text.len() > MAX_SENTENCE {
            let cut = text[..MAX_SENTENCE]
                .rfind([',', ';', ':'])
                .or_else(|| text[..MAX_SENTENCE].rfind(' '))
                .map(|i| i + 1)
                .unwrap_or(MAX_SENTENCE);
            // Never split inside a character.
            let cut = (cut..=text.len())
                .find(|i| text.is_char_boundary(*i))
                .unwrap_or(text.len());
            return Some(cut);
        }
        None
    }
}

/// Whether the word before a period is an abbreviation ("e.g", "Dr", "J").
fn abbreviation(before: &str) -> bool {
    let word = before
        .split_whitespace()
        .next_back()
        .unwrap_or("")
        .trim_start_matches(['(', '"', '\'']);
    word.contains('.')
        || ABBREVIATIONS.contains(&word.to_lowercase().as_str())
        || (word.chars().count() == 1 && word.chars().all(char::is_uppercase))
}

/// What a voice should say for `text`: Markdown marks, code fences and links
/// reduced to their words. `None` when nothing is left to say.
pub(crate) fn speakable(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '_' | '`' | '#' | '>' | '~' | '|' => out.push(' '),
            // [label](url) → label
            ']' if chars.peek() == Some(&'(') => {
                for c in chars.by_ref() {
                    if c == ')' {
                        break;
                    }
                }
            }
            '[' => {}
            _ => out.push(c),
        }
    }
    let words: Vec<&str> = out.split_whitespace().collect();
    let line = words.join(" ");
    let line = line.trim_start_matches(['-', '•', ' ']).trim();
    line.chars()
        .any(char::is_alphanumeric)
        .then(|| line.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_come_out_as_soon_as_they_end() {
        let mut s = Sentences::default();
        assert!(s.push("Sure, I'll ask Pi to run").is_empty());
        assert_eq!(
            s.push(" the tests now. It may take a"),
            vec!["Sure, I'll ask Pi to run the tests now."]
        );
        assert!(s.push(" minute").is_empty());
        assert_eq!(s.finish().as_deref(), Some("It may take a minute"));
        assert_eq!(s.finish(), None);
    }

    #[test]
    fn abbreviations_and_decimals_do_not_split() {
        let mut s = Sentences::default();
        assert!(s.push("Use v2.5 e.g. ").is_empty());
        assert_eq!(
            s.push("the newest one. Done"),
            vec!["Use v2.5 e.g. the newest one."]
        );
    }

    #[test]
    fn run_ons_split_at_a_pause_and_newlines_end_lines() {
        let mut s = Sentences::default();
        let long = format!(
            "{}, and then more words follow {}",
            "word ".repeat(30),
            "more ".repeat(30)
        );
        let out = s.push(&long);
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with(','), "{}", out[0]);
        let mut s = Sentences::default();
        assert_eq!(s.push("First line\nSecond"), vec!["First line"]);
    }

    #[test]
    fn markdown_is_read_as_words() {
        assert_eq!(
            speakable("**Done.** See [the PR](https://x.y/1) for `main.rs`").as_deref(),
            Some("Done. See the PR for main.rs")
        );
        assert_eq!(speakable("- first item").as_deref(), Some("first item"));
        assert_eq!(speakable("```"), None);
        assert_eq!(speakable("  "), None);
    }
}
