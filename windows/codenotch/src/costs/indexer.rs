//! Reads Claude Code's and Codex's transcripts and keeps only token counts, as upstream's
//! CostIndexer does.
//!
//! Claude, per assistant line: timestamp, cwd, gitBranch, sessionId, requestId, message.model and
//! message.usage. Codex, per rollout: the session's id and cwd, the model in use, and each
//! token_count's last_token_usage. Content is never kept or logged: lines are parsed, the numbers
//! taken, and the rest dropped. Nothing here is ever sent anywhere.
//!
//! Reading is incremental: each file's byte offset is stored, so a pass only reads what was
//! appended, and a trailing line without its newline waits for the next pass.

use super::store::{Cursor, Event, Store};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Claude,
    Codex,
}

/// What a Codex rollout said before its token counts: kept per file across passes
#[derive(Debug, Clone, Default)]
struct CodexContext {
    session_id: String,
    cwd: String,
    model: String,
    /// The last cumulative total seen: Codex repeats a token_count with an unchanged total,
    /// and counting it again would count the same turn twice
    last_total: Option<i64>,
}

pub struct Indexer {
    root: PathBuf,
    format: Format,
    codex: HashMap<String, CodexContext>,
    git_roots: HashMap<String, String>,
}

/// Lines that cannot be the ones we read are skipped before any JSON is parsed
const CLAUDE_MARKER: &str = "\"assistant\"";
const CODEX_MARKERS: [&str; 3] = ["token_count", "session_meta", "turn_context"];

