//! The bounded output buffer the shell tools read into — pi's
//! `core/tools/output-accumulator.ts`.
//!
//! A command may print without limit and the model must not pay for that:
//! the buffer keeps only the tail in memory, counts lines and bytes as they
//! arrive so a notice can quote the real totals, and — from the moment the
//! output outgrows the limits — writes the *whole* stream to a temp file, so
//! truncation hides output from the model without losing it.
//!
//! What it keeps is raw bytes, decoded once at the end; pi decodes chunks
//! incrementally and counts decoded bytes. The two differ only for output
//! that is not valid UTF-8, where both sides are already lossy.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    truncate_tail,
};

/// Tail bytes kept in memory before the oldest are dropped: twice the byte
/// limit, which is all truncation ever shows (pi's `maxRollingBytes`).
const WINDOW: usize = DEFAULT_MAX_BYTES * 2;

/// Where the full output has gone, once it outgrew the limits.
#[derive(Debug)]
enum Spill {
    /// Still fits — nothing written anywhere.
    Fits,
    /// The temp file, open and being appended to.
    Open(File, PathBuf),
    /// Writing it failed; the notice says so rather than naming a file that
    /// would be missing its head.
    Failed(String),
}

/// The output as a tool should render it.
#[derive(Debug)]
pub struct Snapshot {
    /// The tail that was kept. `total_lines` and `total_bytes` are the
    /// accumulator's own totals, not the tail window's — the notice names
    /// how much output there really was.
    pub truncation: TruncationResult,
    /// Bytes in the line the output ends on: the number the partial-line
    /// notice calls `line is …`.
    pub last_line_bytes: usize,
    /// Where the whole output was written, when it outgrew the limits.
    pub full_output_path: Option<PathBuf>,
    /// Why it could not be written, when that is what happened.
    pub spill_error: Option<String>,
}

/// Incremental output buffer with the tail kept and the whole spilled.
#[derive(Debug)]
pub struct Accumulator {
    /// Everything seen so far, until it outgrows the byte limit — at which
    /// point it is written to the spill file and dropped.
    buffered: Vec<u8>,
    /// The tail of everything seen, bounded by [`WINDOW`].
    tail: Vec<u8>,
    /// Whether `tail` still starts where the output did (or right after a
    /// newline), i.e. whether its first line is whole.
    starts_at_line_boundary: bool,
    total_bytes: u64,
    completed_lines: u64,
    /// Bytes after the last newline: a non-zero value means the output ends
    /// mid-line, which counts as one more line (pi's `hasOpenLine`).
    open_line_bytes: u64,
    spill_dir: PathBuf,
    prefix: String,
    spill: Spill,
}

impl Accumulator {
    /// A buffer that spills into `spill_dir` under `prefix`, at pi's limits
    /// (2000 lines, 50KB).
    pub fn new(spill_dir: impl Into<PathBuf>, prefix: &str) -> Self {
        Self {
            buffered: Vec::new(),
            tail: Vec::new(),
            starts_at_line_boundary: true,
            total_bytes: 0,
            completed_lines: 0,
            open_line_bytes: 0,
            spill_dir: spill_dir.into(),
            prefix: prefix.to_string(),
            spill: Spill::Fits,
        }
    }

    /// Add the next chunk of output, in arrival order.
    pub fn append(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total_bytes += chunk.len() as u64;

        let mut newlines = 0u64;
        let mut last_newline = None;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'\n' {
                newlines += 1;
                last_newline = Some(index);
            }
        }
        match last_newline {
            None => self.open_line_bytes += chunk.len() as u64,
            Some(index) => {
                self.completed_lines += newlines;
                self.open_line_bytes = (chunk.len() - index - 1) as u64;
            }
        }

        self.tail.extend_from_slice(chunk);
        if self.tail.len() > WINDOW * 2 {
            self.trim_tail();
        }

