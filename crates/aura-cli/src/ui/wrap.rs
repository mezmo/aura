//! Display-width-aware, word-boundary plain-text wrapping for hand-printed
//! terminal lines (e.g. the assistant summary headline) that aren't routed
//! through termimad. termimad already wraps the markdown body correctly; this
//! is for lines we print and style ourselves.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Word-wrap `text` to at most `width` display columns per line.
///
/// - Breaks only on whitespace (word boundaries); runs of whitespace collapse
///   to a single break and never appear at the start/end of a line.
/// - Display width is measured with `unicode-width`, so double-width glyphs
///   (CJK, many emoji) count as 2 columns.
/// - A single word wider than `width` is hard-split at grapheme boundaries as a
///   last resort, so output never exceeds `width` and multi-scalar glyphs are
///   never cut.
/// - Returns no trailing spaces; an all-whitespace or empty input yields `[]`.
pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;

    for word in text.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        let sep = if cur.is_empty() { 0 } else { 1 };

        // Fits on the current line (with a separating space)?
        if !cur.is_empty() && cur_w + sep + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += sep + word_w;
            continue;
        }

        // Start a fresh line for this word.
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }

        if word_w <= width {
            cur.push_str(word);
            cur_w = word_w;
        } else {
            // Hard-split an over-long word at grapheme boundaries. Full chunks
            // become their own lines; the trailing remainder stays on the
            // current line so following words can still pack onto it.
            let mut rest = word;
            loop {
                let (chunk, tail) = take_prefix(rest, width);
                rest = tail;
                if rest.is_empty() {
                    cur.push_str(chunk);
                    cur_w = UnicodeWidthStr::width(chunk);
                    break;
                }
                lines.push(chunk.to_string());
            }
        }
    }

    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Split off the longest prefix of `s` whose display width is `<= width`,
/// always advancing by at least one grapheme cluster (so a single grapheme
/// wider than `width` is kept whole rather than cut). Returns `(prefix, rest)`.
fn take_prefix(s: &str, width: usize) -> (&str, &str) {
    let mut w = 0usize;
    let mut end = 0usize;
    for (i, g) in s.grapheme_indices(true) {
        let gw = UnicodeWidthStr::width(g);
        if end > 0 && w + gw > width {
            break;
        }
        w += gw;
        end = i + g.len();
    }
    if end == 0 {
        end = s.len();
    }
    (&s[..end], &s[end..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn widths(lines: &[String]) -> Vec<usize> {
        lines
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .collect()
    }

    #[test]
    fn short_text_is_a_single_line() {
        assert_eq!(
            wrap_plain("hello world", 80),
            vec!["hello world".to_string()]
        );
    }

    #[test]
    fn wraps_on_word_boundaries_never_mid_word() {
        // This is the screenshot bug: a long sentence must break between words.
        let s = "I'm an SRE orchestration assistant. I help triage operational \
                 issues by routing work to specialized agents for incidents, \
                 metrics, and logs.";
        let lines = wrap_plain(s, 40);
        // No line exceeds the width.
        assert!(
            widths(&lines).iter().all(|&w| w <= 40),
            "widths: {:?}",
            widths(&lines)
        );
        // Round-trips to the original word sequence (no characters lost/split).
        let rejoined = lines.join(" ");
        assert_eq!(
            rejoined.split_whitespace().collect::<Vec<_>>(),
            s.split_whitespace().collect::<Vec<_>>()
        );
        // Specifically: "routing" is never split across lines.
        assert!(lines.iter().all(|l| !l.ends_with("routin")));
    }

    #[test]
    fn empty_and_whitespace_yield_no_lines() {
        assert_eq!(wrap_plain("", 40), Vec::<String>::new());
        assert_eq!(wrap_plain("   \t  ", 40), Vec::<String>::new());
    }

    #[test]
    fn word_longer_than_width_is_hard_split() {
        let s = "supercalifragilistic and short";
        let lines = wrap_plain(s, 10);
        assert!(
            widths(&lines).iter().all(|&w| w <= 10),
            "widths: {:?}",
            widths(&lines)
        );
        // The long word's characters are all preserved across the split lines.
        let joined: String = lines.join("");
        assert!(joined.contains("supercalifragilistic"));
    }

    #[test]
    fn counts_double_width_glyphs() {
        // Four CJK chars = 8 display columns; at width 4 only two fit per line.
        let lines = wrap_plain("中文测试", 4);
        assert!(
            widths(&lines).iter().all(|&w| w <= 4),
            "widths: {:?}",
            widths(&lines)
        );
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn does_not_split_multiscalar_grapheme_when_hard_splitting() {
        // A run of family emoji (each a multi-scalar grapheme, width 2) longer
        // than the line: hard-split must fall on grapheme boundaries.
        let family = "👨‍👩‍👧";
        let s = family.repeat(5);
        let lines = wrap_plain(&s, 4);
        // Every line is whole families only (reconstructs to the original).
        let joined: String = lines.join("");
        assert_eq!(joined, s);
    }
}
