//! Context-overflow rejections, read from LM Studio's server log.
//!
//! When a prompt doesn't fit the model's loaded context, LM Studio refuses the request
//! before generating anything, so `lms log stream` never reports stats for it. The only
//! trace is one `[ERROR]` line in `<server-logs>/YYYY-MM/YYYY-MM-DD.N.log`. `Tailer`
//! follows those files and picks such lines out; everything else, request bodies included,
//! is dropped as it's read and never kept or logged.

#![allow(dead_code)]

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// A request LM Studio refused because its prompt didn't fit the model's loaded context.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextRejection {
    pub model_id: String,
    /// When LM Studio logged the refusal, to the second.
    pub at: DateTime<Utc>,
    pub input_tokens: Option<u64>,
    pub context_length: Option<u64>,
}

pub fn parse_rejection(line: &str) -> Option<ContextRejection> {
    parse_rejection_in(line, &Local)
}

/// Parses `[YYYY-MM-DD HH:MM:SS][ERROR][<model>] <overflow message>`, reading the
/// timestamp as local time in `tz`. The line has to start that way: request bodies are in
/// the same log, and they can quote the message.
pub fn parse_rejection_in<Tz: TimeZone>(line: &str, tz: &Tz) -> Option<ContextRejection> {
    let after_bracket = line.strip_prefix('[')?;
    let rest = after_bracket.get(19..)?.strip_prefix("][ERROR][")?;
    let (model_id, message) = rest.split_once("] ")?;
    let (input_tokens, context_length) = overflow_numbers(message)?;
    let stamp = after_bracket.get(..19)?;
    let naive = NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S").ok()?;
    let at = tz
        .from_local_datetime(&naive)
        .earliest()
        .map_or_else(Utc::now, |t| t.with_timezone(&Utc));
    Some(ContextRejection {
        model_id: model_id.to_string(),
        at,
        input_tokens,
        context_length,
    })
}

/// Recognizes the engines' context-overflow errors and pulls out the prompt size and the
/// context length where the message gives them. `None` means it isn't one.
fn overflow_numbers(message: &str) -> Option<(Option<u64>, Option<u64>)> {
    // MLX engine, as seen in real logs: "Input does not fit in context length. The input
    // has 359277 tokens, but the context length only supports 262144 tokens."
    if message.contains("Input does not fit in context length") {
        return Some((
            number_before_tokens(message, "The input has "),
            number_before_tokens(message, "only supports "),
        ));
    }
    // llama.cpp engine, from its message template: "request (5000 tokens) exceeds the
    // available context size (4096 tokens), try increasing it".
    if message.contains("exceeds the available context size") {
        return Some((
            number_before_tokens(message, "request ("),
            number_before_tokens(message, "available context size ("),
        ));
    }
    // Any engine, when the overflow policy can't keep the start of the prompt. LM Studio's
    // API error for it adds "(n_keep: 13055>= n_ctx: 4096)"; n_keep is the part of the
    // prompt to keep, not the whole prompt, so only the context length is taken.
    if message.contains("tokens to keep from the initial prompt is greater than the context length")
    {
        return Some((None, number_after(message, "n_ctx: ")));
    }
    None
}

/// The number right after `label`, provided ` tokens` follows it, so that a line cut off
/// mid-number doesn't yield a smaller one.
fn number_before_tokens(message: &str, label: &str) -> Option<u64> {
    let (number, rest) = digits_after(message, label)?;
    rest.starts_with(" tokens").then(|| number.parse().ok())?
}

/// The number right after `label`, provided something other than a digit follows it, for
/// the same reason.
fn number_after(message: &str, label: &str) -> Option<u64> {
    let (number, rest) = digits_after(message, label)?;
    (!rest.is_empty()).then(|| number.parse().ok())?
}

/// The run of digits right after `label`, and what follows it.
fn digits_after<'a>(message: &'a str, label: &str) -> Option<(&'a str, &'a str)> {
    let (_, after) = message.split_once(label)?;
    let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    (digits > 0).then(|| after.split_at(digits))
}

