//! Word, Excel and PowerPoint (Office Open XML): a zip of XML parts. Only
//! the text and structure are read; styling, images and formulas are not.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use roxmltree::{Document as Xml, Node, ParsingOptions};

use crate::limits::{
    MAX_BLOCKS, MAX_CELL_CHARS, MAX_COLS, MAX_ENTRIES, MAX_PART_BYTES, MAX_ROWS, MAX_SHEETS,
    MAX_SLIDES, MAX_TOTAL_BYTES, MAX_XML_NODES,
};
use crate::{Block, PreviewError, Sheet, Slide};

/// A zip reader that counts inflated bytes against the per-part and total
/// budgets, whatever sizes the archive declares.
struct Package {
    zip: zip::ZipArchive<Cursor<Vec<u8>>>,
    read: u64,
}

impl Package {
    fn open(bytes: &[u8]) -> Result<Self, PreviewError> {
        let zip = zip::ZipArchive::new(Cursor::new(bytes.to_vec()))
            .map_err(|_| PreviewError::Unreadable)?;
        if zip.len() > MAX_ENTRIES {
            return Err(PreviewError::TooLarge);
        }
        Ok(Self { zip, read: 0 })
    }

    /// A part's text, `None` when the archive has no such part.
    fn part(&mut self, name: &str) -> Result<Option<String>, PreviewError> {
        let mut file = match self.zip.by_name(name) {
            Ok(file) => file,
            Err(zip::result::ZipError::FileNotFound) => return Ok(None),
            Err(zip::result::ZipError::UnsupportedArchive(_)) => {
                return Err(PreviewError::Encrypted);
            }
            Err(_) => return Err(PreviewError::Unreadable),
        };
        let budget = MAX_PART_BYTES.min(MAX_TOTAL_BYTES.saturating_sub(self.read));
        let mut bytes = Vec::new();
        (&mut file)
            .take(budget + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| PreviewError::Unreadable)?;
        if bytes.len() as u64 > budget {
            return Err(PreviewError::TooLarge);
        }
        self.read += bytes.len() as u64;
        Ok(Some(crate::text::decode(&bytes)))
    }

    fn required(&mut self, name: &str) -> Result<String, PreviewError> {
        self.part(name)?.ok_or(PreviewError::Unreadable)
    }
}

fn parse(text: &str) -> Result<Xml<'_>, PreviewError> {
    Xml::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
            ..ParsingOptions::default()
        },
    )
    .map_err(|err| match err {
        roxmltree::Error::NodesLimitReached => PreviewError::TooLarge,
        _ => PreviewError::Unreadable,
    })
}

const R_NS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// A part's relationships: id → absolute part name. External targets
/// (links to other files or URLs) are dropped; they are never followed.
fn relationships(pkg: &mut Package, part: &str) -> Result<HashMap<String, String>, PreviewError> {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let rels_name = if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    let Some(text) = pkg.part(&rels_name)? else {
        return Ok(HashMap::new());
    };
    let xml = parse(&text)?;
    Ok(xml
        .descendants()
        .filter(|node| node.has_tag_name("Relationship"))
        .filter(|node| node.attribute("TargetMode") != Some("External"))
        .filter_map(|node| {
            let target = node.attribute("Target")?;
            Some((node.attribute("Id")?.to_string(), resolve(dir, target)))
        })
        .collect())
}

/// Resolve a relationship target against the source part's directory.
fn resolve(dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        dir.split('/').filter(|p| !p.is_empty()).collect()
    };
    for piece in target.split('/') {
        match piece {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            piece => parts.push(piece),
        }
    }
    parts.join("/")
}

fn clip(mut text: String) -> String {
    if let Some((end, _)) = text.char_indices().nth(MAX_CELL_CHARS) {
        text.truncate(end);
        text.push('…');
    }
    text
}

