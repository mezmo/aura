//! String utilities for safe text processing.

/// Truncate a string at a UTF-8 safe byte boundary.
///
/// Returns (truncated_str, was_truncated) tuple indicating whether truncation occurred.
/// Uses `floor_char_boundary` to ensure we never split a multi-byte character.
/// Alias for backward compatibility
pub use safe_truncate as truncate_for_log;

pub fn safe_truncate(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let truncate_at = s.floor_char_boundary(max_bytes);
    (&s[..truncate_at], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_ascii() {
        let (result, truncated) = safe_truncate("hello world", 5);
        assert_eq!(result, "hello");
        assert!(truncated);
    }

    #[test]
    fn test_no_truncation_needed() {
        let (result, truncated) = safe_truncate("short", 100);
        assert_eq!(result, "short");
        assert!(!truncated);
    }

    #[test]
    fn test_emoji_boundary() {
        // "Hello 🎉 World" - emoji is 4 bytes at positions 6-9
        let input = "Hello 🎉 World";
        assert_eq!(input.len(), 16); // 6 + 4 + 6 bytes

        // Truncate at byte 8 (middle of emoji) should back up to byte 6
        let (result, truncated) = safe_truncate(input, 8);
        assert_eq!(result, "Hello ");
        assert!(truncated);

        // Truncate at byte 10 (at emoji end) includes emoji but not space after
        let (result, truncated) = safe_truncate(input, 10);
        assert_eq!(result, "Hello 🎉");
        assert!(truncated);

        // Truncate at byte 11 includes the space after emoji
        let (result, truncated) = safe_truncate(input, 11);
        assert_eq!(result, "Hello 🎉 ");
        assert!(truncated);
    }

    #[test]
    fn test_empty() {
        let (result, truncated) = safe_truncate("", 10);
        assert_eq!(result, "");
        assert!(!truncated);
    }

    #[test]
    fn test_cjk_characters() {
        // Each CJK character is 3 bytes
        let input = "日本語テスト";
        assert_eq!(input.len(), 18); // 6 chars * 3 bytes

        // Truncate in middle of second character
        let (result, truncated) = safe_truncate(input, 4);
        assert_eq!(result, "日");
        assert!(truncated);

        // Truncate exactly at character boundary
        let (result, truncated) = safe_truncate(input, 6);
        assert_eq!(result, "日本");
        assert!(truncated);
    }
}