fn parse_bytes(line: &[u8]) -> Option<ContextRejection> {
    let text = match std::str::from_utf8(line) {
        Ok(s) => s,
        // A kept line prefix can end partway through a character.
        Err(e) => std::str::from_utf8(&line[..e.valid_up_to()]).ok()?,
    };
    parse_rejection(text)
}

/// A server-log file name, `YYYY-MM-DD.N.log`. LM Studio starts each day at `.1` and moves
/// to the next number at about 10 MiB, so files sort by date, then by N as a number: `.10`
/// comes after `.9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LogKey {
    date: NaiveDate,
    n: u32,
}

impl LogKey {
    fn parse(file_name: &str) -> Option<Self> {
        let (date, n) = file_name.strip_suffix(".log")?.rsplit_once('.')?;
        Some(Self {
            date: NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?,
            n: n.parse().ok()?,
        })
    }

    fn month(&self) -> (i32, u32) {
        (self.date.year(), self.date.month())
    }
}

/// A month folder's name, `YYYY-MM`.
fn parse_month(name: &str) -> Option<(i32, u32)> {
    let (y, m) = name.split_once('-')?;
    if y.len() != 4 || m.len() != 2 {
        return None;
    }
    Some((y.parse().ok()?, m.parse().ok()?))
}

struct LogFile {
    key: LogKey,
    path: PathBuf,
}

/// The log files in month folders from `from` on (in every folder for `None`), oldest first.
fn list_logs(root: &Path, from: Option<(i32, u32)>) -> io::Result<Vec<LogFile>> {
    let mut files = Vec::new();
    for month in fs::read_dir(root)?.flatten() {
        let Some(ym) = month.file_name().to_str().and_then(parse_month) else {
            continue;
        };
        if from.is_some_and(|from| ym < from) {
            continue;
        }
        let Ok(entries) = fs::read_dir(month.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(key) = entry.file_name().to_str().and_then(LogKey::parse) {
                files.push(LogFile {
                    key,
                    path: entry.path(),
                });
            }
        }
    }
    files.sort_by_key(|f| f.key);
    Ok(files)
}

/// Longest line start kept: plenty for an overflow line. A request body can put megabytes
/// on one line; the rest of such a line is dropped as it's read.
const LINE_PREFIX: usize = 4096;

#[derive(Default)]
struct Lines {
    prefix: Vec<u8>,
    /// The line being read began before the tailer started, so it's history.
    skip: bool,
}

impl Lines {
    fn push(&mut self, mut bytes: &[u8], emit: &mut impl FnMut(&[u8])) {
        while let Some(i) = bytes.iter().position(|&b| b == b'\n') {
            self.keep(&bytes[..i]);
            if !std::mem::take(&mut self.skip) {
                emit(&self.prefix);
            }
            self.prefix.clear();
            bytes = &bytes[i + 1..];
        }
        self.keep(bytes);
    }

