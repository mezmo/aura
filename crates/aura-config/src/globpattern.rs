use globset::{Glob, GlobMatcher};
use serde::*;

peg::parser! {
    grammar glob_parser() for str {
        // List of allowed characters in a SEP-986 tool name and the globset wildcard characters.
        // Since the literal name char set does not include the wildcard chars, there is no need
        // to support escaping and why the escape char set is not included.
        rule literal_char()    = ['a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '/' | '.']
        rule wildcard()        = "*" / "?"

        // Define the globset class of allowed characters. The globset package treats all chars
        // in the class as literals, including wildcard characters. Including globset patterns
        // results in a glob compile error.
        rule class_match()     = "[" "!"? literal_char()+ "]"

        // Define the globset alternate token. The globset package allows for multiple
        // alternate arms even though it's not listed in the examples. Each arm of the
        // alternate branch is allowed to contain glob wildcards or a character class
        // but not another altiernate (according to the crate documentation).
        rule alt_branch()      = (literal_char() / wildcard() / class_match())*
        rule alt_match()       = "{" alt_branch() ++ "," "}"

        // Each element of the patter is either a literal element (e.g. nothing but a tool name)
        // or a glob matching element. A glob element must contain at least one globset construct
        // with 0..n literal elements. For literal elements, the length is enforced to be between
        // 1 and 64 characters, per SPE-986.
        rule glob_match()      = wildcard() / class_match() / alt_match()
        rule glob_element()    = literal_char()* glob_match() (literal_char() / glob_match())*
        rule literal_element() = literal_char()+
        rule element()         = glob_element() / literal_element()

        // These define the fully quallifed glob pattern supported in Aura config. The namespace
        // pattern was split out for human clarity. Captures the matching string slices and returns
        // them. This allows the validation to also do the splitting of the values.
        rule ns_pattern() -> &'input str = ns:$element() ":" { ns }
        pub rule fq_glob_pattern() -> (Option<&'input str>, &'input str)
            =  ns:ns_pattern()? n:$element() { (ns, n) }
    }
}

/// Longest unbroken run of literal characters allowed anywhere in a pattern.
pub const MAX_LITERAL_RUN: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum GlobPatternError {
    #[error("glob pattern failed to compile: {0}")]
    GlobsetError(#[from] globset::Error),

    #[error(
        "tool name pattern has a run of {found} literal characters ending at \
         column {column}; the limit is {MAX_LITERAL_RUN}"
    )]
    LiteralRunTooLong { found: usize, column: usize },

    #[error(
        "invalid tool name pattern at column {}: expected a name character \
         (a-z A-Z 0-9 _ - . /), a wildcard (* ?), a class ([abc]), or an alternate ({{a,b}})",
        .0.location.column
    )]
    ParseError(#[from] peg::error::ParseError<peg::str::LineCol>),
}

/// Reject a literal run longer than [`MAX_LITERAL_RUN`].
///
/// The grammar bounds a pure-literal element but leaves the literal runs
/// inside a glob element unbounded, so a single wildcard anywhere would
/// otherwise lift the limit. Checking the source directly applies one rule to
/// every run, and reports the length as the cause — which the grammar's
/// character-class error cannot, since every character in an over-long run is
/// itself legal.
fn check_literal_runs(source: &str) -> Result<(), GlobPatternError> {
    let is_literal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.');
    let mut run = 0usize;
    for (i, c) in source.char_indices() {
        if is_literal(c) {
            run += 1;
            if run > MAX_LITERAL_RUN {
                return Err(GlobPatternError::LiteralRunTooLong {
                    found: run,
                    column: i + 1,
                });
            }
        } else {
            run = 0;
        }
    }
    Ok(())
}

/// A glob pattern matching MCP tool names, optionally namespace-qualified as
/// `<namespace>:<name>`.
#[derive(Debug, Clone)]
pub struct GlobPattern {
    source: String,
    name_matcher: GlobMatcher,
    ns_matcher: Option<GlobMatcher>,
}

impl GlobPattern {
    /// Compile a glob pattern from its source text.
    pub fn new(source: impl Into<String>) -> Result<Self, GlobPatternError> {
        let source = source.into();
        check_literal_runs(&source)?;
        let (parsed_ns, parsed_name) = glob_parser::fq_glob_pattern(&source)?;
        let ns_matcher = parsed_ns
            .map(Glob::new)
            .transpose()?
            .as_ref()
            .map(Glob::compile_matcher);
        let name_matcher = Glob::new(parsed_name)?.compile_matcher();
        Ok(Self {
            source,
            ns_matcher,
            name_matcher,
        })
    }

