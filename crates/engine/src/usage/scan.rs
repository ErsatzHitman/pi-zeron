//! Pi session files to usage records, read incrementally: a file is parsed
//! from where the last scan stopped, and only lines that mention `"usage"`
//! are decoded, through a projection that never keeps message text.
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

/// One request's usage, serialized as an array to keep the index small:
/// dedupe key, epoch ms, index into [`FileState::names`], tokens (input,
/// output, cache read, cache write, reasoning), and the cost Pi recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Record(pub u64, pub i64, pub u32, pub [u64; 5], pub f64);

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
}

/// What one session file has given so far.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FileState {
    pub size: u64,
    pub mtime_ms: i64,
    /// Where the next scan resumes: just past the last complete line.
    pub offset: u64,
    /// Hex of the bytes just before `offset`.
    pub guard: String,
    /// The session header's start, epoch ms. The earliest file is credited
    /// with records a fork copied.
    pub started_ms: i64,
    /// `[provider, model]` pairs the records point into.
    pub names: Vec<[String; 2]>,
    pub records: Vec<Record>,
    /// The file is no longer on disk; its records still count.
    pub gone: bool,
}

impl FileState {
    pub fn name(&self, record: &Record) -> (&str, &str) {
        self.names
            .get(record.2 as usize)
            .map_or(("", ""), |[provider, model]| (provider, model))
    }

    fn intern(&mut self, provider: &str, model: &str) -> u32 {
        if let Some(ix) = self
            .names
            .iter()
            .position(|[p, m]| p == provider && m == model)
        {
            return ix as u32;
        }
        self.names.push([provider.to_owned(), model.to_owned()]);
        (self.names.len() - 1) as u32
    }

    fn read_line(&mut self, line: &[u8], first: bool) {
        if first {
            if let Ok(header) = serde_json::from_slice::<Entry>(line)
                && header.kind == "session"
            {
                self.started_ms = header.timestamp.as_ref().and_then(epoch_ms).unwrap_or(0);
            }
            return;
        }
        if !contains(line, b"\"usage\"") {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Entry>(line) else {
            return;
        };
        if let Some((provider, model, record)) = record(entry) {
            let name = self.intern(&provider, &model);
            self.records
                .push(Record(record.0, record.1, name, record.3, record.4));
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

fn count(usage: &Value, field: &str) -> u64 {
    usage[field]
        .as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(0.0) as u64
}

/// Pi records `cost` as `{input, output, cacheRead, cacheWrite, total}`;
/// old sessions as a bare number.
fn cost(usage: &Value) -> f64 {
    let cost = &usage["cost"];
    let total = cost
        .as_f64()
        .or_else(|| cost["total"].as_f64())
        .unwrap_or_else(|| {
            ["input", "output", "cacheRead", "cacheWrite"]
                .iter()
                .filter_map(|part| cost[*part].as_f64())
                .sum()
        });
    if total.is_finite() && total > 0.0 {
        total
    } else {
        0.0
    }
}

/// The usage an entry carries, under its provider and model, or none.
fn record(entry: Entry) -> Option<(String, String, Record)> {
    let Entry {
        kind,
        id,
        timestamp,
        message,
        usage,
        provider,
        model,
    } = entry;
    let (source, provider, model, usage, ms, response) = match kind.as_str() {
        "message" => {
            let message = message?;
            let role = text(&message.role)?;
            let ms = message.timestamp.as_ref().and_then(epoch_ms);
            match role {
                "assistant" => (
                    "assistant",
                    text(&message.provider).map(str::to_owned),
                    text(&message.response_model)
                        .or_else(|| text(&message.model))
                        .map(str::to_owned),
                    message.usage?,
                    ms,
                    message.response_id,
                ),
                // Tools that call a model themselves report it on their result.
                "toolResult" => (
                    "tool",
                    None,
                    None,
                    message.usage.or(usage)?,
                    ms,
                    message.response_id,
                ),
                _ => return None,
            }
        }
        "usage" => (
            "usage",
            text(&provider).map(str::to_owned),
            text(&model).map(str::to_owned),
            usage?,
            None,
            None,
        ),
        // Their `tokensBefore` is context size, not spend; only `usage` counts.
        "compaction" | "branch_summary" => (
            if kind == "compaction" {
                "compaction"
            } else {
                "branch_summary"
            },
            None,
            None,
            usage?,
            None,
            None,
        ),
        _ => return None,
    };
    let ms = ms.or_else(|| timestamp.as_ref().and_then(epoch_ms))?;
    let provider = provider.unwrap_or_default();
    if TEST_PROVIDERS.contains(&provider.as_str()) {
        return None;
    }
    let model = model.unwrap_or_default();
    let model = model.strip_suffix(":thinking").unwrap_or(&model).to_owned();
    let tokens = [
        count(&usage, "input"),
        count(&usage, "output"),
        count(&usage, "cacheRead"),
        count(&usage, "cacheWrite"),
        count(&usage, "reasoning"),
    ];
    let recorded = cost(&usage);
    // An aborted request spent nothing. Reasoning is part of output, so it
    // alone is no spend.
    if tokens[..4].iter().all(|n| *n == 0) && recorded == 0.0 {
        return None;
    }
    // A fork copies entries verbatim into a new file: the same entry hashes
    // the same there, while distinct entries differ in time.
    let raw = |value: Option<Value>| value.map(|v| v.to_string()).unwrap_or_default();
    let (id, timestamp, response) = (raw(id), raw(timestamp), raw(response));
    let mut key = Fnv::default();
    for part in [
        source,
        provider.as_str(),
        model.as_str(),
        id.as_str(),
        timestamp.as_str(),
        response.as_str(),
        ms.to_string().as_str(),
    ] {
        key.write(part.as_bytes());
    }
    for n in tokens {
        key.write(&n.to_le_bytes());
    }
    Some((
        provider,
        model,
        Record(key.finish(), ms, 0, tokens, recorded),
    ))
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
            if size >= prev.offset
                && guard_at(&mut file, prev.offset).is_ok_and(|g| g == prev.guard) =>
        {
            FileState {
                gone: false,
                ..prev.clone()
            }
        }
        _ => FileState::default(),
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
        let state = scan_file(&path, None).unwrap().unwrap();
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
                (("", ""), 1_790_848_980_000, [40, 20, 0, 0, 0], 0.0),
                (("", ""), 4_000, [3, 1, 0, 0, 0], 0.0),
            ]
        );
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
        let state = scan_file(&path, None).unwrap().unwrap();
        assert_eq!(state.records.len(), 1);
        assert!(scan_file(&path, Some(&state)).unwrap().is_none());

        std::fs::write(
            &path,
            format!("{}\n{first}\n{second}\n", header("2026-10-01T09:00:00Z")),
        )
        .unwrap();
        let resumed = scan_file(&path, Some(&state)).unwrap().unwrap();
        let full = scan_file(&path, None).unwrap().unwrap();
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
        let state = scan_file(&path, None).unwrap().unwrap();
        write(&path, &lines[..2]);
        let shrunk = scan_file(&path, Some(&state)).unwrap().unwrap();
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
        let again = scan_file(&path, Some(&shrunk)).unwrap().unwrap();
        assert_eq!(again.name(&again.records[0]), ("q", "m"));
        assert_eq!(again.records.len(), 1);
    }

    #[test]
    fn fork_copies_hash_alike_and_distinct_entries_do_not() {
        let line = assistant("a1", "p", "m", 1_000, 10, 0.0);
        let key = |line: &str| record(serde_json::from_str(line).unwrap()).unwrap().2.key();
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
}