        match &self.spill {
            Spill::Open(..) => self.write_spill(chunk),
            // A failed spill is not retried: the tail is what the model sees
            // either way, and a full temp directory does not fix itself.
            Spill::Failed(_) => {}
            Spill::Fits => {
                if self.outgrew_the_limits() {
                    self.start_spill();
                    self.write_spill(chunk);
                } else {
                    self.buffered.extend_from_slice(chunk);
                }
            }
        }
    }

    /// The output as it stands, truncation numbers included.
    pub fn snapshot(&self) -> Snapshot {
        let text = self.snapshot_text();
        let tail = truncate_tail(
            &text,
            TruncationOptions {
                max_lines: DEFAULT_MAX_LINES,
                max_bytes: DEFAULT_MAX_BYTES,
            },
        );

        let total_lines = self.total_lines() as usize;
        let total_bytes = self.total_bytes as usize;
        let truncated = total_lines > DEFAULT_MAX_LINES || total_bytes > DEFAULT_MAX_BYTES;
        // The tail window can be within the limits while the totals are not,
        // when output scrolled out of the window — which is the case the
        // fallback below names (pi's `??`).
        let truncated_by = if !truncated {
            None
        } else {
            tail.truncated_by
                .or(Some(if total_bytes > DEFAULT_MAX_BYTES {
                    TruncatedBy::Bytes
                } else {
                    TruncatedBy::Lines
                }))
        };

        let (full_output_path, spill_error) = match &self.spill {
            Spill::Open(_, path) => (Some(path.clone()), None),
            Spill::Failed(error) => (None, Some(error.clone())),
            Spill::Fits => (None, None),
        };
        Snapshot {
            truncation: TruncationResult {
                truncated,
                truncated_by,
                total_lines,
                total_bytes,
                max_lines: DEFAULT_MAX_LINES,
                max_bytes: DEFAULT_MAX_BYTES,
                ..tail
            },
            last_line_bytes: self.open_line_bytes as usize,
            full_output_path,
            spill_error,
        }
    }

    /// Lines so far: the completed ones plus the open one, if the output does
    /// not end on a newline.
    fn total_lines(&self) -> u64 {
        self.completed_lines + u64::from(self.open_line_bytes > 0)
    }

    fn outgrew_the_limits(&self) -> bool {
        self.total_bytes > DEFAULT_MAX_BYTES as u64 || self.total_lines() > DEFAULT_MAX_LINES as u64
    }

    /// The text to truncate: the kept tail, minus a partial first line —
    /// showing half a line the model never wrote would be worse than
    /// dropping it (pi's `getSnapshotText`).
    fn snapshot_text(&self) -> String {
        let text = String::from_utf8_lossy(&self.tail);
        if self.starts_at_line_boundary {
            return text.into_owned();
        }
        match text.find('\n') {
            Some(index) => text[index + 1..].to_string(),
            None => text.into_owned(),
        }
    }

    /// Drop the oldest bytes, keeping a whole number of lines where the cut
    /// allows it.
    fn trim_tail(&mut self) {
        let mut start = self.tail.len() - WINDOW;
        while start < self.tail.len() && (self.tail[start] & 0xc0) == 0x80 {
            start += 1;
        }
        self.starts_at_line_boundary = start == 0 || self.tail[start - 1] == b'\n';
        self.tail.drain(..start);
    }

    fn start_spill(&mut self) {
        let path = self
            .spill_dir
            .join(format!("{}-{}.log", self.prefix, unique_suffix()));
        let buffered = std::mem::take(&mut self.buffered);
        match File::create(&path).and_then(|mut file| file.write_all(&buffered).map(|()| file)) {
            Ok(file) => self.spill = Spill::Open(file, path),
            Err(error) => self.spill = Spill::Failed(error.to_string()),
        }
    }

    fn write_spill(&mut self, chunk: &[u8]) {
        if let Spill::Open(file, _) = &mut self.spill
            && let Err(error) = file.write_all(chunk)
        {
            self.spill = Spill::Failed(error.to_string());
        }
    }
}

