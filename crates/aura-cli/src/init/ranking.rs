//! Pure selection logic: which providers were sensed, the display order, and
//! the curated best-first model shortlist. All deterministic and unit-tested —
//! no terminal, network, or environment access.

use super::provider::{Provider, default_key_env, family_roots};

/// Substrings that mark an entry in a provider's model list as not a chat
/// model (embeddings, audio, images, instruct/search variants, …).
const NON_CHAT_MARKERS: &[&str] = &[
    "embedding",
    "whisper",
    "tts",
    "dall-e",
    "audio",
    "realtime",
    "moderation",
    "transcribe",
    "image",
    "davinci",
    "babbage",
    "instruct",
    "search",
];

/// Providers whose conventional key env var is set, in canonical order.
/// `is_set` is injected so tests don't touch the process environment.
pub(crate) fn sensed_providers(is_set: &dyn Fn(&str) -> bool) -> Vec<Provider> {
    Provider::ALL
        .iter()
        .copied()
        .filter(|&p| default_key_env(p).is_some_and(is_set))
        .collect()
}

/// Provider display order: sensed providers first, then the rest.
pub(crate) fn provider_display_order(sensed: &[Provider]) -> Vec<Provider> {
    let mut order: Vec<Provider> = sensed.to_vec();
    order.extend(
        Provider::ALL
            .iter()
            .copied()
            .filter(|p| !sensed.contains(p)),
    );
    order
}

/// Drop obviously-non-chat entries from a provider's model list (display
/// only; the operator can still type any model).
pub(crate) fn filter_chat_models(models: &[String]) -> Vec<String> {
    models
        .iter()
        .filter(|m| {
            let lower = m.to_lowercase();
            !NON_CHAT_MARKERS.iter().any(|marker| lower.contains(marker))
        })
        .cloned()
        .collect()
}

/// Version key for natural ("human") ordering of model ids: the sequence of
/// digit runs parsed as integers (`gpt-5.6` → `[5, 6]`, `o3` → `[3]`). Compared
/// lexicographically, a higher key means a newer model within a family.
fn natural_key(id: &str) -> Vec<u64> {
    let mut keys = Vec::new();
    let mut run = String::new();
    for ch in id.chars() {
        if ch.is_ascii_digit() {
            run.push(ch);
        } else if !run.is_empty() {
            keys.push(run.parse().unwrap_or(0));
            run.clear();
        }
    }
    if !run.is_empty() {
        keys.push(run.parse().unwrap_or(0));
    }
    keys
}

/// True when an id carries a dated/snapshot segment: any `-`-delimited segment
/// that is all digits and at least 4 long (`2024`, `20250514`, `1106`, `0125`).
/// Plain version ids (`gpt-4o`, `gpt-5.1`, `o3`) are not flagged.
fn is_dated(id: &str) -> bool {
    id.split('-')
        .any(|seg| seg.len() >= 4 && seg.bytes().all(|b| b.is_ascii_digit()))
}

