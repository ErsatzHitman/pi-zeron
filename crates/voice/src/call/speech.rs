//! The agent's messages become speakable sentences as they stream: each one
//! is voiced while the agent is still writing the next, and code, tables and
//! markup stay on screen.

/// Below this a sentence end is too likely an abbreviation ("e.g. ") to split on.
const MIN_SENTENCE: usize = 12;
/// A run-on longer than this is split at its last pause.
const MAX_SENTENCE: usize = 240;
/// Words whose period does not end a sentence.
const ABBREVIATIONS: &[&str] = &["mr", "mrs", "ms", "dr", "st", "vs", "etc", "approx"];

/// Said once a long message reaches its limit.
pub const MORE_IN_CHAT: &str = "The rest is in the chat.";

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

    /// Whatever is left once the text ends.
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

/// How a line of Markdown is read.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Prose,
    /// Opens (or closes) a code block with this marker.
    Fence(&'static str),
    /// A table row or an HTML tag: not read.
    Skip,
}

impl Line {
    /// What `line` is, once its start says; `None` while it could still be
    /// either (a lone "`" may become a fence).
    fn of(line: &str) -> Option<Self> {
        let start = line.trim_start();
        for marker in ["```", "~~~"] {
            if start.starts_with(marker) {
                return Some(Self::Fence(marker));
            }
        }
        let mut chars = start.chars();
        match (chars.next(), chars.next()) {
            (None, _) => None,
            (Some('|'), _) => Some(Self::Skip),
            (Some('<'), None) => None,
            (Some('<'), Some(c)) if c == '/' || c.is_ascii_alphabetic() => Some(Self::Skip),
            // "`" or "``" may still become a fence.
            (Some(c @ ('`' | '~')), next) if start.len() < 3 && next.is_none_or(|n| n == c) => None,
            _ => Some(Self::Prose),
        }
    }
}

/// Reads one streamed Markdown message aloud. Complete sentences come out as
/// soon as they are written; code blocks, tables and HTML are skipped, and a
/// message longer than its limit stops with [`MORE_IN_CHAT`].
pub struct Reader {
    sentences: Sentences,
    /// The line being written, and how much of it the sentences have.
    line: String,
    fed: usize,
    /// The marker of the open code block.
    fence: Option<&'static str>,
    limit: usize,
    said: usize,
    cut: bool,
}

impl Reader {
    /// A reader that says at most about `limit` characters.
    pub fn new(limit: usize) -> Self {
        Self {
            sentences: Sentences::default(),
            line: String::new(),
            fed: 0,
            fence: None,
            limit,
            said: 0,
            cut: false,
        }
    }

    /// Add streamed text; returns the sentences it completed.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = delta;
        while let Some(newline) = rest.find('\n') {
            self.line.push_str(&rest[..newline]);
            self.end_line(&mut out);
            rest = &rest[newline + 1..];
        }
        self.line.push_str(rest);
        // An unfinished line is read as it streams once it is known to be prose.
        if self.fence.is_none() && Line::of(&self.line) == Some(Line::Prose) {
            self.feed(&mut out);
        }
        out
    }

    /// Whatever is left once the message ends.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out);
        }
        if let Some(rest) = self.sentences.finish() {
            self.say(rest, &mut out);
        }
        self.fence = None;
        out
    }

    fn end_line(&mut self, out: &mut Vec<String>) {
        if let Some(marker) = self.fence {
            if self.line.trim_start().starts_with(marker) {
                self.fence = None;
            }
        } else {
            match Line::of(&self.line).unwrap_or(Line::Prose) {
                Line::Fence(marker) => self.fence = Some(marker),
                Line::Skip => {}
                Line::Prose => {
                    self.feed(out);
                    let sentences = self.sentences.push("\n");
                    for sentence in sentences {
                        self.say(sentence, out);
                    }
                }
            }
        }
        self.line.clear();
        self.fed = 0;
    }

    fn feed(&mut self, out: &mut Vec<String>) {
        let sentences = self.sentences.push(&self.line[self.fed..]);
        self.fed = self.line.len();
        for sentence in sentences {
            self.say(sentence, out);
        }
    }

    fn say(&mut self, sentence: String, out: &mut Vec<String>) {
        if self.cut {
            return;
        }
        let length = sentence.chars().count();
        if self.said > 0 && self.said + length > self.limit {
            self.cut = true;
            out.push(MORE_IN_CHAT.to_owned());
            return;
        }
        self.said += length;
        out.push(sentence);
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

/// What a voice should say for `text`: Markdown marks and links reduced to
/// their words, web addresses to "a link" and file paths to their file name.
/// `None` when nothing is left to say.
pub(crate) fn speakable(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '`' => {}
            '#' | '>' | '~' | '|' => out.push(' '),
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
    let words: Vec<String> = out
        .split_whitespace()
        .map(spoken_word)
        .filter(|w| !w.is_empty())
        .collect();
    let line = words.join(" ");
    let line = line.trim_start_matches(['-', '•', ' ']).trim();
    line.chars()
        .any(char::is_alphanumeric)
        .then(|| line.to_owned())
}

/// A web address says "a link", a path its last part ("main.rs") and
/// `snake_case` its words.
fn spoken_word(word: &str) -> String {
    let core =
        word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | '.' | ';' | ':' | '"' | '\''));
    let word = if core.is_empty() {
        word.to_owned()
    } else if core.contains("://") {
        word.replacen(core, "a link", 1)
    } else if let Some(name) = file_name(core) {
        word.replacen(core, name, 1)
    } else {
        word.to_owned()
    };
    word.replace('_', " ").trim().to_owned()
}

