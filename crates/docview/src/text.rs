//! Plain text and CSV.

use crate::Sheet;
use crate::limits::{MAX_CELL_CHARS, MAX_COLS, MAX_LINE_CHARS, MAX_LINES, MAX_ROWS};

/// Decode text: UTF-8 (BOM stripped) or UTF-16 with a BOM; invalid bytes
/// become U+FFFD.
pub(crate) fn decode(bytes: &[u8]) -> String {
    let utf16 = |little: bool| {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xff, 0xfe, ..] => utf16(true),
        [0xfe, 0xff, ..] => utf16(false),
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The file's lines, clipped to [`MAX_LINES`] lines of [`MAX_LINE_CHARS`].
pub(crate) fn read_lines(bytes: &[u8]) -> (Vec<String>, bool) {
    let text = decode(bytes);
    let mut lines = Vec::new();
    let mut truncated = false;
    for line in text.lines() {
        if lines.len() == MAX_LINES {
            truncated = true;
            break;
        }
        lines.push(clip(&line.replace('\t', "    "), MAX_LINE_CHARS));
    }
    (lines, truncated)
}

/// The delimiter the first line uses most outside quotes: comma, semicolon
/// or tab.
fn sniff_delimiter(text: &str) -> char {
    let first = text.lines().next().unwrap_or_default();
    let mut counts = [(',', 0usize), (';', 0), ('\t', 0)];
    let mut quoted = false;
    for c in first.chars() {
        if c == '"' {
            quoted = !quoted;
        } else if !quoted && let Some(entry) = counts.iter_mut().find(|(d, _)| *d == c) {
            entry.1 += 1;
        }
    }
    counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| *n > 0)
        .map_or(',', |(d, _)| *d)
}

/// Parse CSV (RFC 4180 quoting; delimiter sniffed) into one sheet.
pub(crate) fn read_csv(bytes: &[u8]) -> Sheet {
    let text = decode(bytes);
    let delimiter = sniff_delimiter(&text);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut cell_chars = 0usize;
    let mut quoted = false;
    let mut truncated = false;
    let mut chars = text.chars().peekable();
    let push_char = |cell: &mut String, cell_chars: &mut usize, c: char| {
        if *cell_chars < MAX_CELL_CHARS {
            cell.push(c);
        }
        *cell_chars += 1;
    };
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    push_char(&mut cell, &mut cell_chars, '"');
                }
                '"' => quoted = false,
                c => push_char(&mut cell, &mut cell_chars, c),
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() => quoted = true,
            c if c == delimiter => {
                if row.len() < MAX_COLS {
                    row.push(std::mem::take(&mut cell));
                } else {
                    truncated = true;
                    cell.clear();
                }
                cell_chars = 0;
            }
            '\r' => {}
            '\n' => {
                if row.len() < MAX_COLS {
                    row.push(std::mem::take(&mut cell));
                }
                cell.clear();
                cell_chars = 0;
                rows.push(std::mem::take(&mut row));
                if rows.len() == MAX_ROWS {
                    truncated = chars.peek().is_some();
                    break;
                }
            }
            c => push_char(&mut cell, &mut cell_chars, c),
        }
    }
    if rows.len() < MAX_ROWS && (!cell.is_empty() || !row.is_empty()) {
        if row.len() < MAX_COLS {
            row.push(cell);
        }
        rows.push(row);
    }
    Sheet {
        name: String::new(),
        rows,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_delimiters_and_line_endings() {
        let sheet = read_csv(b"name,note\r\n\"Doe, J\",\"said \"\"hi\"\"\nthere\"\nlast,");
        assert_eq!(
            sheet.rows,
            vec![
                vec!["name".to_string(), "note".into()],
                vec!["Doe, J".into(), "said \"hi\"\nthere".into()],
                vec!["last".into(), "".into()],
            ]
        );
        assert!(!sheet.truncated);
        let semi = read_csv("\u{feff}a;b;c\n1;2;3".as_bytes());
        assert_eq!(semi.rows[1], vec!["1", "2", "3"]);
        let tabs = read_csv(b"a\tb\n1\t2\n");
        assert_eq!(tabs.rows, vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    #[test]
    fn csv_and_text_are_clipped() {
        let mut big = String::new();
        for i in 0..(MAX_ROWS + 10) {
            big.push_str(&format!("{i},x\n"));
        }
        let sheet = read_csv(big.as_bytes());
        assert_eq!(sheet.rows.len(), MAX_ROWS);
        assert!(sheet.truncated);
        let wide = (0..MAX_COLS + 5)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sheet = read_csv(wide.as_bytes());
        assert_eq!(sheet.rows[0].len(), MAX_COLS);
        assert!(sheet.truncated);

        let (lines, truncated) = read_lines("a\tb\n".repeat(MAX_LINES + 1).as_bytes());
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[0], "a    b");
        assert!(truncated);
    }

    #[test]
    fn utf16_text_decodes() {
        let mut bytes = vec![0xff, 0xfe];
        for unit in "hé\nyo".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(read_lines(&bytes).0, vec!["hé", "yo"]);
    }
}