    fn keep(&mut self, bytes: &[u8]) {
        let room = LINE_PREFIX.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// The file is complete: a last line without a newline still counts.
    fn finish(&mut self, emit: &mut impl FnMut(&[u8])) {
        if !self.prefix.is_empty() && !self.skip {
            emit(&self.prefix);
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.prefix.clear();
        self.skip = false;
    }
}

struct Current {
    key: LogKey,
    path: PathBuf,
    /// Bytes read so far.
    offset: u64,
    /// Device and inode as of the last read, to notice the file being replaced.
    id: Option<(u64, u64)>,
}

impl Current {
    fn at_start(file: &LogFile) -> Self {
        Self {
            key: file.key,
            path: file.path.clone(),
            offset: 0,
            id: None,
        }
    }
}

/// Positions at the end of `file`. If that's partway through a line, the rest of the line
/// is history too.
fn start_at_end(file: &LogFile, lines: &mut Lines) -> Current {
    let mut cur = Current::at_start(file);
    if let Ok(mut f) = File::open(&file.path)
        && let Ok(meta) = f.metadata()
    {
        cur.offset = meta.len();
        cur.id = Some((meta.dev(), meta.ino()));
        let mut last = [0u8; 1];
        if meta.len() > 0
            && f.seek(SeekFrom::End(-1)).is_ok()
            && f.read_exact(&mut last).is_ok()
            && last[0] != b'\n'
        {
            lines.skip = true;
        }
    }
    cur
}

/// Reads whatever was added to `cur` since the last call.
fn drain(cur: &mut Current, lines: &mut Lines, buf: &mut [u8], emit: &mut impl FnMut(&[u8])) {
    let Ok(mut file) = File::open(&cur.path) else {
        return;
    };
    let Ok(meta) = file.metadata() else {
        return;
    };
    let id = (meta.dev(), meta.ino());
    if cur.id.is_some_and(|seen| seen != id) || meta.len() < cur.offset {
        debug!(
            "server log: {} was replaced or truncated; rereading it",
            cur.path.display()
        );
        cur.offset = 0;
        lines.reset();
    }
    cur.id = Some(id);
    if meta.len() == cur.offset || file.seek(SeekFrom::Start(cur.offset)).is_err() {
        return;
    }
    loop {
        match file.read(buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                cur.offset += n as u64;
                lines.push(&buf[..n], emit);
            }
        }
    }
}

/// Follows LM Studio's server log and returns the rejections logged since the last `poll`.
pub struct Tailer {
    root: PathBuf,
    /// Anything logged before this is history. Normally none of it is read anyway; this
    /// covers a file that has to be read again from the start after being replaced.
    not_before: DateTime<Utc>,
    started: bool,
    current: Option<Current>,
    lines: Lines,
    buf: Vec<u8>,
    /// The last listing failed, so the next failure isn't logged again.
    unreadable: bool,
}

impl Tailer {
    pub fn new(root: PathBuf, not_before: DateTime<Utc>) -> Self {
        Self {
            root,
            not_before,
            started: false,
            current: None,
            lines: Lines::default(),
            buf: vec![0; 64 * 1024],
            unreadable: false,
        }
    }

