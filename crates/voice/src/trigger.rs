//! The trigger phrase, after OpenTalk: a dictation that ends with it ("…rewrite
//! this concisely") is rewritten by the provider's language model. Words said
//! after the phrase ("…rewrite this concisely and make it formal") become extra
//! instructions. Matching is token aligned and ignores case, punctuation and
//! apostrophes; it is never fuzzy.

pub const DEFAULT_PHRASE: &str = "rewrite this concisely";
pub const MAX_PHRASE_CHARS: usize = 80;
const MIN_TOKENS: usize = 2;
const MIN_LETTERS: usize = 6;
/// Connectives before the phrase are not part of the text to rewrite.
const CONNECTIVES: &[&str] = &["and", "then", "so", "please", "also"];
/// One courtesy group may follow the phrase without becoming instructions.
const COURTESY: &[&[&str]] = &[&["please"], &["thanks"], &["thank", "you"]];
const LEADING_CONNECTIVES: &[&str] = &["and", "then", "also", "so", "but", "just"];
const POLITE: &[&str] = &[
    "please", "thanks", "thank", "you", "very", "much", "so", "a", "lot",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    squashed: String,
}

impl Trigger {
    /// The phrase as matched: `clean` applies the dictation's own cleanup, so
    /// "rewrite, um, this concisely" still matches. `Err` says what to change.
    pub fn check(phrase: &str, clean: impl Fn(&str) -> String) -> Result<Self, &'static str> {
        let phrase = phrase.trim();
        if phrase.chars().count() > MAX_PHRASE_CHARS {
            return Err("Use at most 80 characters.");
        }
        let toks = tokens(&clean(phrase));
        let letters: usize = toks.iter().map(|t| t.sq.chars().count()).sum();
        if toks.len() < MIN_TOKENS || letters < MIN_LETTERS {
            return Err("Use at least two words (six letters).");
        }
        Ok(Self {
            squashed: toks.iter().map(|t| t.sq.as_str()).collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detect {
    NoMatch,
    /// Only the phrase was said: there is nothing to rewrite.
    OnlyPhrase,
    Match {
        body: String,
        instructions: Option<String>,
    },
}

pub fn word_count(text: &str) -> usize {
    tokens(text).len()
}

pub fn detect(text: &str, trigger: &Trigger) -> Detect {
    let toks = tokens(text);
    if toks.is_empty() || trigger.squashed.is_empty() {
        return Detect::NoMatch;
    }
    // The phrase ends the dictation, optionally before one courtesy group.
    if let Some(cut) = align(&toks, toks.len(), &trigger.squashed)
        .or_else(|| courtesy_start(&toks).and_then(|end| align(&toks, end, &trigger.squashed)))
    {
        return split_at(text, &toks, cut, None);
    }
    // The last occurrence followed by more words: those words are instructions.
    for end in (1..toks.len()).rev() {
        if let Some(cut) = align(&toks, end, &trigger.squashed) {
            return split_at(
                text,
                &toks,
                cut,
                trim_instructions(&text[toks[end - 1].end..]),
            );
        }
    }
    Detect::NoMatch
}

fn split_at(text: &str, toks: &[Tok], cut: usize, instructions: Option<String>) -> Detect {
    match trim_body(&text[..toks[cut].start]) {
        Some(body) => Detect::Match { body, instructions },
        None => Detect::OnlyPhrase,
    }
}

fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            ',' | ';'
                | ':'
                | '-'
                | '\u{2013}'
                | '\u{2014}'
                | '\u{2026}'
                | '('
                | '['
                | '"'
                | '\u{201C}'
        )
}

fn trim_instructions(raw: &str) -> Option<String> {
    let lead = |c: char| is_separator(c) || matches!(c, '.' | '!' | '?');
    let mut s = raw.trim_start_matches(lead);
    while let Some(first) = tokens(s)
        .first()
        .filter(|t| t.start == 0 && LEADING_CONNECTIVES.contains(&t.sq.as_str()))
    {
        s = s[first.end..].trim_start_matches(lead);
    }
    if tokens(s).iter().all(|t| POLITE.contains(&t.sq.as_str())) {
        return None;
    }
    Some(s.trim_end().to_owned())
}

/// The first token of `squashed` when it ends exactly at token `end`.
fn align(toks: &[Tok], end: usize, squashed: &str) -> Option<usize> {
    let mut acc = String::new();
    for i in (0..end).rev() {
        acc.insert_str(0, &toks[i].sq);
        if acc.len() == squashed.len() {
            return (acc == squashed).then_some(i);
        }
        if acc.len() > squashed.len() {
            return None;
        }
    }
    None
}

fn courtesy_start(toks: &[Tok]) -> Option<usize> {
    COURTESY
        .iter()
        .filter(|group| group.len() < toks.len())
        .find(|group| {
            toks[toks.len() - group.len()..]
                .iter()
                .zip(group.iter())
                .all(|(t, w)| t.sq == *w)
        })
        .map(|group| toks.len() - group.len())
}

fn trim_body(raw: &str) -> Option<String> {
    let mut end = raw.trim_end_matches(is_separator).len();
    let toks = tokens(&raw[..end]);
    let mut n = toks.len();
    while n > 0 && toks[n - 1].end == end && CONNECTIVES.contains(&toks[n - 1].sq.as_str()) {
        end = raw[..toks[n - 1].start]
            .trim_end_matches(is_separator)
            .len();
        n -= 1;
    }
    (n > 0).then(|| raw[..end].trim().to_owned())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Tok {
    sq: String,
    start: usize,
    end: usize,
}

/// Lowercase alphanumeric runs; apostrophes vanish ("don't" == "dont").
fn tokens(s: &str) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    let mut cur: Option<Tok> = None;
    for (i, ch) in s.char_indices() {
        let end = i + ch.len_utf8();
        if matches!(ch, '\'' | '\u{2019}' | '\u{2018}' | '`') {
            continue;
        }
        if ch.is_alphanumeric() {
            let t = cur.get_or_insert_with(|| Tok {
                sq: String::new(),
                start: i,
                end,
            });
            t.sq.extend(ch.to_lowercase());
            t.end = end;
        } else {
            out.extend(cur.take());
        }
    }
    out.extend(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger() -> Trigger {
        Trigger::check(DEFAULT_PHRASE, |p| p.to_owned()).unwrap()
    }

    #[test]
    fn the_phrase_at_the_end_rewrites_what_came_before() {
        assert_eq!(
            detect(
                "Send it Friday, and then rewrite this concisely.",
                &trigger()
            ),
            Detect::Match {
                body: "Send it Friday".into(),
                instructions: None
            }
        );
        assert_eq!(
            detect(
                "We ship Friday. Rewrite this concisely and make it formal.",
                &trigger()
            ),
            Detect::Match {
                body: "We ship Friday.".into(),
                instructions: Some("make it formal.".into())
            }
        );
        assert_eq!(
            detect("Notes here. Re-write this concisely, thanks.", &trigger()),
            Detect::Match {
                body: "Notes here.".into(),
                instructions: None
            }
        );
        assert_eq!(
            detect("Rewrite this concisely.", &trigger()),
            Detect::OnlyPhrase
        );
    }

    #[test]
    fn near_misses_do_not_match() {
        assert_eq!(
            detect("Please write this concisely.", &trigger()),
            Detect::NoMatch
        );
        assert_eq!(
            detect("Just a normal message.", &trigger()),
            Detect::NoMatch
        );
    }

    #[test]
    fn short_phrases_are_refused() {
        assert!(Trigger::check("go", |p| p.to_owned()).is_err());
        assert!(Trigger::check("do it", |p| p.to_owned()).is_err());
        assert!(Trigger::check("make it shorter", |p| p.to_owned()).is_ok());
    }
}
