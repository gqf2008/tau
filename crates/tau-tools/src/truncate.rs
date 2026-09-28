//! Output truncation — a port of pi's `core/tools/truncate.ts`.
//!
//! Two independent limits, lines and bytes, and whichever is hit first wins.
//! Head truncation never cuts a line in half; tail truncation may return a
//! partial *first* line, which is the only place a partial line is allowed
//! (it is the tail of the output the model asked to see, and dropping it
//! would hide the most recent bytes).

/// Line limit for file-ish output (pi's `DEFAULT_MAX_LINES`).
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Byte limit for tool output (pi's `DEFAULT_MAX_BYTES`).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Per-line cap for grep match lines (pi's `GREP_MAX_LINE_LENGTH`).
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which limit stopped the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    /// The line limit was reached first.
    Lines,
    /// The byte limit was reached first.
    Bytes,
}

/// The limits to apply. [`Default`] is pi's pair (2000 lines, 50KB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncationOptions {
    /// Maximum lines kept.
    pub max_lines: usize,
    /// Maximum bytes kept (the notice text is extra).
    pub max_bytes: usize,
}

impl Default for TruncationOptions {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// What a truncation did, and the numbers a notice needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationResult {
    /// The kept content (no notice appended — that is the caller's text).
    pub content: String,
    /// True when anything was dropped.
    pub truncated: bool,
    /// Which limit dropped it (`None` when `truncated` is false).
    pub truncated_by: Option<TruncatedBy>,
    /// Lines in the input, by [`split_lines_for_counting`].
    pub total_lines: usize,
    /// Bytes in the input.
    pub total_bytes: usize,
    /// Lines in `content`.
    pub output_lines: usize,
    /// Bytes in `content`.
    pub output_bytes: usize,
    /// True when the first kept line is itself cut (tail truncation only).
    pub last_line_partial: bool,
    /// True when the very first line alone exceeds the byte limit, so
    /// nothing could be kept without cutting it.
    pub first_line_exceeds_limit: bool,
    /// The line limit that was applied.
    pub max_lines: usize,
    /// The byte limit that was applied.
    pub max_bytes: usize,
}

/// Byte size as pi's `formatSize` prints it: `512B`, `1.5KB`, `2.0MB`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Split for counting: a trailing newline does not start a further (empty)
/// line, but an empty input has no lines at all (pi's
/// `splitLinesForCounting`).
pub fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Keep the start of `content`: whole lines only, first limit hit wins.
pub fn truncate_head(content: &str, opts: TruncationOptions) -> TruncationResult {
    let TruncationOptions {
        max_lines,
        max_bytes,
    } = opts;
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // A first line that alone blows the byte limit: nothing to keep without
    // cutting it, and head truncation does not cut lines.
    if lines[0].len() > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(i > 0); // +1 for the \n before it
        if bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        bytes += line_bytes;
    }
    if kept.len() >= max_lines && bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = kept.join("\n");
    TruncationResult {
        output_lines: kept.len(),
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the end of `content`: whole lines, except that when not even one
/// fits the first kept line is cut from the *front* of its text — the tail
/// is what the caller asked for.
pub fn truncate_tail(content: &str, opts: TruncationOptions) -> TruncationResult {
    let TruncationOptions {
        max_lines,
        max_bytes,
    } = opts;
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<String> = Vec::new(); // built tail-first, reversed below
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                // Not one whole line fits: keep the last `max_bytes` bytes of
                // it, on a character boundary.
                kept.push(truncate_str_to_bytes_from_end(line, max_bytes));
                last_line_partial = true;
            }
            break;
        }
        kept.push((*line).to_string());
        bytes += line_bytes;
    }
    if kept.len() >= max_lines && bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    kept.reverse();

    let output = kept.join("\n");
    TruncationResult {
        output_lines: kept.len(),
        output_bytes: output.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// The last `max_bytes` bytes of `s`, advanced to the next character
/// boundary so the result is valid UTF-8 (pi walks the same way forward past
/// continuation bytes).
fn truncate_str_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    let bytes = s.as_bytes();
    if bytes.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    s[start..].to_string()
}