impl Indexer {
    pub fn new(root: PathBuf, format: Format) -> Indexer {
        Indexer { root, format, codex: HashMap::new(), git_roots: HashMap::new() }
    }

    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }

    /// One pass over every transcript, newest first so the current period is right before the
    /// backfill ends. Returns how many turns were written
    pub fn scan(&mut self, store: &mut Store) -> usize {
        let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        collect(&self.root, &mut files, 0);
        files.sort_by(|a, b| b.1.cmp(&a.1));
        let mut written = 0;
        for (path, _) in files {
            written += self.index_file(&path, store);
        }
        written
    }

    fn index_file(&mut self, path: &Path, store: &mut Store) -> usize {
        let Ok(meta) = std::fs::metadata(path) else { return 0 };
        let size = meta.len();
        let inode = inode_of(&meta);
        let key = path.to_string_lossy().to_string();
        let mut offset = 0;
        if let Some(c) = store.cursor(&key) {
            if c.inode != inode || size < c.offset {
                offset = 0; // replaced or truncated: read again
            } else if size == c.offset {
                return 0;
            } else {
                offset = c.offset;
            }
        }
        if size <= offset {
            return 0;
        }
        let Ok(mut file) = std::fs::File::open(path) else { return 0 };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return 0;
        }
        if self.format == Format::Codex && offset > 0 && !self.codex.contains_key(&key) {
            self.prime_codex(path, &key);
        }
        // The folder a Claude transcript sits in names where its session started
        let folder = match self.format {
            Format::Claude => path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            Format::Codex => String::new(),
        };
        let mut reader = BufReader::new(file.by_ref().take(size - offset));
        let mut events = Vec::new();
        let mut consumed = 0u64;
        let mut line = Vec::new();
        loop {
            line.clear();
            let Ok(n) = reader.read_until(b'\n', &mut line) else { break };
            if n == 0 || line.last() != Some(&b'\n') {
                break; // end, or a line still being written
            }
            consumed += n as u64;
            let text = String::from_utf8_lossy(&line[..n - 1]);
            let event = match self.format {
                Format::Claude => self.parse_claude(&text, &folder),
                Format::Codex => self.parse_codex(&text, &key, path),
            };
            if let Some(e) = event {
                events.push(e);
            }
        }
        if consumed == 0 {
            return 0;
        }
        let cursor = Cursor { inode, size, offset: offset + consumed };
        match store.commit(&events, &key, cursor) {
            Ok(()) => events.len(),
            Err(e) => {
                crate::applog(&format!("costs: could not record a transcript ({e})"));
                0
            }
        }
    }

    fn parse_claude(&mut self, line: &str, folder: &str) -> Option<Event> {
        if !line.contains(CLAUDE_MARKER) {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v.get("type")?.as_str()? != "assistant" {
            return None;
        }
        let message = v.get("message")?;
        let usage = message.get("usage")?;
        let model = message.get("model")?.as_str()?;
        // Synthetic turns are local error placeholders, not billed requests
        if model == "<synthetic>" {
            return None;
        }
        let cwd = v.get("cwd")?.as_str()?;
        let session_id = v.get("sessionId").or_else(|| v.get("session_id"))?.as_str()?;
        let ts = timestamp(v.get("timestamp")?.as_str()?)?;
        let input = int(usage.get("input_tokens"))?;
        let output = int(usage.get("output_tokens"))?;
        let cache_read = int(usage.get("cache_read_input_tokens")).unwrap_or(0);
        let cache_write = int(usage.get("cache_creation_input_tokens")).unwrap_or(0);
        // One turn is written several times while it streams; its request id makes it one row
        let dedupe_key = match v.get("requestId").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
            Some(r) => format!("r:{r}"),
            None => format!("s:{session_id}:{ts}:{input}:{output}:{cache_read}:{cache_write}"),
        };
        let branch = v.get("gitBranch").and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(String::from);
        Some(Event {
            ts,
            session_id: session_id.to_string(),
            dedupe_key,
            project: self.project_root(cwd, folder),
            cwd: cwd.to_string(),
            branch,
            model: model.to_string(),
            input,
            output,
            cache_read,
            cache_write,
        })
    }

    /// Resuming a rollout mid-file: its first lines name the session and where it ran
    fn prime_codex(&mut self, path: &Path, key: &str) {
        let Ok(f) = std::fs::File::open(path) else { return };
        let mut head = String::new();
        let _ = f.take(256 * 1024).read_to_string(&mut head);
        let mut ctx = CodexContext::default();
        for line in head.lines() {
            if !CODEX_MARKERS.iter().any(|m| line.contains(m)) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            self.apply_codex_context(&v, &mut ctx, path);
        }
        self.codex.insert(key.to_string(), ctx);
    }

    fn apply_codex_context(&self, v: &serde_json::Value, ctx: &mut CodexContext, path: &Path) {
        let Some(payload) = v.get("payload") else { return };
        if v.get("type").and_then(|x| x.as_str()) == Some("session_meta") {
            ctx.session_id = payload
                .get("id")
                .or_else(|| payload.get("session_id"))
                .and_then(|x| x.as_str())
                .map(String::from)
                .unwrap_or_else(|| stem(path));
            if let Some(c) = payload.get("cwd").and_then(|x| x.as_str()) {
                ctx.cwd = c.to_string();
            }
        }
        if payload.get("type").and_then(|x| x.as_str()) == Some("turn_context") {
            if let Some(c) = payload.get("cwd").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                ctx.cwd = c.to_string();
            }
            if let Some(m) = payload.get("model").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                ctx.model = m.to_string();
            }
        }
    }

    fn parse_codex(&mut self, line: &str, key: &str, path: &Path) -> Option<Event> {
        if !CODEX_MARKERS.iter().any(|m| line.contains(m)) {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let mut ctx = self.codex.remove(key).unwrap_or_default();
        self.apply_codex_context(&v, &mut ctx, path);
        let event = self.codex_event(&v, &mut ctx, path);
        self.codex.insert(key.to_string(), ctx);
        event
    }

    fn codex_event(&mut self, v: &serde_json::Value, ctx: &mut CodexContext, path: &Path) -> Option<Event> {
        let payload = v.get("payload")?;
        if payload.get("type")?.as_str()? != "token_count" {
            return None;
        }
        let info = payload.get("info").filter(|x| x.is_object())?; // rate-limit-only ticks carry none
        let last = info.get("last_token_usage")?;
        let total = info.get("total_token_usage").and_then(|t| int(t.get("total_tokens")));
        if total.is_some() && total == ctx.last_total {
            return None; // the same turn reported again
        }
        ctx.last_total = total.or(ctx.last_total);
        let ts = timestamp(v.get("timestamp")?.as_str()?)?;
        let input_all = int(last.get("input_tokens"))?;
        let output = int(last.get("output_tokens"))?;
        if input_all + output <= 0 {
            return None;
        }
        let cached = int(last.get("cached_input_tokens")).unwrap_or(0);
        let cache_write = int(last.get("cache_write_input_tokens")).unwrap_or(0);
        let session_id = if ctx.session_id.is_empty() { stem(path) } else { ctx.session_id.clone() };
        let cwd = if ctx.cwd.is_empty() {
            dirs::home_dir().map(|h| h.to_string_lossy().to_string()).unwrap_or_default()
        } else {
            ctx.cwd.clone()
        };
        let dedupe_key = match total {
            Some(t) => format!("c:{session_id}:{t}"),
            None => format!("c:{session_id}:{ts}:{input_all}:{output}"),
        };
        Some(Event {
            ts,
            project: self.project_root(&cwd, ""),
            session_id,
            dedupe_key,
            cwd,
            branch: None,
            model: if ctx.model.is_empty() { "codex".into() } else { ctx.model.clone() },
            input: (input_all - cached).max(0),
            output,
            cache_read: cached,
            cache_write,
        })
    }

    /// Sessions started in subfolders of one repository read as one project: the cwd is walked up
    /// to its git root. When that is gone, the Claude transcript folder (the start folder with
    /// every non-alphanumeric character turned into "-") recovers the real prefix. Else the cwd
    fn project_root(&mut self, cwd: &str, folder: &str) -> String {
        let key = format!("{folder}\0{cwd}");
        if let Some(r) = self.git_roots.get(&key) {
            return r.clone();
        }
        let mut result = None;
        let mut dir = PathBuf::from(cwd);
        for _ in 0..12 {
            if dir.join(".git").exists() {
                result = Some(dir.to_string_lossy().to_string());
                break;
            }
            match dir.parent() {
                Some(p) if p != dir && p != Path::new("/") => dir = p.to_path_buf(),
                _ => break,
            }
        }
        if result.is_none() && !folder.is_empty() && folder.chars().count() < cwd.chars().count() {
            let normalized: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
            if normalized.starts_with(folder) {
                result = Some(cwd.chars().take(folder.chars().count()).collect());
            }
        }
        let resolved = result.unwrap_or_else(|| cwd.to_string());
        self.git_roots.insert(key, resolved.clone());
        resolved
    }
}