/// Build the curated, best-first shortlist shown after verification. For each
/// family root (in table order) pick one representative from the live list:
/// prefer non-dated ids, then the highest `natural_key` (newest version), tie
/// broken by the shortest id (the base). Models matching no root are omitted
/// from the shortlist but remain typeable. Falls back to the chat-filtered list
/// (newest-first) when no root matches at all.
pub(crate) fn rank_shortlist(provider: Provider, models: &[String]) -> Vec<String> {
    match provider {
        // Ollama's `/api/tags` returns whatever the user installed —
        // non-canonical names with quant suffixes and aliases — so family-root
        // matching and the chat-marker filter are both unreliable (many good
        // local models are tagged `*-instruct`). Show the list as-is.
        Provider::Ollama => return models.to_vec(),
        // OpenRouter has thousands of models and opinionated users; we make no
        // recommendation. An empty shortlist makes the operator type an id.
        Provider::OpenRouter => return Vec::new(),
        _ => {}
    }
    let chat = filter_chat_models(models);
    let mut claimed = vec![false; chat.len()];
    let mut shortlist = Vec::new();

    for root in family_roots(provider) {
        let family: Vec<usize> = chat
            .iter()
            .enumerate()
            .filter(|(i, m)| !claimed[*i] && m.starts_with(root))
            .map(|(i, _)| i)
            .collect();
        if family.is_empty() {
            continue;
        }
        for &i in &family {
            claimed[i] = true;
        }
        let non_dated: Vec<usize> = family
            .iter()
            .copied()
            .filter(|&i| !is_dated(&chat[i]))
            .collect();
        let pool = if non_dated.is_empty() {
            family
        } else {
            non_dated
        };
        if let Some(&best) = pool.iter().max_by(|&&a, &&b| {
            natural_key(&chat[a])
                .cmp(&natural_key(&chat[b]))
                .then_with(|| chat[b].len().cmp(&chat[a].len()))
        }) {
            shortlist.push(chat[best].clone());
        }
    }

    if shortlist.is_empty() {
        let mut fallback = chat;
        fallback.sort_by(|a, b| {
            natural_key(b)
                .cmp(&natural_key(a))
                .then_with(|| a.len().cmp(&b.len()))
        });
        fallback.truncate(8);
        return fallback;
    }
    shortlist
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::test_support::no_keys;

    #[test]
    fn sensing_orders_found_providers_first() {
        let only_gemini = |var: &str| var == "GEMINI_API_KEY";
        let sensed = sensed_providers(&only_gemini);
        assert_eq!(sensed, vec![Provider::Gemini]);
        assert_eq!(
            provider_display_order(&sensed),
            vec![
                Provider::Gemini,
                Provider::OpenAI,
                Provider::Anthropic,
                Provider::Bedrock,
                Provider::Ollama,
                Provider::OpenRouter,
            ]
        );
    }

    #[test]
    fn sensing_none_keeps_default_order() {
        let sensed = sensed_providers(&no_keys);
        assert!(sensed.is_empty());
        assert_eq!(provider_display_order(&sensed), Provider::ALL.to_vec());
    }

    #[test]
    fn sensing_multiple_keys_preserves_provider_order() {
        let two = |var: &str| var == "OPENROUTER_API_KEY" || var == "ANTHROPIC_API_KEY";
        assert_eq!(
            sensed_providers(&two),
            vec![Provider::Anthropic, Provider::OpenRouter]
        );
    }

    #[test]
    fn chat_filter_drops_non_chat_entries() {
        let models = vec![
            "gpt-5.1".to_string(),
            "text-embedding-3-small".to_string(),
            "whisper-1".to_string(),
            "gpt-4o-audio-preview".to_string(),
        ];
        assert_eq!(filter_chat_models(&models), vec!["gpt-5.1".to_string()]);
    }

    #[test]
    fn natural_key_orders_versions() {
        assert!(natural_key("gpt-5.6") > natural_key("gpt-5.1"));
        assert!(natural_key("gpt-5.1") > natural_key("gpt-5"));
        assert!(natural_key("o4-mini") > natural_key("o3"));
        assert_eq!(natural_key("gpt-4o"), vec![4]);
        assert_eq!(natural_key("gpt-4o-2024-08-06"), vec![4, 2024, 8, 6]);
    }

    #[test]
    fn is_dated_flags_snapshots_only() {
        assert!(is_dated("gpt-4o-2024-05-13"));
        assert!(is_dated("gpt-3.5-turbo-1106"));
        assert!(is_dated("gpt-3.5-turbo-0125"));
        assert!(is_dated("claude-sonnet-4-20250514"));
        assert!(!is_dated("gpt-4o"));
        assert!(!is_dated("gpt-5.1"));
        assert!(!is_dated("o3"));
    }

    #[test]
    fn rank_shortlist_curates_openai_to_recommended_ids() {
        let models: Vec<String> = [
            "gpt-3.5-turbo",
            "gpt-4o",
            "o3",
            "o4-mini",
            "gpt-4.1",
            "gpt-5.4",
            "gpt-5.5",
            "gpt-5.5-2025-11-01", // dated snapshot of a recommended id
            "text-embedding-3-small",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        // Only the recommended gpt-5.5 is shown, and the clean id wins over its
        // dated snapshot. gpt-5.4 / o-series / 4o / 3.5 are dropped.
        assert_eq!(
            rank_shortlist(Provider::OpenAI, &models),
            vec!["gpt-5.5".to_string()]
        );
    }

    #[test]
    fn rank_shortlist_ollama_shows_installed_unfiltered() {
        // Ollama: show whatever /api/tags returns, unchanged — including
        // `*-instruct` variants the generic chat-filter would drop, and in the
        // installed order (no curation).
        let models: Vec<String> = ["qwen3:30b-a3b", "qwen2.5-coder:7b-instruct", "gpt-oss:20b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(rank_shortlist(Provider::Ollama, &models), models);
    }

    #[test]
    fn rank_shortlist_anthropic_picks_newest_dated_per_family() {
        let models: Vec<String> = [
            "claude-sonnet-4-6-20251001",
            "claude-sonnet-4-6-20251115", // newer sonnet snapshot
            "claude-opus-4-8-20251101",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Sonnet is the default (first); the newest dated id wins per family.
        assert_eq!(
            rank_shortlist(Provider::Anthropic, &models),
            vec![
                "claude-sonnet-4-6-20251115".to_string(),
                "claude-opus-4-8-20251101".to_string(),
            ]
        );
    }

    #[test]
    fn rank_shortlist_openrouter_makes_no_recommendation() {
        let models = vec![
            "openai/gpt-5".to_string(),
            "anthropic/claude-sonnet-4".to_string(),
        ];
        assert!(rank_shortlist(Provider::OpenRouter, &models).is_empty());
    }

    #[test]
    fn rank_shortlist_falls_back_when_no_family_matches() {
        let models = vec!["weird-model".to_string(), "another-thing".to_string()];
        let shortlist = rank_shortlist(Provider::OpenAI, &models);
        assert!(shortlist.contains(&"weird-model".to_string()));
        assert!(shortlist.contains(&"another-thing".to_string()));
    }
}
