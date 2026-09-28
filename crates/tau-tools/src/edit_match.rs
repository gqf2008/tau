//! Exact-then-fuzzy text matching for the `edit` tool — a port of the
//! matching half of pi's `core/tools/edit-diff.ts`.
//!
//! The other half (unified patches and display diffs) is not ported: tau's
//! [`tau_core::ToolOutput`] has no `details` channel to carry it.
//!
//! Offsets are byte offsets, where pi's are UTF-16 code units. Every offset
//! here is produced and consumed by this module, so the two never mix; the
//! behaviour differs only for a match that starts after a non-BMP character.

use unicode_normalization::UnicodeNormalization;

/// The line ending a file uses, as pi's `detectLineEnding` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n` — also the answer for a file with no newline at all.
    Lf,
    /// `\r\n`, when the first line break is a CRLF.
    Crlf,
}

/// One targeted replacement, as the model sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Text to find; must be unique in the file and not overlap other edits.
    pub old_text: String,
    /// Text to put in its place.
    pub new_text: String,
}

/// The matched base and result, both LF-normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdits {
    /// The content the edits were matched against.
    pub base_content: String,
    /// The content after the replacements.
    pub new_content: String,
}

/// The line break this content uses: the first one decides (pi's rule), and
/// a file with no `\n` at all reads as LF.
pub fn detect_line_ending(content: &str) -> LineEnding {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}

/// `\r\n` and lone `\r` both become `\n` (in that order, so a CRLF is one
/// line break, not two).
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Put the file's own line ending back.
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Crlf => text.replace('\n', "\r\n"),
        LineEnding::Lf => text.to_string(),
    }
}

/// Split into lines **keeping** each line's ending (pi's
/// `splitLinesWithEndings`), so unchanged pieces can be re-joined
/// byte-for-byte. A trailing newline does not produce a further empty line.
pub fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, _) in content.match_indices('\n') {
        lines.push(&content[start..=i]);
        start = i + 1;
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

/// Normalize text for fuzzy matching, progressively: NFKC, then trailing
/// whitespace stripped per line, smart quotes folded to ASCII, unicode
/// dashes to `-`, and special spaces to a plain space.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();

    // Split on '\n', trim each line's end, join with '\n' — the trailing
    // newline survives because the split produced a final empty piece.
    let mut trimmed = String::with_capacity(nfkc.len());
    for (i, line) in nfkc.split('\n').enumerate() {
        if i > 0 {
            trimmed.push('\n');
        }
        trimmed.push_str(line.trim_end());
    }

    trimmed
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Where `old_text` is, and in which space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Whether the text was found at all.
    pub found: bool,
    /// Match start in `content_for_replacement`.
    pub index: usize,
    /// Match length in `content_for_replacement`.
    pub match_length: usize,
    /// True when the exact search failed and the normalized search found it.
    pub used_fuzzy_match: bool,
    /// The space the offsets refer to: the original content, or its
    /// fuzzy-normalized form.
    pub content_for_replacement: String,
}

/// Find `old_text` in `content`: exact first, then fuzzy. A fuzzy hit
/// reports offsets into the *normalized* content, which is what the caller
/// must then edit.
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatch {
    if let Some(index) = content.find(old_text) {
        return FuzzyMatch {
            found: true,
            index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    match fuzzy_content.find(&fuzzy_old_text) {
        Some(index) => FuzzyMatch {
            found: true,
            index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        },
        None => FuzzyMatch {
            found: false,
            index: 0,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        },
    }
}

/// How many times `old_text` occurs, counted in normalized space (pi's
/// `countOccurrences`).
fn count_occurrences(content: &str, old_text: &str) -> usize {
    normalize_for_fuzzy_match(content)
        .matches(&normalize_for_fuzzy_match(old_text))
        .count()
}

/// A matched edit, ready to apply: offsets are into the replacement base.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

/// Byte ranges of the lines of `content`, endings included.
fn get_line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = (offset, offset + line.len());
            offset = span.1;
            span
        })
        .collect()
}

