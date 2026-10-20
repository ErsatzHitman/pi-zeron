//! The in-chat document viewer the attachment lightbox shows for a PDF,
//! spreadsheet, Word, PowerPoint, CSV or text attachment.
//!
//! - PDF: one page at a time, rendered off-thread and shown in the image
//!   viewer (zoom and pan as for images); arrows and buttons page.
//! - Sheets and CSV: a virtualized grid with column letters and row numbers,
//!   and one tab per sheet.
//! - Word and PowerPoint: the text and structure as a reading view.
//! - Text: monospace lines.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Context, Image, ImageFormat, ListAlignment, ListState, SharedString, Task,
    Window, div, list, prelude::*, px, uniform_list,
};
use zeron_docview::{Block, DocKind, Document, Sheet};

use crate::doc_preview::LoadedDoc;
use crate::theme::Theme;

/// Width a viewer page renders at.
const PAGE_RENDER_WIDTH: u32 = 1_800;
const CELL_W: f32 = 128.0;
const ROW_NUMBER_W: f32 = 48.0;
const GRID_ROW_H: f32 = 24.0;
const TEXT_ROW_H: f32 = 19.0;
/// Approximate monospace advance at the viewer's 12.5px code size.
const MONO_CHAR_W: f32 = 7.6;

enum PageState {
    Ready {
        image: Arc<Image>,
        width: u32,
        height: u32,
    },
    Failed,
}

pub type CloseFn = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct DocViewer {
    doc: Arc<LoadedDoc>,
    /// The PDF page or the sheet shown.
    index: usize,
    pages: HashMap<usize, PageState>,
    page_tasks: HashMap<usize, Task<()>>,
    image: crate::image_viewer::ImageView,
    /// Word blocks or slides (variable height).
    list: ListState,
    rows: gpui::UniformListScrollHandle,
    /// A text file's longest line in characters, for its scroll width.
    longest_line: usize,
    /// Set by the lightbox each frame: the header's close button.
    pub(crate) on_close: Option<CloseFn>,
}

impl DocViewer {
    pub fn new(doc: Arc<LoadedDoc>, cx: &mut Context<Self>) -> Self {
        let items = match doc.doc.as_ref() {
            Document::Blocks(blocks) => blocks.len(),
            Document::Slides(slides) => slides.len(),
            _ => 0,
        };
        let longest_line = match doc.doc.as_ref() {
            Document::Text { lines, .. } => {
                lines.iter().map(|l| l.chars().count()).max().unwrap_or(0)
            }
            _ => 0,
        };
        let mut viewer = Self {
            longest_line,
            doc,
            index: 0,
            pages: HashMap::new(),
            page_tasks: HashMap::new(),
            image: Default::default(),
            list: ListState::new(items, ListAlignment::Top, px(800.0)),
            rows: gpui::UniformListScrollHandle::new(),
            on_close: None,
        };
        viewer.request_pages(cx);
        viewer
    }

    pub fn name(&self) -> &SharedString {
        &self.doc.name
    }

    /// PDF pages or sheets, whichever this document steps through.
    fn steps(&self) -> usize {
        match self.doc.doc.as_ref() {
            Document::Pdf(pdf) => pdf.pages.len(),
            Document::Sheets(sheets) => sheets.len(),
            _ => 1,
        }
    }