/// The last part of a path such as `src/ui/main.rs`; `None` for words like
/// "and/or", "50/50" or a date.
fn file_name(word: &str) -> Option<&str> {
    let parts: Vec<&str> = word.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let last = *parts.last()?;
    let path = parts.len() >= 3 || (parts.len() == 2 && last.contains('.'));
    (path && word.chars().any(char::is_alphabetic)).then_some(last)
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

    #[test]
    fn links_and_paths_are_said_briefly() {
        assert_eq!(
            speakable("Edited `crates/ui/src/invoice.rs`, see https://example.com/a/b.").as_deref(),
            Some("Edited invoice.rs, see a link.")
        );
        assert_eq!(
            speakable("The `invoice_color` variable").as_deref(),
            Some("The invoice color variable")
        );
        assert_eq!(
            speakable("Use and/or 50/50 on 14/10/2026.").as_deref(),
            Some("Use and/or 50/50 on 14/10/2026.")
        );
        assert_eq!(
            speakable("Open src/main.rs").as_deref(),
            Some("Open main.rs")
        );
    }

    fn read(chunks: &[&str]) -> Vec<String> {
        let mut reader = Reader::new(10_000);
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(reader.push(chunk));
        }
        out.extend(reader.finish());
        out
    }

    #[test]
    fn the_reader_speaks_prose_while_it_streams() {
        let mut reader = Reader::new(10_000);
        assert!(reader.push("I'm looking through the invoice").is_empty());
        assert_eq!(
            reader.push(" component to find the color. Then"),
            vec!["I'm looking through the invoice component to find the color."]
        );
        assert_eq!(reader.finish(), vec!["Then"]);
    }

    #[test]
    fn the_reader_skips_code_tables_and_html() {
        let message = "I found the variable.\n\n```css\n.invoice { color: red; }\n\
                       ```\n\n| File | Change |\n|---|---|\n| a.css | red → blue |\n\
                       <details>\nDone, it is blue now.";
        let chunked: Vec<&str> = message
            .split_inclusive(|c: char| c == ' ' || c == '`')
            .collect();
        let expected = vec!["I found the variable.", "Done, it is blue now."];
        assert_eq!(read(&chunked), expected);
        assert_eq!(read(&[message]), expected);
    }

    #[test]
    fn an_unclosed_code_block_is_never_read() {
        assert_eq!(
            read(&["Here it is:\n", "~~~\nlet secret = 1;\n", "more code"]),
            vec!["Here it is:"]
        );
    }

    #[test]
    fn long_messages_stop_with_a_pointer_to_the_chat() {
        let mut reader = Reader::new(60);
        let mut out = reader.push(
            "The first sentence is spoken in full. The second one is spoken too. \
             The third goes past the limit. The fourth is never mentioned. ",
        );
        out.extend(reader.finish());
        assert_eq!(
            out,
            vec!["The first sentence is spoken in full.", MORE_IN_CHAT,]
        );
    }
}