/// A name no other tau process is about to pick: pid, clock, and a counter
/// for two spills inside the same nanosecond.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{nanos:x}{:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn accumulate(chunks: &[&[u8]], spill_dir: &Path) -> Snapshot {
        let mut accumulator = Accumulator::new(spill_dir, "tau-test");
        for chunk in chunks {
            accumulator.append(chunk);
        }
        accumulator.snapshot()
    }

    #[test]
    fn output_within_the_limits_is_kept_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = accumulate(&[b"one\ntwo\n"], tmp.path());
        assert_eq!(snapshot.truncation.content, "one\ntwo\n");
        assert!(!snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.total_lines, 2);
        assert_eq!(snapshot.truncation.total_bytes, 8);
        assert_eq!(snapshot.full_output_path, None);
    }

    #[test]
    fn an_unterminated_last_line_counts_as_one() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = accumulate(&[b"one\ntw"], tmp.path());
        assert_eq!(snapshot.truncation.total_lines, 2);
        assert_eq!(snapshot.last_line_bytes, 2);
    }

    #[test]
    fn a_chunk_that_ends_on_a_newline_leaves_no_open_line() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = accumulate(&[b"one\n", b"two\n"], tmp.path());
        assert_eq!(snapshot.truncation.total_lines, 2);
        assert_eq!(snapshot.last_line_bytes, 0);
    }

    #[test]
    fn the_line_limit_keeps_the_tail_and_counts_the_whole_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let body = (1..=3000)
            .map(|n| format!("line {n}\n"))
            .collect::<String>();
        let snapshot = accumulate(&[body.as_bytes()], tmp.path());
        let truncation = &snapshot.truncation;
        assert!(truncation.truncated);
        assert_eq!(truncation.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(truncation.total_lines, 3000);
        assert_eq!(truncation.output_lines, DEFAULT_MAX_LINES);
        assert!(truncation.content.starts_with("line 1001\n"));
        assert!(truncation.content.ends_with("line 3000"));
        // The whole stream is in the temp file, its head included.
        let path = snapshot.full_output_path.expect("spilled");
        let spilled = std::fs::read_to_string(&path).unwrap();
        assert!(spilled.starts_with("line 1\n"));
        assert_eq!(spilled, body);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn output_that_scrolled_out_of_the_window_still_reports_the_totals() {
        let tmp = tempfile::tempdir().unwrap();
        // One byte per line: the line limit is what trips, and far more than
        // the rolling window goes by.
        let body = "\n".repeat(300_000);
        let snapshot = accumulate(&[body.as_bytes()], tmp.path());
        let truncation = &snapshot.truncation;
        assert!(truncation.truncated);
        assert_eq!(truncation.total_lines, 300_000);
        assert_eq!(truncation.output_lines, DEFAULT_MAX_LINES);
        assert_eq!(truncation.total_bytes, 300_000);
    }

    #[test]
    fn the_byte_limit_can_leave_a_partial_line() {
        let tmp = tempfile::tempdir().unwrap();
        // One enormous line, still open: the byte limit keeps its tail.
        let body = "x".repeat(DEFAULT_MAX_BYTES * 2);
        let snapshot = accumulate(&[body.as_bytes()], tmp.path());
        let truncation = &snapshot.truncation;
        assert!(truncation.truncated);
        assert_eq!(truncation.truncated_by, Some(TruncatedBy::Bytes));
        assert!(truncation.last_line_partial);
        assert_eq!(truncation.output_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(truncation.output_lines, 1);
        assert_eq!(snapshot.last_line_bytes, DEFAULT_MAX_BYTES * 2);
    }

    #[test]
    fn chunks_are_merged_in_arrival_order() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = accumulate(&[b"out", b"err", b"\n"], tmp.path());
        assert_eq!(snapshot.truncation.content, "outerr\n");
    }

    #[test]
    fn a_spill_that_cannot_be_written_is_reported() {
        let missing = tempfile::tempdir().unwrap().path().join("gone");
        let body = (1..=3000)
            .map(|n| format!("line {n}\n"))
            .collect::<String>();
        let snapshot = accumulate(&[body.as_bytes()], &missing);
        assert!(snapshot.truncation.truncated);
        assert_eq!(snapshot.full_output_path, None);
        assert!(snapshot.spill_error.is_some());
    }
}
