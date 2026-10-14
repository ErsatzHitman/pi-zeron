//! Pages an agent published with the Zeron MCP server's `html_render`,
//! served from a loopback server to native webviews embedded in the reply
//! (T3 Code's sandboxed iframe, done natively).
//!
//! Each page gets an unguessable address and a `sandbox` CSP, so it runs in
//! an opaque origin: it can run its own scripts and open links in a new tab,
//! but cannot fetch, post forms, or reach Zeron. Zeron's theme rides in as
//! CSS variables the agent's tool description names, and a small script
//! reports the page's height through its title so the reply can fit it.
//!
//! Inline webviews are positioned by the frame that paints them. One that
//! scrolled out of the transcript, or whose transcript stopped painting, is
//! not painted at all, so the shell sweeps every frame ([`sweep`]) and hides
//! the pages no row marked as painted.

use std::cell::{Cell, RefCell};
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use gpui::{App, Entity, Hsla, WeakEntity, Window};

use crate::browser::{BrowserContext, BrowserSurface, model::Presentation};
use crate::theme::Theme;

/// The type a published page's message part carries.
pub use zeron_proto::PAGE_MIME;
/// Pages kept addressable; the oldest goes first.
const MAX_PAGES: usize = 32;
const CSP: &str = "sandbox allow-scripts allow-popups; connect-src 'none'; form-action 'none'";

/// An inline page's frame, in logical pixels (T3's bounds).
pub const MIN_HEIGHT: f32 = 80.0;
pub const MAX_HEIGHT: f32 = 2000.0;
/// What the page reports its height as, through `document.title`.
const HEIGHT_TITLE: &str = "zeron-height:";
/// Reports the document's laid-out height whenever it changes.
const MEASURE: &str = "<script>(function(){function m(){var v='zeron-height:'+Math.ceil(document.documentElement.getBoundingClientRect().height);if(document.title!==v)document.title=v}addEventListener('DOMContentLoaded',m);addEventListener('load',m);if(window.ResizeObserver)new ResizeObserver(m).observe(document.documentElement)})()</script>";

struct Page {
    token: String,
    title: String,
    html: Arc<Vec<u8>>,
}

struct Server {
    port: u16,
    pages: Mutex<Vec<Page>>,
}

static SERVER: OnceLock<Result<Arc<Server>, String>> = OnceLock::new();

fn server() -> Result<Arc<Server>, String> {
    SERVER
        .get_or_init(|| {
            let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
            let port = listener.local_addr().map_err(|e| e.to_string())?.port();
            let server = Arc::new(Server {
                port,
                pages: Mutex::new(Vec::new()),
            });
            let accept = server.clone();
            std::thread::Builder::new()
                .name("zeron-pages".into())
                .spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let server = accept.clone();
                        let _ = std::thread::Builder::new()
                            .name("zeron-page".into())
                            .spawn(move || server.answer(stream));
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(server)
        })
        .clone()
}

/// Serve `html` with Zeron's theme `style` and return its address.
pub fn serve(html: &[u8], style: &str, title: &str) -> Result<String, String> {
    let server = server()?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let mut pages = server.pages.lock().unwrap_or_else(PoisonError::into_inner);
    if pages.len() >= MAX_PAGES {
        pages.remove(0);
    }
    pages.push(Page {
        token: token.clone(),
        title: title.to_owned(),
        html: Arc::new(themed(html, style)),
    });
    Ok(format!("http://127.0.0.1:{}/page/{token}", server.port))
}

/// The agent's title for a served page (its document title carries the
/// height report instead).
pub fn page_title(url: &str) -> Option<String> {
    let server = SERVER.get()?.as_ref().ok()?;
    let token = url
        .strip_prefix(&format!("http://127.0.0.1:{}/page/", server.port))?
        .split(['?', '#'])
        .next()?;
    let pages = server.pages.lock().unwrap_or_else(PoisonError::into_inner);
    pages
        .iter()
        .find(|page| page.token == token)
        .map(|page| page.title.clone())
}

