//! `html_preview`: render a page in a headless Chromium-family browser on this
//! device (Chrome, Edge or Chromium) and hand the agent a screenshot, the
//! height the page needs, and its console output, so it can check a page
//! before publishing it with `html_render`.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a tool's picture rides in its result; the server moves it into an
/// MCP image block.
pub const IMAGE_KEY: &str = "__image";
/// The reply column a published page usually shows at.
pub const DEFAULT_WIDTH: u32 = 760;
const MIN_WIDTH: u32 = 240;
const MAX_WIDTH: u32 = 1600;
const MAX_SHOT_HEIGHT: u32 = 4000;
const RUN_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_CONSOLE: usize = 50;

pub const DESCRIPTION: &str = "Render an HTML page in a headless browser on this device (Chrome, Edge or Chromium) and get back a PNG screenshot, contentHeight (the height the page needs at this width) and its console output: log, warnings, errors and uncaught exceptions. console.log is a fine way to report your own checks. Use it to check and iterate on a page before html_render. Takes the same self-contained document as html_render, with local images inlined the same way, and the same theme variables.";

/// Zeron's default theme for a page previewed away from the app, matching
/// the variables `html_render` promises.
fn theme_style(dark: bool) -> String {
    let vars: &[(&str, &str)] = if dark {
        &[
            ("--background", "#060606"),
            ("--foreground", "#e5e5e5"),
            ("--muted", "#0d0d0d"),
            ("--muted-foreground", "#a1a1a1"),
            ("--card", "#0e0e0e"),
            ("--card-foreground", "#e5e5e5"),
            ("--border", "#1f1f1f"),
            ("--accent", "#22d3ee"),
            ("--accent-foreground", "#062a30"),
            ("--destructive", "#f87171"),
            ("--warning", "#fbbf24"),
            ("--success", "#34d399"),
            ("--code-background", "#0b1416"),
            ("--code-foreground", "#22d3ee"),
        ]
    } else {
        &[
            ("--background", "#ffffff"),
            ("--foreground", "#262626"),
            ("--muted", "#f5f5f5"),
            ("--muted-foreground", "#525252"),
            ("--card", "#ffffff"),
            ("--card-foreground", "#262626"),
            ("--border", "#e6e6e6"),
            ("--accent", "#0891b2"),
            ("--accent-foreground", "#ffffff"),
            ("--destructive", "#dc2626"),
            ("--warning", "#b45309"),
            ("--success", "#059669"),
            ("--code-background", "#ecf8fb"),
            ("--code-foreground", "#0e7490"),
        ]
    };
    let chart_first = if dark { "#22d3ee" } else { "#0891b2" };
    let mut root: String = vars
        .iter()
        .map(|(name, value)| format!("{name}:{value};"))
        .collect();
    for (ix, color) in [
        chart_first,
        "#8b5cf6",
        "#ec4899",
        "#f59e0b",
        "#10b981",
        "#06b6d4",
    ]
    .into_iter()
    .enumerate()
    {
        root.push_str(&format!("--chart-{}:{color};", ix + 1));
    }
    root.push_str("--radius:10px;--font-sans:system-ui,-apple-system,\"Segoe UI\",sans-serif;--font-mono:ui-monospace,Menlo,Consolas,monospace;");
    let scheme = if dark { "dark" } else { "light" };
    format!(
        ":root{{{root}color-scheme:{scheme}}}html{{background:var(--background);color:var(--foreground);font-family:var(--font-sans)}}body{{margin:0}}html{{scrollbar-width:none}}html::-webkit-scrollbar{{display:none}}"
    )
}

/// The measuring hook: once the page has loaded, its height lands in the
/// title, which `--dump-dom` prints.
const MEASURE: &str = "<script>addEventListener('load',function(){var d=document.documentElement,b=document.body;document.title='zeron-height:'+Math.ceil(Math.max(d.scrollHeight,b?b.scrollHeight:0));});</script>";

