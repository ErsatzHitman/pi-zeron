//! Document attachments (PDF, Excel, Word, PowerPoint, CSV, text): their
//! parsed form and thumbnail, cached like transcript images are, and the
//! thumbnail drawn in the composer strip and above a sent message. Clicking a
//! thumbnail opens [`crate::doc_viewer::DocViewer`] in the attachment
//! lightbox.
//!
//! Parsing runs on the background executor, never the UI thread, bounded by
//! `zeron_docview::limits`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{AnyElement, Image, ImageFormat, ObjectFit, SharedString, div, img, prelude::*, px};
use zeron_docview::{DocKind, Document, PreviewError};

use crate::theme::Theme;

/// Width a PDF thumbnail renders at: 3× the transcript frame, so it stays
/// sharp on dense displays.
const THUMB_RENDER_WIDTH: u32 = 336;
/// Parsed documents kept in memory across chats (their models plus PDF bytes).
const DOC_CACHE_BUDGET_BYTES: usize = 96 * 1024 * 1024;

/// A document ready to show: its thumbnail and the parsed whole.
pub struct LoadedDoc {
    pub name: SharedString,
    pub kind: DocKind,
    pub doc: Arc<Document>,
    /// PDF page 1, cropped from the top to the thumbnail frame's shape.
    pub thumb: Option<Arc<Image>>,
    /// The first lines of anything else, drawn as a card.
    pub lines: Vec<SharedString>,
}

/// What a render pass sees for one source.
#[derive(Clone)]
pub enum DocSnapshot {
    Loading,
    Ready(Arc<LoadedDoc>),
    /// `retry_in` is `None` when the file itself can't be previewed (a
    /// retry won't help); otherwise a transport failure retries after it.
    Failed {
        retry_in: Option<Duration>,
    },
}

enum Entry {
    Loading {
        attempts: u32,
    },
    Ready {
        doc: Arc<LoadedDoc>,
        bytes: usize,
        last_used: u64,
    },
    Failed {
        attempts: u32,
        at: Instant,
        permanent: bool,
    },
}

#[derive(Default)]
struct Cache {
    map: HashMap<(String, String), Entry>,
    tick: u64,
    bytes: usize,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_millis((2_000u64 << attempts.min(3)).min(15_000))
}

fn key(device: &str, path: &str) -> (String, String) {
    (device.to_string(), path.to_string())
}

/// The composer's key for a staged attachment (no device or path yet).
pub fn staged_key(attachment_id: &str) -> (&'static str, String) {
    ("staged", attachment_id.to_string())
}

impl Cache {
    fn insert_ready(&mut self, key: (String, String), doc: Arc<LoadedDoc>) {
        let bytes =
            doc.doc.retained_bytes() + doc.thumb.as_ref().map_or(0, |thumb| thumb.bytes.len() * 4);
        self.tick += 1;
        if let Some(Entry::Ready { bytes: old, .. }) = self.map.insert(
            key.clone(),
            Entry::Ready {
                doc,
                bytes,
                last_used: self.tick,
            },
        ) {
            self.bytes = self.bytes.saturating_sub(old);
        }
        self.bytes += bytes;
        while self.bytes > DOC_CACHE_BUDGET_BYTES {
            let oldest = self
                .map
                .iter()
                .filter(|(k, _)| **k != key && !crate::attachments::is_protected(&k.0, &k.1))
                .filter_map(|(k, entry)| match entry {
                    Entry::Ready { last_used, .. } => Some((*last_used, k.clone())),
                    _ => None,
                })
                .min_by_key(|(tick, _)| *tick);
            let Some((_, evict)) = oldest else { break };
            if let Some(Entry::Ready { bytes, .. }) = self.map.remove(&evict) {
                self.bytes = self.bytes.saturating_sub(bytes);
            }
        }
    }
}