/// The text of every `t` element under `node` (Word `w:t`, DrawingML
/// `a:t`, spreadsheet `t`), with tabs and breaks.
fn run_text(node: Node) -> String {
    let mut out = String::new();
    for child in node.descendants() {
        match child.tag_name().name() {
            "t" if child.is_element() => out.extend(child.text()),
            "tab" => out.push('\t'),
            "br" | "cr" => out.push('\n'),
            _ => {}
        }
    }
    out
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn attr<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

// ---- Word ----

pub(crate) fn read_docx(bytes: &[u8]) -> Result<Vec<Block>, PreviewError> {
    let mut pkg = Package::open(bytes)?;
    let text = pkg.required("word/document.xml")?;
    let xml = parse(&text)?;
    let body = xml
        .descendants()
        .find(|node| {
            node.has_tag_name((
                "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
                "body",
            ))
        })
        .ok_or(PreviewError::Unreadable)?;
    let mut blocks = Vec::new();
    for node in body.children().filter(Node::is_element) {
        if blocks.len() == MAX_BLOCKS {
            break;
        }
        match node.tag_name().name() {
            "p" => {
                let block = docx_paragraph(node);
                // One blank paragraph is spacing; a run of them is not.
                let blank = block == Block::Paragraph(String::new());
                if !blank || blocks.last().is_some_and(|last| *last != block) {
                    blocks.push(block);
                }
            }
            "tbl" => {
                let rows: Vec<Vec<String>> = node
                    .children()
                    .filter(|c| c.tag_name().name() == "tr")
                    .take(MAX_ROWS)
                    .map(|row| {
                        row.children()
                            .filter(|c| c.tag_name().name() == "tc")
                            .take(MAX_COLS)
                            .map(|cell| {
                                let paragraphs: Vec<String> = cell
                                    .children()
                                    .filter(|c| c.tag_name().name() == "p")
                                    .map(run_text)
                                    .collect();
                                clip(paragraphs.join("\n"))
                            })
                            .collect()
                    })
                    .collect();
                if !rows.is_empty() {
                    blocks.push(Block::Table(rows));
                }
            }
            _ => {}
        }
    }
    Ok(blocks)
}

fn docx_paragraph(node: Node) -> Block {
    let text = run_text(node);
    let props = child(node, "pPr");
    let style = props
        .and_then(|p| child(p, "pStyle"))
        .and_then(|s| attr(s, "val"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let list_level = props.and_then(|p| child(p, "numPr")).map(|num| {
        child(num, "ilvl")
            .and_then(|l| attr(l, "val"))
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(0)
    });
    if text.trim().is_empty() {
        return Block::Paragraph(String::new());
    }
    let heading = if style == "title" {
        Some(1)
    } else if style == "subtitle" {
        Some(2)
    } else {
        style
            .strip_prefix("heading")
            .and_then(|n| n.trim().parse::<u8>().ok())
            .map(|n| n.clamp(1, 6))
    };
    match (heading, list_level) {
        (Some(level), _) => Block::Heading { level, text },
        (None, Some(level)) => Block::Bullet { level, text },
        (None, None) if style.contains("list") => Block::Bullet { level: 0, text },
        (None, None) => Block::Paragraph(text),
    }
}

// ---- Excel ----

pub(crate) fn read_xlsx(bytes: &[u8]) -> Result<Vec<Sheet>, PreviewError> {
    let mut pkg = Package::open(bytes)?;
    let workbook_text = pkg.required("xl/workbook.xml")?;
    let workbook = parse(&workbook_text)?;
    let rels = relationships(&mut pkg, "xl/workbook.xml")?;
    let shared = match pkg.part("xl/sharedStrings.xml")? {
        Some(text) => {
            let xml = parse(&text)?;
            xml.root_element()
                .children()
                .filter(|c| c.tag_name().name() == "si")
                .map(|si| clip(run_text(si)))
                .collect()
        }
        None => Vec::new(),
    };
    let mut sheets = Vec::new();
    for node in workbook
        .descendants()
        .filter(|n| n.tag_name().name() == "sheet")
        .take(MAX_SHEETS)
    {
        let name = attr(node, "name").unwrap_or("Sheet").to_string();
        let Some(part) = node.attribute((R_NS, "id")).and_then(|id| rels.get(id)) else {
            continue;
        };
        let Some(text) = pkg.part(part)? else {
            continue;
        };
        let (rows, truncated) = xlsx_rows(&parse(&text)?, &shared);
        sheets.push(Sheet {
            name,
            rows,
            truncated,
        });
    }
    if sheets.is_empty() {
        return Err(PreviewError::Unreadable);
    }
    Ok(sheets)
}

/// Column index of a cell reference (`"C7"` → 2).
fn column_of(reference: &str) -> Option<usize> {
    let letters = reference.bytes().take_while(u8::is_ascii_alphabetic);
    let mut col = 0usize;
    let mut any = false;
    for b in letters {
        any = true;
        col = col.checked_mul(26)? + (b.to_ascii_uppercase() - b'A') as usize + 1;
    }
    any.then(|| col - 1)
}

/// A worksheet's cells as a dense grid. Rows and columns are placed by
/// their references but kept only inside the limits, so a sheet with one
/// cell at A1 and one at XFD1048576 stays small.
fn xlsx_rows(sheet: &Xml, shared: &[String]) -> (Vec<Vec<String>>, bool) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;
    let Some(data) = sheet
        .descendants()
        .find(|n| n.tag_name().name() == "sheetData")
    else {
        return (rows, false);
    };
    let mut next_row = 0usize;
    for row in data.children().filter(|c| c.tag_name().name() == "row") {
        let index = attr(row, "r")
            .and_then(|r| r.parse::<usize>().ok())
            .map_or(next_row, |r| r.saturating_sub(1).max(next_row));
        if index >= MAX_ROWS {
            truncated = true;
            break;
        }
        rows.resize_with(index, Vec::new);
        let mut cells: Vec<String> = Vec::new();
        for cell in row.children().filter(|c| c.tag_name().name() == "c") {
            let col = attr(cell, "r").and_then(column_of).unwrap_or(cells.len());
            if col >= MAX_COLS {
                truncated = true;
                continue;
            }
            let value = child(cell, "v").and_then(|v| v.text()).unwrap_or_default();
            let text = match attr(cell, "t") {
                Some("s") => value
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| shared.get(i))
                    .cloned()
                    .unwrap_or_default(),
                Some("inlineStr") => child(cell, "is").map(run_text).unwrap_or_default(),
                Some("b") => (if value.trim() == "1" { "TRUE" } else { "FALSE" }).to_string(),
                _ => value.to_string(),
            };
            if cells.len() <= col {
                cells.resize_with(col + 1, String::new);
            }
            cells[col] = clip(text);
        }
        while cells.last().is_some_and(String::is_empty) {
            cells.pop();
        }
        rows.push(cells);
        next_row = index + 1;
    }
    while rows.last().is_some_and(Vec::is_empty) {
        rows.pop();
    }
    (rows, truncated)
}

// ---- PowerPoint ----

pub(crate) fn read_pptx(bytes: &[u8]) -> Result<Vec<Slide>, PreviewError> {
    let mut pkg = Package::open(bytes)?;
    let presentation_text = pkg.required("ppt/presentation.xml")?;
    let presentation = parse(&presentation_text)?;
    let rels = relationships(&mut pkg, "ppt/presentation.xml")?;
    let parts: Vec<String> = presentation
        .descendants()
        .filter(|n| n.tag_name().name() == "sldId")
        .filter_map(|n| rels.get(n.attribute((R_NS, "id"))?).cloned())
        .take(MAX_SLIDES)
        .collect();
    let mut slides = Vec::new();
    for part in parts {
        let Some(text) = pkg.part(&part)? else {
            continue;
        };
        slides.push(pptx_slide(&parse(&text)?));
    }
    if slides.is_empty() {
        return Err(PreviewError::Unreadable);
    }
    Ok(slides)
}

fn pptx_slide(xml: &Xml) -> Slide {
    let mut slide = Slide::default();
    for shape in xml.descendants().filter(|n| n.tag_name().name() == "sp") {
        let is_title = shape
            .descendants()
            .find(|n| n.tag_name().name() == "ph")
            .and_then(|ph| attr(ph, "type"))
            .is_some_and(|kind| kind == "title" || kind == "ctrTitle");
        let Some(body) = child(shape, "txBody") else {
            continue;
        };
        let paragraphs: Vec<String> = body
            .children()
            .filter(|c| c.tag_name().name() == "p")
            .map(run_text)
            .filter(|text| !text.trim().is_empty())
            .collect();
        if is_title && slide.title.is_none() && !paragraphs.is_empty() {
            slide.title = Some(paragraphs.join(" "));
        } else {
            slide.lines.extend(paragraphs);
        }
    }
    slide
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in parts {
            out.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            out.write_all(body.as_bytes()).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    pub(crate) fn docx() -> Vec<u8> {
        let document = format!(
            r#"<w:document {W}><w:body>
<w:p><w:pPr><w:pStyle w:val="Title"/></w:pPr><w:r><w:t>Quarterly</w:t></w:r><w:r><w:t xml:space="preserve"> report</w:t></w:r></w:p>
<w:p><w:r><w:t>Revenue grew.</w:t></w:r></w:p>
<w:p><w:pPr><w:pStyle w:val="Heading2"/></w:pPr><w:r><w:t>Risks</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>Churn</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body></w:document>"#
        );
        zip_of(&[("word/document.xml", &document)])
    }

    #[test]
    fn word_paragraphs_headings_lists_and_tables() {
        assert_eq!(
            read_docx(&docx()).unwrap(),
            vec![
                Block::Heading {
                    level: 1,
                    text: "Quarterly report".into()
                },
                Block::Paragraph("Revenue grew.".into()),
                Block::Heading {
                    level: 2,
                    text: "Risks".into()
                },
                Block::Bullet {
                    level: 1,
                    text: "Churn".into()
                },
                Block::Table(vec![vec!["a".into(), "b".into()]]),
            ]
        );
    }

    pub(crate) fn xlsx() -> Vec<u8> {
        zip_of(&[
            (
                "xl/workbook.xml",
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/><sheet name="Gone" sheetId="2" r:id="rId9"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId9" Type="worksheet" Target="https://example.com/x.xml" TargetMode="External"/></Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>Item</t></si><si><r><t>Co</t></r><r><t>st</t></r></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="s"><v>1</v></c></row>
<row r="3"><c r="A3" t="inlineStr"><is><t>Rent</t></is></c><c r="B3" t="b"><v>1</v></c><c r="C3"><v>1200.5</v></c></row>
<row r="1048576"><c r="XFD1048576"><v>9</v></c></row>
</sheetData></worksheet>"#,
            ),
        ])
    }

    #[test]
    fn excel_cells_land_by_reference_inside_the_limits() {
        let sheets = read_xlsx(&xlsx()).unwrap();
        assert_eq!(sheets.len(), 1, "the external sheet is never read");
        assert_eq!(sheets[0].name, "Budget");
        assert_eq!(
            sheets[0].rows,
            vec![
                vec!["Item".to_string(), "".into(), "Cost".into()],
                vec![],
                vec!["Rent".into(), "TRUE".into(), "1200.5".into()],
            ]
        );
        assert!(
            sheets[0].truncated,
            "the far corner cell is past the limits"
        );
        assert_eq!(column_of("XFD1"), Some(16_383));
        assert_eq!(column_of("A1"), Some(0));
        assert_eq!(column_of("12"), None);
    }

    #[test]
    fn powerpoint_slides_in_presentation_order() {
        let slide = |title: &str, body: &str| {
            format!(
                r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree>
<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp>
<p:sp><p:txBody><a:p><a:r><a:t>{body}</a:t></a:r></a:p><a:p></a:p></p:txBody></p:sp>
</p:spTree></p:cSld></p:sld>"#
            )
        };
        let (one, two) = (slide("Intro", "Hello"), slide("Plan", "Ship it"));
        let pptx = zip_of(&[
            (
                "ppt/presentation.xml",
                r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="257" r:id="rId3"/><p:sldId id="256" r:id="rId2"/></p:sldIdLst></p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Target="slides/slide1.xml"/><Relationship Id="rId3" Target="/ppt/slides/slide2.xml"/></Relationships>"#,
            ),
            ("ppt/slides/slide1.xml", &one),
            ("ppt/slides/slide2.xml", &two),
        ]);
        let slides = read_pptx(&pptx).unwrap();
        assert_eq!(slides.len(), 2);
        assert_eq!(slides[0].title.as_deref(), Some("Plan"));
        assert_eq!(slides[0].lines, vec!["Ship it"]);
        assert_eq!(slides[1].title.as_deref(), Some("Intro"));
    }

    #[test]
    fn an_oversized_part_is_refused_while_inflating() {
        // Highly compressible: tiny on disk, past the part budget inflated.
        let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
        out.start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        let chunk = vec![b' '; 1 << 20];
        for _ in 0..(MAX_PART_BYTES >> 20) + 1 {
            out.write_all(&chunk).unwrap();
        }
        let bomb = out.finish().unwrap().into_inner();
        assert!(bomb.len() < 1 << 20);
        assert_eq!(read_docx(&bomb), Err(PreviewError::TooLarge));
    }

    #[test]
    fn relationship_targets_resolve_against_their_part() {
        assert_eq!(
            resolve("xl", "worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(resolve("ppt", "/ppt/slides/s.xml"), "ppt/slides/s.xml");
        assert_eq!(resolve("ppt/slides", "../media/a.png"), "ppt/media/a.png");
    }
}