    pub fn poll(&mut self) -> Vec<ContextRejection> {
        let mut out = Vec::new();
        let from = self.current.as_ref().map(|c| c.key.month());
        let files = match list_logs(&self.root, from) {
            Ok(files) => {
                if std::mem::take(&mut self.unreadable) {
                    info!("server log folder {} is readable now", self.root.display());
                }
                files
            }
            Err(e) => {
                if !self.unreadable {
                    self.unreadable = true;
                    warn!(
                        "server log folder {}: {e}; context-overflow rejections won't show",
                        self.root.display()
                    );
                }
                return out;
            }
        };

        if !self.started {
            // What's logged already is history, so start at the end of the newest file.
            // With no files yet, every file that appears later is new.
            self.started = true;
            self.current = files.last().map(|f| start_at_end(f, &mut self.lines));
            if let Some(cur) = &self.current {
                debug!(
                    "server log: following {} from byte {}",
                    cur.path.display(),
                    cur.offset
                );
            }
            return out;
        }

        if self.current.is_none() {
            self.current = files.first().map(Current::at_start);
        } else if let Some(gone) = self
            .current
            .as_ref()
            .filter(|c| !files.iter().any(|f| f.path == c.path))
            .map(|c| c.key)
        {
            // The file went away (logs cleared?): carry on with what LM Studio writes next.
            self.lines.reset();
            let next = files.iter().find(|f| f.key > gone).or(files.last());
            self.current = next.map(Current::at_start);
        }

        let not_before = self.not_before;
        let mut emit = |line: &[u8]| {
            if let Some(r) = parse_bytes(line)
                && r.at >= not_before
            {
                out.push(r);
            }
        };
        while let Some(cur) = self.current.as_mut() {
            drain(cur, &mut self.lines, &mut self.buf, &mut emit);
            // LM Studio writes only to its newest file, so once a later one is listed, this
            // one is complete; it was listed before the drain, so the drain read all of it.
            let Some(next) = files.iter().find(|f| f.key > cur.key) else {
                break;
            };
            self.lines.finish(&mut emit);
            debug!("server log: following {}", next.path.display());
            *cur = Current::at_start(next);
        }
        out
    }
}

/// Follows the server log under `root`, sending each rejection as LM Studio logs it.
pub async fn run(
    root: PathBuf,
    interval: Duration,
    tx: mpsc::Sender<ContextRejection>,
    shutdown: watch::Receiver<bool>,
) {
    // LM Studio stamps whole seconds, and a line can land a moment after its stamp; the
    // margin keeps a rejection logged just as the monitor starts.
    let not_before = Utc::now() - chrono::Duration::seconds(60);
    run_tailer(Tailer::new(root, not_before), interval, tx, shutdown).await;
}

pub async fn run_tailer(
    mut tailer: Tailer,
    interval: Duration,
    tx: mpsc::Sender<ContextRejection>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                for rej in tailer.poll() {
                    info!(
                        "context overflow: {} refused a {:?}-token prompt (context {:?})",
                        rej.model_id, rej.input_tokens, rej.context_length
                    );
                    if tx.send(rej).await.is_err() {
                        return;
                    }
                }
            }
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Verbatim from LM Studio's server log (MLX engine).
    const REAL_REJECTIONS: [&str; 4] = [
        "[2026-09-25 10:12:51][ERROR][qwen/qwen3.8-27b] Input does not fit in context length. The input has 523041 tokens, but the context length only supports 262144 tokens.. Error Data: n/a, Additional Data: n/a",
        "[2026-09-28 19:09:11][ERROR][qwen/qwen3.8-27b] Input does not fit in context length. The input has 311057 tokens, but the context length only supports 262144 tokens.. Error Data: n/a, Additional Data: n/a",
        "[2026-09-28 20:14:49][ERROR][qwen/qwen3.8-27b] Input does not fit in context length. The input has 284279 tokens, but the context length only supports 262144 tokens.. Error Data: n/a, Additional Data: n/a",
        "[2026-10-08 15:37:38][ERROR][qwen/qwen3.8-27b] Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens.. Error Data: n/a, Additional Data: n/a",
    ];

    /// Verbatim; the monitor's own model polling fills most of the log with these.
    const POLL_LINE: &str = "[2026-10-08 15:37:39][DEBUG] Received request: GET to /api/v0/models";

    fn pdt() -> FixedOffset {
        FixedOffset::west_opt(7 * 3600).unwrap()
    }

    #[test]
    fn parses_real_mlx_rejections() {
        let parsed: Vec<ContextRejection> = REAL_REJECTIONS
            .iter()
            .map(|line| parse_rejection_in(line, &pdt()).expect(line))
            .collect();
        let inputs: Vec<Option<u64>> = parsed.iter().map(|r| r.input_tokens).collect();
        assert_eq!(
            inputs,
            [Some(523_041), Some(311_057), Some(284_279), Some(359_277)]
        );
        for r in &parsed {
            assert_eq!(r.model_id, "qwen/qwen3.8-27b");
            assert_eq!(r.context_length, Some(262_144));
        }
        assert_eq!(
            parsed[3].at,
            Utc.with_ymd_and_hms(2026, 10, 8, 22, 37, 38).unwrap()
        );
    }

    #[test]
    fn ignores_real_non_overflow_lines() {
        for line in [
            "[2026-07-10 09:10:29][ERROR][qwen/qwen3.6-35b-a3b] Model unloaded.. Error Data: n/a, Additional Data: n/a",
            "[2026-05-25 12:42:16][ERROR] No models loaded. Please load a model in the developer page or use the 'lms load' command.",
            // Logged in the same second as the last rejection, with the same numbers.
            "[2026-10-08 15:37:38][DEBUG][transformers] Token indices sequence length is longer than the specified maximum sequence length for this model (359277 > 262144). Running this sequence through the model will result in indexing errors",
            POLL_LINE,
            "",
        ] {
            assert_eq!(parse_rejection_in(line, &pdt()), None, "{line}");
        }
    }

