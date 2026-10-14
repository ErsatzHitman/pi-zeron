//! Filler-word cleanup for dictated text, after OpenTalk's rules. Hesitations
//! go everywhere; "like" and "you know" only where they are fillers (set off by
//! commas, or opening a sentence); a repeated small word keeps one copy; and
//! the punctuation around each removed word is repaired.

/// Hesitations in any language once repeated letters collapse ("ummm" → "um").
/// "ah", "eh" and "oh" stay: they carry meaning ("Ah, I see").
const HESITATIONS: &[&str] = &["uh", "uhm", "erm", "hm", "m", "ehm"];
/// Words, not hesitations, outside English ("um" in German, "er" in Dutch).
const ENGLISH_HESITATIONS: &[&str] = &["um", "er"];
/// Back-channel words a hesitation pairs with: "uh huh", "uh oh", "mm hmm".
const BACK_CHANNEL: &[&str] = &["huh", "oh", "hm"];
/// Small words whose immediate repeat is a stutter, not emphasis.
const STUTTER: &[&str] = &[
    "i", "a", "an", "the", "to", "and", "but", "or", "so", "we", "you", "he", "she", "it", "they",
    "is", "was", "of", "in", "on", "at", "for", "my", "i'm", "it's", "if",
];
/// Sentence openers that keep their comma when a filler after them goes:
/// "So, uh, I" → "So, I".
const OPENERS: &[&str] = &[
    "so", "well", "and", "but", "okay", "ok", "yeah", "yes", "no", "oh", "now", "then", "also",
    "right", "anyway",
];

struct Word {
    text: String,
    removed: bool,
}

impl Word {
    /// Lowercase letters, digits and apostrophes, without surrounding marks.
    fn core(&self) -> String {
        self.text
            .trim_matches(|c: char| !c.is_alphanumeric())
            .chars()
            .map(|c| if c == '\u{2019}' { '\'' } else { c })
            .flat_map(char::to_lowercase)
            .collect()
    }
    fn trailing(&self) -> &str {
        let end = self
            .text
            .trim_end_matches(|c: char| !c.is_alphanumeric())
            .len();
        &self.text[end..]
    }
    fn quoted(&self) -> bool {
        self.text.starts_with(['"', '\u{201C}', '\''])
    }
    fn ends_clause(&self) -> bool {
        self.trailing().contains([',', '.', '!', '?', ';', ':'])
    }
}

/// "ummm" → "um", "hmmm" → "hm".
fn collapse(core: &str) -> String {
    let mut out = String::with_capacity(core.len());
    for c in core.chars() {
        if !out.ends_with(c) {
            out.push(c);
        }
    }
    out
}

fn is_hesitation(core: &str, english: bool) -> bool {
    let core = collapse(core);
    HESITATIONS.contains(&core.as_str())
        || (english && ENGLISH_HESITATIONS.contains(&core.as_str()))
}

/// Remove filler words from one dictation. `english` enables the rules that
/// only hold in English ("um", "er", "like", "you know"). Returns an empty
/// string when nothing but fillers was said.
pub fn clean(text: &str, english: bool) -> String {
    let mut words: Vec<Word> = text
        .split_whitespace()
        .map(|w| Word {
            text: w.to_owned(),
            removed: false,
        })
        .collect();
    let cores: Vec<String> = words.iter().map(Word::core).collect();
    let live = |words: &[Word], from: usize, back: bool| -> Option<usize> {
        if back {
            (0..from).rev().find(|&j| !words[j].removed)
        } else {
            (from + 1..words.len()).find(|&j| !words[j].removed)
        }
    };
    // A word that opens a sentence or follows a comma is "set off".
    let set_off =
        |words: &[Word], i: usize| live(words, i, true).is_none_or(|p| words[p].ends_clause());

    for i in 0..words.len() {
        if words[i].removed || words[i].quoted() {
            continue;
        }
        let core = &cores[i];
        let prev = live(&words, i, true).map(|p| collapse(&cores[p]));
        let next = live(&words, i, false).map(|n| collapse(&cores[n]));
        let hesitation = is_hesitation(core, english)
            // "uh huh", "mm hmm", "uh oh" are answers, not hesitations.
            && !next.as_deref().is_some_and(|n| BACK_CHANNEL.contains(&n) && words[i].trailing().is_empty())
            && !(collapse(core) == "hm" && prev.as_deref().is_some_and(|p| p == "m" || p == "uh"))
            // "5 mm" is a unit.
            && !(collapse(core) == "m" && prev.as_deref().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit())));
        if hesitation {
            remove(&mut words, &cores, i);
            continue;
        }
        if !english || !set_off(&words, i) {
            continue;
        }
        // "Like, …" / ", like, …": never "I like it" or "I was like, no way".
        if core == "like" && words[i].trailing().starts_with(',') {
            remove(&mut words, &cores, i);
            continue;
        }
        // ", you know, …" / "You know, …" / a flat trailing ", you know."
        if (core == "you" || core == "ya" || core == "y'know") && words[i].trailing().is_empty() {
            let know = if core == "y'know" {
                Some(i)
            } else {
                live(&words, i, false)
            };
            if let Some(k) = know.filter(|&k| k == i || cores[k] == "know") {
                let tail = words[k].trailing().to_owned();
                let flat_tag = tail == "." && i > 0;
                if tail.starts_with(',') || flat_tag {
                    if k != i {
                        words[i].removed = true;
                    }
                    remove(&mut words, &cores, k);
                }
            }
        }
    }

    // Stutters: "the the client", "I-I think".
    for i in 0..words.len() {
        if words[i].removed {
            continue;
        }
        let parts: Vec<&str> = cores[i].split('-').collect();
        if parts.len() > 1 && parts.iter().all(|p| !p.is_empty() && *p == parts[0]) {
            let last = words[i]
                .text
                .rsplit('-')
                .next()
                .unwrap_or_default()
                .to_owned();
            words[i].text = last;
        }
        if let Some(n) = live(&words, i, false)
            && STUTTER.contains(&cores[i].as_str())
            && cores[n] == cores[i]
            && matches!(words[i].trailing(), "" | ",")
        {
            words[i].removed = true;
        }
    }

    render(&words)
}