/// Cap one line at `max_chars` **characters**, appending pi's
/// `... [truncated]` marker when it was cut.
///
/// pi counts UTF-16 code units; this counts `char`s, so an astral character
/// (an emoji) counts two there and one here — the only divergence, and only
/// for lines longer than the cap.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let cut: String = line.chars().take(max_chars).collect();
    (format!("{cut}... [truncated]"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(max_lines: usize, max_bytes: usize) -> TruncationOptions {
        TruncationOptions {
            max_lines,
            max_bytes,
        }
    }

    #[test]
    fn the_limits_are_pis() {
        assert_eq!(DEFAULT_MAX_LINES, 2000);
        assert_eq!(DEFAULT_MAX_BYTES, 51200);
        assert_eq!(GREP_MAX_LINE_LENGTH, 500);
        assert_eq!(TruncationOptions::default(), opts(2000, 51200));
    }

    #[test]
    fn counting_drops_only_the_final_newline() {
        assert_eq!(split_lines_for_counting(""), Vec::<&str>::new());
        assert_eq!(split_lines_for_counting("a"), vec!["a"]);
        assert_eq!(split_lines_for_counting("a\n"), vec!["a"]);
        assert_eq!(split_lines_for_counting("a\nb"), vec!["a", "b"]);
        // A file ending in a blank line still has that blank line...
        assert_eq!(split_lines_for_counting("a\n\n"), vec!["a", ""]);
        // ...but a lone newline is one empty line, not two.
        assert_eq!(split_lines_for_counting("\n"), vec![""]);
    }

    #[test]
    fn under_both_limits_nothing_is_dropped() {
        let r = truncate_head("a\nb", opts(2000, 51200));
        assert!(!r.truncated);
        assert_eq!(r.truncated_by, None);
        assert_eq!(r.content, "a\nb");
        assert_eq!((r.total_lines, r.output_lines), (2, 2));
        assert_eq!((r.total_bytes, r.output_bytes), (3, 3));
    }

    #[test]
    fn an_empty_input_is_not_truncated() {
        let r = truncate_head("", opts(5, 100));
        assert!(!r.truncated);
        assert_eq!(r.total_lines, 0);
        assert_eq!(r.content, "");
    }

    #[test]
    fn the_line_limit_wins_and_stops_on_a_line_boundary() {
        let r = truncate_head("a\nb\nc\nd\ne", opts(2, 51200));
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(r.content, "a\nb");
        assert_eq!((r.total_lines, r.output_lines), (5, 2));
        assert_eq!((r.total_bytes, r.output_bytes), (9, 3));
    }

    #[test]
    fn the_byte_limit_wins_and_never_keeps_half_a_line() {
        // "aaaa\nbbbb\ncccc": 4+1+4+1+4 = 14 bytes; 10 fits "aaaa\nbbbb"
        // (9 bytes) and the next line would need 14.
        let r = truncate_head("aaaa\nbbbb\ncccc", opts(2000, 10));
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(r.content, "aaaa\nbbbb");
        assert_eq!(r.output_bytes, 9);
        assert!(!r.last_line_partial);
        assert!(!r.first_line_exceeds_limit);
    }

    #[test]
    fn a_first_line_over_the_byte_limit_is_flagged_not_cut() {
        let r = truncate_head("aaaaaaaaaaaa\nb", opts(2000, 8));
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert!(r.first_line_exceeds_limit);
        assert_eq!(r.content, "");
        assert_eq!(r.output_lines, 0);
        assert_eq!(r.total_lines, 2);
    }

    #[test]
    fn a_line_that_exactly_fits_is_kept() {
        // 4 bytes, exactly the limit.
        let r = truncate_head("aaaa\nbbbb", opts(2000, 4));
        assert_eq!(r.content, "aaaa");
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[test]
    fn tail_keeps_the_end() {
        let r = truncate_tail("a\nb\nc\nd\ne", opts(2, 51200));
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(r.content, "d\ne");
        assert_eq!((r.total_lines, r.output_lines), (5, 2));
        assert!(!r.last_line_partial);
    }

    #[test]
    fn tail_bytes_keep_whole_lines_while_they_fit() {
        let r = truncate_tail("aaaa\nbbbb\ncccc", opts(2000, 9));
        assert_eq!(r.content, "bbbb\ncccc");
        assert_eq!(r.output_bytes, 9);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert!(!r.last_line_partial);
    }

    #[test]
    fn tail_cuts_the_first_kept_line_when_not_even_one_fits() {
        let r = truncate_tail("short line\n0123456789", opts(2000, 4));
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert!(r.last_line_partial);
        assert_eq!(r.content, "6789");
        assert_eq!(r.output_lines, 1);
        assert_eq!(r.output_bytes, 4);
    }

    #[test]
    fn tail_cutting_lands_on_a_character_boundary() {
        // "你好世界" is 12 bytes; 5 bytes back lands inside 世, so the cut
        // advances past it to the start of 界.
        let r = truncate_tail("你好世界", opts(2000, 5));
        assert!(r.last_line_partial);
        assert_eq!(r.content, "界");
        assert_eq!(r.output_bytes, 3);
    }

    #[test]
    fn tail_under_both_limits_is_untouched() {
        let r = truncate_tail("a\nb", TruncationOptions::default());
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb");
    }

    #[test]
    fn the_tail_wins_over_the_head_when_both_would_truncate() {
        let content = (1..=100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let head = truncate_head(&content, opts(3, 51200));
        let tail = truncate_tail(&content, opts(3, 51200));
        assert_eq!(head.content, "1\n2\n3");
        assert_eq!(tail.content, "98\n99\n100");
        assert_eq!(head.total_lines, tail.total_lines);
    }

    #[test]
    fn format_size_rounds_like_pi() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(51200), "50.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
        assert_eq!(format_size(1024 * 1024 * 3 / 2), "1.5MB");
    }

    #[test]
    fn a_line_over_the_cap_is_cut_with_the_marker() {
        let long = "x".repeat(600);
        let (cut, was_cut) = truncate_line(&long, GREP_MAX_LINE_LENGTH);
        assert!(was_cut);
        assert_eq!(cut, format!("{}... [truncated]", "x".repeat(500)));

        let exact = "y".repeat(500);
        let (kept, was_cut) = truncate_line(&exact, GREP_MAX_LINE_LENGTH);
        assert!(!was_cut);
        assert_eq!(kept, exact);
    }

    #[test]
    fn line_cutting_counts_characters_not_bytes() {
        // 600 chars, 1800 bytes: a byte-shaped cap would cut this at 166
        // chars.
        let long = "汉".repeat(600);
        let (cut, was_cut) = truncate_line(&long, GREP_MAX_LINE_LENGTH);
        assert!(was_cut);
        assert_eq!(cut.chars().count(), 500 + "... [truncated]".chars().count());
        assert!(cut.starts_with(&"汉".repeat(500)));
    }
}
