//! Safe resolution of agent-authored Markdown links into workspace files.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

const FILE_MENTION_SCHEME: &str = "zeron-file:";

/// Windows accepts `\` as a path separator; POSIX treats it as an ordinary
/// file-name character, so only a Windows host folds a span's backslashes.
const BACKSLASH_SEPARATOR: bool = cfg!(windows);

/// Where a classified file link sits relative to a surface's ordered roots.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileLinkResolution {
    /// `roots[index]` owns the target: the link opens in that root's file
    /// context and `link.path` stays workspace-relative.
    Owned {
        root: usize,
        link: WorkspaceFileLink,
    },
    /// An absolute path no known root owns: the linking chat reads it as a
    /// host file and the file surface keeps it read-only.
    Outside(WorkspaceFileLink),
}

/// The first root in `roots` that owns `target`. Roots are tried in order —
/// the linking chat's own checkout first — so the search only widens for a
/// target the own root cannot own: an absolute path inside the parent
/// chat's checkout, say. An absolute path every root declines is still a file
/// link: the linking chat reads it read-only as a host file
/// ([`FileLinkResolution::Outside`]). A relative target stays
/// inside-or-unresolved, as before.
pub(crate) fn first_root_owning<'a>(
    target: &str,
    roots: impl IntoIterator<Item = &'a str>,
) -> Option<FileLinkResolution> {
    let classified = classify_file_link(target)?;
    match classified.kind {
        ClassifiedKind::Relative => roots.into_iter().next().map(|_| FileLinkResolution::Owned {
            root: 0,
            link: classified.link,
        }),
        // Mentions resolve inside a root or not at all, exactly like before.
        ClassifiedKind::Mention => roots.into_iter().enumerate().find_map(|(ix, root)| {
            resolve_decoded_path(&classified.link.path, root).map(|path| {
                FileLinkResolution::Owned {
                    root: ix,
                    link: WorkspaceFileLink {
                        path,
                        ..classified.link.clone()
                    },
                }
            })
        }),
        ClassifiedKind::Absolute => {
            for (ix, root) in roots.into_iter().enumerate() {
                if let Some(path) = resolve_decoded_path(&classified.link.path, root) {
                    return Some(FileLinkResolution::Owned {
                        root: ix,
                        link: WorkspaceFileLink {
                            path,
                            ..classified.link.clone()
                        },
                    });
                }
            }
            Some(FileLinkResolution::Outside(WorkspaceFileLink {
                outside: true,
                ..classified.link
            }))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceFileLink {
    /// Workspace-relative wire path for owned links; the absolute host path
    /// for outside ones.
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
    /// Outside every known root: read-only through the linking chat.
    pub outside: bool,
}

/// One checkout a file link may resolve against, in priority order: a chat's
/// working directory (its file context opens the match) or a bare project
/// root — project roots carry no chat, so their links open in the linking
/// chat's file context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileLinkRoot {
    pub chat: Option<String>,
    pub root: String,
    /// The owner lives on this device — the file is on this disk, so the
    /// link menu's system-level rows (default app, file manager) apply.
    pub local: bool,
}

impl FileLinkRoot {
    /// The on-disk path `link` names under this root — never built from the
    /// raw link text, always from the resolved root join.
    pub(crate) fn absolute(&self, link: &WorkspaceFileLink) -> PathBuf {
        Path::new(&self.root).join(&link.path)
    }
}

/// What one filesystem probe found at a candidate path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathKind {
    File,
    Directory,
    Missing,
}

/// Memoized path probes behind inline-code file links: one `metadata` call
/// per path per link-roots revision, however many spans and frames ask.
#[derive(Default)]
pub(crate) struct PathProbes {
    kinds: HashMap<PathBuf, PathKind>,
}

impl PathProbes {
    pub(crate) fn kind(&mut self, path: &Path) -> PathKind {
        if let Some(kind) = self.kinds.get(path) {
            return *kind;
        }
        let kind = match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => PathKind::File,
            Ok(metadata) if metadata.is_dir() => PathKind::Directory,
            _ => PathKind::Missing,
        };
        self.kinds.insert(path.to_path_buf(), kind);
        kind
    }
}

/// An inline code span that names something on this device.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InlineCodePath {
    /// An existing regular file: the `file://` target that opens it, its
    /// `:line`/`#L` anchor kept.
    File(String),
    /// An existing directory: a later span in the same text part may resolve
    /// a bare name against it.
    Directory(PathBuf),
}

