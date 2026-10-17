//! Per-session on-disk event journal (port of zeron's `run-journal.ts`, JSONL-shaped).
//!
//! One append-only JSONL file per chat under `{data_dir}/journals/{chat_id}.jsonl`; each
//! line is `{"seq": n, "event": AgentEvent}` with a monotonically increasing `seq`. The
//! journal is the durable replay source for live streams (`Subscribe` = replay then tail
//! the broadcast hub) and the crash-recovery gauge: a journal whose LAST event is not
//! `Done` belongs to a run that died mid-stream — boot recovery stamps its doc entry
//! `aborted` and closes the journal with a synthetic `Done`.
//!
//! Compaction ([`RunJournal::compact`], driven by the sessions idle sweep) drops the
//! text/reasoning deltas of turns that already ended in a `Done` — the doc entry was
//! finalized there, so the deltas are redundant — and keeps every other event, the tail
//! after the last `Done`, and every original `seq`. A torn trailing line from a crash
//! mid-write is tolerated everywhere.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use zeron_proto::AgentEvent;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalLine {
    seq: u64,
    event: AgentEvent,
}

struct ChatJournal {
    file: File,
    next_seq: u64,
    /// True when the file ends without a newline (torn write) — the next append
    /// starts with one so the torn line stays isolated.
    needs_newline: bool,
}

/// What one [`RunJournal::compact`] pass reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactStats {
    pub before_bytes: u64,
    pub after_bytes: u64,
    /// Events removed from the file.
    pub dropped: usize,
}

/// Prefix/suffix of the temp files compaction writes beside a journal. The name does
/// not end in `.jsonl`, so journal scans never mistake one for a chat.
const COMPACT_TMP_PREFIX: &str = ".compact-";
const COMPACT_TMP_SUFFIX: &str = ".tmp";

/// Append-only JSONL journal store, one file per chat.
pub struct RunJournal {
    dir: PathBuf,
    open_files: Mutex<HashMap<String, ChatJournal>>,
}