    /// Move `delta` pages (or sheets), clamped.
    pub fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let last = self.steps().saturating_sub(1);
        let next = self.index.saturating_add_signed(delta).min(last);
        self.go_to(next, cx);
    }

    pub fn go_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == self.index || index >= self.steps() {
            return;
        }
        self.index = index;
        self.image.reset();
        self.rows = gpui::UniformListScrollHandle::new();
        self.request_pages(cx);
        cx.notify();
    }

    /// Render the shown PDF page and the next one, if not already.
    fn request_pages(&mut self, cx: &mut Context<Self>) {
        let Document::Pdf(pdf) = self.doc.doc.as_ref() else {
            return;
        };
        // Pages far from the one shown are dropped, so a long PDF holds a
        // few rasters, not all of them.
        let keep = self.index.saturating_sub(2)..=self.index + 2;
        self.pages.retain(|page, _| keep.contains(page));
        self.page_tasks.retain(|page, _| keep.contains(page));
        for page in [self.index, self.index + 1] {
            if page >= pdf.pages.len()
                || self.pages.contains_key(&page)
                || self.page_tasks.contains_key(&page)
            {
                continue;
            }
            let pdf = pdf.clone();
            let task = cx.spawn(async move |this, cx| {
                let rendered = cx
                    .background_executor()
                    .spawn(async move { pdf.render_png(page, PAGE_RENDER_WIDTH) })
                    .await
                    .ok();
                this.update(cx, |this, cx| {
                    this.page_tasks.remove(&page);
                    let state = rendered
                        .and_then(|png| {
                            let (width, height) = crate::appshots::png_dimensions(&png)?;
                            Some(PageState::Ready {
                                image: Arc::new(Image::from_bytes(ImageFormat::Png, png)),
                                width,
                                height,
                            })
                        })
                        .unwrap_or(PageState::Failed);
                    this.pages.insert(page, state);
                    cx.notify();
                })
                .ok();
            });
            self.page_tasks.insert(page, task);
        }
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let steps = self.steps();
        let paged = matches!(self.doc.doc.as_ref(), Document::Pdf(_)) && steps > 1;
        let nav_button = |id: &'static str, icon: &'static str, enabled: bool, delta: isize| {
            div()
                .id(id)
                .size(px(26.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .when(enabled, |el| {
                    el.cursor_pointer()
                        .hover(|style| style.bg(theme.element_hover))
                        .on_click(cx.listener(move |this, _, _, cx| this.step(delta, cx)))
                })
                .opacity(if enabled { 1.0 } else { 0.35 })
                .child(
                    crate::icons::icon(icon)
                        .size(px(15.0))
                        .text_color(theme.text_muted),
                )
        };
        let close = self.on_close.clone();
        div()
            .flex_none()
            .h(px(44.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .child(
                crate::file_icons::icon(
                    crate::file_icons::FileIconIdentity::file(&self.doc.name),
                    theme.appearance,
                )
                .size(px(18.0)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text)
                    .child(self.doc.name.clone()),
            )
            .when(paged, |el| {
                el.child(nav_button(
                    "doc-viewer-prev",
                    crate::icons::ALT_ARROW_LEFT,
                    self.index > 0,
                    -1,
                ))
                .child(
                    div()
                        .id("doc-viewer-page")
                        .debug_selector(|| "doc-viewer-page".into())
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(format!("{} / {steps}", self.index + 1))),
                )
                .child(nav_button(
                    "doc-viewer-next",
                    crate::icons::ALT_ARROW_RIGHT,
                    self.index + 1 < steps,
                    1,
                ))
            })
            .children(close.map(|close| {
                div()
                    .id("doc-viewer-close")
                    .size(px(26.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.element_hover))
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close(window, cx);
                    })
                    .child(
                        crate::icons::icon(crate::icons::CLOSE)
                            .size(px(15.0))
                            .text_color(theme.text_muted),
                    )
            }))
            .into_any_element()
    }

    fn note(text: impl Into<SharedString>, theme: &Theme) -> AnyElement {
        div()
            .flex_none()
            .px(px(14.0))
            .py(px(6.0))
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_muted)
            .child(text.into())
            .into_any_element()
    }

    fn render_pdf(&self, window: &mut Window, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        match self.pages.get(&self.index) {
            Some(PageState::Ready {
                image,
                width,
                height,
            }) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(self.image.render(
                    image.clone(),
                    gpui::size(px(*width as f32 / 2.0), px(*height as f32 / 2.0)),
                    None,
                    None,
                    window,
                    cx,
                ))
                .into_any_element(),
            Some(PageState::Failed) => centered("This page could not be rendered.", theme),
            None => centered("Rendering page…", theme),
        }
    }

    fn render_sheet(&self, sheet: &Sheet, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let columns = sheet.columns().max(1);
        let width = ROW_NUMBER_W + CELL_W * columns as f32;
        let grid_line = theme.border;
        let header_bg = theme.ink(0.04);
        let text = theme.text;
        let muted = theme.text_muted;
        let header = div()
            .flex_none()
            .h(px(GRID_ROW_H))
            .flex()
            .bg(header_bg)
            .border_b_1()
            .border_color(grid_line)
            .child(
                div()
                    .w(px(ROW_NUMBER_W))
                    .flex_none()
                    .border_r_1()
                    .border_color(grid_line),
            )
            .children((0..columns).map(|col| {
                div()
                    .w(px(CELL_W))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_r_1()
                    .border_color(grid_line)
                    .text_size(px(11.0))
                    .text_color(muted)
                    .child(SharedString::from(column_letters(col)))
            }));
        let doc = self.doc.clone();
        let index = self.index;
        let body = uniform_list(
            "doc-viewer-sheet-rows",
            sheet.rows.len(),
            move |range, _, _| {
                let Document::Sheets(sheets) = doc.doc.as_ref() else {
                    return Vec::new();
                };
                let rows = &sheets[index].rows;
                range
                    .map(|ix| {
                        let cells = &rows[ix];
                        div()
                            .h(px(GRID_ROW_H))
                            .flex()
                            .border_b_1()
                            .border_color(grid_line)
                            .child(
                                div()
                                    .w(px(ROW_NUMBER_W))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(header_bg)
                                    .border_r_1()
                                    .border_color(grid_line)
                                    .text_size(px(11.0))
                                    .text_color(muted)
                                    .child(SharedString::from((ix + 1).to_string())),
                            )
                            .children((0..columns).map(|col| {
                                div()
                                    .w(px(CELL_W))
                                    .flex_none()
                                    .px(px(6.0))
                                    .flex()
                                    .items_center()
                                    .border_r_1()
                                    .border_color(grid_line)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_size(px(12.5))
                                    .text_color(text)
                                    .child(SharedString::from(
                                        cells
                                            .get(col)
                                            .map_or("", |c| c.as_str())
                                            .replace('\n', " "),
                                    ))
                            }))
                            .into_any_element()
                    })
                    .collect()
            },
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&self.rows);
        let sheets = match self.doc.doc.as_ref() {
            Document::Sheets(sheets) => sheets.len(),
            _ => 1,
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("doc-viewer-sheet")
                    .debug_selector(|| "doc-viewer-sheet".into())
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .child(
                        div()
                            .w(px(width))
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(header)
                            .child(body),
                    ),
            )
            .when(sheet.truncated, |el| {
                el.child(Self::note(
                    format!(
                        "Showing the first {} rows and {} columns.",
                        zeron_docview::limits::MAX_ROWS,
                        zeron_docview::limits::MAX_COLS
                    ),
                    theme,
                ))
            })
            .when(sheets > 1, |el| el.child(self.sheet_tabs(theme, cx)))
            .into_any_element()
    }

    fn sheet_tabs(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Document::Sheets(sheets) = self.doc.doc.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        div()
            .id("doc-viewer-sheet-tabs")
            .flex_none()
            .h(px(34.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .overflow_x_scroll()
            .border_t_1()
            .border_color(theme.border)
            .children(sheets.iter().enumerate().map(|(ix, sheet)| {
                let active = ix == self.index;
                div()
                    .id(("doc-viewer-sheet-tab", ix))
                    .flex_none()
                    .px(px(10.0))
                    .h(px(24.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(if active { theme.text } else { theme.text_muted })
                    .when(active, |el| el.bg(theme.ink(0.08)))
                    .hover(|style| style.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.go_to(ix, cx)))
                    .child(SharedString::from(sheet.name.clone()))
            }))
            .into_any_element()
    }

    fn render_text(
        &self,
        lines: &[String],
        truncated: bool,
        theme: &Theme,
        cx: &App,
    ) -> AnyElement {
        let width = (self.longest_line as f32 * MONO_CHAR_W + 96.0).max(400.0);
        let family = crate::typography::code_effective_family_name(cx);
        let text = theme.text;
        let muted = theme.text_faint;
        let gutter = (lines.len().max(1).ilog10() + 1) as f32 * MONO_CHAR_W + 16.0;
        let doc = self.doc.clone();
        let body = uniform_list("doc-viewer-text-lines", lines.len(), move |range, _, _| {
            let Document::Text { lines, .. } = doc.doc.as_ref() else {
                return Vec::new();
            };
            range
                .map(|ix| {
                    div()
                        .h(px(TEXT_ROW_H))
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .child(
                            div()
                                .w(px(gutter))
                                .flex_none()
                                .pr(px(10.0))
                                .flex()
                                .justify_end()
                                .text_color(muted)
                                .child(SharedString::from((ix + 1).to_string())),
                        )
                        .child(
                            div()
                                .text_color(text)
                                .child(SharedString::from(lines[ix].clone())),
                        )
                        .into_any_element()
                })
                .collect()
        })
        .flex_1()
        .min_h_0()
        .py(px(8.0))
        .track_scroll(&self.rows);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("doc-viewer-text")
                    .debug_selector(|| "doc-viewer-text".into())
                    .flex_1()
                    .min_h_0()
                    .overflow_x_scroll()
                    .font_family(family)
                    .text_size(px(12.5))
                    .child(div().w(px(width)).h_full().flex().flex_col().child(body)),
            )
            .when(truncated, |el| {
                el.child(Self::note(
                    format!(
                        "Showing the first {} lines.",
                        zeron_docview::limits::MAX_LINES
                    ),
                    theme,
                ))
            })
            .into_any_element()
    }

    /// One Word block or slide, for the variable-height list.
    fn render_item(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).for_popup();
        let column = |child: AnyElement| {
            div()
                .w_full()
                .flex()
                .justify_center()
                .px(px(24.0))
                .child(div().w_full().max_w(px(760.0)).child(child))
                .into_any_element()
        };
        match self.doc.doc.as_ref() {
            Document::Blocks(blocks) => {
                let Some(block) = blocks.get(ix) else {
                    return gpui::Empty.into_any_element();
                };
                column(render_block(block, &theme))
            }
            Document::Slides(slides) => {
                let Some(slide) = slides.get(ix) else {
                    return gpui::Empty.into_any_element();
                };
                column(
                    div()
                        .my(px(10.0))
                        .p(px(22.0))
                        .min_h(px(240.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_card)
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(format!("Slide {}", ix + 1))),
                        )
                        .children(slide.title.clone().map(|title| {
                            div()
                                .text_size(px(20.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(SharedString::from(title))
                        }))
                        .children(slide.lines.iter().map(|line| {
                            div()
                                .text_size(px(14.0))
                                .text_color(theme.text)
                                .child(SharedString::from(format!("• {line}")))
                        }))
                        .into_any_element(),
                )
            }
            _ => gpui::Empty.into_any_element(),
        }
    }
}

fn centered(text: &'static str, theme: &Theme) -> AnyElement {
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .text_size(crate::typography::ui_rems(12.5))
        .text_color(theme.text_muted)
        .child(text)
        .into_any_element()
}

fn render_block(block: &Block, theme: &Theme) -> AnyElement {
    match block {
        Block::Heading { level, text } => div()
            .pt(px(if *level <= 2 { 18.0 } else { 12.0 }))
            .pb(px(4.0))
            .text_size(px(match level {
                1 => 24.0,
                2 => 19.0,
                3 => 16.5,
                _ => 15.0,
            }))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Block::Paragraph(text) if text.is_empty() => div().h(px(10.0)).into_any_element(),
        Block::Paragraph(text) => div()
            .py(px(4.0))
            .text_size(px(14.0))
            .line_height(px(21.0))
            .text_color(theme.text)
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Block::Bullet { level, text } => div()
            .py(px(2.0))
            .pl(px(18.0 * (*level as f32 + 1.0)))
            .flex()
            .gap(px(8.0))
            .text_size(px(14.0))
            .line_height(px(21.0))
            .text_color(theme.text)
            .child(div().flex_none().child("•"))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .child(SharedString::from(text.clone())),
            )
            .into_any_element(),
        Block::Table(rows) => div()
            .my(px(8.0))
            .border_1()
            .border_color(theme.border)
            .rounded(px(6.0))
            .overflow_hidden()
            .flex()
            .flex_col()
            .children(rows.iter().enumerate().map(|(ix, row)| {
                div()
                    .flex()
                    .when(ix > 0, |el| el.border_t_1().border_color(theme.border))
                    .when(ix == 0, |el| el.bg(theme.ink(0.04)))
                    .children(row.iter().map(|cell| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .px(px(8.0))
                            .py(px(5.0))
                            .text_size(px(13.0))
                            .text_color(theme.text)
                            .child(SharedString::from(cell.clone()))
                    }))
            }))
            .into_any_element(),
    }
}

