//! Previews of document attachments (PDF, Excel, Word, PowerPoint, CSV and
//! text) for the chat's thumbnails and in-chat viewer.
//!
//! [`open`] parses a file's bytes into a [`Document`]: PDF pages stay lazily
//! rendered ([`PdfDoc::render_png`]); every other kind is read once into a
//! small model the UI draws itself. Parsing is bounded (see [`limits`]) and
//! never panics out: a parser panic becomes [`PreviewError::Unreadable`].
//! Callers run it off the UI thread.

pub mod limits;
mod ooxml;
mod pdf;
mod text;

use std::sync::Arc;

pub use pdf::PdfDoc;

/// The kinds of file that get a preview, by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocKind {
    Pdf,
    Word,
    Sheet,
    Slides,
    Csv,
    Text,
}

impl DocKind {
    /// The kind for a file name or path, or `None` when it gets no preview
    /// (images have their own pipeline; `.doc`, `.xls`, `.ppt` are binary
    /// formats this crate does not read).
    pub fn of(name: &str) -> Option<Self> {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let (_, ext) = base.rsplit_once('.')?;
        Some(match ext.to_ascii_lowercase().as_str() {
            "pdf" => Self::Pdf,
            "docx" | "docm" => Self::Word,
            "xlsx" | "xlsm" => Self::Sheet,
            "pptx" | "pptm" => Self::Slides,
            "csv" | "tsv" => Self::Csv,
            "txt" | "md" | "markdown" | "log" | "json" | "jsonl" | "xml" | "yaml" | "yml"
            | "toml" | "ini" | "cfg" | "conf" | "env" | "html" | "htm" | "css" | "js" | "mjs"
            | "cjs" | "ts" | "tsx" | "jsx" | "py" | "rs" | "go" | "java" | "kt" | "c" | "h"
            | "cc" | "cpp" | "hpp" | "cs" | "rb" | "php" | "sh" | "bash" | "zsh" | "ps1"
            | "sql" | "swift" | "lua" | "diff" | "patch" => Self::Text,
            _ => return None,
        })
    }

    /// The short badge shown on a thumbnail.
    pub fn badge(self, name: &str) -> String {
        match self {
            Self::Pdf => "PDF".into(),
            Self::Word => "DOCX".into(),
            Self::Sheet => "XLSX".into(),
            Self::Slides => "PPTX".into(),
            Self::Csv | Self::Text => name
                .rsplit_once('.')
                .map(|(_, ext)| ext.to_ascii_uppercase())
                .filter(|ext| ext.len() <= 5)
                .unwrap_or_else(|| "TXT".into()),
        }
    }
}

/// Why a file could not be previewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewError {
    /// Over one of the [`limits`].
    TooLarge,
    /// Password-protected or encrypted.
    Encrypted,
    /// Corrupt, or not the format its extension claims.
    Unreadable,
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "This file is too large to preview.",
            Self::Encrypted => "This file is password-protected.",
            Self::Unreadable => "This file could not be read.",
        })
    }
}

impl std::error::Error for PreviewError {}

/// One sheet of a workbook, or a CSV file: rows of cell text, clipped to
/// [`limits::MAX_ROWS`] × [`limits::MAX_COLS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<String>>,
    /// Rows or columns past the limits were left out.
    pub truncated: bool,
}

impl Sheet {
    /// The widest row's cell count.
    pub fn columns(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }
}

/// A paragraph-level piece of a Word document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// `level` 1 is a title or top heading.
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    /// A list item; `level` 0 is the outermost.
    Bullet {
        level: u8,
        text: String,
    },
    Table(Vec<Vec<String>>),
}

/// One PowerPoint slide's text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Slide {
    pub title: Option<String>,
    pub lines: Vec<String>,
}

/// A parsed document.
#[derive(Debug, Clone)]
pub enum Document {
    Pdf(PdfDoc),
    Sheets(Vec<Sheet>),
    Blocks(Vec<Block>),
    Slides(Vec<Slide>),
    Text { lines: Vec<String>, truncated: bool },
}