/// The height a page reported through its title.
pub fn measured_height(title: &str) -> Option<f32> {
    title.strip_prefix(HEIGHT_TITLE)?.trim().parse().ok()
}

/// Whether `url` is one of this server's page addresses (no remote-device
/// localhost warning applies to it).
pub fn is_page_url(url: &str) -> bool {
    let Some(Ok(server)) = SERVER.get() else {
        return false;
    };
    url.starts_with(&format!("http://127.0.0.1:{}/page/", server.port))
}

impl Server {
    fn answer(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let Ok(reader) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reader);
        let mut request = String::new();
        if reader.read_line(&mut request).is_err() {
            return;
        }
        let mut host = None;
        for _ in 0..100 {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.trim().is_empty() => break,
                Ok(_) => {
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("host")
                    {
                        host = Some(value.trim().to_owned());
                    }
                }
            }
        }
        let routed = route(&request, host.as_deref(), self.port);
        let head = routed.is_some_and(|(_, head)| head);
        let page = routed.and_then(|(token, _)| {
            let pages = self.pages.lock().unwrap_or_else(PoisonError::into_inner);
            pages
                .iter()
                .find(|page| page.token == token)
                .map(|page| page.html.clone())
        });
        let _ = match page {
            Some(page) => {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nContent-Security-Policy: {CSP}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
                    page.len()
                );
                stream.write_all(header.as_bytes()).and_then(|_| {
                    if head {
                        Ok(())
                    } else {
                        stream.write_all(&page)
                    }
                })
            }
            None => stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ),
        };
        let _ = stream.flush();
    }
}

/// The page token a request asks for, and whether it is a `HEAD`. Only
/// `GET`/`HEAD` for `/page/{token}` addressed to this exact host and port
/// (a rebound DNS name gets nothing).
fn route<'a>(request: &'a str, host: Option<&str>, port: u16) -> Option<(&'a str, bool)> {
    let mut parts = request.split_whitespace();
    let head = match parts.next()? {
        "GET" => false,
        "HEAD" => true,
        _ => return None,
    };
    let target = parts.next()?;
    if host != Some(format!("127.0.0.1:{port}").as_str()) {
        return None;
    }
    let token = target.strip_prefix("/page/")?;
    let token = token.split(['?', '#']).next().unwrap_or(token);
    (!token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric())).then_some((token, head))
}

/// What goes first in a page's `<head>`.
fn head(style: &str) -> String {
    format!("<style id=\"zeron-theme\">{style}</style>{MEASURE}")
}

/// The page with Zeron's theme as its first stylesheet, so its own CSS wins,
/// and the height report.
fn themed(html: &[u8], style: &str) -> Vec<u8> {
    let tag = head(style);
    let lower = html.to_ascii_lowercase();
    let at = open_tag_end(&lower, b"<head")
        .or_else(|| open_tag_end(&lower, b"<html"))
        .or_else(|| open_tag_end(&lower, b"<!doctype"))
        .unwrap_or(0);
    let mut out = Vec::with_capacity(html.len() + tag.len());
    out.extend_from_slice(&html[..at]);
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(&html[at..]);
    out
}

/// Just past the `>` of the first `name` tag (`<head>`, not `<header>`).
fn open_tag_end(lower: &[u8], name: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = lower[from..]
        .windows(name.len())
        .position(|w| w == name)
        .map(|i| from + i)
    {
        let next = lower.get(i + name.len()).copied();
        if next.is_some_and(|b| b == b'>' || b == b'/' || b.is_ascii_whitespace()) {
            return lower[i..]
                .iter()
                .position(|&b| b == b'>')
                .map(|j| i + j + 1);
        }
        from = i + 1;
    }
    None
}

