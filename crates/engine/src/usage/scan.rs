//! Usage records from Pi's files. Session files and the usage ledger are
//! read incrementally, from where the last scan stopped; session lines are
//! decoded only when they can carry usage, through a projection that never
//! keeps message text. Workflow journals are rewritten whole, so they are
//! read whole.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Bytes before the resume offset kept to tell an append from a rewrite.
const GUARD: u64 = 64;
/// Directory entries visited at most per scan.
const MAX_ENTRIES: usize = 200_000;
/// Providers Pi's own tests use.
const TEST_PROVIDERS: [&str; 2] = ["faux-provider", "fake-provider"];
/// The message a finished subagent job delivers to its parent session.
const SUBAGENT_RESULT: &str = "subagent-result";
/// The advisor's recommendations, delivered after its tool call returned.
const ADVISOR_ADVICE: &str = "pi-advisor-advice";
/// Thinking levels Pi appends to a model reference after a colon.
const THINKING_LEVELS: [&str; 8] = [
    "thinking", "off", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// Where a record came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u8", from = "u8")]
pub(super) enum Source {
    /// An assistant message in a session file.
    #[default]
    Assistant,
    /// A `usage` entry, which Pi writes for a cache warm.
    Usage,
    /// A tool result that reported usage.
    Tool,
    Compaction,
    BranchSummary,
    /// A call in Pi's usage ledger.
    Ledger,
    /// Spend recovered from what a subagent, the advisor or a workflow
    /// logged before the ledger existed.
    Backfill,
}

impl From<Source> for u8 {
    fn from(source: Source) -> u8 {
        source as u8
    }
}

impl From<u8> for Source {
    fn from(n: u8) -> Self {
        match n {
            1 => Source::Usage,
            2 => Source::Tool,
            3 => Source::Compaction,
            4 => Source::BranchSummary,
            5 => Source::Ledger,
            6 => Source::Backfill,
            _ => Source::Assistant,
        }
    }
}

/// One request's usage, serialized as an array to keep the index small:
/// dedupe key, epoch ms, index into [`FileState::names`], tokens (input,
/// output, cache read, cache write, reasoning), the cost Pi recorded, where
/// it came from, and the key a ledger call shares with its session copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Record(
    pub u64,
    pub i64,
    pub u32,
    pub [u64; 5],
    pub f64,
    pub Source,
    pub u64,
);

impl Record {
    pub fn key(&self) -> u64 {
        self.0
    }
    pub fn ms(&self) -> i64 {
        self.1
    }
    pub fn tokens(&self) -> zeron_proto::usage::UsageTokens {
        let [input, output, cache_read, cache_write, reasoning] = self.3;
        zeron_proto::usage::UsageTokens {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
        }
    }
    pub fn recorded(&self) -> f64 {
        self.4
    }
    pub fn source(&self) -> Source {
        self.5
    }
    /// Zero for sources a ledger call never duplicates.
    pub fn match_key(&self) -> u64 {
        self.6
    }
}

/// What kind of file a [`FileState`] reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum FileKind {
    #[default]
    Session,
    /// `<agentDir>/usage-ledger/<yyyy-mm>.jsonl`, appended by every Pi
    /// process that loads the usage-ledger extension.
    Ledger,
    /// A workflow run's journal, `~/.pi/workflows/projects/*/<runId>.json`.
    Journal,
}

/// What one file has given so far.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FileState {
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: i64,
    /// Where the next scan resumes: just past the last complete line.
    pub offset: u64,
    /// Hex of the bytes just before `offset`.
    pub guard: String,
    /// The session header's start, epoch ms. The earliest file is credited
    /// with records a fork copied.
    pub started_ms: i64,
    /// The session header's id; a delegate's starts `dlg-`.
    pub session_id: String,
    /// `[provider, model, surface]` triples the records point into. The
    /// surface is empty where the record's source names it.
    pub names: Vec<[String; 3]>,
    pub records: Vec<Record>,
    /// The latest assistant's name: a summary ran on that model.
    pub last_assistant: Option<u32>,
    /// Ledger: its earliest `start` marker, epoch ms.
    pub ledger_from_ms: Option<i64>,
    /// Ledger: calls a session file counts too, skipped.
    pub kept: u64,
    /// Ledger: problems its writers reported.
    pub warnings: Vec<String>,
    /// The file is no longer on disk; its records still count.
    pub gone: bool,
}

/// A record before it is interned into a file.
struct Found {
    provider: String,
    model: String,
    surface: String,
    record: Record,
}

impl FileState {
    fn new(kind: FileKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }

    pub fn name(&self, record: &Record) -> (&str, &str) {
        self.names
            .get(record.2 as usize)
            .map_or(("", ""), |[provider, model, _]| (provider, model))
    }

    pub fn surface(&self, record: &Record) -> &str {
        self.names
            .get(record.2 as usize)
            .map_or("", |[_, _, surface]| surface)
    }

    fn intern(&mut self, provider: &str, model: &str, surface: &str) -> u32 {
        if let Some(ix) = self
            .names
            .iter()
            .position(|[p, m, s]| p == provider && m == model && s == surface)
        {
            return ix as u32;
        }
        self.names
            .push([provider.to_owned(), model.to_owned(), surface.to_owned()]);
        (self.names.len() - 1) as u32
    }