impl RunJournal {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, JournalError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        // A crash mid-compaction leaves its temp file behind; the journal itself is
        // intact (the rename never happened).
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(COMPACT_TMP_PREFIX) && name.ends_with(COMPACT_TMP_SUFFIX) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(Self {
            dir,
            open_files: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, ChatJournal>> {
        self.open_files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn path_for(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.jsonl", sanitize_id(chat_id)))
    }

    fn attempts_path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{}.resume", sanitize_id(chat_id)))
    }

    /// Auto-resume revival budget (zeron `resumeAttempt`/`MAX_AUTO_RESUME`):
    /// persisted beside the journal so a run that CRASHES THE ENGINE cannot
    /// revive itself in an infinite boot loop.
    pub fn resume_attempts(&self, chat_id: &str) -> u32 {
        std::fs::read_to_string(self.attempts_path(chat_id))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn note_resume_attempt(&self, chat_id: &str) -> u32 {
        let next = self.resume_attempts(chat_id) + 1;
        if let Err(err) = std::fs::write(self.attempts_path(chat_id), next.to_string()) {
            tracing::warn!(chat = %chat_id, error = %err, "resume-attempt ledger write failed");
        }
        next
    }

    /// A cleanly completed turn resets the budget — only consecutive
    /// crash-revive-crash cycles exhaust it.
    pub fn clear_resume_attempts(&self, chat_id: &str) {
        let _ = std::fs::remove_file(self.attempts_path(chat_id));
    }

    /// Append one event; returns its journal seq.
    pub fn append(&self, chat_id: &str, event: &AgentEvent) -> Result<u64, JournalError> {
        let mut files = self.lock();
        if !files.contains_key(chat_id) {
            // Bound the open-fd set: entries were never removed, so every chat
            // ever run held a descriptor for the process lifetime. Dropping is
            // safe — the next append reopens and rescans the tail. The cap
            // comfortably exceeds concurrent runs, so eviction stays rare.
            const OPEN_FILE_CAP: usize = 16;
            if files.len() >= OPEN_FILE_CAP {
                files.clear();
            }
            let path = self.path_for(chat_id);
            let (next_seq, needs_newline) = scan_tail(&path)?;
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            files.insert(
                chat_id.to_string(),
                ChatJournal {
                    file,
                    next_seq,
                    needs_newline,
                },
            );
        }
        // Entry guaranteed present; avoid unwrap in a library path regardless.
        let Some(journal) = files.get_mut(chat_id) else {
            return Err(JournalError::Io(std::io::Error::other(
                "journal entry vanished under lock",
            )));
        };
        let seq = journal.next_seq;
        let line = serde_json::to_string(&JournalLine {
            seq,
            event: event.clone(),
        })?;
        let mut buf = Vec::with_capacity(line.len() + 2);
        if journal.needs_newline {
            buf.push(b'\n');
        }
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        journal.file.write_all(&buf)?;
        journal.file.flush()?;
        journal.needs_newline = false;
        journal.next_seq = seq + 1;
        Ok(seq)
    }

    /// Events with `seq > after_seq`, in order. A cursor ahead of the last issued seq is
    /// from a previous era (file replaced) — falls back to a full replay, mirroring zeron.
    /// After [`Self::compact`] a replay below the compaction boundary no longer yields the
    /// text/reasoning deltas of completed turns; transcripts come from the chat doc.
    pub fn replay(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
        let path = self.path_for(chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let all = read_lines(&path)?;
        let last_seq = all.last().map(|(seq, _)| *seq).unwrap_or(0);
        let from = if after_seq > last_seq { 0 } else { after_seq };
        Ok(all.into_iter().filter(|(seq, _)| *seq > from).collect())
    }

    /// The last event in a chat's journal, if any (ignores a torn tail line).
    pub fn last_event(&self, chat_id: &str) -> Result<Option<(u64, AgentEvent)>, JournalError> {
        last_valid_line(&self.path_for(chat_id))
    }

    /// Valid events newest-first, read backwards from the end of the file — for
    /// callers that want the most recent match and can stop early instead of
    /// materialising the whole journal like `replay` does.
    pub fn events_rev(
        &self,
        chat_id: &str,
    ) -> Result<impl Iterator<Item = Result<(u64, AgentEvent), JournalError>>, JournalError> {
        RevEvents::open(&self.path_for(chat_id))
    }

    /// Crash-recovery scan: chat ids whose journal's last event is NOT a `Done` — their
    /// runs died mid-stream and need recovery (stamp `aborted`, close the journal).
    pub fn stale_sessions(&self) -> Result<Vec<String>, JournalError> {
        let mut stale = Vec::new();
        for (chat_id, path) in self.journal_files()? {
            // Tail read only: journals reach tens of MB and boot scans every one.
            match last_valid_line(&path)? {
                Some((_, AgentEvent::Done { .. })) | None => {}
                Some(_) => stale.push(chat_id),
            }
        }
        stale.sort();
        Ok(stale)
    }

    /// `(chat id stem, path)` of every `*.jsonl` journal file.
    fn journal_files(&self) -> Result<Vec<(String, PathBuf)>, JournalError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(chat_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            out.push((chat_id.to_string(), path));
        }
        Ok(out)
    }

    /// Every chat id that has a journal file, sorted.
    pub fn journaled_chats(&self) -> Vec<String> {
        let mut chats: Vec<String> = self
            .journal_files()
            .map(|files| files.into_iter().map(|(id, _)| id).collect())
            .unwrap_or_default();
        chats.sort();
        chats
    }

    /// Size of a chat's journal file (0 when absent).
    pub fn size_bytes(&self, chat_id: &str) -> u64 {
        std::fs::metadata(self.path_for(chat_id)).map_or(0, |m| m.len())
    }

    /// Combined size of every journal file.
    pub fn total_bytes(&self) -> u64 {
        self.journal_files()
            .map(|files| {
                files
                    .iter()
                    .filter_map(|(_, p)| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// How long since the chat's journal was last written; `None` when absent.
    pub fn idle_for(&self, chat_id: &str) -> Option<Duration> {
        let modified = std::fs::metadata(self.path_for(chat_id))
            .ok()?
            .modified()
            .ok()?;
        // A clock that moved backwards reads as "just written".
        Some(modified.elapsed().unwrap_or_default())
    }

    /// Drop the text/reasoning deltas of turns that already ended in a `Done` (the doc
    /// entry was finalized there), keeping every other event, the tail after the last
    /// `Done`, and all original `seq` numbers. `Ok(None)` when the file is smaller than
    /// `min_bytes`, has no `Done`, or has nothing to drop.
    ///
    /// The caller guarantees the chat has no live run. The rewrite goes to a temp file
    /// in the same directory and is renamed over the journal; the open-fd lock is held
    /// only to sample the length and for the final catch-up + rename, so concurrent
    /// appends are copied verbatim and never land in the replaced inode.
    pub fn compact(
        &self,
        chat_id: &str,
        min_bytes: u64,
    ) -> Result<Option<CompactStats>, JournalError> {
        let path = self.path_for(chat_id);
        let len0 = {
            let mut files = self.lock();
            let len0 = match std::fs::metadata(&path) {
                Ok(m) => m.len(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            // Release the fd so none stays open on the inode we are about to replace.
            files.remove(chat_id);
            len0
        };
        if len0 == 0 || len0 < min_bytes {
            return Ok(None);
        }
        let Some(boundary) = compaction_boundary(&path)? else {
            return Ok(None);
        };

        // Phase 1 (unlocked): filtered copy of the first `len0` bytes.
        let mut tmp = tempfile::Builder::new()
            .prefix(COMPACT_TMP_PREFIX)
            .suffix(COMPACT_TMP_SUFFIX)
            .tempfile_in(&self.dir)?;
        let mut consumed = 0u64;
        let mut dropped = 0usize;
        {
            let mut reader = BufReader::new(File::open(&path)?.take(len0));
            let mut out = std::io::BufWriter::new(tmp.as_file_mut());
            let mut raw = Vec::new();
            loop {
                raw.clear();
                if reader.read_until(b'\n', &mut raw)? == 0 {
                    break;
                }
                // An unterminated final line is a torn tail: the catch-up copies it.
                if raw.last() != Some(&b'\n') {
                    break;
                }
                if is_covered_delta(&path, &raw, boundary) {
                    dropped += 1;
                } else {
                    out.write_all(&raw)?;
                }
                consumed += raw.len() as u64;
            }
            out.flush()?;
        }
        if dropped == 0 {
            return Ok(None);
        }

        // Phase 2 (locked): copy whatever was appended meanwhile, swap, drop the fd.
        let mut files = self.lock();
        files.remove(chat_id);
        let mut old = File::open(&path)?;
        let before_bytes = old.metadata()?.len();
        let old_last = last_valid_line(&path)?.map(|(seq, _)| seq);
        old.seek(SeekFrom::Start(consumed))?;
        std::io::copy(&mut old, tmp.as_file_mut())?;
        tmp.as_file_mut().flush()?;
        let new_last = last_valid_line(tmp.path())?.map(|(seq, _)| seq);
        let after_bytes = tmp.as_file().metadata()?.len();
        if old_last != new_last || after_bytes == 0 {
            return Err(JournalError::Io(std::io::Error::other(
                "journal compaction would change the last seq; aborted",
            )));
        }
        if let Ok(meta) = std::fs::metadata(&path) {
            let _ = tmp.as_file().set_permissions(meta.permissions());
        }
        tmp.as_file().sync_all()?;
        tmp.persist(&path).map_err(|e| e.error)?;
        #[cfg(unix)]
        if let Ok(dir) = File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        // The next append reopens the new file and rescans its tail.
        files.remove(chat_id);
        Ok(Some(CompactStats {
            before_bytes,
            after_bytes,
            dropped,
        }))
    }

    /// Remove a chat's journal file entirely (tests / future compaction).
    pub fn discard(&self, chat_id: &str) -> Result<(), JournalError> {
        self.lock().remove(chat_id);
        let path = self.path_for(chat_id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// Text and reasoning deltas — the only events the chat doc's final text makes
/// redundant (a subagent's nested deltas likewise: its doc is finalized with the run).
/// Tool calls/results exist only in the journal and are never droppable.
fn is_droppable_delta(event: &AgentEvent) -> bool {
    match event {
        AgentEvent::TextDelta { .. } | AgentEvent::ReasoningDelta { .. } => true,
        AgentEvent::Subagent { event, .. } => is_droppable_delta(event),
        _ => false,
    }
}

/// Seq of the last valid `Done`: everything up to it belongs to finalized turns. `None`
/// when the journal has no `Done` (a run that never finished), so nothing is compacted.
fn compaction_boundary(path: &Path) -> Result<Option<u64>, JournalError> {
    for item in RevEvents::open(path)? {
        let (seq, event) = item?;
        if matches!(event, AgentEvent::Done { .. }) {
            return Ok(Some(seq));
        }
    }
    Ok(None)
}

/// Whether a raw journal line parses, sits at or before `boundary`, and is a droppable
/// delta. Anything unparsable is kept verbatim.
fn is_covered_delta(path: &Path, raw: &[u8], boundary: u64) -> bool {
    let Ok(line) = std::str::from_utf8(raw) else {
        return false;
    };
    parse_line(path, line).is_some_and(|(seq, event)| seq <= boundary && is_droppable_delta(&event))
}

/// Parse every valid line; malformed lines (torn tail writes) are skipped.
fn read_lines(path: &Path) -> Result<Vec<(u64, AgentEvent)>, JournalError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        if let Some(parsed) = parse_line(path, &line?) {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// One journal line → `(seq, event)`; blank lines are skipped silently, malformed
/// ones (torn tail writes) with a warning.
fn parse_line(path: &Path, line: &str) -> Option<(u64, AgentEvent)> {
    if line.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<JournalLine>(line) {
        Ok(parsed) => Some((parsed.seq, parsed.event)),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "journal: skipping malformed line");
            None
        }
    }
}

/// The last valid event in the file — what `read_lines(path).last()` returns, without
/// reading more than the tail.
fn last_valid_line(path: &Path) -> Result<Option<(u64, AgentEvent)>, JournalError> {
    RevEvents::open(path)?.next().transpose()
}

/// Next seq (last valid seq + 1, starting at 1) and whether the file ends mid-line.
fn scan_tail(path: &Path) -> Result<(u64, bool), JournalError> {
    let Some(mut lines) = RevLines::open(path)? else {
        return Ok((1, false));
    };
    let needs_newline = lines.ends_mid_line()?;
    let next_seq = RevEvents::from_lines(path, Some(lines))
        .next()
        .transpose()?
        .map(|(seq, _)| seq + 1)
        .unwrap_or(1);
    Ok((next_seq, needs_newline))
}

/// First tail-read size. Journal lines are usually far shorter; a longer line grows the
/// read geometrically, so a multi-MB line costs O(len) rather than O(len²).
const TAIL_CHUNK: usize = 64 * 1024;

/// Raw lines of a file, last to first, read backwards in chunks — memory is bounded by
/// the longest line rather than the file. `\n` separators are stripped (a preceding `\r`
/// is left in place; JSON parsing treats it as whitespace), so a file ending in `\n`
/// yields an empty line first, which callers skip like any blank line.
struct RevLines {
    file: File,
    /// File offset of `buf[0]`; bytes before it have not been read yet.
    pos: u64,
    /// Read but not yet yielded: the file's bytes `[pos, pos + buf.len())`.
    buf: Vec<u8>,
    finished: bool,
}

impl RevLines {
    /// `None` when the file does not exist (an empty journal, as `read_lines` treats it).
    fn open(path: &Path) -> std::io::Result<Option<Self>> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let pos = file.metadata()?.len();
        Ok(Some(Self {
            file,
            pos,
            buf: Vec::new(),
            finished: false,
        }))
    }

    /// Prepend the previous chunk of the file to `buf` (at least as large as `buf`).
    fn fill(&mut self) -> std::io::Result<()> {
        let want = TAIL_CHUNK.max(self.buf.len()) as u64;
        let n = want.min(self.pos);
        self.pos -= n;
        let mut chunk = vec![0u8; n as usize];
        self.file.seek(SeekFrom::Start(self.pos))?;
        self.file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&self.buf);
        self.buf = chunk;
        Ok(())
    }

    /// True when the file is non-empty and its last byte is not `\n` (a torn write).
    /// Call before the first `next_line`.
    fn ends_mid_line(&mut self) -> std::io::Result<bool> {
        if self.buf.is_empty() && self.pos > 0 {
            self.fill()?;
        }
        Ok(self.buf.last().is_some_and(|b| *b != b'\n'))
    }

    fn next_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if self.finished {
                return Ok(None);
            }
            if let Some(i) = self.buf.iter().rposition(|b| *b == b'\n') {
                let line = self.buf.split_off(i + 1);
                self.buf.truncate(i);
                return Ok(Some(line));
            }
            if self.pos == 0 {
                self.finished = true;
                return Ok(Some(std::mem::take(&mut self.buf)));
            }
            self.fill()?;
        }
    }
}

/// Valid journal events newest-first: `read_lines` in reverse, with the same
/// blank/malformed-line skipping, reading only as far back as the caller iterates.
struct RevEvents {
    path: PathBuf,
    /// `None` once exhausted, after an I/O error, or when the file does not exist.
    lines: Option<RevLines>,
}

impl RevEvents {
    fn open(path: &Path) -> Result<Self, JournalError> {
        Ok(Self::from_lines(path, RevLines::open(path)?))
    }

    fn from_lines(path: &Path, lines: Option<RevLines>) -> Self {
        Self {
            path: path.to_path_buf(),
            lines,
        }
    }
}

impl Iterator for RevEvents {
    type Item = Result<(u64, AgentEvent), JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let raw = match self.lines.as_mut()?.next_line() {
                Ok(Some(raw)) => raw,
                Ok(None) => {
                    self.lines = None;
                    return None;
                }
                Err(err) => {
                    self.lines = None;
                    return Some(Err(err.into()));
                }
            };
            // A torn write can split a multi-byte char: treat it as malformed too.
            let Ok(line) = std::str::from_utf8(&raw) else {
                tracing::warn!(path = %self.path.display(), "journal: skipping non-UTF-8 line");
                continue;
            };
            if let Some(parsed) = parse_line(&self.path, line) {
                return Some(Ok(parsed));
            }
        }
    }
}

/// Journal (`.jsonl`) and resume-budget (`.resume`) paths for `chat_id` under an
/// arbitrary journals directory — profile import copies these files between
/// profiles without opening a `RunJournal`.
pub fn journal_paths(dir: &Path, chat_id: &str) -> (PathBuf, PathBuf) {
    let stem = sanitize_id(chat_id);
    (
        dir.join(format!("{stem}.jsonl")),
        dir.join(format!("{stem}.resume")),
    )
}

/// Chat ids become file names; anything outside a conservative set is replaced so a
/// hostile id cannot traverse paths. (Ids are uuids in practice.)
pub(crate) fn sanitize_id(chat_id: &str) -> String {
    chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::DoneStatus;

    fn text(s: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: s.into() }
    }

    fn done() -> AgentEvent {
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
            class: None,
        }
    }

    #[test]
    fn appends_are_monotonic_and_replayable() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("a")).unwrap(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        assert_eq!(journal.append("chat-1", &done()).unwrap(), 3);

        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 3);
        let after = journal.replay("chat-1", 2).unwrap();
        assert_eq!(after.len(), 1);
        assert!(matches!(after[0].1, AgentEvent::Done { .. }));
        // Era fallback: cursor ahead of last seq replays everything.
        assert_eq!(journal.replay("chat-1", 99).unwrap().len(), 3);
    }

    #[test]
    fn seq_continues_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
    }