/// Zeron's theme as the CSS variables the `html_render` description lists,
/// plus a base stylesheet that uses them.
pub fn theme_style(theme: &Theme) -> String {
    let hex = |color: Hsla| opaque_hex(color, theme.bg);
    let mut vars = vec![
        ("--background", hex(theme.bg)),
        ("--foreground", hex(theme.text)),
        ("--muted", hex(theme.surface)),
        ("--muted-foreground", hex(theme.text_muted)),
        ("--card", hex(theme.surface_card)),
        ("--card-foreground", hex(theme.text)),
        ("--border", hex(theme.border)),
        ("--accent", hex(theme.accent)),
        ("--accent-foreground", hex(theme.on_accent)),
        ("--destructive", hex(theme.danger)),
        ("--warning", hex(theme.warning)),
        ("--success", hex(theme.success)),
        ("--code-background", hex(theme.code_wash)),
        ("--code-foreground", hex(theme.code_text)),
    ];
    let charts = [
        "--chart-1",
        "--chart-2",
        "--chart-3",
        "--chart-4",
        "--chart-5",
        "--chart-6",
    ];
    for (ix, name) in charts.into_iter().enumerate() {
        vars.push((name, hex(crate::markdown::chart::color(theme, ix))));
    }
    let font = |name: &str| -> String {
        name.chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
            .collect()
    };
    let mut root: String = vars
        .into_iter()
        .map(|(name, value)| format!("{name}:{value};"))
        .collect();
    root.push_str("--radius:10px;");
    root.push_str(&format!(
        "--font-sans:\"{}\",system-ui,sans-serif;--font-mono:\"{}\",ui-monospace,monospace;",
        font(&theme.font_sans),
        font(&theme.font_mono)
    ));
    let scheme = if theme.appearance.is_dark() {
        "dark"
    } else {
        "light"
    };
    format!(
        ":root{{{root}color-scheme:{scheme}}}html{{background:var(--background);color:var(--foreground);font-family:var(--font-sans)}}body{{margin:0}}html{{scrollbar-width:none}}html::-webkit-scrollbar{{display:none}}"
    )
}

thread_local! {
    /// Inline pages, each with the flag its row sets when it paints.
    static INLINE: RefCell<Vec<(WeakEntity<BrowserSurface>, Rc<Cell<bool>>)>> =
        RefCell::default();
    /// Inline pages share one ephemeral website-data profile, apart from
    /// the browser tabs'.
    static CONTEXT: BrowserContext = BrowserContext::default();
}

pub fn inline_context() -> BrowserContext {
    CONTEXT.with(Clone::clone)
}

/// Track an inline page; its row sets the returned flag whenever it paints.
pub fn register_inline(browser: &Entity<BrowserSurface>) -> Rc<Cell<bool>> {
    let painted = Rc::new(Cell::new(false));
    INLINE.with(|pages| {
        pages
            .borrow_mut()
            .push((browser.downgrade(), painted.clone()))
    });
    painted
}

