//! Grapheme- and width-aware text helpers for terminal display.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Truncate `text` to at most `max` display columns, appending `...` when
/// truncation occurs (so the result stays within `max` columns).
///
/// Width is measured with `unicode-width`, so double-width glyphs (CJK, many
/// emoji) count as 2 columns. Splits only on grapheme cluster boundaries, so
/// multi-scalar glyphs — ZWJ emoji sequences (👨‍👩‍👧), skin-tone modifiers
/// (👋🏽), and regional-indicator flags (🇯🇵) — are never cut mid-glyph.
pub fn truncate_with_ellipsis(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    if max < 3 {
        return ".".repeat(max);
    }
    // Reserve three columns for the ellipsis so the result fits in `max`.
    let keep = max - 3;
    let mut width = 0;
    let mut end = 0;
    for (i, grapheme) in text.grapheme_indices(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if width + w > keep {
            break;
        }
        width += w;
        end = i + grapheme.len();
    }
    format!("{}...", text[..end].trim_end())
}

/// Drop control characters (C0, C1, DEL) so text from a remote source cannot
/// carry terminal escape sequences into the display.
pub fn strip_control_chars(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Collapse every run of whitespace (newlines included) into one space and
/// trim the ends, so multi-line text renders on a single terminal row.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Greedy word-wrap of `text` into lines of at most `width` columns.
///
/// Counts `char`s, not grapheme clusters, so multi-scalar glyphs may be
/// over-counted when measuring line width.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        if current.is_empty() {
            current.push_str(word);
            current_len = word_len;
        } else if current_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
            current_len += 1 + word_len;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_len = word_len;
        }
    }

    lines.push(current);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_unchanged() {
        assert_eq!(truncate_with_ellipsis("hello", 120), "hello");
    }

    #[test]
    fn truncation_never_exceeds_max_even_below_the_ellipsis_width() {
        assert_eq!(truncate_with_ellipsis("hello", 2), "..");
        assert_eq!(truncate_with_ellipsis("hello", 0), "");
        assert_eq!(truncate_with_ellipsis("hi", 2), "hi");
    }

    #[test]
    fn truncation_does_not_leave_a_space_before_the_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello brave world", 9), "hello...");
        assert_eq!(
            truncate_with_ellipsis("hello brave world", 10),
            "hello b..."
        );
    }

    #[test]
    fn collapse_whitespace_flattens_runs_and_newlines() {
        assert_eq!(collapse_whitespace("  a\tb  \n\n c\r\n"), "a b c");
        assert_eq!(collapse_whitespace("single"), "single");
        assert_eq!(collapse_whitespace("   "), "");
    }

    #[test]
    fn long_ascii_is_truncated_with_ellipsis() {
        let s = "a".repeat(200);
        let out = truncate_with_ellipsis(&s, 120);
        assert_eq!(out.graphemes(true).count(), 120);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncation_counts_display_columns_not_scalars() {
        // Each CJK glyph is one scalar but two columns.
        let s = "漢字".repeat(20);
        let out = truncate_with_ellipsis(&s, 11);
        // 8 columns of glyphs (4 whole chars) + "..." = 11 columns.
        assert_eq!(out, "漢字漢字...");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 11);
        // A boundary that would split a wide glyph rounds down instead.
        assert_eq!(truncate_with_ellipsis(&s, 10), "漢字漢...");
    }

    #[test]
    fn does_not_split_zwj_emoji() {
        // Family emoji is a single grapheme made of multiple scalars.
        let family = "👨‍👩‍👧";
        let family_w = UnicodeWidthStr::width(family);
        // Pad with enough families to force truncation at a boundary.
        let s = family.repeat(50);
        let out = truncate_with_ellipsis(&s, 10);
        // Whole families only, as many as fit beside "...".
        let kept = (10 - 3) / family_w;
        assert_eq!(out, format!("{}...", family.repeat(kept)));
        assert!(UnicodeWidthStr::width(out.as_str()) <= 10);
    }

    #[test]
    fn does_not_split_skin_tone_or_flag() {
        let wave = "👋🏽"; // base + skin-tone modifier
        let flag = "🇯🇵"; // two regional indicators
        let wave_w = UnicodeWidthStr::width(wave);
        let s = format!("{}{}", wave.repeat(8), flag.repeat(8));
        let out = truncate_with_ellipsis(&s, 6);
        // Whole waves only, as many as fit beside "...".
        let kept = (6 - 3) / wave_w;
        assert_eq!(out, format!("{}...", wave.repeat(kept)));
    }

    #[test]
    fn strip_control_chars_removes_escape_introducers() {
        assert_eq!(
            strip_control_chars("red\x1b[31m text\x07 \u{9b}31m ok\x7f"),
            "red[31m text 31m ok"
        );
        assert_eq!(
            strip_control_chars("plain ünïcödé 漢字"),
            "plain ünïcödé 漢字"
        );
    }

    #[test]
    fn wrap_words_preserves_words() {
        let wrapped = wrap_words("one two three four five", 8);
        assert!(wrapped.len() > 1);
        assert!(wrapped.iter().all(|line| line.chars().count() <= 8));
        assert_eq!(wrapped.join(" "), "one two three four five");
    }
}
