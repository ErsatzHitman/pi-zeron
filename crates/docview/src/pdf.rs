//! PDF pages rasterized with hayro (pure Rust, no native PDF library).

use std::sync::Arc;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::{LoadPdfError, Pdf};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};

use crate::PreviewError;
use crate::limits::{MAX_PAGE_PIXELS, MAX_PAGE_SIDE};

/// An opened PDF: its bytes and page sizes. Pages render on demand, each
/// from a fresh parse (cheap next to the render), so this stays `Send`.
#[derive(Clone)]
pub struct PdfDoc {
    bytes: Arc<Vec<u8>>,
    /// Each page's displayed size in PDF points, rotation applied.
    pub pages: Vec<(f32, f32)>,
}

impl std::fmt::Debug for PdfDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfDoc")
            .field("bytes", &self.bytes.len())
            .field("pages", &self.pages.len())
            .finish()
    }
}

fn load(bytes: &Arc<Vec<u8>>) -> Result<Pdf, PreviewError> {
    Pdf::new(bytes.clone()).map_err(|err| match err {
        LoadPdfError::Decryption(_) => PreviewError::Encrypted,
        _ => PreviewError::Unreadable,
    })
}

impl PdfDoc {
    pub(crate) fn open(bytes: Arc<Vec<u8>>) -> Result<Self, PreviewError> {
        let pdf = load(&bytes)?;
        let pages: Vec<(f32, f32)> = pdf
            .pages()
            .iter()
            .map(|page| page.render_dimensions())
            .collect();
        if pages.is_empty() {
            return Err(PreviewError::Unreadable);
        }
        Ok(Self { bytes, pages })
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Render page `index` (0-based) at most `max_width` pixels wide, on
    /// white, as PNG bytes. The size is also capped by
    /// [`MAX_PAGE_SIDE`] and [`MAX_PAGE_PIXELS`].
    pub fn render_png(&self, index: usize, max_width: u32) -> Result<Vec<u8>, PreviewError> {
        let &(width, height) = self.pages.get(index).ok_or(PreviewError::Unreadable)?;
        let scale = page_scale(width, height, max_width);
        let bytes = self.bytes.clone();
        std::panic::catch_unwind(move || {
            let pdf = load(&bytes)?;
            let page = pdf.pages().get(index).ok_or(PreviewError::Unreadable)?;
            let pixmap = hayro::render(
                page,
                &RenderCache::new(),
                &InterpreterSettings::default(),
                &RenderSettings::default(),
                &PixmapSettings {
                    x_scale: scale,
                    y_scale: scale,
                    bg_color: WHITE,
                },
            );
            if pixmap.width() == 0 || pixmap.height() == 0 {
                return Err(PreviewError::Unreadable);
            }
            pixmap.into_png().map_err(|_| PreviewError::Unreadable)
        })
        .unwrap_or(Err(PreviewError::Unreadable))
    }
}

/// The scale that fits a `width` × `height` point page in `max_width`
/// pixels and the page caps. hayro casts the pixel size to `u16`, so the
/// caps also keep it from wrapping.
fn page_scale(width: f32, height: f32, max_width: u32) -> f32 {
    let (width, height) = (width.max(1.0), height.max(1.0));
    let side = MAX_PAGE_SIDE as f32;
    let by_width = max_width.clamp(1, MAX_PAGE_SIDE) as f32 / width;
    let by_side = side / width.max(height);
    let by_pixels = (MAX_PAGE_PIXELS as f32 / (width * height)).sqrt();
    by_width.min(by_side).min(by_pixels)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page PDF (200 × 100 pt) with a black rectangle, built with a
    /// correct xref so hayro reads it without its recovery path.
    pub(crate) fn tiny_pdf() -> Vec<u8> {
        let content = b"0 0 0 rg 20 20 80 40 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_string(),
            format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                content.len(),
                String::from_utf8_lossy(content)
            ),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn a_page_renders_to_png_within_the_width() {
        let doc = PdfDoc::open(Arc::new(tiny_pdf())).unwrap();
        assert_eq!(doc.pages, vec![(200.0, 100.0)]);
        let png = doc.render_png(0, 400).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        // IHDR width/height.
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!((width, height), (400, 200));
        assert!(doc.render_png(1, 400).is_err());
    }

    #[test]
    fn huge_pages_are_capped() {
        // A 14400 pt square page (the PDF maximum) at a large requested width.
        let scale = page_scale(14_400.0, 14_400.0, 100_000);
        let side = 14_400.0 * scale;
        assert!(side <= MAX_PAGE_SIDE as f32);
        assert!((side * side) as u64 <= MAX_PAGE_PIXELS);
    }
}