/// Resolve one inline code span under the file-link grammar and report what
/// exists behind it: an absolute path — POSIX, or a Windows drive path on the
/// host that has one — with `~/` expanded to the home directory, then each
/// root in order, then each context directory an earlier span in the same
/// text part named. Only local roots are probed — a remote chat's checkout is
/// not on this disk. A path shape with nothing behind it stays plain code.
pub(crate) fn resolve_inline_code_path(
    candidate: &str,
    roots: &[FileLinkRoot],
    context_dirs: &[PathBuf],
    probes: &mut PathProbes,
) -> Option<InlineCodePath> {
    // A file name is one token: wrapping whitespace or a newline means the
    // model wrote prose, and a URL shape means it wrote a link.
    if candidate.is_empty()
        || candidate.trim() != candidate
        || candidate.contains(['\n', '\r'])
        || candidate.contains("://")
        || candidate.starts_with("mailto:")
    {
        return None;
    }
    let (raw, fragment) = split_line_fragment(candidate)?;
    let (raw, suffix) = split_line_suffix(raw)?;
    let decoded: Cow<str> = if raw.contains('%') {
        match percent_decode_path(raw) {
            Some(decoded) => Cow::Owned(decoded),
            None => Cow::Borrowed(raw),
        }
    } else {
        Cow::Borrowed(raw)
    };
    // A Windows span may separate with `\`; folding it to the wire's `/`
    // before the checks keeps every path shape below host-agnostic. A POSIX
    // span keeps its backslashes — they are ordinary file-name characters.
    let decoded = span_wire_path(&decoded, BACKSLASH_SEPARATOR);
    // The trailing slash of a directory the text introduces ("all under
    // `dir/`:") is presentation, not part of the path.
    let decoded = decoded.trim_end_matches('/');
    if decoded.is_empty() || !clean_path(decoded) {
        return None;
    }
    // The anchor rides along on the link target, so opening it lands on the
    // line the author named.
    let anchor = match fragment.line.or(suffix.line) {
        Some(line) => match fragment.column.or(suffix.column) {
            Some(column) => format!(":{line}:{column}"),
            None => format!(":{line}"),
        },
        None => String::new(),
    };
    let file = |path: &Path| {
        // The target is a `file:///` URL over the path's wire spelling, so a
        // drive path takes the RFC 8089 `/C:/…` form. A path with no wire
        // spelling the link grammar can reopen would be a dead link, so the
        // span keeps its inline-code look instead.
        let wire = inline_code_wire_path(path)?;
        Some(InlineCodePath::File(format!(
            "file://{}{anchor}",
            percent_encode_path(&wire)
        )))
    };
    if let Some(rest) = decoded.strip_prefix("~/") {
        return probe_path(&home_dir()?.join(rest), probes, file);
    }
    if decoded.starts_with('~') {
        return None;
    }
    if Path::new(decoded).has_root() {
        return probe_path(Path::new(decoded), probes, file);
    }
    if has_url_scheme(decoded) {
        return None;
    }
    let relative = safe_relative_path(Path::new(decoded))?;
    for root in roots.iter().filter(|root| root.local) {
        let path = Path::new(&root.root).join(&relative);
        match probes.kind(&path) {
            PathKind::File => return file(&path),
            PathKind::Directory => return Some(InlineCodePath::Directory(path)),
            PathKind::Missing => {}
        }
    }
    for dir in context_dirs {
        let path = dir.join(&relative);
        if probes.kind(&path) == PathKind::File {
            return file(&path);
        }
    }
    None
}

/// Probe `path` once and describe what the caller asked for: a file becomes
/// its link target, a directory the context it provides.
fn probe_path(
    path: &Path,
    probes: &mut PathProbes,
    file: impl Fn(&Path) -> Option<InlineCodePath>,
) -> Option<InlineCodePath> {
    match probes.kind(path) {
        PathKind::File => file(path),
        PathKind::Directory => Some(InlineCodePath::Directory(path.to_path_buf())),
        PathKind::Missing => None,
    }
}

/// A file link's resolved match: the absolute path on the owning device, the
/// wire path its presentation shows, and whether that device is this one.
pub(crate) struct FileLink {
    pub absolute: PathBuf,
    /// The decoded path the hover card shows: workspace-relative for owned
    /// links, absolute for outside ones.
    pub path: String,
    pub local: bool,
}

/// A decoded path plus its line reference, tagged by shape so resolution can
/// apply each kind's own inside/outside rule.
struct ClassifiedLink {
    kind: ClassifiedKind,
    link: WorkspaceFileLink,
}

enum ClassifiedKind {
    /// `zeron-file:` mention — resolution stays inside-or-unresolved.
    Mention,
    /// Root-relative path.
    Relative,
    /// POSIX-absolute path — inside a root when one owns it, a host file
    /// read-only otherwise.
    Absolute,
}

/// What a `#`/`:` line suffix or fragment contributed: a valid first-line
/// anchor, nothing usable (plain `#anchor` fragments are dropped), or an
/// anchor that kills the whole target (line 0, reversed range).
enum LineAnchor {
    None,
    Line { line: u32, column: Option<u32> },
    Invalid,
}

pub(crate) fn resolve_workspace_file_link(
    target: &str,
    workspace_root: &str,
) -> Option<WorkspaceFileLink> {
    let classified = classify_file_link(target)?;
    match classified.kind {
        ClassifiedKind::Mention | ClassifiedKind::Relative => {
            let path = resolve_decoded_path(&classified.link.path, workspace_root)?;
            Some(WorkspaceFileLink {
                path,
                ..classified.link
            })
        }
        ClassifiedKind::Absolute => Some(
            match resolve_decoded_path(&classified.link.path, workspace_root) {
                Some(path) => WorkspaceFileLink {
                    path,
                    ..classified.link
                },
                None => WorkspaceFileLink {
                    outside: true,
                    ..classified.link
                },
            },
        ),
    }
}