/// The current state of `(device, path)`. A committed upload's rewritten
/// path resolves from the bytes the send seeded under its upload id, as
/// images do (`attachments::seed_attachment_alias`).
pub fn snapshot(device: &str, path: &str) -> DocSnapshot {
    let mut cache = cache().lock().unwrap();
    cache.tick += 1;
    let tick = cache.tick;
    match cache.map.get_mut(&key(device, path)) {
        Some(Entry::Ready { doc, last_used, .. }) => {
            *last_used = tick;
            DocSnapshot::Ready(doc.clone())
        }
        Some(Entry::Failed {
            attempts,
            at,
            permanent,
        }) => DocSnapshot::Failed {
            retry_in: (!*permanent)
                .then(|| retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed())),
        },
        Some(Entry::Loading { .. }) => DocSnapshot::Loading,
        None => {
            let alias = crate::attachments::upload_alias_id8(path).and_then(|id8| {
                match cache.map.get(&alias_key(device, &id8)) {
                    Some(Entry::Ready { doc, .. }) => Some(doc.clone()),
                    _ => None,
                }
            });
            match alias {
                Some(doc) => {
                    cache.insert_ready(key(device, path), doc.clone());
                    DocSnapshot::Ready(doc)
                }
                None => DocSnapshot::Loading,
            }
        }
    }
}

/// Claim the load for a source: `true` means the caller fetches it now.
/// A failed transport load is handed out again after its backoff; a file
/// that can't be previewed never is.
pub fn begin_load(device: &str, path: &str) -> bool {
    let mut cache = cache().lock().unwrap();
    match cache.map.entry(key(device, path)) {
        std::collections::hash_map::Entry::Vacant(vacant) => {
            vacant.insert(Entry::Loading { attempts: 0 });
            true
        }
        std::collections::hash_map::Entry::Occupied(mut occupied) => match occupied.get() {
            Entry::Failed {
                attempts,
                at,
                permanent: false,
            } if at.elapsed() >= retry_delay(attempts.saturating_sub(1)) => {
                let attempts = *attempts;
                occupied.insert(Entry::Loading { attempts });
                true
            }
            _ => false,
        },
    }
}

pub fn store_ready(device: &str, path: &str, doc: Arc<LoadedDoc>) {
    cache().lock().unwrap().insert_ready(key(device, path), doc);
}