    fn push(&mut self, found: Found) {
        let Found {
            provider,
            model,
            surface,
            mut record,
        } = found;
        let summary = matches!(record.source(), Source::Compaction | Source::BranchSummary);
        let last = self.last_assistant;
        record.2 = match last {
            // Pi stores a summary's usage without its model.
            Some(name) if summary && provider.is_empty() && model.is_empty() => name,
            _ => self.intern(&provider, &model, &surface),
        };
        if record.source() == Source::Assistant {
            self.last_assistant = Some(record.2);
        }
        self.records.push(record);
    }

    fn read_line(&mut self, line: &[u8], first: bool) {
        match self.kind {
            FileKind::Session => self.read_session_line(line, first),
            FileKind::Ledger => self.read_ledger_line(line),
            FileKind::Journal => {}
        }
    }

    fn read_session_line(&mut self, line: &[u8], first: bool) {
        if first {
            if let Ok(header) = serde_json::from_slice::<Entry>(line)
                && header.kind == "session"
            {
                self.started_ms = header.timestamp.as_ref().and_then(epoch_ms).unwrap_or(0);
                self.session_id = text(&header.id).unwrap_or_default().to_owned();
            }
            return;
        }
        if !contains(line, b"\"usage\"") && !contains(line, SUBAGENT_RESULT.as_bytes()) {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Entry>(line) else {
            return;
        };
        if let Some(found) = record(entry) {
            self.push(found);
        }
    }

    fn read_ledger_line(&mut self, line: &[u8]) {
        let Ok(mut line) = serde_json::from_slice::<LedgerLine>(line) else {
            return;
        };
        let ms = line.ts.as_ref().and_then(epoch_ms);
        match std::mem::take(&mut line.t).as_str() {
            "start" => {
                if let Some(ms) = ms {
                    self.ledger_from_ms = Some(self.ledger_from_ms.map_or(ms, |from| from.min(ms)));
                }
            }
            "warn" => {
                if let Some(what) = text(&line.what)
                    && !self.warnings.iter().any(|known| known == what)
                {
                    self.warnings.push(what.to_owned());
                }
            }
            "call" | "other" if line.kept => self.kept += 1,
            "call" | "other" => {
                if let Some(found) = ms.and_then(|ms| ledger_call(line, ms)) {
                    self.push(found);
                }
            }
            _ => {}
        }
    }
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    timestamp: Option<Value>,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    provider: Option<Value>,
    #[serde(default)]
    model: Option<Value>,
    #[serde(default, rename = "customType")]
    custom_type: Option<Value>,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    details: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    #[serde(default)]
    role: Option<Value>,
    #[serde(default)]
    provider: Option<Value>,
    #[serde(default)]
    model: Option<Value>,
    #[serde(default)]
    response_model: Option<Value>,
    #[serde(default)]
    response_id: Option<Value>,
    #[serde(default)]
    timestamp: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    tool_name: Option<Value>,
    #[serde(default)]
    details: Option<Value>,
}

/// One line of the usage ledger; see `agent/extensions/00-usage-ledger.ts`.
#[derive(Deserialize)]
struct LedgerLine {
    #[serde(default)]
    t: String,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    ts: Option<Value>,
    #[serde(default)]
    provider: Option<Value>,
    #[serde(default)]
    model: Option<Value>,
    #[serde(default)]
    rmodel: Option<Value>,
    #[serde(default)]
    surface: Option<Value>,
    #[serde(default)]
    u: Option<Value>,
    #[serde(default)]
    cost: Option<Value>,
    #[serde(default)]
    kept: bool,
    #[serde(default)]
    what: Option<Value>,
}

fn text(value: &Option<Value>) -> Option<&str> {
    value
        .as_ref()
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Epoch ms from Pi's ISO strings or millisecond numbers.
fn epoch_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_f64().map(|ms| ms as i64).filter(|ms| *ms > 0),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis()),
        _ => None,
    }
}

fn positive(n: Option<f64>) -> f64 {
    n.filter(|n| n.is_finite() && *n > 0.0).unwrap_or(0.0)
}

fn count(usage: &Value, field: &str) -> u64 {
    positive(usage[field].as_f64()) as u64
}

/// Input, output, cache read, cache write and reasoning, under Pi's names.
fn tokens(usage: &Value) -> [u64; 5] {
    ["input", "output", "cacheRead", "cacheWrite", "reasoning"].map(|field| count(usage, field))
}

/// Pi records `cost` as `{input, output, cacheRead, cacheWrite, total}`;
/// old sessions as a bare number.
fn cost(usage: &Value) -> f64 {
    let cost = &usage["cost"];
    positive(Some(
        cost.as_f64()
            .or_else(|| cost["total"].as_f64())
            .unwrap_or_else(|| {
                ["input", "output", "cacheRead", "cacheWrite"]
                    .iter()
                    .filter_map(|part| cost[*part].as_f64())
                    .sum()
            }),
    ))
}

/// A model id without the thinking level Pi may append.
fn bare_model(model: &str) -> &str {
    match model.rsplit_once(':') {
        Some((id, level)) if THINKING_LEVELS.contains(&level) => id,
        _ => model,
    }
}