    /// The original pattern text (the wire `matched_pattern`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether a tool's namespace and name match this pattern.
    #[must_use]
    pub fn matches(&self, tool_ns: Option<&str>, tool_name: &str) -> bool {
        let ns_matches = match &self.ns_matcher {
            None => true,
            Some(matcher) => tool_ns.is_some_and(|ns| matcher.is_match(ns)),
        };
        ns_matches && self.name_matcher.is_match(tool_name)
    }
}

#[cfg(any(test, feature = "test_util"))]
impl From<&str> for GlobPattern {
    fn from(value: &str) -> Self {
        GlobPattern::new(value).expect("glob pattern to compile")
    }
}

impl PartialEq for GlobPattern {
    fn eq(&self, other: &Self) -> bool {
        self.source.eq(&other.source)
    }
}

impl Serialize for GlobPattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for GlobPattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = String::deserialize(deserializer)?;
        Self::new(source).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_parser() {
        // Shape only: length is not the grammar's to enforce, because a run
        // can span a glob element the grammar cannot bound without losing the
        // ability to say *why* it refused. `GlobPattern::new` owns the cap —
        // see `literal_run_cap_applies_with_and_without_glob_constructs`.
        let over_cap = "a".repeat(MAX_LITERAL_RUN + 1);
        let test_cases = [
            // literal tool names
            ("get_user", None, Some("get_user")),
            ("getUser", None, Some("getUser")),
            ("get-user", None, Some("get-user")),
            ("get.user", None, Some("get.user")),
            ("get/user", None, Some("get/user")),
            ("get/user", None, Some("get/user")),
            ("GetUser", None, Some("GetUser")),
            ("GET-USER", None, Some("GET-USER")),
            ("get user", None, None),
            ("+user", None, None),
            ("us+er", None, None),
            ("us\\er", None, None),
            (&over_cap, None, Some(&over_cap)),
            ("café", None, None),
            (" get_user", None, None),
            ("get_user ", None, None),
            // tool name patterns
            ("get_*", None, Some("get_*")),
            ("get_?", None, Some("get_?")),
            ("get_[?]", None, None),
            ("{a,b}", None, Some("{a,b}")),
            ("{a,b,c,d,e}", None, Some("{a,b,c,d,e}")),
            ("get_{a,b}", None, Some("get_{a,b}")),
            ("get_{a?,b*}", None, Some("get_{a?,b*}")),
            ("get_{[abc],d}", None, Some("get_{[abc],d}")),
            (
                "k?s_*_pod{s,es}_v[!23]",
                None,
                Some("k?s_*_pod{s,es}_v[!23]"),
            ),
            ("get_{{a,b},c}", None, None),
            ("get_{a, {b,c,d}}", None, None),
            ("get_[abc]", None, Some("get_[abc]")),
            ("root/**/tool", None, Some("root/**/tool")),
            (
                "get_[abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz0123456789]_user",
                None,
                Some("get_[abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz0123456789]_user"),
            ),
            // literal namespaces
            ("github:get_commit", Some("github"), Some("get_commit")),
            ("git_hub:get_commit", Some("git_hub"), Some("get_commit")),
            ("git-hub:get_commit", Some("git-hub"), Some("get_commit")),
            ("GitHub:get_commit", Some("GitHub"), Some("get_commit")),
            ("git/hub:get_commit", Some("git/hub"), Some("get_commit")),
            ("github :get_commit", None, None),
            (" github:get_commit", None, None),
            ("github: get_commit", None, None),
            ("github : get_commit", None, None),
            ("github::get_commit", None, None),
            ("git:hub:get_commit", None, None),
            ("github:", None, None),
            (":get_commit", None, None),
            // namespace patterns
            ("*:repo", Some("*"), Some("repo")),
            ("github:*", Some("github"), Some("*")),
            ("git??b:repo", Some("git??b"), Some("repo")),
            ("git??b:*repo", Some("git??b"), Some("*repo")),
            ("git{hub,lab}:repo", Some("git{hub,lab}"), Some("repo")),
            ("git[abc]*:repo", Some("git[abc]*"), Some("repo")),
            ("git[*]:repo", None, None),
            ("git\\*:repo", None, None),
        ];

        for (glob, expected_ns, expected_name) in test_cases {
            let res = glob_parser::fq_glob_pattern(glob);
            match res {
                Err(_) => {
                    assert!(
                        expected_name.is_none(),
                        "{glob} should not have passed parsing"
                    );
                }
                Ok((actual_ns, actual_name)) => {
                    let expected_name =
                        expected_name.expect("valid test globs require expected_name");
                    assert_eq!(
                        actual_ns, expected_ns,
                        "parsing {glob} did not produce the expected namespace"
                    );
                    assert_eq!(
                        actual_name, expected_name,
                        "parsing {glob} did not produce the expected name"
                    );
                }
            };
        }
    }

    /// The cap binds on every literal run, not only on a pattern that happens
    /// to contain no glob construct. A wildcard used to lift it entirely.
    #[test]
    fn literal_run_cap_applies_with_and_without_glob_constructs() {
        let at_cap = "x".repeat(MAX_LITERAL_RUN);
        let over = "x".repeat(MAX_LITERAL_RUN + 1);

        assert!(
            GlobPattern::new(&at_cap).is_ok(),
            "the cap itself is allowed"
        );
        assert!(GlobPattern::new(format!("{at_cap}*")).is_ok());

        for pattern in [
            over.clone(),
            format!("{over}*"),
            format!("*{over}"),
            format!("{over}:tool"),
            format!("tool:{over}"),
            format!("{at_cap}x*"),
        ] {
            assert!(
                matches!(
                    GlobPattern::new(&pattern),
                    Err(GlobPatternError::LiteralRunTooLong { .. })
                ),
                "{pattern:.20}… must be refused for its literal run length"
            );
        }
    }

    /// A run broken by a glob construct starts over, so two legal runs either
    /// side of a wildcard are fine even though their total exceeds the cap.
    #[test]
    fn literal_runs_are_measured_per_run_not_per_pattern() {
        let half = "x".repeat(MAX_LITERAL_RUN);
        assert!(GlobPattern::new(format!("{half}*{half}")).is_ok());
    }

    /// The message names the length, not the character set: every character
    /// in an over-long run is itself legal, so blaming the class misdirects.
    #[test]
    fn literal_run_error_names_the_length() {
        let err = GlobPattern::new("x".repeat(MAX_LITERAL_RUN + 1))
            .expect_err("an over-long run is refused");
        let message = err.to_string();
        assert!(
            message.contains("literal characters") && message.contains("limit is 64"),
            "expected a length-based message, got: {message}"
        );
    }

    #[test]
    fn glob_pattern_matches() -> Result<(), GlobPatternError> {
        assert!(GlobPattern::new("list_users")?.matches(None, "list_users"));
        assert!(GlobPattern::new("list_*")?.matches(None, "list_users"));
        assert!(GlobPattern::new("list_user?")?.matches(None, "list_users"));
        assert!(GlobPattern::new("*")?.matches(None, "list_users"));

        assert!(GlobPattern::new("*:list_users")?.matches(Some("github"), "list_users"));
        assert!(GlobPattern::new("git{hub,lab}:list_users")?.matches(Some("github"), "list_users"));

        assert!(!GlobPattern::new("*:list_users")?.matches(None, "list_users"));
        assert!(!GlobPattern::new("github:list_users")?.matches(None, "list_users"));

        Ok(())
    }

    #[test]
    fn glob_pattern_serialize() -> Result<(), GlobPatternError> {
        assert_eq!(
            serde_json::to_string(&GlobPattern::new("list_users")?).expect("json"),
            "\"list_users\""
        );

        assert_eq!(
            serde_json::to_string(&GlobPattern::new("*list_users")?).expect("json"),
            "\"*list_users\""
        );

        assert_eq!(
            serde_json::to_string(&GlobPattern::new("*:list_users")?).expect("json"),
            "\"*:list_users\""
        );

        assert_eq!(
            serde_json::to_string(&GlobPattern::new("???:*")?).expect("json"),
            "\"???:*\""
        );

        Ok(())
    }

    #[test]
    fn glob_pattern_deserialize() -> Result<(), GlobPatternError> {
        assert_eq!(
            serde_json::from_str::<GlobPattern>("\"list_users\"")
                .expect("type")
                .as_str(),
            "list_users"
        );

        assert_eq!(
            serde_json::from_str::<GlobPattern>("\"*:list_users\"")
                .expect("type")
                .as_str(),
            "*:list_users"
        );

        assert_eq!(
            serde_json::from_str::<GlobPattern>("\"github:list_users\"")
                .expect("type")
                .as_str(),
            "github:list_users"
        );

        assert!(serde_json::from_str::<GlobPattern>("\":list_users\"").is_err());
        assert!(serde_json::from_str::<GlobPattern>("\"github:\"").is_err());

        Ok(())
    }
}
