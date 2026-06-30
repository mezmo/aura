/// Serde helper for accepting booleans as either native bool or string ("true"/"false").
///
/// Helm/Go tooling can render booleans as quoted strings in generated configs.
use serde::{Deserialize, Deserializer};

pub fn deserialize_bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrString {
        Bool(bool),
        Str(String),
    }

    match BoolOrString::deserialize(deserializer)? {
        BoolOrString::Bool(b) => Ok(b),
        BoolOrString::Str(s) => match s.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "expected \"true\" or \"false\", got \"{other}\""
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestBool {
        #[serde(deserialize_with = "deserialize_bool")]
        val: bool,
    }

    fn from_json(json: &str) -> Result<TestBool, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn accepts_native_true() {
        assert!(from_json(r#"{"val": true}"#).unwrap().val);
    }

    #[test]
    fn accepts_native_false() {
        assert!(!from_json(r#"{"val": false}"#).unwrap().val);
    }

    #[test]
    fn accepts_string_true() {
        assert!(from_json(r#"{"val": "true"}"#).unwrap().val);
    }

    #[test]
    fn accepts_string_false() {
        assert!(!from_json(r#"{"val": "false"}"#).unwrap().val);
    }

    #[test]
    fn accepts_padded_whitespace() {
        assert!(from_json(r#"{"val": "  true  "}"#).unwrap().val);
    }

    #[test]
    fn rejects_mixed_case() {
        assert!(from_json(r#"{"val": "True"}"#).is_err());
    }

    #[test]
    fn rejects_yes() {
        assert!(from_json(r#"{"val": "yes"}"#).is_err());
    }

    #[test]
    fn rejects_numeric_string() {
        assert!(from_json(r#"{"val": "1"}"#).is_err());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(from_json(r#"{"val": ""}"#).is_err());
    }

    #[test]
    fn rejects_whitespace_only() {
        assert!(from_json(r#"{"val": "   "}"#).is_err());
    }

    #[test]
    fn error_message_shows_the_bad_value() {
        let err = from_json(r#"{"val": "yes"}"#).unwrap_err();
        assert!(
            err.to_string()
                .contains("expected \"true\" or \"false\", got \"yes\"")
        );
    }
}
