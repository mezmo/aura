#[cfg(test)]
mod tests {
    use crate::string_utils::*;

    #[test]
    fn test_truncate_for_log_zero_max_bytes() {
        let (result, truncated) = truncate_for_log("hello", 0);
        assert_eq!(result, "");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_single_ascii_char() {
        let (result, truncated) = truncate_for_log("a", 1);
        assert_eq!(result, "a");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_two_chars_truncate_to_one() {
        let (result, truncated) = truncate_for_log("ab", 1);
        assert_eq!(result, "a");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_backs_up_to_char_boundary_on_emoji() {
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
    fn test_truncate_for_log_backs_up_to_char_boundary_on_cjk() {
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
    fn test_truncate_for_log_single_cjk_char() {
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
    fn test_truncate_for_log_backs_up_to_char_boundary_on_two_byte_utf8() {
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
        let _len = input.len();
        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result.chars().count(), 1);
        assert!(truncated);

        let len = input.len();
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
        assert_eq!(result, "a".repeat(100));
        assert!(truncated);

        let input = "a".repeat(100);
        let (result, truncated) = truncate_for_log(&input, 100);
        assert_eq!(result.len(), 100);
        assert_eq!(result, input);
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
    fn test_truncate_for_log_newline_and_tab() {
        let input = "hello\nworld";
        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "hello");
        assert!(truncated);

        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "hello\n");
        assert!(truncated);

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
    fn test_truncate_for_log_devanagari() {
        let input = "नमस्ते";
        let len = input.len();
        let (result, truncated) = truncate_for_log(input, len);
        assert_eq!(result, input);
        assert!(!truncated);

        let (result, _truncated) = truncate_for_log(input, 5);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_for_log_max_one() {
        let (result, truncated) = truncate_for_log("hello", 1);
        assert_eq!(result, "h");
        assert!(truncated);

        let (result, truncated) = truncate_for_log("日本", 1);
        assert_eq!(result, "");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_ascii_before_multibyte() {
        let input = "a🎉";
        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "a");
        assert!(truncated);

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
    fn test_truncate_for_log_two_cjk_exact() {
        let input = "日本";
        assert_eq!(input.len(), 6);
        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "日本");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_whitespace_only() {
        let input = "\t\n\r ";
        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result, "\t\n");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_carriage_return_and_null() {
        let input = "hello\rworld";
        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "hello\r");
        assert!(truncated);

        let input = "hello\0world";
        let (result, truncated) = truncate_for_log(input, 6);
        assert_eq!(result, "hello\0");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_multiple_newlines() {
        let input = "a\n\n\nb";
        let (result, truncated) = truncate_for_log(input, 3);
        assert_eq!(result, "a\n\n");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_mixed_whitespace() {
        let input = " \t\n\r";
        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, " \t\n\r");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_for_log_emoji_sequence() {
        let input = "🏴󠁧󠁢󠁥󠁮󠁧󠁿";
        let len = input.len();
        let (result, truncated) = truncate_for_log(input, len);
        assert_eq!(result, input);
        assert!(!truncated);

        let (result, truncated) = truncate_for_log(input, 5);
        assert!(truncated);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_for_log_skin_tone_modifier() {
        let input = "👋🏽";
        let len = input.len();
        let (result, truncated) = truncate_for_log(input, len);
        assert_eq!(result, input);
        assert!(!truncated);

        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "👋");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_alternating_ascii_and_multibyte() {
        let input = "a🎉b🎊c";
        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "a🎉");
        assert!(truncated);

        let input = "a日b本c";
        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "a日");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_long_ascii_string() {
        let input = "abcdefghijklmnopqrstuvwxyz";
        let (result, truncated) = truncate_for_log(input, 10);
        assert_eq!(result, "abcdefghij");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_repeated_multibyte() {
        let input = "🎉".repeat(10);
        let (result, truncated) = truncate_for_log(&input, 20);
        assert_eq!(result, "🎉".repeat(5));
        assert!(truncated);

        let input = "日".repeat(10);
        let (result, truncated) = truncate_for_log(&input, 15);
        assert_eq!(result, "日".repeat(5));
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_special_characters() {
        let input = "!@#$%^&*()";
        let (result, truncated) = truncate_for_log(input, 5);
        assert_eq!(result, "!@#$%");
        assert!(truncated);

        let input = "[]{}()<>";
        let (result, truncated) = truncate_for_log(input, 4);
        assert_eq!(result, "[]{}");
        assert!(truncated);

        let input = "\"'`";
        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result, "\"'");
        assert!(truncated);

        let input = "|||&&&~~~@@@###$$$%%%^^^***+++===___---;;;:::,,,...???!!!<<<>>>";
        let (result, truncated) = truncate_for_log(input, 10);
        assert_eq!(result, "|||&&&~~~@");
        assert!(truncated);
    }

    #[test]
    fn test_truncate_for_log_backslash_and_forward_slash() {
        let input = "\\\\\\";
        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result, "\\\\");
        assert!(truncated);

        let input = "///";
        let (result, truncated) = truncate_for_log(input, 2);
        assert_eq!(result, "//");
        assert!(truncated);
    }
}
