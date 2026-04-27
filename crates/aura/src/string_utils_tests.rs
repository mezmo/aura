#[cfg(test)]
mod tests {
    use crate::string_utils::*;

    #[test]
    fn test_truncate_for_log_ascii() {
        let (result, truncated) = truncate_for_log("hello world", 5);
        assert_eq!(result, "hello");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_no_truncation() {
        let (result, truncated) = truncate_for_log("short", 100);
        assert_eq!(result, "short");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_exact_length() {
        let (result, truncated) = truncate_for_log("hello", 5);
        assert_eq!(result, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_one_byte_over() {
        let (result, truncated) = truncate_for_log("hello", 4);
        assert_eq!(result, "hell");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_empty_string() {
        let (result, truncated) = truncate_for_log("", 10);
        assert_eq!(result, "");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_empty_string_zero_max() {
        let (result, truncated) = truncate_for_log("", 0);
        assert_eq!(result, "");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_zero_max_bytes() {
        let (result, truncated) = truncate_for_log("hello", 0);
        assert_eq!(result, "");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_single_char() {
        let (result, truncated) = truncate_for_log("a", 1);
        assert_eq!(result, "a");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_single_char_truncated() {
        let (result, truncated) = truncate_for_log("ab", 1);
        assert_eq!(result, "a");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_emoji() {
        let input = "Hello 🎉 World";
        assert_eq!(input.len(), 16);

        let (result, truncated) = truncate_for_log(input, 8);
        assert_eq!(result, "Hello ");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_emoji_boundary() {
        let input = "Hello 🎉 World";
        let (result, truncated) = truncate_for_log(input, 10);
        assert_eq!(result, "Hello 🎉");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_emoji_after() {
        let input = "Hello 🎉 World";
        let (result, truncated) = truncate_for_log(input, 11);
        assert_eq!(result, "Hello 🎉 ");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_only_emoji() {
        let input = "🎉";
        assert_eq!(input.len(), 4);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "🎉");
        assert!(!truncated);

        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result, "");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 1);
        assert_eq!(result, "");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_multiple_emojis() {
        let input = "🎉🎊🎈";
        assert_eq!(input.len(), 12);

        let (result, truncated) = truncate_for_log(input, 8);
        assert_eq!(result, "🎉🎊");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "🎉");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_emoji_at_start() {
        let input = "🎉hello";
        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "🎉");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "🎉h");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_emoji_at_end() {
        let input = "hello🎉";
        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "hello");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 8);
        assert_eq!(result, "hello");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 9);
        assert_eq!(result, "hello🎉");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_cjk() {
        let input = "日本語テスト";
        assert_eq!(input.len(), 18);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "日");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_cjk_boundary() {
        let input = "日本語テスト";
        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "日本");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_cjk_mid_character() {
        let input = "日本語";
        assert_eq!(input.len(), 9);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "日");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "日");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_cjk_single_char() {
        let input = "日";
        assert_eq!(input.len(), 3);

        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "日");
        assert!(!truncated);

        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result, "");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 1);
        assert_eq!(result, "");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_cjk_with_ascii() {
        let input = "Hello日本";
        assert_eq!(input.len(), 11);

        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "Hello");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 7);
        assert_eq!(result, "Hello");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 8);
        assert_eq!(result, "Hello日");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_mixed_unicode() {
        let input = "Hello🎉日本語";

        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "Hello");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 9);
        assert_eq!(result, "Hello🎉");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 12);
        assert_eq!(result, "Hello🎉日");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 18);
        assert_eq!(result, "Hello🎉日本語");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_two_byte_chars() {
        let input = "café";
        assert_eq!(input.len(), 5);

        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "caf");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "caf");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "café");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_cyrillic() {
        let input = "Привет";
        assert_eq!(input.len(), 12);

        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result, "П");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "П");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "Пр");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_arabic() {
        let input = "مرحبا";
        let len = input.len();

        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result.chars().count(), 1);
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, len);
        assert_eq!(result, input);
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_very_large_max() {
        let input = "hello";
        let (result, truncated) = truncate_for_log(input, usize::MAX);
        assert_eq!(result, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_large_string() {
        let input = "a".repeat(10000);
        let (result, truncated) = truncate_for_log(&input, 100);
        assert_eq!(result.len(), 100);
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_large_string_no_truncation() {
        let input = "a".repeat(100);
        let (result, truncated) = truncate_for_log(&input, 100);
        assert_eq!(result.len(), 100);
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_only_spaces() {
        let input = "     ";
        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "   ");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_newlines() {
        let input = "hello\nworld";
        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "hello");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "hello\n");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_tabs() {
        let input = "hello\tworld";
        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "hello\t");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_zero_width_joiner() {
        let input = "👨‍👩‍👧‍👦";
        let len = input.len();

        let (result, truncated) = truncate_for_log(input, len);
        assert_eq!(result, input);
        assert!(!truncated);

        let (result, truncated) = truncate_for_log(input, 5);
        assert!(truncated);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_for_log_combining_diacritics() {
        let input = "e\u{0301}";
        assert_eq!(input.len(), 3);

        let (result, truncated) = truncate_for_log(input, 1);
        assert_eq!(result, "e");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, input);
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_grapheme_clusters() {
        let input = "नमस्ते";
        let len = input.len();

        let (result, truncated) = truncate_for_log(input, len);
        assert_eq!(result, input);
        assert!(!truncated);

        let (result, _truncated) = truncate_for_log(input, 5);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_for_log_max_one_ascii() {
        let (result, truncated) = truncate_for_log("hello", 1);
        assert_eq!(result, "h");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_max_one_multibyte() {
        let (result, truncated) = truncate_for_log("日本", 1);
        assert_eq!(result, "");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_exact_emoji_boundary() {
        let input = "🎉";
        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "🎉");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_one_less_than_emoji() {
        let input = "a🎉";
        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "a");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_ascii_then_multibyte() {
        let input = "abc日";
        assert_eq!(input.len(), 6);

        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "abc");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "abc");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "abc日");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_all_multibyte_exact() {
        let input = "日本";
        assert_eq!(input.len(), 6);

        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "日本");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_tuple_structure() {
        let (s, b) = truncate_for_log("test", 2);
        assert_eq!(s, "te");
        assert!(b);

        let (s, b) = truncate_for_log("test", 10);
        assert_eq!(s, "test");
        assert!(!b);
    }
}