/// `provider/model` as Pi prints a model reference; a bare id has no
/// provider.
fn split_model(reference: &str) -> (String, String) {
    let reference = bare_model(reference.trim());
    match reference.split_once('/') {
        Some((provider, model)) => (provider.to_owned(), model.to_owned()),
        None => (String::new(), reference.to_owned()),
    }
}

/// What a ledger call and its session copy share: who answered, when, and
/// how many tokens.
fn match_key(provider: &str, model: &str, ms: i64, tokens: &[u64; 5]) -> u64 {
    let mut key = Fnv::default();
    for part in [provider, model, ms.to_string().as_str()] {
        key.write(part.as_bytes());
    }
    for n in &tokens[..4] {
        key.write(&n.to_le_bytes());
    }
    key.finish()
}

/// A record, unless it is a test provider's or spent nothing. `parts`
/// identify it so the same entry copied into a fork hashes the same.
#[allow(clippy::too_many_arguments)]
fn found(
    source: Source,
    provider: &str,
    model: &str,
    surface: &str,
    ms: i64,
    tokens: [u64; 5],
    recorded: f64,
    parts: &[&str],
) -> Option<Found> {
    let provider = provider.trim();
    if TEST_PROVIDERS.contains(&provider) {
        return None;
    }
    let model = bare_model(model.trim());
    // An aborted request spent nothing. Reasoning is part of output, so it
    // alone is no spend.
    if tokens[..4].iter().all(|n| *n == 0) && recorded == 0.0 {
        return None;
    }
    let mut key = Fnv::default();
    for part in parts {
        key.write(part.as_bytes());
    }
    key.write(ms.to_string().as_bytes());
    for n in tokens {
        key.write(&n.to_le_bytes());
    }
    let matched = match source {
        Source::Assistant | Source::Ledger => match_key(provider, model, ms, &tokens),
        _ => 0,
    };
    Some(Found {
        provider: provider.to_owned(),
        model: model.to_owned(),
        surface: surface.to_owned(),
        record: Record(key.finish(), ms, 0, tokens, recorded, source, matched),
    })
}

/// The usage an entry carries, under its provider and model, or none.
fn record(entry: Entry) -> Option<Found> {
    let Entry {
        kind,
        id,
        timestamp,
        message,
        usage,
        provider,
        model,
        custom_type,
        content,
        details,
    } = entry;
    let entry_ms = timestamp.as_ref().and_then(epoch_ms);
    let raw = |value: &Option<Value>| value.as_ref().map(Value::to_string).unwrap_or_default();
    let (source, provider, model, usage, ms, response) = match kind.as_str() {
        "message" => {
            let message = message?;
            let role = text(&message.role)?;
            let ms = message.timestamp.as_ref().and_then(epoch_ms);
            match role {
                "assistant" => (
                    Source::Assistant,
                    text(&message.provider).map(str::to_owned),
                    text(&message.response_model)
                        .or_else(|| text(&message.model))
                        .map(str::to_owned),
                    message.usage?,
                    ms,
                    message.response_id,
                ),
                // Tools that call a model themselves report it on their result.
                "toolResult" => match message.usage.or(usage) {
                    Some(usage) => (Source::Tool, None, None, usage, ms, message.response_id),
                    // A tool that ran its own agent, like the advisor, puts
                    // that agent's usage in its details.
                    None => {
                        let details = message.details?;
                        let usage = details.get("usage").filter(|u| u.is_object())?;
                        let (provider, model) = details["model"]
                            .as_str()
                            .map(split_model)
                            .unwrap_or_default();
                        let tool = text(&message.tool_name).unwrap_or("tool");
                        return found(
                            Source::Backfill,
                            &provider,
                            &model,
                            tool,
                            ms.or(entry_ms)?,
                            tokens(usage),
                            cost(usage),
                            &["tool", tool, &raw(&id), &raw(&timestamp)],
                        );
                    }
                },
                _ => return None,
            }
        }
        "custom_message" if text(&custom_type) == Some(SUBAGENT_RESULT) => {
            return subagent_result(content.as_ref()?, details.as_ref(), entry_ms?);
        }
        // The advisor answers after its tool call has returned, so its agent's
        // usage rides on the message that delivers the advice.
        "custom_message" if text(&custom_type) == Some(ADVISOR_ADVICE) => {
            let details = details?;
            let usage = details.get("usage").filter(|u| u.is_object())?;
            let (provider, model) = details["model"]
                .as_str()
                .map(split_model)
                .unwrap_or_default();
            return found(
                Source::Backfill,
                &provider,
                &model,
                "advisor",
                entry_ms?,
                tokens(usage),
                cost(usage),
                &["advice", &raw(&id), &raw(&timestamp)],
            );
        }
        "usage" => (
            Source::Usage,
            text(&provider).map(str::to_owned),
            text(&model).map(str::to_owned),
            usage?,
            None,
            None,
        ),
        // Their `tokensBefore` is context size, not spend; only `usage` counts.
        "compaction" => (Source::Compaction, None, None, usage?, None, None),
        "branch_summary" => (Source::BranchSummary, None, None, usage?, None, None),
        _ => return None,
    };
    let name = match source {
        Source::Assistant => "assistant",
        Source::Tool => "tool",
        Source::Usage => "usage",
        Source::Compaction => "compaction",
        _ => "branch_summary",
    };
    found(
        source,
        provider.as_deref().unwrap_or_default(),
        model.as_deref().unwrap_or_default(),
        "",
        ms.or(entry_ms)?,
        tokens(&usage),
        cost(&usage),
        &[
            name,
            provider.as_deref().unwrap_or_default(),
            model.as_deref().unwrap_or_default(),
            &raw(&id),
            &raw(&timestamp),
            &raw(&response),
        ],
    )
}