/// Drop word `i`, keeping the sentence's punctuation sound: an end mark moves
/// to the word before ("it, um." → "it."), and a filler set off by commas takes
/// one of them along ("I think, uh, we" → "I think we").
fn remove(words: &mut [Word], cores: &[String], i: usize) {
    words[i].removed = true;
    let tail = words[i].trailing().to_owned();
    let Some(p) = (0..i).rev().find(|&j| !words[j].removed) else {
        return;
    };
    let prev_tail = words[p].trailing().to_owned();
    if let Some(end) = tail.chars().rev().find(|c| matches!(c, '.' | '!' | '?')) {
        if !prev_tail.contains(['.', '!', '?']) {
            let base = words[p].text.trim_end_matches([',', ';', ':']).to_owned();
            words[p].text = format!("{base}{end}");
        }
        return;
    }
    if tail.contains(',') && prev_tail == "," && !OPENERS.contains(&cores[p].as_str()) {
        let base = words[p].text.trim_end_matches(',').to_owned();
        words[p].text = base;
    }
}

fn render(words: &[Word]) -> String {
    let joined = words
        .iter()
        .filter(|w| !w.removed)
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if !joined.chars().any(char::is_alphanumeric) {
        return String::new();
    }
    let trimmed = joined
        .trim_start_matches(|c: char| {
            !c.is_alphanumeric() && !matches!(c, '"' | '\'' | '(' | '\u{201C}')
        })
        .trim_end_matches([',', ';', ':', ' ']);
    // Sentences open with a capital, and "i" is "I".
    let mut out = String::with_capacity(trimmed.len());
    let mut capitalize = true;
    for (ix, word) in trimmed.split(' ').enumerate() {
        if ix > 0 {
            out.push(' ');
        }
        let lone_i = word
            .trim_end_matches(|c: char| !c.is_alphanumeric())
            .split('\'')
            .next()
            .is_some_and(|w| w == "i");
        let mut chars = word.chars();
        match chars.next() {
            Some(first) if (capitalize || lone_i) && first.is_lowercase() => {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
            _ => out.push_str(word),
        }
        if word.chars().any(char::is_alphanumeric) {
            capitalize = word.ends_with(['.', '!', '?']);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn fillers_go_and_the_sentence_reads_cleanly() {
        assert_eq!(
            clean(
                "Um, so I think, uh, we should ship the new release on Friday, you know, because the the client is waiting.",
                true
            ),
            "So I think we should ship the new release on Friday because the client is waiting."
        );
        assert_eq!(clean("So, uh, I left.", true), "So, I left.");
        assert_eq!(clean("We ship it, um.", true), "We ship it.");
        assert_eq!(clean("Like, it works now.", true), "It works now.");
        assert_eq!(clean("Ummm uhhh", true), "");
    }

    #[test]
    fn real_words_stay() {
        for kept in [
            "I like it.",
            "I was like, no way.",
            "Do you know the answer?",
            "Uh huh, that works.",
            "Mm hmm.",
            "It is 5 mm wide.",
            "He had had enough.",
            "Say \"um\" less.",
        ] {
            assert_eq!(clean(kept, true), kept);
        }
        assert_eq!(
            clean("Ich habe um acht Uhr Zeit.", false),
            "Ich habe um acht Uhr Zeit."
        );
    }

    #[test]
    fn stutters_collapse_and_i_is_capital() {
        assert_eq!(clean("i-i think i'm ready", true), "I think I'm ready");
        assert_eq!(clean("we we need to go", true), "We need to go");
    }
}