    #[test]
    fn stale_scan_flags_journals_without_terminal_done() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        journal.append("dead", &text("partial")).unwrap();
        journal.append("clean", &text("full")).unwrap();
        journal.append("clean", &done()).unwrap();
        assert_eq!(journal.stale_sessions().unwrap(), vec!["dead".to_string()]);
        // Closing the stale journal with a Done clears the flag.
        journal.append("dead", &done()).unwrap();
        assert!(journal.stale_sessions().unwrap().is_empty());
    }

    #[test]
    fn torn_tail_line_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = RunJournal::open(dir.path()).unwrap();
            journal.append("chat-1", &text("a")).unwrap();
        }
        // Simulate a crash mid-write: garbage with no trailing newline.
        let path = dir.path().join("chat-1.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"seq\":2,\"event\":{\"type\":\"textD")
            .unwrap();
        drop(f);

        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.replay("chat-1", 0).unwrap().len(), 1);
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 2);
        let all = journal.replay("chat-1", 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].0, 2);
    }

    fn line(seq: u64, event: AgentEvent) -> String {
        serde_json::to_string(&JournalLine { seq, event }).unwrap()
    }

    fn rev_lines(path: &Path) -> Vec<Vec<u8>> {
        let Some(mut lines) = RevLines::open(path).unwrap() else {
            return Vec::new();
        };
        std::iter::from_fn(|| lines.next_line().unwrap()).collect()
    }

    /// Journal contents covering the tail reader's edge cases; each must read the same
    /// backwards as the full forward parse did.
    fn tail_cases() -> Vec<(&'static str, Vec<u8>)> {
        let huge = "x".repeat(5 * TAIL_CHUNK + 123);
        let a = line(1, text("a"));
        let b = line(2, text("b"));
        let big = line(3, text(&huge));
        let fin = line(4, done());
        vec![
            ("empty", Vec::new()),
            ("only-newlines", b"\n\n\r\n  \n".to_vec()),
            ("trailing-newline", format!("{a}\n{b}\n").into_bytes()),
            ("no-trailing-newline", format!("{a}\n{b}").into_bytes()),
            ("crlf", format!("{a}\r\n{b}\r\n").into_bytes()),
            (
                "blank-lines-at-end",
                format!("{a}\n{b}\n\n  \n").into_bytes(),
            ),
            (
                "garbage-last-line",
                format!("{a}\n{b}\n{{\"seq\":3,\"event\":{{\"type\":\"textD").into_bytes(),
            ),
            (
                "garbage-then-newline",
                format!("{a}\n{b}\nnot json\n\n").into_bytes(),
            ),
            ("all-garbage", b"nope\n{\"seq\":\nstill nope".to_vec()),
            (
                "multi-chunk-last-line",
                format!("{a}\n{big}\n").into_bytes(),
            ),
            (
                "multi-chunk-last-line-unterminated",
                format!("{a}\n{big}").into_bytes(),
            ),
            (
                "multi-chunk-middle-line",
                format!("{a}\n{big}\n{fin}\n").into_bytes(),
            ),
            (
                "multi-chunk-garbage-tail",
                format!("{a}\n{b}\n{}", &big[..big.len() - 7]).into_bytes(),
            ),
            ("multi-chunk-only-line", big.clone().into_bytes()),
        ]
    }

    #[test]
    fn rev_lines_are_forward_lines_reversed() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in tail_cases() {
            let path = dir.path().join(format!("{name}.jsonl"));
            std::fs::write(&path, &bytes).unwrap();
            let mut expected: Vec<Vec<u8>> =
                bytes.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
            expected.reverse();
            assert_eq!(rev_lines(&path), expected, "{name}");
        }
        assert!(rev_lines(&dir.path().join("missing.jsonl")).is_empty());
    }

    #[test]
    fn tail_reads_match_a_full_forward_parse() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in tail_cases() {
            let path = dir.path().join(format!("{name}.jsonl"));
            std::fs::write(&path, &bytes).unwrap();
            let all = read_lines(&path).unwrap();

            // Every valid event, newest-first.
            let mut expected = all.clone();
            expected.reverse();
            let rev: Vec<_> = RevEvents::open(&path)
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(rev, expected, "{name}");

            assert_eq!(
                last_valid_line(&path).unwrap(),
                all.last().cloned(),
                "{name}"
            );
            let old_scan_tail = (
                all.last().map(|(seq, _)| seq + 1).unwrap_or(1),
                bytes.last().is_some_and(|b| *b != b'\n'),
            );
            assert_eq!(scan_tail(&path).unwrap(), old_scan_tail, "{name}");
        }
        let missing = dir.path().join("missing.jsonl");
        assert_eq!(last_valid_line(&missing).unwrap(), None);
        assert_eq!(scan_tail(&missing).unwrap(), (1, false));
    }

    #[test]
    fn stale_sessions_unchanged_by_tail_reads() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in tail_cases() {
            std::fs::write(dir.path().join(format!("{name}.jsonl")), bytes).unwrap();
        }
        std::fs::write(dir.path().join("ignored.resume"), "2").unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();

        // The pre-tail-read implementation: full parse, inspect the last event.
        let mut expected = Vec::new();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let last = read_lines(&path).unwrap().into_iter().next_back();
            if matches!(last, Some((_, ref event)) if !matches!(event, AgentEvent::Done { .. })) {
                expected.push(path.file_stem().unwrap().to_str().unwrap().to_string());
            }
        }
        expected.sort();
        assert_eq!(journal.stale_sessions().unwrap(), expected);
        assert!(expected.contains(&"multi-chunk-garbage-tail".to_string()));
        assert!(!expected.contains(&"multi-chunk-middle-line".to_string()));
    }

    #[test]
    fn append_after_multi_chunk_torn_tail_isolates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat-1.jsonl");
        let big = line(7, text(&"y".repeat(3 * TAIL_CHUNK)));
        std::fs::write(
            &path,
            format!("{}\n{}", line(6, text("a")), &big[..big.len() / 2]),
        )
        .unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert_eq!(journal.append("chat-1", &text("b")).unwrap(), 7);
        assert!(matches!(
            journal.last_event("chat-1").unwrap(),
            Some((7, AgentEvent::TextDelta { .. }))
        ));
        assert_eq!(journal.replay("chat-1", 0).unwrap().len(), 2);
    }

    #[test]
    fn non_utf8_tail_line_is_skipped_as_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat-1.jsonl");
        let mut bytes = format!("{}\n", line(1, done())).into_bytes();
        // A torn write that split a multi-byte char.
        bytes
            .extend_from_slice(b"{\"seq\":2,\"event\":{\"type\":\"textDelta\",\"text\":\"\xE2\x82");
        std::fs::write(&path, bytes).unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert!(matches!(
            journal.last_event("chat-1").unwrap(),
            Some((1, AgentEvent::Done { .. }))
        ));
        assert!(journal.stale_sessions().unwrap().is_empty());
    }

    fn started(session: &str) -> AgentEvent {
        AgentEvent::SessionStarted {
            harness: zeron_proto::HarnessId::Mock,
            model: "m".into(),
            tools: vec![],
            cwd: "/w".into(),
            session_id: session.into(),
            assistant_message_id: "a".into(),
        }
    }

    fn reasoning(s: &str) -> AgentEvent {
        AgentEvent::ReasoningDelta { text: s.into() }
    }

    fn tool_call(id: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            id: id.into(),
            call: zeron_proto::ToolCall::Exec {
                command: "ls".into(),
            },
        }
    }

    fn tool_result(id: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            id: id.into(),
            is_error: false,
            output: Some("out".into()),
            diff: None,
        }
    }

    fn nested(event: AgentEvent) -> AgentEvent {
        AgentEvent::Subagent {
            parent_tool_use_id: "p".into(),
            event: Box::new(event),
        }
    }

    /// One finished turn (seqs 1..=7, `Done` = 7) plus a two-delta tail (8, 9).
    fn standard_events() -> Vec<AgentEvent> {
        vec![
            started("hs-1"),
            text("a"),
            text("b"),
            tool_call("t1"),
            tool_result("t1"),
            reasoning("r"),
            done(),
            text("tail1"),
            text("tail2"),
        ]
    }

    fn fill(journal: &RunJournal, chat: &str, events: &[AgentEvent]) {
        for event in events {
            journal.append(chat, event).unwrap();
        }
    }

    fn seqs_of(journal: &RunJournal, chat: &str) -> Vec<u64> {
        let all = journal.replay(chat, 0).unwrap();
        all.into_iter().map(|(seq, _)| seq).collect()
    }

    #[test]
    fn compact_drops_covered_deltas_and_keeps_seq() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        fill(&journal, "c", &standard_events());
        let before = journal.size_bytes("c");

        let stats = journal.compact("c", 0).unwrap().unwrap();
        assert_eq!(stats.dropped, 3);
        assert_eq!(stats.before_bytes, before);
        assert_eq!(stats.after_bytes, journal.size_bytes("c"));
        assert!(stats.after_bytes < stats.before_bytes);

        assert_eq!(seqs_of(&journal, "c"), vec![1, 4, 5, 7, 8, 9]);
        let all = journal.replay("c", 0).unwrap();
        assert!(matches!(all[0].1, AgentEvent::SessionStarted { .. }));
        assert!(matches!(all[1].1, AgentEvent::ToolCall { .. }));
        assert!(matches!(all[2].1, AgentEvent::ToolResult { .. }));
        assert!(matches!(all[3].1, AgentEvent::Done { .. }));
        assert_eq!(all[4].1, text("tail1"));
        assert_eq!(all[5].1, text("tail2"));
        // Seq stays stable for the next append.
        assert_eq!(journal.append("c", &text("more")).unwrap(), 10);
        // A second pass has nothing left to drop below the boundary.
        assert_eq!(journal.compact("c", 0).unwrap(), None);
    }

    #[test]
    fn compact_replay_after_seq_matches_uncompacted() {
        let events = standard_events();
        let plain_dir = tempfile::tempdir().unwrap();
        let packed_dir = tempfile::tempdir().unwrap();
        let plain = RunJournal::open(plain_dir.path()).unwrap();
        let packed = RunJournal::open(packed_dir.path()).unwrap();
        fill(&plain, "c", &events);
        fill(&packed, "c", &events);
        packed.compact("c", 0).unwrap().unwrap();

        let boundary = 7;
        let last = events.len() as u64;
        for after in 0..=last + 1 {
            let expected: Vec<_> = plain
                .replay("c", after)
                .unwrap()
                .into_iter()
                .filter(|(seq, ev)| !is_droppable_delta(ev) || *seq > boundary)
                .collect();
            assert_eq!(
                packed.replay("c", after).unwrap(),
                expected,
                "after {after}"
            );
        }
    }

    #[test]
    fn compact_keeps_nested_subagent_non_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        fill(
            &journal,
            "c",
            &[
                started("hs"),
                nested(text("x")),
                nested(nested(reasoning("deep"))),
                nested(tool_call("s1")),
                nested(tool_result("s1")),
                done(),
            ],
        );
        let stats = journal.compact("c", 0).unwrap().unwrap();
        assert_eq!(stats.dropped, 2);
        assert_eq!(seqs_of(&journal, "c"), vec![1, 4, 5, 6]);
    }

    #[test]
    fn only_text_and_reasoning_deltas_are_droppable() {
        for ev in [
            started("s"),
            done(),
            tool_call("t"),
            tool_result("t"),
            AgentEvent::Error {
                message: "e".into(),
            },
            AgentEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
            },
            AgentEvent::ContextUsage {
                tokens: Some(1),
                window: None,
            },
            AgentEvent::Steered {
                assistant_message_id: None,
                next_assistant_message_id: None,
            },
            AgentEvent::AssistantMessageCompleted {
                assistant_message_id: "a".into(),
            },
            nested(done()),
        ] {
            assert!(!is_droppable_delta(&ev), "{ev:?}");
        }
        assert!(is_droppable_delta(&text("x")));
        assert!(is_droppable_delta(&reasoning("x")));
    }

    #[test]
    fn compact_noop_without_done_or_below_min() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        // A run that never finished: no boundary, untouched byte for byte.
        fill(&journal, "live", &[started("s"), text("a"), text("b")]);
        let path = dir.path().join("live.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(journal.compact("live", 0).unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        fill(&journal, "done", &standard_events());
        let path = dir.path().join("done.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let too_big = bytes.len() as u64 + 1;
        assert_eq!(journal.compact("done", too_big).unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(journal.compact("missing", 0).unwrap(), None);
    }

    #[test]
    fn compact_crash_revival_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        // Turn 1 finished (and is compacted); turn 2 died mid-stream.
        fill(&journal, "c", &standard_events()[..7]);
        journal.compact("c", 0).unwrap().unwrap();
        fill(
            &journal,
            "c",
            &[started("hs-2"), text("partial"), tool_call("t2")],
        );

        assert_eq!(journal.stale_sessions().unwrap(), vec!["c".to_string()]);
        let last = journal.last_event("c").unwrap().unwrap();
        assert_eq!(last.1, tool_call("t2"));
        let sessions: Vec<String> = journal
            .events_rev("c")
            .unwrap()
            .filter_map(|e| match e.unwrap().1 {
                AgentEvent::SessionStarted { session_id, .. } => Some(session_id),
                _ => None,
            })
            .collect();
        assert_eq!(sessions, vec!["hs-2".to_string(), "hs-1".to_string()]);

        // Revival closes the journal with a synthetic Done.
        journal.append("c", &done()).unwrap();
        assert!(journal.stale_sessions().unwrap().is_empty());
        // Turn 2's deltas then compact too; the closing Done stays last.
        journal.compact("c", 0).unwrap().unwrap();
        assert!(matches!(
            journal.last_event("c").unwrap(),
            Some((_, AgentEvent::Done { .. }))
        ));
    }

    #[test]
    fn compact_preserves_torn_tail_and_append_isolates_it() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        fill(&journal, "c", &standard_events());
        let path = dir.path().join("c.jsonl");
        let torn: &[u8] = b"{\"seq\":10,\"event\":{\"type\":\"textD";
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(torn).unwrap();
        drop(f);

        journal.compact("c", 0).unwrap().unwrap();
        assert!(std::fs::read(&path).unwrap().ends_with(torn));
        // The reopened fd recomputes needs_newline: the torn line stays isolated.
        assert_eq!(journal.append("c", &text("next")).unwrap(), 10);
        let all = journal.replay("c", 0).unwrap();
        assert_eq!(all.last().unwrap(), &(10, text("next")));
        assert_eq!(all.len(), 7);
    }

    #[test]
    fn compact_append_after_reopen_lands_in_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        // The appends cache an open fd for "c".
        fill(&journal, "c", &standard_events());
        journal.compact("c", 0).unwrap().unwrap();
        assert_eq!(journal.append("c", &text("after")).unwrap(), 10);

        let fresh = RunJournal::open(dir.path()).unwrap();
        let all = fresh.replay("c", 0).unwrap();
        assert_eq!(all.last().unwrap(), &(10, text("after")));
        let seqs: Vec<u64> = all.iter().map(|(s, _)| *s).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    }

    #[test]
    fn compact_concurrent_appends() {
        let dir = tempfile::tempdir().unwrap();
        let journal = std::sync::Arc::new(RunJournal::open(dir.path()).unwrap());
        let mut events = vec![started("s")];
        events.extend((0..300).map(|i| text(&format!("pre{i}"))));
        events.push(done());
        fill(&journal, "c", &events);

        let writer = {
            let journal = journal.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    let ev = if i % 10 == 0 {
                        tool_call(&format!("w{i}"))
                    } else {
                        text(&format!("live{i}"))
                    };
                    journal.append("c", &ev).unwrap();
                }
            })
        };
        let mut compacted = 0;
        while !writer.is_finished() {
            compacted += usize::from(journal.compact("c", 0).unwrap().is_some());
        }
        writer.join().unwrap();
        compacted += usize::from(journal.compact("c", 0).unwrap().is_some());
        assert!(compacted >= 1);

        let seqs = seqs_of(&journal, "c");
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
        // Everything appended after the Done (seq 302) survives, in order.
        let tail: Vec<u64> = seqs.iter().copied().filter(|s| *s > 302).collect();
        assert_eq!(tail, (303..=502).collect::<Vec<_>>());
        // The pre-Done deltas are gone; SessionStarted and Done remain.
        assert_eq!(seqs[..2], [1, 302]);
    }

    #[test]
    fn compact_leftover_tmp_is_ignored_and_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join(".compact-x.tmp");
        std::fs::write(&tmp, "junk").unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        assert!(!tmp.exists());
        std::fs::write(&tmp, "junk").unwrap();
        assert!(journal.stale_sessions().unwrap().is_empty());
        assert!(journal.journaled_chats().is_empty());
    }

    #[test]
    fn size_metrics_and_chat_listing() {
        let dir = tempfile::tempdir().unwrap();
        let journal = RunJournal::open(dir.path()).unwrap();
        fill(&journal, "b", &[text("x")]);
        fill(&journal, "a", &[text("y"), done()]);
        std::fs::write(dir.path().join("a.resume"), "1").unwrap();
        assert_eq!(
            journal.journaled_chats(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            journal.total_bytes(),
            journal.size_bytes("a") + journal.size_bytes("b")
        );
        assert_eq!(journal.size_bytes("none"), 0);
        assert!(journal.idle_for("a").is_some());
        assert!(journal.idle_for("none").is_none());
    }
}