/// Record a failed load. `permanent` for a file that can't be previewed;
/// otherwise the bytes didn't arrive and the load retries.
pub fn store_failed(device: &str, path: &str, permanent: bool) {
    let mut cache = cache().lock().unwrap();
    let attempts = match cache.map.get(&key(device, path)) {
        Some(Entry::Loading { attempts }) => attempts + 1,
        Some(Entry::Failed { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.map.insert(
        key(device, path),
        Entry::Failed {
            attempts,
            at: Instant::now(),
            permanent,
        },
    );
}

/// Held by a load task: dropping it before the load finished (the view
/// went away) releases the claim so a later render can retry.
pub struct LoadGuard {
    pub device: String,
    pub path: String,
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        let mut cache = cache().lock().unwrap();
        let key = key(&self.device, &self.path);
        if let Some(Entry::Loading { attempts }) = cache.map.get(&key) {
            let attempts = attempts.saturating_add(1);
            cache.map.insert(
                key,
                Entry::Failed {
                    attempts,
                    at: Instant::now(),
                    permanent: false,
                },
            );
        }
    }
}

fn alias_key(device: &str, id8: &str) -> (String, String) {
    key(device, &format!("upload-alias://{id8}"))
}

/// Seed a just-sent document so its bubble shows the thumbnail at once:
/// under its path, and under its upload id for the path the host rewrites
/// it to.
pub fn seed_staged(device: &str, path: &str, attachment_id: &str) {
    if let Some(doc) = staged(attachment_id) {
        store_ready(device, path, doc);
    }
}

pub fn seed_staged_alias(device: &str, upload_id: &str, attachment_id: &str) {
    if let Some(doc) = staged(attachment_id) {
        let id8: String = upload_id.chars().take(8).collect();
        let (device, path) = alias_key(device, &id8);
        store_ready(&device, &path, doc);
    }
}

/// A staged attachment's parsed document, once the composer has read it.
pub fn staged(attachment_id: &str) -> Option<Arc<LoadedDoc>> {
    let (device, path) = staged_key(attachment_id);
    match snapshot(device, &path) {
        DocSnapshot::Ready(doc) => Some(doc),
        _ => None,
    }
}

/// Parse `bytes` and build the thumbnail on the background executor.
pub async fn build(
    executor: &gpui::BackgroundExecutor,
    name: String,
    kind: DocKind,
    bytes: Arc<Vec<u8>>,
) -> Result<LoadedDoc, PreviewError> {
    executor
        .spawn(async move { build_now(name, kind, bytes) })
        .await
}

fn build_now(name: String, kind: DocKind, bytes: Arc<Vec<u8>>) -> Result<LoadedDoc, PreviewError> {
    let doc = zeron_docview::open(kind, bytes)?;
    let thumb = match &doc {
        Document::Pdf(pdf) => {
            let png = pdf.render_png(0, THUMB_RENDER_WIDTH)?;
            Some(Arc::new(Image::from_bytes(
                ImageFormat::Png,
                crop_to_frame(png),
            )))
        }
        _ => None,
    };
    let lines = doc
        .thumb_lines(8)
        .into_iter()
        .map(SharedString::from)
        .collect();
    Ok(LoadedDoc {
        name: crate::attachments::attachment_display_name(&name)
            .to_string()
            .into(),
        kind,
        doc: Arc::new(doc),
        thumb,
        lines,
    })
}

/// Keep the top of a page in the thumbnail frame's aspect: `img`'s Cover
/// crops around the center, which would show the middle of page 1.
fn crop_to_frame(png: Vec<u8>) -> Vec<u8> {
    let Ok(page) = image::load_from_memory_with_format(&png, image::ImageFormat::Png) else {
        return png;
    };
    let width = page.width();
    let height = page.height().min(
        (width as f32 * crate::transcript::ATT_THUMB_H / crate::transcript::ATT_THUMB_W) as u32,
    );
    let mut out = std::io::Cursor::new(Vec::new());
    match page
        .crop_imm(0, 0, width, height)
        .write_to(&mut out, image::ImageFormat::Png)
    {
        Ok(()) => out.into_inner(),
        Err(_) => png,
    }
}

#[cfg(test)]
pub(crate) fn test_doc(name: &str, kind: DocKind, bytes: &[u8]) -> Arc<LoadedDoc> {
    Arc::new(build_now(name.to_string(), kind, Arc::new(bytes.to_vec())).unwrap())
}

/// The badge color for a kind (the colors these apps' own icons use).
fn badge_color(kind: DocKind) -> gpui::Hsla {
    match kind {
        DocKind::Pdf => gpui::rgb(0xd93a2b).into(),
        DocKind::Word => gpui::rgb(0x2b5fd9).into(),
        DocKind::Sheet | DocKind::Csv => gpui::rgb(0x1f8a4c).into(),
        DocKind::Slides => gpui::rgb(0xd2601a).into(),
        DocKind::Text => gpui::rgb(0x6b7280).into(),
    }
}

/// A document thumbnail's content for a `width` × `height` frame with
/// corner `radius`: the page (PDF) or a paper card with the first lines, a
/// type badge, and the name when `named`. `state` is what the cache holds;
/// loading pulses, failure keeps the frame with the file's name so the row
/// doesn't jump. Every layer rounds its own corners: a frame's rounding only
/// clips rectangularly.
#[allow(clippy::too_many_arguments)]
pub fn thumb_body(
    name: &str,
    kind: DocKind,
    state: &DocSnapshot,
    width: f32,
    height: f32,
    radius: f32,
    named: bool,
    pulse: f32,
    theme: &Theme,
) -> AnyElement {
    let paper = gpui::hsla(0.0, 0.0, 0.985, 1.0);
    let ink = gpui::hsla(0.0, 0.0, 0.18, 1.0);
    let badge = div()
        .flex_none()
        .px(px(4.0))
        .h(px(13.0))
        .rounded(px(3.0))
        .bg(badge_color(kind))
        .text_color(gpui::white())
        .text_size(px(8.5))
        .font_weight(gpui::FontWeight::BOLD)
        .flex()
        .items_center()
        .child(SharedString::from(kind.badge(name)));
    let label = crate::attachments::attachment_display_name(name).to_string();
    let footer = div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .px(px(5.0))
        .pb(px(4.0))
        .pt(px(10.0))
        .flex()
        .items_center()
        .gap(px(4.0))
        .rounded_b(px(radius))
        .bg(gpui::linear_gradient(
            180.0,
            gpui::linear_color_stop(gpui::hsla(0.0, 0.0, 0.0, 0.0), 0.0),
            gpui::linear_color_stop(gpui::hsla(0.0, 0.0, 0.0, 0.55), 1.0),
        ))
        .child(badge)
        .when(named, |el| {
            el.child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(9.5))
                    .text_color(gpui::white())
                    .child(SharedString::from(label.clone())),
            )
        });
    let body: AnyElement = match state {
        DocSnapshot::Ready(doc) => match &doc.thumb {
            Some(thumb) => img(thumb.clone())
                .w(px(width))
                .h(px(height))
                .rounded(px(radius))
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            None => div()
                .size_full()
                .rounded(px(radius))
                .bg(paper)
                .px(px(6.0))
                .pt(px(5.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .text_size(px(6.5))
                .line_height(px(8.5))
                .text_color(ink)
                .children(doc.lines.iter().map(|line| {
                    div()
                        .w_full()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(line.clone())
                }))
                .into_any_element(),
        },
        DocSnapshot::Loading => div()
            .size_full()
            .rounded(px(radius))
            .bg(theme.ink(0.055))
            .opacity(0.35 + 0.4 * pulse)
            .into_any_element(),
        DocSnapshot::Failed { .. } => div()
            .size_full()
            .rounded(px(radius))
            .bg(theme.ink(0.035))
            .flex()
            .items_center()
            .justify_center()
            .pb(px(12.0))
            .child(
                crate::file_icons::icon(
                    crate::file_icons::FileIconIdentity::file(&label),
                    theme.appearance,
                )
                .size(px(22.0)),
            )
            .into_any_element(),
    };
    div()
        .relative()
        .w(px(width))
        .h(px(height))
        .overflow_hidden()
        .child(body)
        .child(footer)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_cannot_be_previewed_is_never_retried() {
        let (device, path) = ("dev-a", "/up/ab12cd34-broken.pdf");
        assert!(begin_load(device, path));
        assert!(!begin_load(device, path), "one load at a time");
        store_failed(device, path, true);
        assert!(matches!(
            snapshot(device, path),
            DocSnapshot::Failed { retry_in: None }
        ));
        assert!(!begin_load(device, path));

        let flaky = "/up/ab12cd34-later.pdf";
        assert!(begin_load(device, flaky));
        store_failed(device, flaky, false);
        assert!(matches!(
            snapshot(device, flaky),
            DocSnapshot::Failed { retry_in: Some(_) }
        ));
    }

    #[test]
    fn a_dropped_load_releases_its_claim() {
        let (device, path) = ("dev-b", "/up/ab12cd34-notes.md");
        assert!(begin_load(device, path));
        drop(LoadGuard {
            device: device.into(),
            path: path.into(),
        });
        assert!(matches!(
            snapshot(device, path),
            DocSnapshot::Failed { retry_in: Some(_) }
        ));
    }

    #[test]
    fn a_sent_document_resolves_under_its_rewritten_path() {
        let doc = test_doc("notes.md", DocKind::Text, b"# Plan\nship it\n");
        assert_eq!(
            doc.lines,
            vec![SharedString::from("# Plan"), "ship it".into()]
        );
        assert_eq!(doc.name.as_ref(), "notes.md");
        let (device, path) = staged_key("att-1");
        store_ready(device, &path, doc);
        seed_staged_alias("host", "ab12cd34-0000-uuid", "att-1");
        // The host rewrites `pending://` to `{uploads}/{id8}-{name}`.
        assert!(matches!(
            snapshot("host", "/srv/uploads/ab12cd34-notes.md"),
            DocSnapshot::Ready(_)
        ));
        assert!(matches!(
            snapshot("host", "/srv/uploads/ffffffff-notes.md"),
            DocSnapshot::Loading
        ));
    }

    #[test]
    fn a_pdf_thumbnail_keeps_the_top_of_page_one() {
        let pdf = zeron_docview_test_pdf();
        let doc = test_doc("report.pdf", DocKind::Pdf, &pdf);
        let thumb = doc.thumb.as_ref().expect("a page thumbnail");
        let (width, height) = crate::appshots::png_dimensions(&thumb.bytes).unwrap();
        assert_eq!(width, THUMB_RENDER_WIDTH);
        // The 2:1 test page is shorter than the frame's 7:5, so nothing is cut.
        assert_eq!(height, THUMB_RENDER_WIDTH / 2);
    }

    /// A one-page 200 x 100 pt PDF.
    fn zeron_docview_test_pdf() -> Vec<u8> {
        let content = "0 0 0 rg 20 20 80 40 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_string(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 5\n0000000000 65535 f \n").as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        out
    }
}
