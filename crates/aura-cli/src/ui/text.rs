//! Grapheme-aware text helpers for terminal display.

use unicode_segmentation::UnicodeSegmentation;

/// Truncate `text` to at most `max` grapheme clusters, appending `...` when
/// truncation occurs (so the result stays within `max` clusters).
///
/// Counts and splits on grapheme cluster boundaries, not `char`s, so
/// multi-scalar glyphs — ZWJ emoji sequences (👨‍👩‍👧), skin-tone modifiers
/// (👋🏽), and regional-indicator flags (🇯🇵) — are never cut mid-glyph.
pub fn truncate_with_ellipsis(text: &str, max: usize) -> String {
    if text.graphemes(true).count() <= max {
        return text.to_string();
    }
    // Reserve three clusters for the ellipsis so the result fits in `max`.
    let keep = max.saturating_sub(3);
    let prefix: String = text.graphemes(true).take(keep).collect();
    format!("{prefix}...")
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
    fn long_ascii_is_truncated_with_ellipsis() {
        let s = "a".repeat(200);
        let out = truncate_with_ellipsis(&s, 120);
        assert_eq!(out.graphemes(true).count(), 120);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn does_not_split_zwj_emoji() {
        // Family emoji is a single grapheme made of multiple scalars.
        let family = "👨‍👩‍👧";
        // Pad with enough families to force truncation at a boundary.
        let s = family.repeat(50);
        let out = truncate_with_ellipsis(&s, 10);
        // Truncation point keeps 7 whole families + "...". Crucially, the
        // output must still be valid and contain only whole families.
        let body = out.strip_suffix("...").unwrap();
        assert_eq!(body, family.repeat(7));
        assert_eq!(out, format!("{}...", family.repeat(7)));
    }

    #[test]
    fn does_not_split_skin_tone_or_flag() {
        let wave = "👋🏽"; // base + skin-tone modifier
        let flag = "🇯🇵"; // two regional indicators
        let s = format!("{}{}", wave.repeat(8), flag.repeat(8));
        let out = truncate_with_ellipsis(&s, 6);
        // 3 kept clusters + ellipsis; all kept clusters are whole waves.
        assert_eq!(out, format!("{}...", wave.repeat(3)));
    }

    #[test]
    fn wrap_words_preserves_words() {
        let wrapped = wrap_words("one two three four five", 8);
        assert!(wrapped.len() > 1);
        assert!(wrapped.iter().all(|line| line.chars().count() <= 8));
        assert_eq!(wrapped.join(" "), "one two three four five");
    }
}