/// Classify a link destination under the file-link grammar — without a root
/// to resolve against yet. Line references split on the raw target so an
/// escaped `#` or `:` stays inside the path; the path itself then decodes
/// exactly once and every safety check runs on the decoded string.
fn classify_file_link(target: &str) -> Option<ClassifiedLink> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }

    // `zeron-file:` mentions keep their strict canonical spelling: the whole
    // path decodes once and must re-encode to the identical string.
    if let Some(encoded) = target.strip_prefix(FILE_MENTION_SCHEME) {
        let decoded = percent_decode_path(encoded)?;
        if percent_encode_path(&decoded) != encoded
            || decoded.ends_with('/')
            || decoded.contains(':')
            || !clean_path(&decoded)
        {
            return None;
        }
        return Some(ClassifiedLink {
            kind: ClassifiedKind::Mention,
            link: WorkspaceFileLink {
                path: decoded,
                line: None,
                column: None,
                outside: false,
            },
        });
    }

    let file_url = target.starts_with("file://");
    let raw = if file_url {
        let rest = &target["file://".len()..];
        // Only an empty host keeps this a file path — `file://localhost/…`
        // and friends are ordinary URLs — and a `?query` is never a file.
        let host_end = rest.find('/').unwrap_or(rest.len());
        if !rest[..host_end].is_empty() || rest.contains('?') {
            return None;
        }
        &rest[host_end..]
    } else {
        if target.contains("://") || target.starts_with("mailto:") {
            return None;
        }
        target
    };

    let (raw, fragment) = split_line_fragment(raw)?;
    let (raw, suffix) = split_line_suffix(raw)?;
    let decoded: Cow<str> = if raw.contains('%') {
        match percent_decode_path(raw) {
            Some(decoded) => Cow::Owned(decoded),
            // Broken escapes keep the raw spelling — a literal `%` in a file
            // name still opens.
            None => Cow::Borrowed(raw),
        }
    } else {
        Cow::Borrowed(raw)
    };
    // A Windows drive path is absolute on its own terms, whichever host
    // reads it (a remote engine may hand one to a POSIX viewport and the
    // other way round — see `shell::is_host_absolute_path`). Normalizing it
    // to the wire's `/` spelling lets the rest of the grammar read it like
    // any other absolute path; a bare `C:` or drive-relative `C:dir` is not
    // one.
    let drive = windows_drive_path(&decoded);
    let decoded: Cow<str> = match &drive {
        Some(path) => Cow::Owned(path.clone()),
        None => decoded,
    };
    if decoded.is_empty() || !clean_path(&decoded) {
        return None;
    }
    let kind = if drive.is_some() || decoded.starts_with('/') {
        // Absolute path — POSIX, or a Windows drive path: one leading slash
        // (not the root itself, not `//`), no trailing slash. A plain
        // absolute path also wants a `.` in its file name (`/usr/bin/ls`
        // stays plain text); `file://` is exempt.
        if decoded.ends_with('/')
            || (drive.is_none() && (decoded.len() == 1 || decoded.starts_with("//")))
            || (!file_url && !file_name(&decoded).contains('.'))
        {
            return None;
        }
        ClassifiedKind::Absolute
    } else {
        if file_url || decoded.starts_with('~') || has_url_scheme(&decoded) {
            return None;
        }
        if !file_name(&decoded).contains('.') {
            return None;
        }
        ClassifiedKind::Relative
    };
    Some(ClassifiedLink {
        kind,
        link: WorkspaceFileLink {
            path: decoded.into_owned(),
            line: fragment.line.or(suffix.line),
            column: fragment.column.or(suffix.column),
            outside: false,
        },
    })
}

/// `target` without its `#L12` fragment or `:12` line suffix: the path a
/// location link points into. A target that is not a location stays whole.
pub(crate) fn without_location(target: &str) -> &str {
    split_line_fragment(target)
        .and_then(|(path, _)| split_line_suffix(path))
        .map_or(target, |(path, _)| path)
}

/// A line reference merged across `#` fragment and `:` suffix: the fragment
/// wins each field it provides.
#[derive(Default)]
struct ParsedAnchor {
    line: Option<u32>,
    column: Option<u32>,
}

/// A `#L12`-style fragment (GitHub line anchors, ranges included, all
/// opening at the first line). A non-empty fragment without `/` that is not
/// one of these drops away, leaving the file path; `0` and reversed ranges
/// make the whole target not a file link.
fn split_line_fragment(target: &str) -> Option<(&str, ParsedAnchor)> {
    let Some((path, fragment)) = target.rsplit_once('#') else {
        return Some((target, ParsedAnchor::default()));
    };
    if fragment.is_empty() {
        return Some((path, ParsedAnchor::default()));
    }
    if fragment.contains('/') {
        // The `#` is inside the path, not an anchor.
        return Some((target, ParsedAnchor::default()));
    }
    match parse_anchor(fragment) {
        LineAnchor::Invalid => None,
        LineAnchor::None => Some((path, ParsedAnchor::default())),
        LineAnchor::Line { line, column } => Some((
            path,
            ParsedAnchor {
                line: Some(line),
                column,
            },
        )),
    }
}

/// `path:12`, `path:12:5` and `path:12-20` line suffixes. As with fragments,
/// a zero line or a backwards range makes the whole target not a file link.
fn split_line_suffix(target: &str) -> Option<(&str, ParsedAnchor)> {
    let Some(colon) = target.rfind(':') else {
        return Some((target, ParsedAnchor::default()));
    };
    let last = &target[colon + 1..];
    if let Some((start, end)) = last.split_once('-') {
        let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>()) else {
            return Some((target, ParsedAnchor::default()));
        };
        return if start == 0 || end < start {
            None
        } else {
            Some((
                &target[..colon],
                ParsedAnchor {
                    line: Some(start),
                    column: None,
                },
            ))
        };
    }
    if last.parse::<u32>().is_ok_and(|number| number == 0) {
        return None;
    }
    let Some(last_number) = positive_number(last) else {
        return Some((target, ParsedAnchor::default()));
    };
    let before = &target[..colon];
    let anchor = match before.rfind(':') {
        Some(colon2) => match positive_number(&before[colon2 + 1..]) {
            Some(line) => (
                &before[..colon2],
                ParsedAnchor {
                    line: Some(line),
                    column: Some(last_number),
                },
            ),
            // `a:0:5` rejects like `a:0`; `a:x:5` keeps `a:x` as the path —
            // the `:` it carries rejects it as a scheme-shaped target anyway.
            None if before[colon2 + 1..].parse::<u32>() == Ok(0) => return None,
            None => (
                before,
                ParsedAnchor {
                    line: Some(last_number),
                    column: None,
                },
            ),
        },
        None => (
            before,
            ParsedAnchor {
                line: Some(last_number),
                column: None,
            },
        ),
    };
    Some(anchor)
}