impl Document {
    /// A few lines of text for a thumbnail card. PDFs render a page instead.
    pub fn thumb_lines(&self, max: usize) -> Vec<String> {
        let clip = |text: &str| text.chars().take(80).collect::<String>();
        match self {
            Self::Pdf(_) => Vec::new(),
            Self::Sheets(sheets) => sheets
                .first()
                .map(|sheet| {
                    sheet
                        .rows
                        .iter()
                        .take(max)
                        .map(|row| clip(&row.join("  ")))
                        .collect()
                })
                .unwrap_or_default(),
            Self::Blocks(blocks) => blocks
                .iter()
                .flat_map(|block| match block {
                    Block::Heading { text, .. }
                    | Block::Paragraph(text)
                    | Block::Bullet { text, .. } => vec![clip(text)],
                    Block::Table(rows) => rows.iter().map(|row| clip(&row.join("  "))).collect(),
                })
                .filter(|line| !line.trim().is_empty())
                .take(max)
                .collect(),
            Self::Slides(slides) => slides
                .first()
                .map(|slide| {
                    slide
                        .title
                        .iter()
                        .chain(&slide.lines)
                        .map(|line| clip(line))
                        .take(max)
                        .collect()
                })
                .unwrap_or_default(),
            Self::Text { lines, .. } => lines.iter().take(max).map(|line| clip(line)).collect(),
        }
    }

    /// Bytes this document keeps in memory, for cache budgets.
    pub fn retained_bytes(&self) -> usize {
        let text = |s: &String| s.len() + 24;
        match self {
            Self::Pdf(pdf) => pdf.retained_bytes(),
            Self::Sheets(sheets) => sheets
                .iter()
                .flat_map(|sheet| &sheet.rows)
                .map(|row| row.iter().map(text).sum::<usize>() + 24)
                .sum(),
            Self::Blocks(blocks) => blocks
                .iter()
                .map(|block| match block {
                    Block::Heading { text: t, .. }
                    | Block::Paragraph(t)
                    | Block::Bullet { text: t, .. } => text(t),
                    Block::Table(rows) => rows.iter().flatten().map(text).sum(),
                })
                .sum(),
            Self::Slides(slides) => slides
                .iter()
                .map(|slide| {
                    slide
                        .title
                        .iter()
                        .chain(&slide.lines)
                        .map(text)
                        .sum::<usize>()
                })
                .sum(),
            Self::Text { lines, .. } => lines.iter().map(text).sum(),
        }
    }
}

/// Parse `bytes` as a `kind` document. Bounded by [`limits`]; a panic inside
/// a parser is caught and reported as [`PreviewError::Unreadable`].
pub fn open(kind: DocKind, bytes: Arc<Vec<u8>>) -> Result<Document, PreviewError> {
    if bytes.len() > limits::MAX_INPUT_BYTES {
        return Err(PreviewError::TooLarge);
    }
    std::panic::catch_unwind(move || match kind {
        DocKind::Pdf => PdfDoc::open(bytes).map(Document::Pdf),
        DocKind::Word => ooxml::read_docx(&bytes).map(Document::Blocks),
        DocKind::Sheet => ooxml::read_xlsx(&bytes).map(Document::Sheets),
        DocKind::Slides => ooxml::read_pptx(&bytes).map(Document::Slides),
        DocKind::Csv => Ok(Document::Sheets(vec![text::read_csv(&bytes)])),
        DocKind::Text => {
            let (lines, truncated) = text::read_lines(&bytes);
            Ok(Document::Text { lines, truncated })
        }
    })
    .unwrap_or(Err(PreviewError::Unreadable))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_come_from_the_extension() {
        assert_eq!(DocKind::of("/up/ab12cd34-Report.PDF"), Some(DocKind::Pdf));
        assert_eq!(DocKind::of("budget.xlsx"), Some(DocKind::Sheet));
        assert_eq!(DocKind::of("C:\\x\\notes.docx"), Some(DocKind::Word));
        assert_eq!(DocKind::of("deck.pptx"), Some(DocKind::Slides));
        assert_eq!(DocKind::of("data.tsv"), Some(DocKind::Csv));
        assert_eq!(DocKind::of("main.rs"), Some(DocKind::Text));
        assert_eq!(DocKind::of("photo.png"), None);
        assert_eq!(DocKind::of("legacy.doc"), None);
        assert_eq!(DocKind::of("LICENSE"), None);
        assert_eq!(DocKind::Text.badge("main.rs"), "RS");
    }

    #[test]
    fn garbage_is_unreadable_not_a_panic() {
        let junk = Arc::new(vec![0xffu8; 4096]);
        for kind in [DocKind::Pdf, DocKind::Word, DocKind::Sheet, DocKind::Slides] {
            assert_eq!(
                open(kind, junk.clone()).err(),
                Some(PreviewError::Unreadable)
            );
        }
        // Text and CSV read anything.
        assert!(open(DocKind::Text, junk.clone()).is_ok());
        assert!(open(DocKind::Csv, junk).is_ok());
    }
}