/// The lines a replacement touches, as `(first, one past the last)`.
fn get_replacement_line_range(
    lines: &[(usize, usize)],
    match_index: usize,
    match_length: usize,
) -> Result<(usize, usize), String> {
    let replacement_end = match_index + match_length;

    let mut start_line = None;
    for (i, &(start, end)) in lines.iter().enumerate() {
        if match_index >= start && match_index < end {
            start_line = Some(i);
            break;
        }
    }
    let Some(start_line) = start_line else {
        return Err("Replacement range is outside the base content.".into());
    };

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].1 < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".into());
    }
    Ok((start_line, end_line + 1))
}

/// Apply replacements back to front so earlier offsets stay valid.
fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let start = replacement.match_index - offset;
        result.replace_range(
            start..start + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

/// Apply replacements matched against `base_content` while copying
/// *untouched* lines back from `original_content` verbatim.
///
/// This is what keeps a fuzzy edit from rewriting the whole file: only the
/// lines a replacement actually touches are taken from the normalized base,
/// so trailing whitespace, quotes and dashes elsewhere survive as written.
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .into(),
        );
    }

    // Group replacements whose line ranges touch, so overlapping groups are
    // rewritten together from the base.
    let mut groups: Vec<(usize, usize, Vec<&MatchedEdit>)> = Vec::new();
    let mut sorted: Vec<&MatchedEdit> = replacements.iter().collect();
    sorted.sort_by_key(|r| r.match_index);
    for replacement in sorted {
        let (start_line, end_line) = get_replacement_line_range(
            &base_lines,
            replacement.match_index,
            replacement.match_length,
        )?;
        match groups.last_mut() {
            Some(current) if start_line < current.1 => {
                current.1 = current.1.max(end_line);
                current.2.push(replacement);
            }
            _ => groups.push((start_line, end_line, vec![replacement])),
        }
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for (start_line, end_line, group) in groups {
        result.extend(
            original_lines[original_line_index..start_line]
                .iter()
                .copied(),
        );

        let group_start = base_lines[start_line].0;
        let group_end = base_lines[end_line - 1].1;
        let owned: Vec<MatchedEdit> = group.into_iter().cloned().collect();
        result.push_str(&apply_replacements(
            &base_content[group_start..group_end],
            &owned,
            group_start,
        ));
        original_line_index = end_line;
    }
    result.extend(original_lines[original_line_index..].iter().copied());
    Ok(result)
}

fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including \
             all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly \
             including all whitespace and newlines."
        )
    }
}

fn duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. \
             Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must \
             be unique. Please provide more context to make it unique."
        )
    }
}

fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might \
             indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