fn collect(dir: &Path, out: &mut Vec<(PathBuf, std::time::SystemTime)>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        let path = e.path();
        if ft.is_dir() {
            collect(&path, out, depth + 1);
        } else if ft.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
            let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            out.push((path, mtime));
        }
    }
}

#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

fn int(v: Option<&serde_json::Value>) -> Option<i64> {
    let v = v?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

fn timestamp(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codenotch-costs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const CLAUDE_TURN: &str = r#"{"type":"assistant","cwd":"/nowhere/proj/src","sessionId":"s1","requestId":"req1","timestamp":"2026-10-06T09:00:00.000Z","gitBranch":"main","message":{"model":"claude-sonnet-5","content":[{"type":"text","text":"SECRET"}],"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":300,"cache_creation_input_tokens":40}}}"#;

    #[test]
    fn claude_turns_are_read_once_and_only_as_numbers() {
        let root = tmp("claude");
        let dir = root.join("-nowhere-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s1.jsonl");
        let streamed = CLAUDE_TURN.replace("\"output_tokens\":20", "\"output_tokens\":5");
        std::fs::write(&file, format!("{streamed}\n{{\"type\":\"user\",\"message\":{{}}}}\n{CLAUDE_TURN}\n")).unwrap();
        let mut store = Store::memory();
        let mut ix = Indexer::new(root.clone(), Format::Claude);
        ix.scan(&mut store);
        assert_eq!(store.event_count(), 1, "the streamed turn is one row");
        let turns = store.turns(0, i64::MAX);
        assert_eq!(turns[0].0, "/nowhere/proj", "the transcript folder recovers the project from a deep cwd");
        assert_eq!(turns[0].2, [10, 20, 300, 40], "the final size of each count");
        assert_eq!(ix.scan(&mut store), 0, "nothing appended, nothing read");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_line_still_being_written_waits_for_its_newline() {
        let root = tmp("partial");
        let file = root.join("s.jsonl");
        let (head, tail) = CLAUDE_TURN.split_at(40);
        std::fs::write(&file, head).unwrap();
        let mut store = Store::memory();
        let mut ix = Indexer::new(root.clone(), Format::Claude);
        assert_eq!(ix.scan(&mut store), 0);
        std::fs::write(&file, format!("{head}{tail}\n")).unwrap();
        assert_eq!(ix.scan(&mut store), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn codex_counts_each_turn_once_with_its_context() {
        let root = tmp("codex");
        let file = root.join("rollout-x.jsonl");
        let tc = |ts: &str, input: i64, cached: i64, out: i64, total: i64| {
            format!(r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{out}}},"total_token_usage":{{"total_tokens":{total}}}}}}}}}"#)
        };
        let lines = [
            r#"{"timestamp":"2026-10-06T09:00:00Z","type":"session_meta","payload":{"id":"sess","cwd":"/nowhere/repo"}}"#.to_string(),
            r#"{"timestamp":"2026-10-06T09:00:01Z","type":"turn_context","payload":{"type":"turn_context","cwd":"/nowhere/repo","model":"gpt-6"}}"#.to_string(),
            tc("2026-10-06T09:00:02Z", 1000, 400, 50, 1050),
            tc("2026-10-06T09:00:03Z", 1000, 400, 50, 1050), // reported again
            r#"{"timestamp":"2026-10-06T09:00:04Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#.to_string(),
        ];
        std::fs::write(&file, lines.join("\n") + "\n").unwrap();
        let mut store = Store::memory();
        let mut ix = Indexer::new(root.clone(), Format::Codex);
        assert_eq!(ix.scan(&mut store), 1);
        // Appended later, read by a fresh indexer: the head names the session again
        let more = tc("2026-10-06T09:05:00Z", 2000, 1000, 100, 3150);
        std::fs::write(&file, lines.join("\n") + "\n" + &more + "\n").unwrap();
        let mut fresh = Indexer::new(root.clone(), Format::Codex);
        assert_eq!(fresh.scan(&mut store), 1);
        let turns = store.turns(0, i64::MAX);
        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|t| t.0 == "/nowhere/repo" && t.1 == "gpt-6"), "{turns:?}");
        assert!(turns.iter().any(|t| t.2 == [600, 50, 400, 0]), "fresh input excludes the cached part");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "Indexes this machine's real transcripts into a throwaway database; opt in to measure"]
    fn live_index_speed() {
        for (root, format) in [
            (dirs::home_dir().unwrap().join(".claude/projects"), Format::Claude),
            (dirs::home_dir().unwrap().join(".codex/sessions"), Format::Codex),
        ] {
            let db = std::env::temp_dir().join(format!("codenotch-live-{}.sqlite", std::process::id()));
            let _ = std::fs::remove_file(&db);
            let mut store = Store::open(&db).unwrap();
            let mut ix = Indexer::new(root.clone(), format);
            let t = std::time::Instant::now();
            let n = ix.scan(&mut store);
            let first = t.elapsed();
            let t = std::time::Instant::now();
            let again = ix.scan(&mut store);
            eprintln!("{format:?}: {n} turns in {first:?}, rows {}, second pass {again} in {:?}", store.event_count(), t.elapsed());
            let _ = std::fs::remove_file(&db);
        }
    }
}
