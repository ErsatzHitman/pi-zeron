//! Bounds on what a preview reads and keeps. A preview is a glance, not an
//! editor: past these the viewer says what was left out.

/// Largest file opened at all (the attachment limit).
pub const MAX_INPUT_BYTES: usize = 24 * 1024 * 1024;
/// Rows and columns kept per sheet.
pub const MAX_ROWS: usize = 5_000;
pub const MAX_COLS: usize = 100;
/// Characters kept per cell.
pub const MAX_CELL_CHARS: usize = 1_000;
/// Sheets kept per workbook.
pub const MAX_SHEETS: usize = 32;
/// Lines kept from a text file, and characters per line.
pub const MAX_LINES: usize = 20_000;
pub const MAX_LINE_CHARS: usize = 2_000;
/// Word blocks and PowerPoint slides kept.
pub const MAX_BLOCKS: usize = 5_000;
pub const MAX_SLIDES: usize = 300;
/// Uncompressed bytes read from one zip part, and from one file overall:
/// counted while inflating, so a part that lies about its size is still
/// stopped (zip bombs).
pub const MAX_PART_BYTES: u64 = 48 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 160 * 1024 * 1024;
/// Zip entries in one OOXML file.
pub const MAX_ENTRIES: usize = 10_000;
/// XML nodes in one part.
pub const MAX_XML_NODES: u32 = 4_000_000;
/// A rendered PDF page: longest side and total pixels.
pub const MAX_PAGE_SIDE: u32 = 4_096;
pub const MAX_PAGE_PIXELS: u64 = 12_000_000;