/// Spreadsheet column letters: 0 → A, 25 → Z, 26 → AA.
fn column_letters(mut col: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

impl Render for DocViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_popup();
        let body = match self.doc.doc.clone().as_ref() {
            Document::Pdf(_) => self.render_pdf(window, &theme, cx),
            Document::Sheets(sheets) => match sheets.get(self.index) {
                Some(sheet) => self.render_sheet(sheet, &theme, cx),
                None => centered("This workbook has no sheets.", &theme),
            },
            Document::Text { lines, truncated } => self.render_text(lines, *truncated, &theme, cx),
            Document::Blocks(_) | Document::Slides(_) => {
                let simplified = !matches!(self.doc.kind, DocKind::Text | DocKind::Csv);
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .when(simplified, |el| {
                        el.child(Self::note(
                            "Simplified preview: text and structure only.",
                            &theme,
                        ))
                    })
                    .child(
                        list(self.list.clone(), cx.processor(Self::render_item))
                            .flex_1()
                            .min_h_0()
                            .pb(px(16.0)),
                    )
                    .into_any_element()
            }
        };
        div()
            .id("doc-viewer")
            .debug_selector(|| "doc-viewer".into())
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(12.0))
            .overflow_hidden()
            .bg(theme.surface_dialog)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            // Clicks inside the viewer never reach the scrim, which closes.
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(self.header(&theme, cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_letters_follow_spreadsheets() {
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_letters(27), "AB");
        assert_eq!(column_letters(701), "ZZ");
        assert_eq!(column_letters(702), "AAA");
    }
}