/// `#L12`, `#L12C5`, `#L12-L20`, `#L12-20` or `#L12C1-L20C3` — the parsed
/// first line plus its column when present.
fn parse_anchor(fragment: &str) -> LineAnchor {
    let Some(rest) = fragment.strip_prefix('L') else {
        return LineAnchor::None;
    };
    let (start_part, end_part) = match rest.split_once('-') {
        Some((start, end)) => (start, Some(end.strip_prefix('L').unwrap_or(end))),
        None => (rest, None),
    };
    let Some((line, column)) = parse_line_column(start_part) else {
        return LineAnchor::None;
    };
    if line == 0 || column == Some(0) {
        return LineAnchor::Invalid;
    }
    if let Some(end_part) = end_part {
        let Some((end, end_column)) = parse_line_column(end_part) else {
            return LineAnchor::None;
        };
        if end < line || end_column == Some(0) {
            return LineAnchor::Invalid;
        }
    }
    LineAnchor::Line { line, column }
}

/// `12` or `12C5` — the line number plus an optional column.
fn parse_line_column(value: &str) -> Option<(u32, Option<u32>)> {
    let (line, column) = match value.split_once('C') {
        Some((line, column)) => (line, Some(column)),
        None => (value, None),
    };
    let line = line.parse::<u32>().ok()?;
    let column = match column {
        Some(column) => Some(column.parse::<u32>().ok()?),
        None => None,
    };
    Some((line, column))
}

/// The resolved link's inside-the-root tail: an absolute decoded path keeps
/// only its workspace-relative remainder, anything else must already be a
/// clean relative path. Returns `None` when the path escapes the root.
fn resolve_decoded_path(target: &str, root: &str) -> Option<String> {
    // A Windows drive path is absolute on its own terms, whichever host reads
    // it — a Windows-backed checkout can be linked from a POSIX viewport. The
    // host's `Path` rules only know its own convention, so a drive target and
    // a drive root compare as normalized wire spellings instead.
    if let Some(target) = windows_drive_path(target) {
        let root = windows_drive_path(root)?;
        let tail = drive_tail(&target, &root)?;
        return safe_relative_path(Path::new(&tail));
    }
    let root = Path::new(root);
    // A remote engine may supply POSIX paths to a Windows viewport. A leading
    // slash has a root on Windows, but is_absolute() also requires a drive;
    // classify the link by its root instead of the viewer's absolute-path rules.
    let relative = if Path::new(target).has_root() {
        Path::new(target).strip_prefix(root).ok()?
    } else {
        Path::new(target)
    };
    safe_relative_path(relative)
}

/// `target`'s tail under `root`, both normalized drive paths (`C:/dir/f.md`).
/// Components compare case-insensitively: Windows paths are, so a drive
/// letter or a directory spelled another way still names the same checkout.
fn drive_tail(target: &str, root: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    let parts: Vec<&str> = target.split('/').collect();
    let root_parts: Vec<&str> = root.split('/').collect();
    if parts.len() <= root_parts.len() {
        return None;
    }
    let owned = parts
        .iter()
        .zip(&root_parts)
        .all(|(part, root_part)| part.eq_ignore_ascii_case(root_part));
    owned.then(|| parts[root_parts.len()..].join("/"))
}

fn safe_relative_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The path-safety checks every classified link passes, on the decoded path:
/// no backslashes, no `?`, no control characters, no `.`/`..`/empty segments.
/// A single leading `/` segment is allowed — it marks an absolute path.
fn clean_path(path: &str) -> bool {
    !path.contains(['\\', '?'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .enumerate()
            .all(|(index, part)| !(part.is_empty() && index != 0) && !matches!(part, "." | ".."))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A Windows drive path — `C:\dir\file.md`, `C:/dir/file.md`, or the RFC 8089
/// `file:///C:/…` spelling — normalized to the wire's `/` separators
/// (`C:/dir/file.md`). `None` for anything else: a bare `C:` is
/// drive-relative, `C:dir` is relative to that drive's own working directory,
/// and a URL or POSIX path has no drive at all.
fn windows_drive_path(path: &str) -> Option<String> {
    // The URI form carries the drive behind the empty host's slash.
    let rest = path.strip_prefix('/').unwrap_or(path);
    let bytes = rest.as_bytes();
    if !(bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':')) {
        return None;
    }
    let tail = rest[2..].strip_prefix(['\\', '/'])?;
    Some(format!("{}:/{}", &rest[..1], tail.replace('\\', "/")))
}

/// A span's path in the link grammar's wire spelling. Windows accepts `\` as
/// a separator, so a Windows span folds to `/` (`src\lib.rs` is `src/lib.rs`
/// there); on POSIX a backslash is an ordinary file-name character and the
/// span keeps it.
fn span_wire_path<'a>(span: &'a str, backslash_separator: bool) -> Cow<'a, str> {
    if backslash_separator && span.contains('\\') {
        Cow::Owned(span.replace('\\', "/"))
    } else {
        Cow::Borrowed(span)
    }
}

