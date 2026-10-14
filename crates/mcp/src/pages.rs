//! `html_render`: an agent publishes a self-contained HTML page into its own
//! chat. The page is stored like an upload on this device. The harness turns
//! the tool's result into a page part, and the transcript shows a card that
//! opens the page in the right pane.

use std::path::Path;

/// The page's own size, before local images are inlined.
pub const MAX_HTML_CHARS: usize = 512_000;
pub const MAX_TITLE_CHARS: usize = 200;
/// Per inlined image, and for the finished page.
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 25 * 1024 * 1024;

/// What the agent reads back: the reader already sees the page.
pub const PUBLISHED: &str = "The page is published in this chat, above your reply. Don't mention, announce or describe it; reply with only what it doesn't already say.";

/// The tool's description, shared with the prompt's wording.
pub const DESCRIPTION: &str = "Show a finished HTML page (chart, table, diagram, image collage, mockup) in this chat, above your final reply; call it before writing that reply. The reader opens it from a card in the transcript, so the reply should not announce it, say where it is or restate it: add only what the page doesn't say. Write one self-contained document with inline <style> and <script>. Local images written as absolute paths (src=\"/abs/shot.png\", CSS url(/abs/bg.webp), or a JS string) are inlined automatically; remote https URLs, such as a CDN chart library, load as-is. The page cannot call back to any server (fetch and XHR are blocked). Zeron sets its theme as CSS variables on :root: --background, --foreground, --muted, --muted-foreground, --card, --border, --accent, --accent-foreground, --destructive, --warning, --success, --chart-1 to --chart-6, --radius, --font-sans and --font-mono. The base stylesheet sets the page's background, color and font from them; your own CSS overrides it. Use a fluid width and give charts fixed pixel heights.";

/// The page with its local images inlined, and the paths that could not be.
#[derive(Debug, PartialEq, Eq)]
pub struct Prepared {
    pub html: String,
    pub missing: Vec<String>,
}

/// Check the arguments and inline local images. `Err` is the model's message.
pub fn prepare(html: &str, title: &str) -> Result<(Prepared, String), String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("title must not be empty".into());
    }
    let title: String = title.chars().take(MAX_TITLE_CHARS).collect();
    if html.trim().is_empty() {
        return Err("html must not be empty".into());
    }
    if html.chars().count() > MAX_HTML_CHARS {
        return Err(format!(
            "html is over {MAX_HTML_CHARS} characters; move big data into images or trim it"
        ));
    }
    let prepared = inline_images(html, |path| std::fs::read(path).ok());
    if prepared.html.len() > MAX_PAGE_BYTES {
        return Err("the page with its images inlined is over 25 MiB".into());
    }
    Ok((prepared, title))
}

/// Replace every quoted or `url(...)` absolute path to a local image with a
/// `data:` URI. A path that cannot be read stays as written and is reported.
pub fn inline_images(html: &str, read: impl Fn(&Path) -> Option<Vec<u8>>) -> Prepared {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut missing = Vec::new();
    let mut total = 0usize;
    let mut last = 0;
    let mut i = 0;
    while i < bytes.len() {
        let close = match bytes[i] {
            b'"' => b'"',
            b'\'' => b'\'',
            b'(' => b')',
            _ => {
                i += 1;
                continue;
            }
        };
        let start = i + 1;
        let Some(len) = bytes[start..]
            .iter()
            .take(1024)
            .position(|&b| b == close || b == b'\n' || b == b'<')
        else {
            i += 1;
            continue;
        };
        // Both ends sit on ASCII bytes, so the slice is on char boundaries.
        let raw = &html[start..start + len];
        let candidate = raw.trim();
        let Some(mime) = local_image(candidate) else {
            i += 1;
            continue;
        };
        let path = candidate.strip_prefix("file://").unwrap_or(candidate);
        let data = read(Path::new(path))
            .filter(|data| (data.len() as u64) <= MAX_IMAGE_BYTES)
            .filter(|data| total + data.len() <= MAX_PAGE_BYTES);
        match data {
            Some(data) => {
                total += data.len();
                out.push_str(&html[last..start]);
                out.push_str("data:");
                out.push_str(mime);
                out.push_str(";base64,");
                out.push_str(&base64(&data));
                last = start + len;
            }
            None => {
                if !missing.iter().any(|m| m == path) {
                    missing.push(path.to_owned());
                }
            }
        }
        i = start + len;
    }
    out.push_str(&html[last..]);
    Prepared { html: out, missing }
}

/// The image type of an absolute local path (`/x.png`, `C:\x.png`,
/// `file:///x.png`); `None` for anything else, including `//host/x.png`.
fn local_image(candidate: &str) -> Option<&'static str> {
    let path = candidate.strip_prefix("file://").unwrap_or(candidate);
    let bytes = path.as_bytes();
    let unix = path.starts_with('/') && !path.starts_with("//");
    let windows = bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    if !(unix || windows) {
        return None;
    }
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        _ => return None,
    })
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe]), "//4=");
    }

    #[test]
    fn local_images_are_inlined_wherever_the_page_names_them() {
        let html = r#"<img src="/tmp/a.png"><div style="background:url( /tmp/b.JPG )"></div><script>const s = '/tmp/a.png';</script>"#;
        let page = inline_images(html, |path| {
            Some(match path.to_str().unwrap() {
                "/tmp/a.png" => b"foo".to_vec(),
                _ => b"fo".to_vec(),
            })
        });
        assert_eq!(
            page.html,
            r#"<img src="data:image/png;base64,Zm9v"><div style="background:url(data:image/jpeg;base64,Zm8=)"></div><script>const s = 'data:image/png;base64,Zm9v';</script>"#
        );
        assert!(page.missing.is_empty());
    }

    #[test]
    fn remote_relative_and_unreadable_references_stay_as_written() {
        let html = r#"<img src="https://x.dev/a.png"><img src="//cdn/a.png"><img src="a.png"><img src="/gone.png"><img src="/gone.png"><p>"/notes.txt"</p>"#;
        let page = inline_images(html, |_| None);
        assert_eq!(page.html, html);
        assert_eq!(page.missing, vec!["/gone.png".to_owned()]);
    }

    #[test]
    fn windows_and_file_url_paths_count_as_local() {
        assert_eq!(local_image(r"C:\shots\a.webp"), Some("image/webp"));
        assert_eq!(local_image("file:///tmp/a.svg"), Some("image/svg+xml"));
        assert_eq!(local_image("/tmp/a"), None);
        let page = inline_images(r#"<img src="file:///tmp/a.gif">"#, |path| {
            (path == Path::new("/tmp/a.gif")).then(|| b"f".to_vec())
        });
        assert_eq!(page.html, r#"<img src="data:image/gif;base64,Zg==">"#);
    }

    #[test]
    fn prepare_checks_the_title_and_page() {
        assert!(prepare("<p>x</p>", "  ").is_err());
        assert!(prepare(" ", "Chart").is_err());
        let (page, title) = prepare("<p>x</p>", "  Build times ").unwrap();
        assert_eq!(title, "Build times");
        assert_eq!(page.html, "<p>x</p>");
    }
}
