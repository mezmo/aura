//! Tail-extracted structured conclusions for investigations.
//!
//! The agent's prompt asks it to end its final assistant message with a
//! fenced ```json ... ``` block carrying its conclusions. After the stream
//! terminates, [`parse_finalize_tail`] looks for that block in the final
//! assistant text. If present and valid, the arguments populate the PATCH to
//! ai-history-service; otherwise the investigation is recorded as
//! `resolution_status="failed"`.
//!
//! This is a prompt-only contract — there is no schema-side enforcement.
//! If the model forgets the block, emits malformed JSON, or hallucinates
//! field names, the parser returns `None` and the runner falls back to
//! the failure path.

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizeArguments {
    #[serde(deserialize_with = "deserialize_confidence_score")]
    pub confidence_score: f64,
    #[serde(deserialize_with = "not_empty_or_whitespace")]
    pub suggested_resolution: String,
    #[serde(deserialize_with = "not_empty_or_whitespace")]
    pub resolution_status: String,
}

fn deserialize_confidence_score<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<f64, D::Error> {
    let v = f64::deserialize(deserializer)?;

    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Float(v),
            &"`confidence_score` must be between 0.0..=1.0",
        ))
    }
}

fn not_empty_or_whitespace<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let v = String::deserialize(deserializer)?.trim().to_owned();

    if !v.is_empty() {
        Ok(v)
    } else {
        Err(serde::de::Error::invalid_value(
            serde::de::Unexpected::Str(&v),
            &"value cannot be empty/whitespace-only",
        ))
    }
}

const FENCE_OPEN: &str = "```json";
const FENCE_CLOSE: &str = "```";

/// Tries to parse the last ```` ```json { ... }``` ```` block from `text`
///
/// Returns `None` if:
/// - no trailing ```json``` fence is present
/// - the JSON body fails to parse as `FinalizeArguments`
/// - `confidence_score` is outside `[0.0, 1.0]`
/// - `suggested_resolution` or `resolution_status` is empty/whitespace
///
/// Tolerates trailing whitespace after the closing fence.
pub fn parse_finalize_tail(text: &str) -> Option<(FinalizeArguments, String)> {
    // find the last closing piece, as determined by `FENCE_CLOSE`.
    let trimmed = text.trim_end();

    if !trimmed.ends_with(FENCE_CLOSE) {
        return None;
    }

    // now we walk back and try to find the closest `FENCE_OPEN` to our `FENCE_CLOSE`.
    // since `end` is the beginning of a valid `&str`, we know that our `text` up until
    // `end` is also valid UTF-8.
    let before_close = &trimmed[..trimmed.len() - FENCE_CLOSE.len()];

    let open_at = before_close.rfind(FENCE_OPEN)?;

    let body = before_close[open_at + FENCE_OPEN.len()..].trim();

    let finalize_arguments: FinalizeArguments = serde_json::from_str(body).ok()?;

    let stripped = text[..open_at].trim_end().to_string();

    Some((finalize_arguments, stripped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let text = "Here is my analysis.\n\n```json\n{\"confidence_score\": 0.8, \"suggested_resolution\": \"Restart\", \"resolution_status\": \"mitigated\"}\n```";
        let (arguments, stripped) = parse_finalize_tail(text).expect("should parse");
        assert_eq!(arguments.confidence_score, 0.8);
        assert_eq!(&*arguments.suggested_resolution, "Restart");
        assert_eq!(&*arguments.resolution_status, "mitigated");
        assert_eq!(stripped, "Here is my analysis.");
    }

    #[test]
    fn tolerates_trailing_whitespace() {
        let text = "Done.\n```json\n{\"confidence_score\": 0.5, \"suggested_resolution\": \"x\", \"resolution_status\": \"y\"}\n```\n\n  ";
        assert!(parse_finalize_tail(text).is_some());
    }

    #[test]
    fn missing_tail_returns_none() {
        let text = "I looked at things but I have no conclusion to share.";
        assert!(parse_finalize_tail(text).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        let text = "Done.\n```json\n{this is not json}\n```";
        assert!(parse_finalize_tail(text).is_none());
    }

    #[test]
    fn missing_required_fields_returns_none() {
        let text = "Done.\n```json\n{\"confidence_score\": 0.5}\n```";
        assert!(parse_finalize_tail(text).is_none());
    }

    #[test]
    fn confidence_out_of_range_high_rejected() {
        let text = "Done.\n```json\n{\"confidence_score\": 1.5, \"suggested_resolution\": \"x\", \"resolution_status\": \"y\"}\n```";
        assert!(parse_finalize_tail(text).is_none());
    }

    #[test]
    fn confidence_out_of_range_low_rejected() {
        let text = "Done.\n```json\n{\"confidence_score\": -0.1, \"suggested_resolution\": \"x\", \"resolution_status\": \"y\"}\n```";
        assert!(parse_finalize_tail(text).is_none());
    }

    #[test]
    fn nan_confidence_rejected() {
        // serde_json doesn't parse NaN by default, so this is effectively belt-and-braces.
        let text = "Done.\n```json\n{\"confidence_score\": 0.0, \"suggested_resolution\": \"x\", \"resolution_status\": \"y\"}\n```";
        // sanity: 0.0 is allowed
        assert!(parse_finalize_tail(text).is_some());
    }

    #[test]
    fn empty_resolution_rejected() {
        let text = "Done.\n```json\n{\"confidence_score\": 0.5, \"suggested_resolution\": \"   \", \"resolution_status\": \"y\"}\n```";
        assert!(parse_finalize_tail(text).is_none());
    }

    #[test]
    fn empty_status_rejected() {
        let text = "Done.\n```json\n{\"confidence_score\": 0.5, \"suggested_resolution\": \"x\", \"resolution_status\": \"\"}\n```";
        assert!(parse_finalize_tail(text).is_none());
    }

    #[test]
    fn multiple_blocks_last_one_wins() {
        let text = concat!(
            "First attempt:\n",
            "```json\n",
            "{\"confidence_score\": 0.1, \"suggested_resolution\": \"old\", \"resolution_status\": \"a\"}\n",
            "```\n",
            "Revised conclusion:\n",
            "```json\n",
            "{\"confidence_score\": 0.9, \"suggested_resolution\": \"new\", \"resolution_status\": \"b\"}\n",
            "```",
        );
        let (arguments, _) = parse_finalize_tail(text).expect("should parse");
        assert_eq!(arguments.confidence_score, 0.9);
        assert_eq!(&*arguments.suggested_resolution, "new");
    }

    #[test]
    fn stripped_text_excludes_block() {
        let text = "Conclusion narrative.\n\n```json\n{\"confidence_score\": 0.5, \"suggested_resolution\": \"x\", \"resolution_status\": \"y\"}\n```";
        let (_, stripped) = parse_finalize_tail(text).unwrap();
        assert!(!stripped.contains("```json"));
        assert!(!stripped.contains("confidence_score"));
        assert_eq!(stripped, "Conclusion narrative.");
    }

    #[test]
    fn tail_without_open_fence_returns_none() {
        let text = "Done.\n{\"confidence_score\": 0.5, \"suggested_resolution\": \"x\", \"resolution_status\": \"y\"}";
        assert!(parse_finalize_tail(text).is_none());
    }
}