/// The page as the preview loads it: theme first in `<head>`, the measuring
/// hook after it.
pub fn staged(html: &str, dark: bool) -> String {
    let insert = format!(
        "<style id=\"zeron-theme\">{}</style>{MEASURE}",
        theme_style(dark)
    );
    let lower = html.to_ascii_lowercase();
    let at = open_tag_end(&lower, "<head")
        .or_else(|| open_tag_end(&lower, "<html"))
        .or_else(|| open_tag_end(&lower, "<!doctype"))
        .unwrap_or(0);
    format!("{}{insert}{}", &html[..at], &html[at..])
}

/// Just past the `>` of the first `name` tag (`<head>`, not `<header>`).
fn open_tag_end(lower: &str, name: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| from + i) {
        let next = lower.as_bytes().get(i + name.len()).copied();
        if next.is_some_and(|b| b == b'>' || b == b'/' || b.is_ascii_whitespace()) {
            return lower[i..].find('>').map(|j| i + j + 1);
        }
        from = i + 1;
    }
    None
}

/// The height `--dump-dom` reported, if the page finished loading.
pub fn measured_height(dom: &str) -> Option<u32> {
    let start = dom.find("zeron-height:")? + "zeron-height:".len();
    let digits: String = dom[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Console lines from Chromium's stderr log
/// (`…:INFO:CONSOLE(3)] "text", source: file:///…/page.html (3)`).
pub fn console_messages(log: &str) -> Vec<(String, String)> {
    log.lines()
        .filter_map(|line| {
            let at = line.find(":CONSOLE")?;
            let level = match line[..at].rsplit(':').next()? {
                "ERROR" | "FATAL" => "error",
                "WARNING" => "warning",
                "VERBOSE1" | "VERBOSE2" => "info",
                _ => "log",
            };
            let rest = &line[at..];
            let text = rest.split_once("] \"")?.1;
            let text = text
                .rsplit_once("\", source:")
                .map_or(text, |(text, _)| text);
            Some((level.to_owned(), text.to_owned()))
        })
        .take(MAX_CONSOLE)
        .collect()
}

/// A Chromium-family browser on this device. `ZERON_PREVIEW_BROWSER` wins.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ZERON_PREVIEW_BROWSER") {
        return Some(PathBuf::from(path));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(var) {
                let root = PathBuf::from(root);
                candidates.push(root.join(r"Google\Chrome\Application\chrome.exe"));
                candidates.push(root.join(r"Chromium\Application\chrome.exe"));
                candidates.push(root.join(r"Microsoft\Edge\Application\msedge.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for app in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ] {
            candidates.push(Path::new("/Applications").join(app));
            if let Some(home) = &home {
                candidates.push(home.join("Applications").join(app));
            }
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for name in [
            "chrome-headless-shell",
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ] {
            candidates.push(dir.join(name));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// What a preview found.
pub struct Preview {
    pub width: u32,
    pub content_height: Option<u32>,
    pub captured_height: u32,
    pub console: Vec<(String, String)>,
    pub png: Vec<u8>,
}

/// Render `html` at `width` and capture it. `Err` is the model's message.
pub async fn render(html: &str, width: Option<u32>, dark: bool) -> Result<Preview, String> {
    let browser = find_browser().ok_or_else(|| {
        "No Chrome, Edge or Chromium found on this device to preview with (set ZERON_PREVIEW_BROWSER to one). Skip the preview and publish with html_render.".to_owned()
    })?;
    let width = width.unwrap_or(DEFAULT_WIDTH).clamp(MIN_WIDTH, MAX_WIDTH);
    let dir = std::env::temp_dir().join(format!("zeron-preview-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = render_in(&browser, &dir, html, width, dark).await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

async fn render_in(
    browser: &Path,
    dir: &Path,
    html: &str,
    width: u32,
    dark: bool,
) -> Result<Preview, String> {
    let page = dir.join("page.html");
    std::fs::write(&page, staged(html, dark)).map_err(|e| e.to_string())?;
    let url = file_url(&page);
    // First pass: the height the page needs, and what it logged.
    let measure = run(
        browser,
        dir,
        &[
            format!("--window-size={width},800"),
            "--dump-dom".into(),
            url.clone(),
        ],
    )
    .await?;
    let content_height = measured_height(&String::from_utf8_lossy(&measure.stdout));
    let console = console_messages(&String::from_utf8_lossy(&measure.stderr));
    // Second pass: the picture, as tall as the page (within reason).
    let captured_height = content_height.unwrap_or(800).clamp(80, MAX_SHOT_HEIGHT);
    let shot = dir.join("shot.png");
    run(
        browser,
        dir,
        &[
            format!("--window-size={width},{captured_height}"),
            format!("--screenshot={}", shot.display()),
            url,
        ],
    )
    .await?;
    let png = std::fs::read(&shot)
        .map_err(|_| "The browser did not produce a screenshot of the page.".to_owned())?;
    Ok(Preview {
        width,
        content_height,
        captured_height,
        console,
        png,
    })
}

async fn run(browser: &Path, dir: &Path, extra: &[String]) -> Result<std::process::Output, String> {
    let mut command = tokio::process::Command::new(browser);
    command
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--hide-scrollbars")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-extensions")
        .arg("--mute-audio")
        .arg("--enable-logging=stderr")
        .arg("--v=0")
        .arg("--virtual-time-budget=3000")
        .arg(format!("--user-data-dir={}", dir.join("profile").display()))
        .args(extra)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(RUN_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(format!("Could not start {}: {e}", browser.display())),
        Err(_) => Err("The preview browser took too long; the page may loop forever.".into()),
    }
}

/// `file:///` URL of an absolute path, on Windows too.
fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    let encoded: String = text
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_owned(),
            '#' => "%23".to_owned(),
            '?' => "%3F".to_owned(),
            '%' => "%25".to_owned(),
            c => c.to_string(),
        })
        .collect();
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_staged_page_gets_the_theme_and_the_measuring_hook_in_its_head() {
        let page = staged(
            "<!doctype html><html><head><title>t</title></head></html>",
            true,
        );
        let head = page.find("<head>").unwrap() + "<head>".len();
        assert!(page[head..].starts_with("<style id=\"zeron-theme\">:root{--background:#060606;"));
        assert!(page.contains("zeron-height:"));
        assert!(page.ends_with("<title>t</title></head></html>"));
        assert!(
            staged("<p>x</p>", false)
                .starts_with("<style id=\"zeron-theme\">:root{--background:#ffffff;")
        );
    }

    #[test]
    fn the_height_comes_from_the_dumped_title() {
        assert_eq!(
            measured_height("<html><head><title>zeron-height:1234</title></head></html>"),
            Some(1234)
        );
        assert_eq!(measured_height("<html></html>"), None);
    }

    #[test]
    fn console_lines_are_read_from_the_browser_log() {
        let log = "[1:2:1014/120000.000:INFO:CONSOLE(3)] \"hello, world\", source: file:///tmp/page.html (3)\n\
                   [1:2:1014/120000.000:ERROR:CONSOLE(9)] \"Uncaught ReferenceError: x is not defined\", source: file:///tmp/page.html (9)\n\
                   [1:2:1014/120000.000:WARNING:gpu_init.cc(1)] unrelated\n\
                   [1:2:1014/120000.000:WARNING:CONSOLE:4] \"careful\", source: file:///tmp/page.html (4)";
        assert_eq!(
            console_messages(log),
            vec![
                ("log".to_owned(), "hello, world".to_owned()),
                (
                    "error".to_owned(),
                    "Uncaught ReferenceError: x is not defined".to_owned()
                ),
                ("warning".to_owned(), "careful".to_owned()),
            ]
        );
    }

    #[test]
    fn file_urls_cover_unix_and_windows_paths() {
        assert_eq!(
            file_url(Path::new("/tmp/zeron preview/page.html")),
            "file:///tmp/zeron%20preview/page.html"
        );
        assert_eq!(
            file_url(Path::new(r"C:\Temp\page.html")),
            "file:///C:/Temp/page.html"
        );
    }
}