/// A probed file's absolute wire spelling for an inline-code link target: a
/// POSIX path as it is, a Windows drive path in the RFC 8089 `/C:/…` form.
/// `None` when the grammar would reject the target (a `//` share reads as a
/// URL host, say), whose span then keeps its inline-code look.
fn inline_code_wire_path(path: &Path) -> Option<String> {
    // A Windows probe path spells separators either way; the wire spelling is
    // always `/`. On POSIX a backslash belongs to the file name, not a path.
    let path = path.to_string_lossy();
    let path = span_wire_path(&path, BACKSLASH_SEPARATOR);
    if let Some(drive) = windows_drive_path(&path) {
        return Some(format!("/{drive}"));
    }
    (!path.starts_with("//") && path.starts_with('/')).then(|| path.into_owned())
}

/// This viewport's home directory: `HOME`, then `USERPROFILE` — the native
/// Windows variable, which a shell-less launch exports instead.
fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|key| std::env::var_os(key).filter(|home| !home.is_empty()))
        .map(PathBuf::from)
}

/// `scheme:` at the start of a relative path means it is a URL, not a file.
fn has_url_scheme(path: &str) -> bool {
    let mut chars = path.chars();
    if !chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
    {
        return false;
    }
    for character in chars {
        match character {
            ':' => return true,
            character
                if character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.') => {}
            _ => return false,
        }
    }
    false
}

fn positive_number(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|number| *number > 0)
}