/// Run once per frame after the transcripts paint: show the inline pages a
/// row painted this frame and hide the rest. The change applies after the
/// frame, and a frame follows it so the native layer commits.
pub fn sweep(window: &mut Window, cx: &mut App) {
    let pages: Vec<(WeakEntity<BrowserSurface>, Presentation)> = INLINE.with(|pages| {
        let mut pages = pages.borrow_mut();
        pages.retain(|(page, _)| page.upgrade().is_some());
        pages
            .iter()
            .map(|(page, painted)| {
                let presentation = if painted.replace(false) {
                    Presentation::Live
                } else {
                    Presentation::Hidden
                };
                (page.clone(), presentation)
            })
            .collect()
    });
    let stale: Vec<_> = pages
        .into_iter()
        .filter(|(page, presentation)| {
            page.read_with(cx, |page, _| page.presentation() != *presentation)
                .unwrap_or(false)
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    window.defer(cx, move |window, cx| {
        for (page, presentation) in stale {
            let _ = page.update(cx, |page, cx| page.set_presentation(presentation, cx));
        }
        window.refresh();
    });
}

/// `#rrggbb` of `color` laid over `background`.
fn opaque_hex(color: Hsla, background: Hsla) -> String {
    let c = color.to_rgb();
    let bg = background.to_rgb();
    let mix = |fg: f32, bg: f32| ((fg * c.a + bg * (1.0 - c.a)) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        mix(c.r, bg.r),
        mix(c.g, bg.g),
        mix(c.b, bg.b)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn the_theme_goes_first_in_the_head_without_breaking_the_doctype() {
        let h = head("x");
        let themed = |html: &str| String::from_utf8(themed(html.as_bytes(), "x")).unwrap();
        assert!(h.starts_with("<style id=\"zeron-theme\">x</style><script>"));
        assert_eq!(
            themed("<!DOCTYPE html><html><HEAD><title>t</title></head></html>"),
            format!("<!DOCTYPE html><html><HEAD>{h}<title>t</title></head></html>")
        );
        assert_eq!(
            themed("<!doctype html><header>h</header>"),
            format!("<!doctype html>{h}<header>h</header>")
        );
        assert_eq!(
            themed("<html lang=en><body>b</body></html>"),
            format!("<html lang=en>{h}<body>b</body></html>")
        );
        assert_eq!(themed("<p>b</p>"), format!("{h}<p>b</p>"));
    }

    #[test]
    fn only_a_page_request_to_this_exact_host_routes() {
        let host = Some("127.0.0.1:4000");
        assert_eq!(
            route("GET /page/ab12 HTTP/1.1\r\n", host, 4000),
            Some(("ab12", false))
        );
        assert_eq!(
            route("HEAD /page/ab12?x HTTP/1.1\r\n", host, 4000),
            Some(("ab12", true))
        );
        assert_eq!(route("POST /page/ab12 HTTP/1.1\r\n", host, 4000), None);
        assert_eq!(route("GET /page/../x HTTP/1.1\r\n", host, 4000), None);
        assert_eq!(route("GET /page/ HTTP/1.1\r\n", host, 4000), None);
        assert_eq!(
            route("GET /page/ab12 HTTP/1.1\r\n", Some("evil.test:4000"), 4000),
            None
        );
        assert_eq!(route("GET /page/ab12 HTTP/1.1\r\n", None, 4000), None);
    }

    #[test]
    fn a_served_page_comes_back_sandboxed_and_themed() {
        let url = serve(b"<html><head></head><body>hi</body></html>", "x", "Hi").unwrap();
        assert!(is_page_url(&url));
        assert_eq!(page_title(&url).as_deref(), Some("Hi"));
        assert_eq!(measured_height("zeron-height:412"), Some(412.0));
        assert_eq!(measured_height("Hi"), None);
        let address = url.strip_prefix("http://").unwrap();
        let (authority, path) = address.split_once('/').unwrap();
        let mut stream = TcpStream::connect(authority).unwrap();
        write!(stream, "GET /{path} HTTP/1.1\r\nHost: {authority}\r\n\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.contains(&format!("Content-Security-Policy: {CSP}")));
        assert!(reply.ends_with(&format!(
            "<html><head>{}</head><body>hi</body></html>",
            head("x")
        )));
        let mut stream = TcpStream::connect(authority).unwrap();
        write!(
            stream,
            "GET /page/missing HTTP/1.1\r\nHost: {authority}\r\n\r\n"
        )
        .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
    }

    #[test]
    fn the_theme_style_names_every_variable_the_tool_promises() {
        let style = theme_style(&Theme::dark());
        for name in [
            "--background:#",
            "--foreground:#",
            "--muted:#",
            "--muted-foreground:#",
            "--card:#",
            "--border:#",
            "--accent:#",
            "--accent-foreground:#",
            "--destructive:#",
            "--warning:#",
            "--success:#",
            "--chart-1:#",
            "--chart-6:#",
            "--radius:",
            "--font-sans:",
            "--font-mono:",
            "color-scheme:dark",
        ] {
            assert!(style.contains(name), "{name} in {style}");
        }
        assert!(!style.contains("</"));
    }
}