    #[test]
    fn ignores_error_text_inside_body_line() {
        // Request bodies are logged too, and a conversation can quote the error.
        let quoted = format!("      \"content\": \"{}\",", REAL_REJECTIONS[3]);
        assert_eq!(parse_rejection_in(&quoted, &pdt()), None);
    }

    #[test]
    fn truncated_number_is_none() {
        let line = REAL_REJECTIONS[3];
        let cut = &line[..line.find("262144").unwrap() + 3];
        let r = parse_rejection_in(cut, &pdt()).unwrap();
        assert_eq!((r.input_tokens, r.context_length), (Some(359_277), None));
        let cut = &line[..line.find("359277").unwrap() + 2];
        let r = parse_rejection_in(cut, &pdt()).unwrap();
        assert_eq!((r.input_tokens, r.context_length), (None, None));
    }

    /// Built from the engines' message templates and LM Studio's API error text; not yet
    /// seen in a real server log.
    #[test]
    fn parses_other_engine_messages() {
        let r = parse_rejection_in(
            "[2026-10-08 15:37:38][ERROR][gemma-3-12b] request (5000 tokens) exceeds the available context size (4096 tokens), try increasing it",
            &pdt(),
        )
        .unwrap();
        assert_eq!(
            (r.model_id.as_str(), r.input_tokens, r.context_length),
            ("gemma-3-12b", Some(5_000), Some(4_096))
        );
        let r = parse_rejection_in(
            "[2026-10-08 15:37:38][ERROR][m] Context overflow policy error: The number of tokens to keep from the initial prompt is greater than the context length.",
            &pdt(),
        )
        .unwrap();
        assert_eq!((r.input_tokens, r.context_length), (None, None));
        // n_keep is only the part of the prompt to keep, so it isn't taken as the size.
        let api = "[2026-10-08 15:37:38][ERROR][m] The number of tokens to keep from the initial prompt is greater than the context length (n_keep: 13055>= n_ctx: 4096). Try to load the model with a larger context length, or provide a shorter input.";
        let r = parse_rejection_in(api, &pdt()).unwrap();
        assert_eq!((r.input_tokens, r.context_length), (None, Some(4_096)));
        // Cut off mid-number: no context length rather than a shorter one.
        let cut = &api[..api.find("n_ctx: 4096").unwrap() + "n_ctx: 40".len()];
        let r = parse_rejection_in(cut, &pdt()).unwrap();
        assert_eq!(r.context_length, None);
    }