fn percent_decode_path(encoded: &str) -> Option<String> {
    let raw = encoded.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' {
            let hex = std::str::from_utf8(raw.get(at + 1..at + 3)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            bytes.push(raw[at]);
            at += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(path: &str, line: Option<u32>, column: Option<u32>) -> WorkspaceFileLink {
        WorkspaceFileLink {
            path: path.into(),
            line,
            column,
            outside: false,
        }
    }

    fn outside_link(path: &str) -> WorkspaceFileLink {
        WorkspaceFileLink {
            path: path.into(),
            line: None,
            column: None,
            outside: true,
        }
    }

    fn owned(ix: usize, link: WorkspaceFileLink) -> FileLinkResolution {
        FileLinkResolution::Owned { root: ix, link }
    }

    fn resolution_eq(actual: Option<FileLinkResolution>, expected: FileLinkResolution) -> bool {
        match (actual, expected) {
            (
                Some(FileLinkResolution::Owned { root, link }),
                FileLinkResolution::Owned {
                    root: expected_root,
                    link: expected_link,
                },
            ) => root == expected_root && link == expected_link,
            (Some(FileLinkResolution::Outside(link)), FileLinkResolution::Outside(expected)) => {
                link == expected
            }
            _ => false,
        }
    }

    #[test]
    fn resolution_widens_from_the_linking_root_to_parent_and_projects() {
        let repo = "/home/dev/project";
        let parent = "/home/dev/parent";
        let roots = [repo, parent];
        // A relative link keeps the linking chat's own root (unchanged
        // behaviour); the wider roots are only consulted when it cannot own
        // the target.
        assert!(resolution_eq(
            first_root_owning("src/lib.rs", roots),
            owned(0, link("src/lib.rs", None, None))
        ));
        // An absolute path inside the parent chat's checkout resolves
        // against the parent's root, not the repo the chat sits in.
        assert!(resolution_eq(
            first_root_owning(&format!("{parent}/README.md"), roots),
            owned(1, link("README.md", None, None))
        ));
        // A relative path outside every root never resolves.
        assert!(first_root_owning("../outside.md", roots).is_none());
        assert!(first_root_owning("elsewhere/file", roots).is_none());
        // An absolute path outside every root resolves as an outside link:
        // still a file, opened read-only by the linking chat.
        assert!(resolution_eq(
            first_root_owning("/tmp/elsewhere.md", roots),
            FileLinkResolution::Outside(outside_link("/tmp/elsewhere.md"))
        ));
        assert!(resolution_eq(
            first_root_owning("/tmp/elsewhere.md#L9", roots),
            FileLinkResolution::Outside(WorkspaceFileLink {
                path: "/tmp/elsewhere.md".into(),
                line: Some(9),
                column: None,
                outside: true,
            })
        ));
    }

    #[test]
    fn resolves_relative_absolute_and_location_links() {
        let root = "/work/comet";
        assert_eq!(
            resolve_workspace_file_link("crates/ui/src/lib.rs", root),
            Some(link("crates/ui/src/lib.rs", None, None))
        );
        assert_eq!(
            resolve_workspace_file_link("/work/comet/crates/ui/src/lib.rs:42:7", root),
            Some(link("crates/ui/src/lib.rs", Some(42), Some(7)))
        );
        assert_eq!(
            resolve_workspace_file_link("file:///work/comet/README.md#L12", root),
            Some(link("README.md", Some(12), None))
        );
    }

    #[test]
    fn decodes_relative_and_absolute_paths_once() {
        let root = "/work/comet";
        for (target, path) in [
            (
                "2026-09-26/Some%20Folder/it's%20here.txt",
                "2026-09-26/Some Folder/it's here.txt",
            ),
            (
                "/work/comet/2026-09-26/Some%20Folder/it's%20here.txt",
                "2026-09-26/Some Folder/it's here.txt",
            ),
            ("file:///work/comet/a%20b.md", "a b.md"),
            ("%2Ehidden%2Ffile.rs", ".hidden/file.rs"),
        ] {
            assert_eq!(
                resolve_workspace_file_link(target, root),
                Some(link(path, None, None)),
                "{target}"
            );
        }
        // An escaped slash is just another byte of the decoded path.
        assert_eq!(
            resolve_workspace_file_link("docs%2Fnote.md", root),
            Some(link("docs/note.md", None, None))
        );
        // Outside absolute paths decode too.
        assert_eq!(
            resolve_workspace_file_link("/tmp/Some%20Folder/it's%20here.txt", root),
            Some(WorkspaceFileLink {
                path: "/tmp/Some Folder/it's here.txt".into(),
                line: None,
                column: None,
                outside: true,
            })
        );
        // Broken escapes keep the raw spelling — a file literally named
        // `a%zz.md` still resolves.
        assert_eq!(
            resolve_workspace_file_link("a%zz.md", root),
            Some(link("a%zz.md", None, None))
        );
        assert_eq!(
            resolve_workspace_file_link("a%2.md", root),
            Some(link("a%2.md", None, None))
        );
        // A decode that would produce unsafe content never resolves.
        assert!(resolve_workspace_file_link("%2E%2E/secret.md", root).is_none());
        assert!(resolve_workspace_file_link("a%5Cb.md", root).is_none());
        assert!(resolve_workspace_file_link("a%00b.md", root).is_none());
    }

    #[test]
    fn line_forms_all_open_at_their_first_line() {
        let root = "/work/comet";
        for (target, expected) in [
            ("src/lib.rs#L12", (Some(12), None)),
            ("src/lib.rs#L12C5", (Some(12), Some(5))),
            ("src/lib.rs#L12-L20", (Some(12), None)),
            ("src/lib.rs#L12-20", (Some(12), None)),
            ("src/lib.rs#L12C1-L20C3", (Some(12), Some(1))),
            ("src/lib.rs:12", (Some(12), None)),
            ("src/lib.rs:12:5", (Some(12), Some(5))),
            ("src/lib.rs:12-20", (Some(12), None)),
        ] {
            assert_eq!(
                resolve_workspace_file_link(target, root),
                Some(link("src/lib.rs", expected.0, expected.1)),
                "{target}"
            );
        }
    }

    #[test]
    fn zero_lines_and_reversed_ranges_are_not_links() {
        let root = "/work/comet";
        for target in [
            "src/lib.rs:0",
            "src/lib.rs:0:5",
            "src/lib.rs:12:0",
            "src/lib.rs:0-4",
            "src/lib.rs:4-0",
            "src/lib.rs:20-10",
            "src/lib.rs#L0",
            "src/lib.rs#L0-L3",
            "src/lib.rs#L20-L10",
            "src/lib.rs#L5-3",
            "src/lib.rs#L5-L3",
        ] {
            assert!(
                resolve_workspace_file_link(target, root).is_none(),
                "{target}"
            );
        }
    }

    #[test]
    fn unrecognized_fragments_drop_without_killing_the_link() {
        let root = "/work/comet";
        for (target, path) in [
            ("docs/readme.md#install", "docs/readme.md"),
            ("docs/readme.md#", "docs/readme.md"),
            ("docs/a.md#v1.2", "docs/a.md"),
        ] {
            assert_eq!(
                resolve_workspace_file_link(target, root),
                Some(link(path, None, None)),
                "{target}"
            );
        }
        // A `#` inside the path stays a path character, not an anchor.
        assert_eq!(
            resolve_workspace_file_link("docs/we#ird/file.md", root),
            Some(link("docs/we#ird/file.md", None, None))
        );
    }

    #[test]
    fn absolute_paths_follow_the_strict_shape() {
        let root = "/work/comet";
        // `//`, a bare root and a trailing slash are not file paths.
        for target in [
            "//work/comet/a.md",
            "/",
            "/work/comet/",
            "/work/comet",
            "/other/dir/",
        ] {
            assert!(
                resolve_workspace_file_link(target, root).is_none(),
                "{target}"
            );
        }
        // `/work/comet` ends in no dot — and is also the root itself minus
        // the trailing slash; either way not a link.
        // The dot rule: plain absolute paths need one in the file name.
        assert!(resolve_workspace_file_link("/usr/bin/ls", root).is_none());
        assert!(resolve_workspace_file_link("/usr/local/bin", root).is_none());
        assert!(resolve_workspace_file_link("/.config/env", root).is_none());
        assert_eq!(
            resolve_workspace_file_link("/usr/lib/libc.so", root),
            Some(outside_link("/usr/lib/libc.so"))
        );
        // `file://` paths are exempt from the dot rule.
        assert_eq!(
            resolve_workspace_file_link("file:///usr/bin/ls", root),
            Some(outside_link("/usr/bin/ls"))
        );
        assert_eq!(
            resolve_workspace_file_link("file:///work/comet/src", root),
            Some(link("src", None, None))
        );
        // A leading-dot file name counts for the dot rule too.
        assert_eq!(
            resolve_workspace_file_link("/etc/.env", root),
            Some(outside_link("/etc/.env"))
        );
        // `?` and control characters are never file paths.
        for target in ["/tmp/a?b.md", "/tmp/a\nb.md"] {
            assert!(resolve_workspace_file_link(target, root).is_none());
        }
        assert!(resolve_workspace_file_link("/tmp/%3Fb.md", root).is_none());
    }

    /// The `file://` target an inline code span links carries the line
    /// anchor the author wrote and reopens through the ordinary file-link
    /// resolution, even for a name the dot-shaped grammar would skip.
    #[test]
    fn an_inline_code_target_reopens_through_the_file_link_resolution() {
        let roots = ["/repo dir", "/other"];
        assert!(resolution_eq(
            first_root_owning("file:///repo%20dir/Makefile:12", roots),
            owned(0, link("Makefile", Some(12), None))
        ));
        assert!(resolution_eq(
            first_root_owning(
                "file:///repo%20dir/2026-09-28/Some%20Title/SOURCES.md#L4",
                roots
            ),
            owned(0, link("2026-09-28/Some Title/SOURCES.md", Some(4), None))
        ));
        assert!(matches!(
            first_root_owning("file:///elsewhere/notes.md", roots),
            Some(FileLinkResolution::Outside(link)) if link.path == "/elsewhere/notes.md"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn inline_code_spans_probe_only_paths_that_exist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        std::fs::create_dir_all(root.join("Some Title")).unwrap();
        std::fs::write(root.join("Some Title/Some Title.txt"), "x").unwrap();
        std::fs::write(root.join("Makefile"), "x").unwrap();
        let local = vec![FileLinkRoot {
            chat: Some("chat".into()),
            root: root.to_string_lossy().into_owned(),
            local: true,
        }];
        let mut probes = PathProbes::default();
        let target = |path: &Path| format!("file://{}", path.to_string_lossy().replace(' ', "%20"));
        // Percent escapes decode once, and the line anchor survives.
        assert_eq!(
            resolve_inline_code_path(
                "Some%20Title/Some%20Title.txt#L12",
                &local,
                &[],
                &mut probes
            ),
            Some(InlineCodePath::File(format!(
                "{}:12",
                target(&root.join("Some Title/Some Title.txt"))
            )))
        );
        // A dotless bare name is still a file when one exists.
        assert!(matches!(
            resolve_inline_code_path("Makefile", &local, &[], &mut probes),
            Some(InlineCodePath::File(t)) if t == target(&root.join("Makefile"))
        ));
        // The directory a span names is context for later spans, never a
        // link of its own.
        assert!(matches!(
            resolve_inline_code_path("Some Title/", &local, &[], &mut probes),
            Some(InlineCodePath::Directory(d)) if d == root.join("Some Title")
        ));
        // A bare name under a directory an earlier span named.
        assert!(matches!(
            resolve_inline_code_path(
                "Some Title.txt",
                &local,
                &[root.join("Some Title")],
                &mut probes
            ),
            Some(InlineCodePath::File(t))
                if t == target(&root.join("Some Title/Some Title.txt"))
        ));
        // Shapes with nothing behind them, URLs, and escapes out of the
        // root stay plain code.
        assert!(resolve_inline_code_path("missing.md", &local, &[], &mut probes).is_none());
        assert!(
            resolve_inline_code_path("https://example.com/x.md", &local, &[], &mut probes)
                .is_none()
        );
        assert!(resolve_inline_code_path("../outside.md", &local, &[], &mut probes).is_none());
        // A remote root's checkout is not on this disk.
        let remote = vec![FileLinkRoot {
            chat: Some("chat".into()),
            root: root.to_string_lossy().into_owned(),
            local: false,
        }];
        assert!(resolve_inline_code_path("Makefile", &remote, &[], &mut probes).is_none());
    }

    #[test]
    fn relative_paths_follow_the_strict_shape() {
        let root = "/work/comet";
        for target in [
            "~/notes.md",
            "#section",
            "?query.md",
            "src/Makefile",
            "Makefile",
            "a:b/file.md",
            "https://example.com/file.rs",
            "mailto:dev@example.com",
            "../secret.rs",
            "src/../../secret.rs",
            "src/./lib.rs",
            "src//lib.rs",
        ] {
            assert!(
                resolve_workspace_file_link(target, root).is_none(),
                "{target}"
            );
        }
        // A sibling directory sharing the root's prefix is outside it, so it
        // resolves as a host file rather than into the workspace.
        assert_eq!(
            resolve_workspace_file_link("/work/comet-other/src/lib.rs", root),
            Some(outside_link("/work/comet-other/src/lib.rs"))
        );
        // Leading-dot relative names are files; mid-path dots do not count.
        assert_eq!(
            resolve_workspace_file_link(".env", root),
            Some(link(".env", None, None))
        );
        assert_eq!(
            resolve_workspace_file_link("docs/.env", root),
            Some(link("docs/.env", None, None))
        );
        assert!(resolve_workspace_file_link("docs.v2/readme", root).is_none());
    }

    /// A Windows-backed checkout is linked with drive paths both ways
    /// (`C:\dir\f.md`, `C:/dir/f.md`, `file:///C:/…`), whichever host reads
    /// the link — the wire spelling is always `/`.
    #[test]
    fn windows_drive_paths_are_absolute_file_links() {
        let root = "C:\\Users\\dev\\project";
        for (target, path) in [
            (
                "C:\\Users\\dev\\project\\src\\lib.rs",
                "C:/Users/dev/project/src/lib.rs",
            ),
            (
                "C:/Users/dev/project/src/lib.rs",
                "C:/Users/dev/project/src/lib.rs",
            ),
            (
                "c:\\Users\\dev\\project\\src\\lib.rs",
                "c:/Users/dev/project/src/lib.rs",
            ),
        ] {
            assert_eq!(
                windows_drive_path(target).as_deref(),
                Some(path),
                "{target}"
            );
        }
        // A bare drive and a drive-relative path are not absolute.
        assert!(windows_drive_path("C:").is_none());
        assert!(windows_drive_path("C:dir/file.md").is_none());
        assert!(windows_drive_path("/work/comet/src/lib.rs").is_none());

        // Inside the root: an owned link carrying the workspace-relative tail.
        assert!(resolution_eq(
            first_root_owning("C:\\Users\\dev\\project\\src\\lib.rs", [root]),
            owned(0, link("src/lib.rs", None, None))
        ));
        // The `file:///C:/…` spelling an inline-code link emits reopens the
        // same way, line anchor and all.
        assert!(resolution_eq(
            first_root_owning("file:///C%3A/Users/dev/project/Makefile:12", [root]),
            owned(0, link("Makefile", Some(12), None))
        ));
        // Windows paths are case-insensitive, so a differently spelled drive
        // or directory still lands in the same checkout.
        assert!(resolution_eq(
            first_root_owning("file:///c%3A/users/dev/project/src/lib.rs", [root]),
            owned(0, link("src/lib.rs", None, None))
        ));
        // Anywhere else on the drive: a host file, read-only.
        assert_eq!(
            resolve_workspace_file_link("C:\\Users\\dev\\elsewhere\\INFORME.md", root),
            Some(outside_link("C:/Users/dev/elsewhere/INFORME.md"))
        );
        // A drive path no drive root can own is still outside, never a
        // POSIX-relative link under the root the way a drive-shaped path on
        // a POSIX root would otherwise read.
        assert_eq!(
            resolve_workspace_file_link("C:/dir/file.md", "/work/comet"),
            Some(outside_link("C:/dir/file.md"))
        );
        // The strict shape still applies: no trailing slash, and a dot in the
        // file name unless the target is a `file://` URL.
        assert!(resolve_workspace_file_link("C:\\Users\\dev\\project\\", root).is_none());
        assert!(resolve_workspace_file_link("C:\\Users\\dev\\BASE", root).is_none());
    }

    /// An inline-code span names the probed file, whatever the host spells
    /// separators with: `\` folds on a Windows host, and a drive path takes
    /// the `file:///C:/…` URI form the grammar reopens.
    #[test]
    fn a_windows_inline_code_span_links_its_file() {
        assert_eq!(
            inline_code_wire_path(Path::new("C:\\Users\\dev\\project\\Makefile")),
            Some("/C:/Users/dev/project/Makefile".into())
        );
        assert_eq!(
            inline_code_wire_path(Path::new("/home/dev/project/Makefile")),
            Some("/home/dev/project/Makefile".into())
        );
        // A `//` share would read as a URL host — no target at all.
        assert!(inline_code_wire_path(Path::new("//server/share/a.md")).is_none());
        // Separators fold on a Windows host and stay file-name characters on
        // POSIX, where a backslash is legal in a name.
        assert_eq!(span_wire_path("src\\lib.rs", true), "src/lib.rs");
        assert_eq!(span_wire_path("src\\lib.rs", false), "src\\lib.rs");
        assert_eq!(span_wire_path("src/lib.rs", false), "src/lib.rs");
        // `drive_tail` compares components, so a sibling sharing the root's
        // prefix is never inside it.
        assert_eq!(
            drive_tail("C:/Users/dev/project/src/lib.rs", "C:/Users/dev/project").as_deref(),
            Some("src/lib.rs")
        );
        assert!(drive_tail("C:/Users/dev/project2/src/lib.rs", "C:/Users/dev/project").is_none());
        assert!(drive_tail("C:/Users/dev/project", "C:/Users/dev/project").is_none());
    }

    #[test]
    fn file_scheme_requires_an_empty_host() {
        let root = "/work/comet";
        for target in [
            "file://localhost/work/comet/a.md",
            "file://host/work/comet/a.md",
            "file:///work/comet/a.md?query",
            "file://a.md",
        ] {
            assert!(
                resolve_workspace_file_link(target, root).is_none(),
                "{target}"
            );
        }
        assert_eq!(
            resolve_workspace_file_link("file:///work/comet/a%20b.md", root),
            Some(link("a b.md", None, None))
        );
    }

    #[test]
    fn outside_absolute_paths_resolve_as_host_files() {
        let root = "/work/comet";
        assert_eq!(
            resolve_workspace_file_link("/tmp/elsewhere/INFORME.md", root),
            Some(outside_link("/tmp/elsewhere/INFORME.md"))
        );
        assert_eq!(
            resolve_workspace_file_link("/tmp/elsewhere/INFORME.md:4:2", root),
            Some(WorkspaceFileLink {
                path: "/tmp/elsewhere/INFORME.md".into(),
                line: Some(4),
                column: Some(2),
                outside: true,
            })
        );
    }

    #[test]
    fn resolves_canonical_file_mentions() {
        assert_eq!(
            resolve_workspace_file_link("zeron-file:src/a%20file.rs", "/work/comet"),
            Some(link("src/a file.rs", None, None))
        );
        assert!(resolve_workspace_file_link("zeron-file:src/%61.rs", "/work/comet").is_none());
        assert!(resolve_workspace_file_link("zeron-file:src/", "/work/comet").is_none());
        // Mentions with dotted-less names keep resolving like before.
        assert_eq!(
            resolve_workspace_file_link("zeron-file:Makefile", "/work/comet"),
            Some(link("Makefile", None, None))
        );
        // An absolute mention resolves inside its root or not at all — it is
        // never an outside link.
        assert_eq!(
            resolve_workspace_file_link("zeron-file:/work/comet/a.md", "/work/comet"),
            Some(link("a.md", None, None))
        );
        assert!(resolve_workspace_file_link("zeron-file:/tmp/a.md", "/work/comet").is_none());
    }
}