#[cfg(test)]
mod viewer_tests {
    use super::*;
    use gpui::TestAppContext;

    fn init(cx: &mut TestAppContext) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::dark());
            crate::settings::init(crate::settings::UiSettings::default(), dir.path(), cx);
        });
        dir
    }

    #[gpui::test]
    fn a_workbook_shows_a_grid_and_a_tab_per_sheet(cx: &mut TestAppContext) {
        let _dir = init(cx);
        let sheet = |name: &str, rows: usize| Sheet {
            name: name.into(),
            rows: (0..rows)
                .map(|r| vec![format!("r{r}"), "x".into()])
                .collect(),
            truncated: false,
        };
        let doc = Arc::new(LoadedDoc {
            name: "budget.xlsx".into(),
            kind: DocKind::Sheet,
            doc: Arc::new(Document::Sheets(vec![sheet("Q1", 3), sheet("Q2", 9)])),
            thumb: None,
            lines: Vec::new(),
        });
        let (viewer, cx) = cx.add_window_view(|_, cx| DocViewer::new(doc, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("doc-viewer-sheet").is_some(), "the grid");
        viewer.update(cx, |viewer, cx| viewer.step(1, cx));
        viewer.read_with(cx, |viewer, _| assert_eq!(viewer.index, 1));
        // Clamped at the last sheet.
        viewer.update(cx, |viewer, cx| viewer.step(1, cx));
        viewer.read_with(cx, |viewer, _| assert_eq!(viewer.index, 1));
    }

    #[gpui::test]
    fn a_text_file_shows_its_lines(cx: &mut TestAppContext) {
        let _dir = init(cx);
        let doc = crate::doc_preview::test_doc("main.rs", DocKind::Text, b"fn main() {}\n");
        let (_viewer, cx) = cx.add_window_view(|_, cx| DocViewer::new(doc, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("doc-viewer-text").is_some());
        assert!(
            cx.debug_bounds("doc-viewer-page").is_none(),
            "no page controls"
        );
    }

    #[gpui::test]
    fn a_word_document_reads_as_blocks(cx: &mut TestAppContext) {
        let _dir = init(cx);
        let doc = Arc::new(LoadedDoc {
            name: "plan.docx".into(),
            kind: DocKind::Word,
            doc: Arc::new(Document::Blocks(vec![
                Block::Heading {
                    level: 1,
                    text: "Plan".into(),
                },
                Block::Paragraph("Ship it.".into()),
            ])),
            thumb: None,
            lines: Vec::new(),
        });
        let (viewer, cx) = cx.add_window_view(|_, cx| DocViewer::new(doc, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("doc-viewer").is_some());
        viewer.read_with(cx, |viewer, _| assert_eq!(viewer.list.item_count(), 2));
    }
}