/// Apply one or more replacements to LF-normalized content.
///
/// Every edit is matched against the same original content, not against the
/// result of the previous one; a replacement is an error when its text is
/// missing, occurs more than once, or overlaps another edit's match.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEdits, String> {
    let edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(empty_old_text_error(path, i, edits.len()));
        }
    }

    // One fuzzy edit puts the whole operation into normalized space.
    let used_fuzzy_match = edits
        .iter()
        .any(|edit| fuzzy_find_text(normalized_content, &edit.old_text).used_fuzzy_match);
    let replacement_base = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched: Vec<MatchedEdit> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let found = fuzzy_find_text(&replacement_base, &edit.old_text);
        if !found.found {
            return Err(not_found_error(path, i, edits.len()));
        }
        let occurrences = count_occurrences(&replacement_base, &edit.old_text);
        if occurrences > 1 {
            return Err(duplicate_error(path, i, edits.len(), occurrences));
        }
        matched.push(MatchedEdit {
            edit_index: i,
            match_index: found.index,
            match_length: found.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched.sort_by_key(|m| m.match_index);
    for pair in matched.windows(2) {
        let (previous, current) = (&pair[0], &pair[1]);
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target \
                 disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base,
            &matched,
        )?
    } else {
        apply_replacements(&replacement_base, &matched, 0)
    };

    if base_content == new_content {
        return Err(no_change_error(path, edits.len()));
    }
    Ok(AppliedEdits {
        base_content,
        new_content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old_text: &str, new_text: &str) -> Edit {
        Edit {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }

    #[test]
    fn the_first_line_break_decides_the_ending() {
        assert_eq!(detect_line_ending("a\nb\r\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\r\nb\r\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("no newline"), LineEnding::Lf);
        assert_eq!(detect_line_ending(""), LineEnding::Lf);
        // A lone CR is not a line ending pi looks for.
        assert_eq!(detect_line_ending("a\rb"), LineEnding::Lf);
    }

    #[test]
    fn normalization_and_restoring_round_trip() {
        assert_eq!(normalize_to_lf("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(
            restore_line_endings("a\nb", LineEnding::Crlf),
            "a\r\nb".to_string()
        );
        assert_eq!(
            restore_line_endings("a\nb", LineEnding::Lf),
            "a\nb".to_string()
        );
    }

    #[test]
    fn line_splitting_keeps_the_endings() {
        assert_eq!(split_lines_with_endings("a\nb"), vec!["a\n", "b"]);
        assert_eq!(split_lines_with_endings("a\nb\n"), vec!["a\n", "b\n"]);
        assert_eq!(split_lines_with_endings(""), Vec::<&str>::new());
        assert_eq!(split_lines_with_endings("\n"), vec!["\n"]);
    }

    #[test]
    fn fuzzy_normalization_folds_what_models_mangle() {
        assert_eq!(normalize_for_fuzzy_match("a   \nb\t\n"), "a\nb\n");
        assert_eq!(
            normalize_for_fuzzy_match("it's “quoted”"),
            "it's \"quoted\""
        );
        assert_eq!(normalize_for_fuzzy_match("a–b—c−d"), "a-b-c-d");
        assert_eq!(normalize_for_fuzzy_match("a\u{a0}b\u{3000}c"), "a b c");
        // NFKC: full-width forms fold to ASCII.
        assert_eq!(normalize_for_fuzzy_match("（ｘ）"), "(x)");
        assert_eq!(normalize_for_fuzzy_match("ｈｅｌｌｏ"), "hello");
        // Indentation is not whitespace at a line's end, so it survives.
        assert_eq!(normalize_for_fuzzy_match("  indented"), "  indented");
    }

    #[test]
    fn an_exact_match_wins_over_a_fuzzy_one() {
        let found = fuzzy_find_text("let x = 1;\n", "let x = 1;");
        assert!(found.found && !found.used_fuzzy_match);
        assert_eq!(found.content_for_replacement, "let x = 1;\n");
    }

    #[test]
    fn a_fuzzy_match_reports_offsets_into_normalized_content() {
        // The model's oldText carries trailing whitespace the file does not:
        // the exact search fails, the normalized one finds it.
        let content = "let x = 1;\nlet y = 2;\n";
        let found = fuzzy_find_text(content, "let x = 1;  ");
        assert!(found.found && found.used_fuzzy_match);
        // The offsets are the caller's contract: they index the returned
        // content, not the file, and the returned content is the normalized
        // one — which is what the caller must then edit.
        assert_eq!(
            &found.content_for_replacement[found.index..found.index + found.match_length],
            "let x = 1;"
        );
        assert_eq!(found.content_for_replacement, content);
        // The exact path is the one that keeps the file's own bytes.
        let exact = fuzzy_find_text(content, "let y = 2;");
        assert!(!exact.used_fuzzy_match);
        assert_eq!(exact.content_for_replacement, content);
    }

    #[test]
    fn a_missing_text_is_not_found_in_either_space() {
        let found = fuzzy_find_text("hello\n", "goodbye");
        assert!(!found.found);
        assert_eq!(found.match_length, 0);
    }

    #[test]
    fn a_single_edit_replaces_once() {
        let applied =
            apply_edits_to_normalized_content("a\nb\nc\n", &[edit("b", "B")], "f.txt").unwrap();
        assert_eq!(applied.base_content, "a\nb\nc\n");
        assert_eq!(applied.new_content, "a\nB\nc\n");
    }

    #[test]
    fn several_edits_apply_against_the_same_original() {
        let applied = apply_edits_to_normalized_content(
            "one\ntwo\nthree\n",
            &[edit("three", "3"), edit("one", "1")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(applied.new_content, "1\ntwo\n3\n");
    }

    #[test]
    fn an_empty_old_text_is_rejected_before_anything_else() {
        let error =
            apply_edits_to_normalized_content("a\n", &[edit("", "x")], "f.txt").unwrap_err();
        assert_eq!(error, "oldText must not be empty in f.txt.");

        let error =
            apply_edits_to_normalized_content("a\n", &[edit("a", "b"), edit("", "x")], "f.txt")
                .unwrap_err();
        assert_eq!(error, "edits[1].oldText must not be empty in f.txt.");
    }

    #[test]
    fn a_missing_text_reports_the_single_or_multi_edit_message() {
        let error =
            apply_edits_to_normalized_content("a\n", &[edit("zz", "x")], "f.txt").unwrap_err();
        assert_eq!(
            error,
            "Could not find the exact text in f.txt. The old text must match exactly including \
             all whitespace and newlines."
        );

        let error =
            apply_edits_to_normalized_content("a\n", &[edit("a", "b"), edit("zz", "x")], "f.txt")
                .unwrap_err();
        assert_eq!(
            error,
            "Could not find edits[1] in f.txt. The oldText must match exactly including all \
             whitespace and newlines."
        );
    }

    #[test]
    fn a_repeated_text_reports_how_often_it_occurred() {
        let error =
            apply_edits_to_normalized_content("x\ny\nx\n", &[edit("x", "z")], "f.txt").unwrap_err();
        assert_eq!(
            error,
            "Found 2 occurrences of the text in f.txt. The text must be unique. Please provide \
             more context to make it unique."
        );

        let error = apply_edits_to_normalized_content(
            "x\ny\nx\n",
            &[edit("y", "Y"), edit("x", "z")],
            "f.txt",
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Found 2 occurrences of edits[1] in f.txt. Each oldText must be unique. Please \
             provide more context to make it unique."
        );
    }

    #[test]
    fn duplicates_are_counted_in_normalized_space() {
        // The two occurrences differ only by trailing whitespace, so the
        // fuzzy search sees two — pi counts occurrences the same way.
        let error = apply_edits_to_normalized_content("x   \ny\nx\n", &[edit("x", "z")], "f.txt")
            .unwrap_err();
        assert!(error.starts_with("Found 2 occurrences"), "{error}");
    }

    #[test]
    fn overlapping_edits_name_both_indices() {
        let error = apply_edits_to_normalized_content(
            "abcdef\n",
            &[edit("abcd", "X"), edit("cdef", "Y")],
            "f.txt",
        )
        .unwrap_err();
        assert_eq!(
            error,
            "edits[0] and edits[1] overlap in f.txt. Merge them into one edit or target disjoint \
             regions."
        );
    }

    #[test]
    fn adjacent_edits_are_not_overlaps() {
        let applied = apply_edits_to_normalized_content(
            "abcdef\n",
            &[edit("ab", "X"), edit("cd", "Y")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(applied.new_content, "XYef\n");
    }

    #[test]
    fn a_replacement_that_changes_nothing_is_an_error() {
        let error =
            apply_edits_to_normalized_content("a\n", &[edit("a", "a")], "f.txt").unwrap_err();
        assert_eq!(
            error,
            "No changes made to f.txt. The replacement produced identical content. This might \
             indicate an issue with special characters or the text not existing as expected."
        );

        let error =
            apply_edits_to_normalized_content("ab\n", &[edit("a", "a"), edit("b", "b")], "f.txt")
                .unwrap_err();
        assert_eq!(
            error,
            "No changes made to f.txt. The replacements produced identical content."
        );
    }

    #[test]
    fn edits_are_matched_against_the_original_not_incrementally() {
        // The second edit's text only exists *after* the first applied.
        let error =
            apply_edits_to_normalized_content("a\n", &[edit("a", "b"), edit("b", "c")], "f.txt")
                .unwrap_err();
        assert!(error.starts_with("Could not find edits[1]"), "{error}");
    }

    #[test]
    fn crlf_in_old_text_matches_an_lf_file() {
        // The model sends what it read; both sides normalize before matching.
        let applied =
            apply_edits_to_normalized_content("a\nb\n", &[edit("a\r\nb", "A")], "f.txt").unwrap();
        assert_eq!(applied.new_content, "A\n");
    }

    #[test]
    fn a_fuzzy_edit_leaves_untouched_lines_byte_identical() {
        // The model writes straight quotes for text the file has in curly
        // ones, so this goes through the fuzzy path — and line 1 keeps its
        // trailing spaces and its own quotes: only the line the replacement
        // touches is taken from the normalized base.
        let content = "keep   \n“edit” this   \ntail\n";
        let applied =
            apply_edits_to_normalized_content(content, &[edit("\"edit\" this", "done")], "f.txt")
                .unwrap();
        assert_eq!(applied.new_content, "keep   \ndone\ntail\n");
    }

    #[test]
    fn an_exact_edit_keeps_the_matched_line_as_written() {
        // No fuzzy path: the surrounding bytes stay, trailing spaces and all.
        let content = "“edit” this   \n";
        let applied =
            apply_edits_to_normalized_content(content, &[edit("“edit” this", "done")], "f.txt")
                .unwrap();
        assert_eq!(applied.new_content, "done   \n");
    }

    #[test]
    fn a_fuzzy_edit_across_lines_keeps_the_rest() {
        let content = "a  \nb  \nc\n";
        let applied =
            apply_edits_to_normalized_content(content, &[edit("a\nb", "ab")], "f.txt").unwrap();
        assert_eq!(applied.new_content, "ab\nc\n");
    }

    #[test]
    fn fuzzy_matching_can_add_indentation_differences_back() {
        // oldText as the model saw it (with indentation), file normalized
        // differently: the fuzzy path still matches.
        let content = "fn main() {\n    let x = 1;\n}\n";
        let applied = apply_edits_to_normalized_content(
            content,
            &[edit("let x = 1;", "let x = 2;")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(applied.new_content, "fn main() {\n    let x = 2;\n}\n");
    }

    #[test]
    fn the_line_range_of_a_replacement_is_found() {
        let spans = get_line_spans("a\nbb\nccc");
        assert_eq!(spans, vec![(0, 2), (2, 5), (5, 8)]);
        assert_eq!(get_replacement_line_range(&spans, 0, 1).unwrap(), (0, 1));
        // "bb\n" onwards: line 1 through line 2.
        assert_eq!(get_replacement_line_range(&spans, 2, 4).unwrap(), (1, 3));
        // The last line, to the end of the content.
        assert_eq!(get_replacement_line_range(&spans, 5, 3).unwrap(), (2, 3));
        // A range that starts inside a line but runs past the content.
        assert!(get_replacement_line_range(&spans, 3, 99).is_err());
        // An index past the end of the content.
        assert!(get_replacement_line_range(&spans, 99, 1).is_err());
    }

    #[test]
    fn preserving_unchanged_lines_needs_matching_line_counts() {
        let error =
            apply_replacements_preserving_unchanged_lines("a\n", "a\nb\n", &[]).unwrap_err();
        assert_eq!(
            error,
            "Cannot preserve unchanged lines because the base content has a different line count."
        );
    }
}