/// A finished subagent job's spend, from the lines its delivery prints:
/// `- Launch model: provider/id` and `- Usage: input N, output N, cache
/// read N, cache write N, cost C, turns N`. The job ran without a session
/// file, so this is the only trace older jobs left.
fn subagent_result(content: &Value, details: Option<&Value>, ms: i64) -> Option<Found> {
    let body = match content {
        Value::String(body) => body.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let (mut usage, mut launch) = (None, None);
    for line in body.lines() {
        let line = line.trim().trim_start_matches("- ");
        if let Some(rest) = line.strip_prefix("Usage: ") {
            usage = Some(rest.trim());
        } else if let Some(rest) = line.strip_prefix("Launch model: ") {
            launch = Some(rest.trim());
        }
    }
    let usage = usage?;
    let field = |name: &str| {
        positive(
            usage
                .split(", ")
                .find_map(|part| part.strip_prefix(name)?.trim().parse::<f64>().ok()),
        )
    };
    let tokens = [
        field("input ") as u64,
        field("output ") as u64,
        field("cache read ") as u64,
        field("cache write ") as u64,
        0,
    ];
    let (provider, model) = launch.map(split_model).unwrap_or_default();
    let detail = |name: &str| {
        details
            .map(|details| match &details[name] {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    found(
        Source::Backfill,
        &provider,
        &model,
        "subagent",
        ms,
        tokens,
        field("cost "),
        &[
            SUBAGENT_RESULT,
            &detail("jobId"),
            &detail("deliveryId"),
            usage,
        ],
    )
}

/// A ledger call. `rmodel` is the model that actually answered.
fn ledger_call(line: LedgerLine, ms: i64) -> Option<Found> {
    let usage = line.u.unwrap_or_default();
    let tokens = ["i", "o", "cr", "cw", "r"].map(|field| count(&usage, field));
    found(
        Source::Ledger,
        text(&line.provider).unwrap_or_default(),
        text(&line.rmodel)
            .or_else(|| text(&line.model))
            .unwrap_or_default(),
        text(&line.surface).unwrap_or("core"),
        ms,
        tokens,
        positive(line.cost.as_ref().and_then(Value::as_f64)),
        &[
            "ledger",
            &line.id.map(|id| id.to_string()).unwrap_or_default(),
        ],
    )
}

/// FNV-1a: stable across builds, unlike std's hasher, since keys persist.
struct Fnv(u64);
impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}
impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes.iter().chain(std::iter::once(&0xff_u8)) {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `GUARD` bytes before `offset`, as hex.
fn guard_at(file: &mut File, offset: u64) -> std::io::Result<String> {
    let start = offset.saturating_sub(GUARD);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (offset - start) as usize];
    file.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

/// The file's state after reading what was appended since `prev`, or none
/// when it has not changed. A file that shrank or was rewritten is read
/// again from the start.
pub(super) fn scan_file(
    path: &Path,
    prev: Option<&FileState>,
    kind: FileKind,
) -> std::io::Result<Option<FileState>> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    let mut file = File::open(path)?;
    let mut state = match prev {
        Some(prev) if prev.size == size && prev.mtime_ms == mtime_ms && !prev.gone => {
            return Ok(None);
        }
        Some(prev)
            if prev.kind == kind
                && size >= prev.offset
                && guard_at(&mut file, prev.offset).is_ok_and(|g| g == prev.guard) =>
        {
            FileState {
                gone: false,
                ..prev.clone()
            }
        }
        _ => FileState::new(kind),
    };
    state.size = size;
    state.mtime_ms = mtime_ms;
    file.seek(SeekFrom::Start(state.offset))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        // A line still being written waits for the next scan.
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        let first = state.offset == 0;
        state.offset += read as u64;
        state.read_line(&line, first);
    }
    let mut file = reader.into_inner();
    state.guard = guard_at(&mut file, state.offset)?;
    Ok(Some(state))
}

/// Every `*.jsonl` under `roots`, without following links.
pub(super) fn session_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = vec![];
    let mut seen = 0;
    let mut dirs: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                tracing::warn!("usage scan stopped after {MAX_ENTRIES} entries");
                return files;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Every run journal under `projects`: `<project>/<runId>.json`. Backups,
/// transcripts and scratch space sit beside them and are left alone.
pub(super) fn journal_files(projects: &Path) -> Vec<PathBuf> {
    let mut files = vec![];
    let Ok(projects) = std::fs::read_dir(projects) else {
        return files;
    };
    for project in projects.flatten() {
        if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_ENTRIES) {
            let path = entry.path();
            let name = entry.file_name();
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && path.extension().is_some_and(|ext| ext == "json")
                && !name.to_string_lossy().starts_with('.')
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Journal {
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    agents: Vec<JournalAgent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalAgent {
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    attempts: Vec<JournalAttempt>,
    #[serde(default)]
    cached: bool,
    #[serde(default)]
    reused_from: Option<Value>,
    #[serde(default)]
    started_at: Option<Value>,
    #[serde(default)]
    ended_at: Option<Value>,
}

#[derive(Deserialize)]
struct JournalAttempt {
    #[serde(default)]
    attempt: u32,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: Option<Value>,
}

/// A workflow journal's spend: each attempt of each agent that actually
/// ran. A replayed or reused result spent nothing this run, and an agent's
/// `usage` is the sum of its attempts, so only one of the two is read.
pub(super) fn scan_journal(
    path: &Path,
    prev: Option<&FileState>,
) -> std::io::Result<Option<FileState>> {
    let meta = std::fs::metadata(path)?;
    let size = meta.len();
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    if prev.is_some_and(|prev| prev.size == size && prev.mtime_ms == mtime_ms && !prev.gone) {
        return Ok(None);
    }
    let mut state = FileState {
        size,
        mtime_ms,
        offset: size,
        ..FileState::new(FileKind::Journal)
    };
    let Ok(journal) = serde_json::from_slice::<Journal>(&std::fs::read(path)?) else {
        return Ok(Some(state));
    };
    for agent in journal.agents {
        if agent.cached || agent.reused_from.is_some() {
            continue;
        }
        let Some(ms) = agent
            .ended_at
            .as_ref()
            .or(agent.started_at.as_ref())
            .and_then(epoch_ms)
        else {
            continue;
        };
        let attempts: Vec<(u32, &str, &Value)> = if agent.attempts.is_empty() {
            agent
                .usage
                .iter()
                .map(|usage| (1, agent.model.as_str(), usage))
                .collect()
        } else {
            agent
                .attempts
                .iter()
                .filter_map(|a| Some((a.attempt, a.model.as_str(), a.usage.as_ref()?)))
                .collect()
        };
        for (attempt, model, usage) in attempts {
            let (provider, model) = split_model(model);
            let attempt = attempt.to_string();
            if let Some(found) = found(
                Source::Backfill,
                &provider,
                &model,
                "workflow",
                ms,
                tokens(usage),
                cost(usage),
                &["workflow", &journal.run_id, &agent.call_id, &attempt],
            ) {
                state.push(found);
            }
        }
    }
    Ok(Some(state))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    pub fn header(ts: &str) -> String {
        json!({"type":"session","version":3,"id":"s1","timestamp":ts,"cwd":"/w"}).to_string()
    }

    pub fn assistant(
        id: &str,
        provider: &str,
        model: &str,
        ms: i64,
        input: u64,
        cost: f64,
    ) -> String {
        json!({
            "type":"message","id":id,"parentId":null,"timestamp":"2026-10-01T10:00:00.000Z",
            "message":{
                "role":"assistant","provider":provider,"model":model,"timestamp":ms,
                "content":[{"type":"text","text":"secret answer text"}],
                "usage":{"input":input,"output":10,"cacheRead":100,"cacheWrite":5,"reasoning":4,
                    "totalTokens":input+115,
                    "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":cost}}
            }
        })
        .to_string()
    }

    fn write(path: &Path, lines: &[String]) {
        let mut file = File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    #[test]
    fn every_kind_of_entry_counts_and_message_text_is_never_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        write(
            &path,
            &[
                header("2026-10-01T09:00:00.000Z"),
                json!({"type":"message","id":"u1","timestamp":"2026-10-01T09:59:00.000Z",
                    "message":{"role":"user","content":"secret question usage"}})
                .to_string(),
                assistant("a1", "anthropic", "claude-opus-5-5", 1_000, 50, 0.25),
                // Old Pi stores cost as a number; responseModel wins.
                json!({"type":"message","id":"a2","timestamp":"2026-10-01T10:01:00.000Z",
                    "message":{"role":"assistant","provider":"openrouter","model":"auto",
                        "responseModel":"deepseek/deepseek-v4.1-flash:thinking","timestamp":2_000,
                        "usage":{"input":7,"output":3,"cost":0.5}}})
                .to_string(),
                json!({"type":"usage","id":"w1","timestamp":"2026-10-01T10:02:00.000Z",
                    "kind":"cache_warm","provider":"anthropic","model":"claude-opus-5-5",
                    "usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":900}})
                .to_string(),
                json!({"type":"compaction","id":"c1","timestamp":"2026-10-01T10:03:00.000Z",
                    "tokensBefore":180_000,"summary":"x","usage":{"input":40,"output":20}})
                .to_string(),
                json!({"type":"message","id":"t1","timestamp":"2026-10-01T10:04:00.000Z",
                    "message":{"role":"toolResult","timestamp":4_000,"usage":{"input":3,"output":1}}})
                .to_string(),
                // Aborted and test-provider requests spend nothing real.
                assistant("a3", "anthropic", "claude-opus-5-5", 5_000, 0, 0.0)
                    .replace("\"output\":10", "\"output\":0")
                    .replace("\"cacheRead\":100", "\"cacheRead\":0")
                    .replace("\"cacheWrite\":5", "\"cacheWrite\":0"),
                assistant("a4", "faux-provider", "m", 6_000, 10, 0.0),
            ],
        );
        let state = scan_file(&path, None, FileKind::Session).unwrap().unwrap();
        assert_eq!(state.started_ms, 1_790_845_200_000);
        let rows: Vec<_> = state
            .records
            .iter()
            .map(|r| (state.name(r), r.ms(), r.3, r.recorded()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    ("anthropic", "claude-opus-5-5"),
                    1_000,
                    [50, 10, 100, 5, 4],
                    0.25
                ),
                (
                    ("openrouter", "deepseek/deepseek-v4.1-flash"),
                    2_000,
                    [7, 3, 0, 0, 0],
                    0.5
                ),
                (
                    ("anthropic", "claude-opus-5-5"),
                    1_790_848_920_000,
                    [0, 0, 0, 900, 0],
                    0.0
                ),
                // A summary ran on the model the session was using.
                (
                    ("openrouter", "deepseek/deepseek-v4.1-flash"),
                    1_790_848_980_000,
                    [40, 20, 0, 0, 0],
                    0.0
                ),
                (("", ""), 4_000, [3, 1, 0, 0, 0], 0.0),
            ]
        );
        let sources: Vec<_> = state.records.iter().map(Record::source).collect();
        assert_eq!(
            sources,
            [
                Source::Assistant,
                Source::Assistant,
                Source::Usage,
                Source::Compaction,
                Source::Tool
            ]
        );
        // Only an assistant can be matched by a ledger call.
        assert_ne!(state.records[0].match_key(), 0);
        assert_eq!(state.records[3].match_key(), 0);
        let index = serde_json::to_string(&state).unwrap();
        assert!(!index.contains("secret"), "{index}");
    }

    #[test]
    fn a_torn_tail_waits_and_appends_resume_where_the_last_scan_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let first = assistant("a1", "acme-llm", "x-1", 1_000, 10, 0.0);
        let second = assistant("a2", "acme-llm", "x-1", 2_000, 20, 0.0);
        std::fs::write(
            &path,
            format!(
                "{}\n{first}\n{}",
                header("2026-10-01T09:00:00Z"),
                &second[..40]
            ),
        )
        .unwrap();
        let state = scan_file(&path, None, FileKind::Session).unwrap().unwrap();
        assert_eq!(state.records.len(), 1);
        assert!(
            scan_file(&path, Some(&state), FileKind::Session)
                .unwrap()
                .is_none()
        );

        std::fs::write(
            &path,
            format!("{}\n{first}\n{second}\n", header("2026-10-01T09:00:00Z")),
        )
        .unwrap();
        let resumed = scan_file(&path, Some(&state), FileKind::Session)
            .unwrap()
            .unwrap();
        let full = scan_file(&path, None, FileKind::Session).unwrap().unwrap();
        assert_eq!(resumed, full);
        assert_eq!(full.records.len(), 2);
        assert_eq!(full.name(&full.records[1]), ("acme-llm", "x-1"));
    }

    #[test]
    fn a_rewritten_or_shrunk_file_is_read_again_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let lines = [
            header("2026-10-01T09:00:00Z"),
            assistant("a1", "p", "m", 1_000, 10, 0.0),
            assistant("a2", "p", "m", 2_000, 20, 0.0),
        ];
        write(&path, &lines);
        let state = scan_file(&path, None, FileKind::Session).unwrap().unwrap();
        write(&path, &lines[..2]);
        let shrunk = scan_file(&path, Some(&state), FileKind::Session)
            .unwrap()
            .unwrap();
        assert_eq!(shrunk.records.len(), 1);
        // Same length, different bytes before the offset.
        let rewritten = [
            lines[0].clone(),
            assistant("b1", "q", "m", 1_000, 10, 0.5).replace("T10:00", "T11:00"),
        ];
        write(&path, &rewritten);
        // Windows can stamp both writes with the same mtime.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        let again = scan_file(&path, Some(&shrunk), FileKind::Session)
            .unwrap()
            .unwrap();
        assert_eq!(again.name(&again.records[0]), ("q", "m"));
        assert_eq!(again.records.len(), 1);
    }

    #[test]
    fn fork_copies_hash_alike_and_distinct_entries_do_not() {
        let line = assistant("a1", "p", "m", 1_000, 10, 0.0);
        let key = |line: &str| {
            record(serde_json::from_str(line).unwrap())
                .unwrap()
                .record
                .key()
        };
        assert_eq!(key(&line), key(&line.clone()));
        // The same 8-hex id in another lineage, at another time.
        let other = line.replace("2026-10-01T10:00:00.000Z", "2026-10-02T10:00:00.000Z");
        assert_ne!(key(&line), key(&other));
    }

    #[test]
    fn session_files_are_found_under_every_root() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("sessions/--w--");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("1_a.jsonl"), "").unwrap();
        std::fs::write(nested.join("notes.txt"), "").unwrap();
        let other = dir.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("b.jsonl"), "").unwrap();
        let files = session_files(&[dir.path().join("sessions"), other, dir.path().join("none")]);
        assert_eq!(files.len(), 2, "{files:?}");
    }

    pub fn ledger_call(
        id: &str,
        surface: &str,
        provider: &str,
        model: &str,
        ms: i64,
        input: u64,
    ) -> String {
        json!({"v":1,"t":"call","id":id,"ts":ms,"provider":provider,"model":model,"rmodel":null,
            "api":"x","stop":"stop","u":{"i":input,"o":10,"cr":100,"cw":5,"r":4},"cost":0.5,
            "surface":surface,"sid":"s1","depth":1,"tag":null,"kept":false})
        .to_string()
    }

    fn row(state: &FileState, record: &Record) -> (String, String, String, [u64; 5], f64) {
        let (provider, model) = state.name(record);
        (
            provider.to_owned(),
            model.to_owned(),
            state.surface(record).to_owned(),
            record.3,
            record.recorded(),
        )
    }

    #[test]
    fn subagent_deliveries_and_advisor_details_are_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        write(
            &path,
            &[
                header("2026-10-01T09:00:00Z"),
                json!({"type":"custom_message","customType":"subagent-result","id":"r1",
                    "timestamp":"2026-10-01T10:00:00.000Z","display":true,
                    "content":"Job job-1 done\n- Launch model: opencode-go/deepseek-v4-flash:high\n- Usage: input 35392, output 481, cache read 8192, cache write 0, cost 0.0123, turns 4\nsecret result text",
                    "details":{"jobId":"job-1","state":"done","operation":"start"}})
                .to_string(),
                // Cancelled jobs print no usage line.
                json!({"type":"custom_message","customType":"subagent-result","id":"r2",
                    "timestamp":"2026-10-01T10:01:00.000Z",
                    "content":[{"type":"text","text":"Job job-2 cancelled"}],
                    "details":{"jobId":"job-2"}})
                .to_string(),
                json!({"type":"custom_message","customType":"note","id":"n1",
                    "timestamp":"2026-10-01T10:02:00.000Z","content":"- Usage: input 9, output 9"})
                .to_string(),
                json!({"type":"message","id":"t1","timestamp":"2026-10-01T10:03:00.000Z",
                    "message":{"role":"toolResult","toolName":"advisor","timestamp":3_000,
                        "content":[{"type":"text","text":"secret advice"}],
                        "details":{"model":"anthropic/claude-opus-5","usage":
                            {"input":120,"output":30,"total":150,"cost":0.02,"cacheRead":0,"cacheWrite":0}}}})
                .to_string(),
                // A failed consult spent nothing.
                json!({"type":"message","id":"t2","timestamp":"2026-10-01T10:04:00.000Z",
                    "message":{"role":"toolResult","toolName":"advisor","timestamp":4_000,
                        "details":{"model":"gpt-5.6-sol","usage":
                            {"input":0,"output":0,"total":0,"cost":0,"cacheRead":0,"cacheWrite":0}}}})
                .to_string(),
                // Advice that arrived after the tool call returned.
                json!({"type":"custom_message","customType":"pi-advisor-advice","id":"a1",
                    "timestamp":"2026-10-01T10:05:00.000Z","display":true,
                    "content":"secret recommendations",
                    "details":{"model":"openai-codex/gpt-5.6-terra","usage":
                        {"input":200,"output":40,"total":240,"cost":0.03,"cacheRead":0,"cacheWrite":0}}})
                .to_string(),
            ],
        );
        let state = scan_file(&path, None, FileKind::Session).unwrap().unwrap();
        let rows: Vec<_> = state.records.iter().map(|r| row(&state, r)).collect();
        assert_eq!(
            rows,
            vec![
                (
                    "opencode-go".to_owned(),
                    "deepseek-v4-flash".to_owned(),
                    "subagent".to_owned(),
                    [35_392, 481, 8_192, 0, 0],
                    0.0123
                ),
                (
                    "anthropic".to_owned(),
                    "claude-opus-5".to_owned(),
                    "advisor".to_owned(),
                    [120, 30, 0, 0, 0],
                    0.02
                ),
                (
                    "openai-codex".to_owned(),
                    "gpt-5.6-terra".to_owned(),
                    "advisor".to_owned(),
                    [200, 40, 0, 0, 0],
                    0.03
                ),
            ]
        );
        assert!(state.records.iter().all(|r| r.source() == Source::Backfill));
        let index = serde_json::to_string(&state).unwrap();
        assert!(!index.contains("secret"), "{index}");
    }

    #[test]
    fn ledger_calls_keep_their_surface_and_match_their_session_copy() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        write(
            &session,
            &[
                header("2026-10-01T09:00:00Z"),
                assistant("a1", "anthropic", "claude-opus-5-5", 1_000, 10, 0.5),
            ],
        );
        let ledger = dir.path().join("2026-10.jsonl");
        let lines = [
            json!({"v":1,"t":"start","ts":900,"pid":1,"sid":"s1","depth":0,"surface":"main","pi":"1.2.0"})
                .to_string(),
            ledger_call("1-a-1", "main", "anthropic", "claude-opus-5-5", 1_000, 10),
            json!({"v":1,"t":"warn","ts":1_100,"what":"stream-hook-blind"}).to_string(),
            json!({"v":1,"t":"warn","ts":1_200,"what":"stream-hook-blind"}).to_string(),
            // Pi wrote this summary's usage into the session too.
            ledger_call("1-a-2", "compaction", "anthropic", "claude-opus-5-5", 1_300, 40)
                .replace("\"kept\":false", "\"kept\":true"),
            json!({"v":1,"t":"call","id":"2-b-1","ts":1_400,"provider":"openrouter","model":"x:thinking",
                "rmodel":null,"u":{"i":7,"o":2},"cost":null,"surface":"jev","est":true})
            .to_string(),
            json!({"v":1,"t":"call","id":"2-b-2","ts":1_500,"provider":"openrouter","model":"auto",
                "rmodel":"deepseek/v4","u":{"i":3,"o":1},"cost":0.1})
            .to_string(),
        ];
        // A line still being written waits.
        std::fs::write(&ledger, lines.join("\n") + "\n{\"v\":1,\"t\":\"ca").unwrap();
        let state = scan_file(&ledger, None, FileKind::Ledger).unwrap().unwrap();
        assert_eq!(state.ledger_from_ms, Some(900));
        assert_eq!(state.warnings, ["stream-hook-blind"]);
        assert_eq!(state.kept, 1);
        let rows: Vec<_> = state.records.iter().map(|r| row(&state, r)).collect();
        assert_eq!(
            rows,
            vec![
                (
                    "anthropic".to_owned(),
                    "claude-opus-5-5".to_owned(),
                    "main".to_owned(),
                    [10, 10, 100, 5, 4],
                    0.5
                ),
                (
                    "openrouter".to_owned(),
                    "x".to_owned(),
                    "jev".to_owned(),
                    [7, 2, 0, 0, 0],
                    0.0
                ),
                (
                    "openrouter".to_owned(),
                    "deepseek/v4".to_owned(),
                    "core".to_owned(),
                    [3, 1, 0, 0, 0],
                    0.1
                ),
            ]
        );
        let copy = scan_file(&session, None, FileKind::Session)
            .unwrap()
            .unwrap();
        assert_eq!(copy.records[0].match_key(), state.records[0].match_key());
        assert_ne!(copy.records[0].key(), state.records[0].key());
        assert_ne!(state.records[1].match_key(), state.records[0].match_key());
    }

    #[test]
    fn a_journal_counts_each_attempt_of_agents_that_ran() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wf_1.json");
        let usage = |input: u64, cost: f64| json!({"input":input,"output":5,"total":input+5,"cost":cost,"cacheRead":0,"cacheWrite":0});
        std::fs::write(
            &path,
            json!({"runId":"wf_1","workflowName":"w","script":"secret script","agents":[
                {"callId":"wf_1:0","model":"anthropic/claude-sonnet-5:high",
                    "startedAt":"2026-10-01T10:00:00.000Z","endedAt":"2026-10-01T10:05:00.000Z",
                    "usage":usage(30, 0.3),"prompt":"secret prompt",
                    "attempts":[
                        {"attempt":1,"model":"openai/gpt-5.6","usage":usage(10, 0.1)},
                        {"attempt":2,"model":"anthropic/claude-sonnet-5:high","usage":usage(20, 0.2)}]},
                {"callId":"wf_1:1","model":"openai/gpt-5.6","startedAt":"2026-10-01T10:06:00.000Z",
                    "usage":usage(40, 0.4)},
                {"callId":"wf_1:2","model":"openai/gpt-5.6","startedAt":"2026-10-01T10:07:00.000Z",
                    "usage":usage(50, 0.5),"cached":true},
                {"callId":"wf_1:3","model":"openai/gpt-5.6","startedAt":"2026-10-01T10:08:00.000Z",
                    "usage":usage(60, 0.6),"reusedFrom":2}
            ]})
            .to_string(),
        )
        .unwrap();
        let state = scan_journal(&path, None).unwrap().unwrap();
        let rows: Vec<_> = state
            .records
            .iter()
            .map(|r| {
                let (provider, model) = state.name(r);
                (provider, model, state.surface(r), r.3[0], r.recorded())
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("openai", "gpt-5.6", "workflow", 10, 0.1),
                ("anthropic", "claude-sonnet-5", "workflow", 20, 0.2),
                ("openai", "gpt-5.6", "workflow", 40, 0.4),
            ]
        );
        assert_eq!(state.records[0].ms(), 1_790_849_100_000);
        assert!(scan_journal(&path, Some(&state)).unwrap().is_none());
        let index = serde_json::to_string(&state).unwrap();
        assert!(!index.contains("secret"), "{index}");
    }

    #[test]
    fn journals_are_found_beside_their_backups_and_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("w-1a2b");
        std::fs::create_dir_all(project.join("wf_1.history")).unwrap();
        for name in ["wf_1.json", "wf_1.json.bak", ".scratch.json", "wf_1.log"] {
            std::fs::write(project.join(name), "{}").unwrap();
        }
        std::fs::write(project.join("wf_1.history/0.json"), "{}").unwrap();
        std::fs::write(dir.path().join("stray.json"), "{}").unwrap();
        assert_eq!(journal_files(dir.path()), [project.join("wf_1.json")]);
        assert!(journal_files(&dir.path().join("none")).is_empty());
    }
}