    #[test]
    fn log_files_sort_by_date_then_number() {
        let mut keys: Vec<LogKey> = [
            "2026-10-08.10.log",
            "2026-10-08.9.log",
            "2026-10-09.1.log",
            "2026-09-30.14.log",
        ]
        .iter()
        .map(|name| LogKey::parse(name).unwrap())
        .collect();
        keys.sort();
        let ns: Vec<u32> = keys.iter().map(|k| k.n).collect();
        assert_eq!(ns, [14, 9, 10, 1]);
        assert_eq!(LogKey::parse("2026-10-08.log"), None);
        assert_eq!(LogKey::parse("notes.txt"), None);
        assert_eq!(parse_month("2026-10"), Some((2026, 10)));
        assert_eq!(parse_month("vendor"), None);
    }

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        let id = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("lmstudio-monitor-serverlog-{pid}-{id}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// `<root>/YYYY-MM/<name>`, creating the folders.
    fn log_file(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(&name[..7]);
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn append(path: &Path, text: &str) {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn tailer(root: &Path) -> Tailer {
        Tailer::new(
            root.to_path_buf(),
            Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap(),
        )
    }

    fn inputs(rejections: &[ContextRejection]) -> Vec<Option<u64>> {
        rejections.iter().map(|r| r.input_tokens).collect()
    }

    #[test]
    fn first_poll_starts_at_eof() {
        let root = temp_root();
        let log = log_file(&root, "2026-10-08.4.log");
        append(&log, &format!("{}\n", REAL_REJECTIONS[3]));
        let mut t = tailer(&root);
        assert!(t.poll().is_empty(), "history was replayed");
        append(&log, &format!("{POLL_LINE}\n{}\n", REAL_REJECTIONS[2]));
        assert_eq!(inputs(&t.poll()), [Some(284_279)]);
        assert!(t.poll().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn startup_mid_line_skips_fragment() {
        let root = temp_root();
        let log = log_file(&root, "2026-10-08.4.log");
        append(&log, &format!("{POLL_LINE}\n      \"content\": \""));
        let mut t = tailer(&root);
        t.poll();
        // The rest of that body line quotes a rejection, which mustn't count.
        append(
            &log,
            &format!("{}\",\n{}\n", REAL_REJECTIONS[3], REAL_REJECTIONS[2]),
        );
        assert_eq!(inputs(&t.poll()), [Some(284_279)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn partial_line_completes_next_poll() {
        let root = temp_root();
        let log = log_file(&root, "2026-10-08.4.log");
        append(&log, &format!("{POLL_LINE}\n"));
        let mut t = tailer(&root);
        t.poll();
        let (head, tail) = REAL_REJECTIONS[3].split_at(60);
        append(&log, head);
        assert!(t.poll().is_empty());
        append(&log, &format!("{tail}\n"));
        assert_eq!(inputs(&t.poll()), [Some(359_277)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rotation_drains_old_then_new_from_zero() {
        let root = temp_root();
        let first = log_file(&root, "2026-10-08.4.log");
        append(&first, &format!("{POLL_LINE}\n"));
        let mut t = tailer(&root);
        t.poll();
        append(&first, &format!("{}\n", REAL_REJECTIONS[0]));
        let second = log_file(&root, "2026-10-08.5.log");
        append(&second, &format!("{POLL_LINE}\n{}\n", REAL_REJECTIONS[1]));
        assert_eq!(inputs(&t.poll()), [Some(523_041), Some(311_057)]);
        append(&second, &format!("{}\n", REAL_REJECTIONS[2]));
        assert_eq!(inputs(&t.poll()), [Some(284_279)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn walks_successors_numerically() {
        let root = temp_root();
        append(
            &log_file(&root, "2026-10-08.9.log"),
            &format!("{POLL_LINE}\n"),
        );
        let mut t = tailer(&root);
        t.poll();
        append(
            &log_file(&root, "2026-10-08.10.log"),
            &format!("{}\n", REAL_REJECTIONS[0]),
        );
        append(
            &log_file(&root, "2026-10-08.11.log"),
            &format!("{}\n", REAL_REJECTIONS[1]),
        );
        assert_eq!(inputs(&t.poll()), [Some(523_041), Some(311_057)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn crosses_day_and_month() {
        let root = temp_root();
        append(
            &log_file(&root, "2026-10-31.6.log"),
            &format!("{POLL_LINE}\n"),
        );
        let mut t = tailer(&root);
        t.poll();
        append(
            &log_file(&root, "2026-11-01.1.log"),
            &format!("{}\n", REAL_REJECTIONS[2]),
        );
        assert_eq!(inputs(&t.poll()), [Some(284_279)]);
        append(
            &log_file(&root, "2026-11-02.1.log"),
            &format!("{}\n", REAL_REJECTIONS[3]),
        );
        assert_eq!(inputs(&t.poll()), [Some(359_277)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncation_restarts_at_zero() {
        let root = temp_root();
        let log = log_file(&root, "2026-10-08.4.log");
        append(&log, &format!("{POLL_LINE}\n").repeat(10));
        let mut t = tailer(&root);
        t.poll();
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&log)
            .unwrap();
        append(&log, &format!("{}\n", REAL_REJECTIONS[3]));
        assert_eq!(inputs(&t.poll()), [Some(359_277)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rename_replacement_restarts_at_zero() {
        let root = temp_root();
        let log = log_file(&root, "2026-10-08.4.log");
        append(&log, &format!("{POLL_LINE}\n").repeat(3));
        let mut t = tailer(&root);
        t.poll();
        // Longer than the original, so only the inode gives the replacement away, and
        // the rejection comes before the old offset, so it's missed unless the replacement
        // is read from the start.
        let replacement = root.join("replacement");
        append(&replacement, &format!("{}\n", REAL_REJECTIONS[3]));
        append(&replacement, &format!("{POLL_LINE}\n").repeat(3));
        fs::rename(&replacement, &log).unwrap();
        assert_eq!(inputs(&t.poll()), [Some(359_277)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deleted_current_falls_back_to_newest() {
        let root = temp_root();
        let five = log_file(&root, "2026-10-08.5.log");
        append(&five, &format!("{POLL_LINE}\n"));
        let mut t = tailer(&root);
        t.poll();
        // Logs cleared, and LM Studio starts again at .1.
        fs::remove_file(&five).unwrap();
        append(
            &log_file(&root, "2026-10-08.1.log"),
            &format!("{}\n", REAL_REJECTIONS[3]),
        );
        assert_eq!(inputs(&t.poll()), [Some(359_277)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn overlong_lines_stay_bounded() {
        let root = temp_root();
        let log = log_file(&root, "2026-10-08.4.log");
        append(&log, &format!("{POLL_LINE}\n"));
        let mut t = tailer(&root);
        t.poll();
        append(&log, &"x".repeat(5_000_000));
        assert!(t.poll().is_empty());
        assert_eq!(t.lines.prefix.len(), LINE_PREFIX);
        append(&log, &format!("\n{}\n", REAL_REJECTIONS[3]));
        assert_eq!(inputs(&t.poll()), [Some(359_277)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_then_starts_at_eof() {
        let root = temp_root();
        fs::remove_dir_all(&root).unwrap();
        let mut t = tailer(&root);
        assert!(t.poll().is_empty());
        let log = log_file(&root, "2026-10-08.1.log");
        append(&log, &format!("{}\n", REAL_REJECTIONS[0]));
        assert!(
            t.poll().is_empty(),
            "the first readable listing is the starting point"
        );
        append(&log, &format!("{}\n", REAL_REJECTIONS[1]));
        assert_eq!(inputs(&t.poll()), [Some(311_057)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_root_then_new_file_from_zero() {
        let root = temp_root();
        let mut t = tailer(&root);
        assert!(t.poll().is_empty());
        append(
            &log_file(&root, "2026-10-08.1.log"),
            &format!("{POLL_LINE}\n{}\n", REAL_REJECTIONS[0]),
        );
        assert_eq!(inputs(&t.poll()), [Some(523_041)]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_lines_before_not_before_dropped() {
        let root = temp_root();
        let log = log_file(&root, "2026-12-01.1.log");
        append(&log, &format!("{POLL_LINE}\n"));
        // Later than 2026-10-08 15:37:38 in any time zone, earlier than 2026-12-01 12:00.
        let cutoff = Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap();
        let mut t = Tailer::new(root.clone(), cutoff);
        t.poll();
        // The last real rejection, re-stamped after the cutoff.
        let later = REAL_REJECTIONS[3].replacen("2026-10-08 15:37:38", "2026-12-01 12:00:00", 1);
        append(&log, &format!("{}\n{later}\n", REAL_REJECTIONS[3]));
        let got = t.poll();
        assert_eq!(got.len(), 1);
        assert!(got[0].at > cutoff);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn run_emits_then_stops_on_shutdown() {
        let root = temp_root();
        let log = log_file(&root, "2026-10-08.4.log");
        append(&log, &format!("{POLL_LINE}\n"));
        let mut t = tailer(&root);
        t.poll();
        let (tx, mut rx) = mpsc::channel(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_tailer(t, Duration::from_millis(10), tx, shutdown_rx));
        append(&log, &format!("{}\n", REAL_REJECTIONS[3]));
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no rejection within 5 s")
            .unwrap();
        assert_eq!(got.input_tokens, Some(359_277));
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("tailer didn't stop")
            .unwrap();
        let _ = fs::remove_dir_all(&root);
    }
}
