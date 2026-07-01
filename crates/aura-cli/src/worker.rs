//! Worker overview shown in the CLI startup banner. Always compiled (not
//! behind `standalone-cli`) so the HTTP-only build can parse the `workers`
//! field from `/v1/models`.

use std::borrow::Cow;
use std::collections::HashMap;

use serde::Deserialize;

use crate::theme::{AuraStyle, Themed};

/// One worker row shown in the startup banner.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerOverview {
    pub name: String,
    pub description: String,
    /// Present only when the worker overrides the coordinator model.
    #[serde(default)]
    pub model: Option<String>,
}

/// Worker overviews for the banner: a model -> workers map, plus the model a
/// request without an explicit one routes to (`None` when such a request would
/// be rejected, so nothing is shown).
#[derive(Debug, Default, Clone)]
pub struct WorkerOverviews {
    pub by_model: HashMap<String, Vec<WorkerOverview>>,
    pub default_model: Option<String>,
    /// The backend routes every request to `default_model` regardless of the
    /// requested model (single-config passthrough).
    pub routes_any_model: bool,
}

impl WorkerOverviews {
    /// Workers for `model`. With `routes_any_model`, the requested model is
    /// ignored and `default_model` wins. Otherwise `None` resolves
    /// `default_model` — deliberately NOT "the sole visible model", since a lone
    /// visible entry can coexist with hidden configs that make a model-less
    /// request fail.
    pub fn resolve(&self, model: Option<&str>) -> Vec<WorkerOverview> {
        let key = if self.routes_any_model {
            self.default_model.as_deref()
        } else {
            model.or(self.default_model.as_deref())
        };
        key.and_then(|k| self.by_model.get(k).cloned())
            .unwrap_or_default()
    }
}

/// Indent for worker rows under the "Workers" heading.
const INDENT: &str = "  ";

/// Styled lines for the worker block; empty when there are no workers. A
/// description too wide for the terminal wraps onto continuation lines that
/// hang-indent under the description.
///
/// ```text
/// Workers
///   • planner — Decomposes the task into a DAG (gpt-4o)
///   • arithmetic — Aggregates results across the fan-out and produces a
///                  single reconciled summary for the coordinator
/// ```
pub fn worker_block_lines(workers: &[WorkerOverview]) -> Vec<String> {
    if workers.is_empty() {
        return Vec::new();
    }

    let width = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);

    let mut lines = Vec::with_capacity(workers.len() + 1);
    lines.push(format!("{}", "Workers".themed(AuraStyle::Heading)));

    for w in workers {
        // annotation trails the description so it wraps with it
        let text = match &w.model {
            Some(m) => Cow::Owned(format!("{} ({m})", w.description)),
            None => Cow::Borrowed(w.description.as_str()),
        };

        // indent + bullet + name + " — "
        let prefix_len = INDENT.len() + 2 + w.name.chars().count() + 3;
        let wrapped = wrap_words(&text, width.saturating_sub(prefix_len));

        lines.push(format!(
            "{INDENT}{} {} {} {}",
            "•".themed(AuraStyle::Muted),
            w.name.as_str().themed(AuraStyle::Heading),
            "—".themed(AuraStyle::Muted),
            wrapped[0].as_str().themed(AuraStyle::Muted),
        ));
        let hang = " ".repeat(prefix_len);
        for cont in &wrapped[1..] {
            lines.push(format!("{hang}{}", cont.as_str().themed(AuraStyle::Muted)));
        }
    }

    lines
}

/// Word-wrap `text` to at most `width` display columns per line, breaking at
/// whitespace. A word longer than `width` takes its own line rather than being
/// split. Always returns at least one (possibly empty) line.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0;
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        if cur.is_empty() {
            cur.push_str(word);
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_len = wlen;
        }
    }
    lines.push(cur);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &str) -> WorkerOverview {
        WorkerOverview {
            name: name.to_string(),
            description: format!("{name} does things"),
            model: None,
        }
    }

    #[test]
    fn empty_workers_render_nothing() {
        assert!(worker_block_lines(&[]).is_empty());
    }

    #[test]
    fn lists_header_and_each_worker() {
        let lines = worker_block_lines(&[w("alpha"), w("beta")]);
        assert!(lines.first().unwrap().contains("Workers"));
        let body = lines[1..].join("\n");
        assert!(body.contains("alpha"));
        assert!(body.contains("beta"));
    }

    #[test]
    fn annotates_only_overridden_workers_with_model() {
        let mut over = w("planner");
        over.model = Some("gpt-4o".to_string());
        let all = worker_block_lines(&[over, w("arithmetic")]).join("\n");
        // exactly one annotation, on the overriding worker
        assert!(all.contains("(gpt-4o)"));
        assert_eq!(all.matches('(').count(), 1);
    }

    #[test]
    fn wrap_words_fits_short_text_on_one_line() {
        assert_eq!(wrap_words("a short line", 40), vec!["a short line"]);
    }

    #[test]
    fn wrap_words_breaks_at_whitespace_within_width() {
        let wrapped = wrap_words("one two three four five", 8);
        assert!(wrapped.len() > 1);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 8));
        // no word is lost or split
        assert_eq!(wrapped.join(" "), "one two three four five");
    }

    #[test]
    fn wrap_words_keeps_overlong_word_whole() {
        let wrapped = wrap_words("supercalifragilistic ok", 5);
        assert_eq!(wrapped, vec!["supercalifragilistic", "ok"]);
    }

    #[test]
    fn resolve_selects_the_right_model() {
        // explicit selection is honored as-is, regardless of default
        let single = WorkerOverviews {
            by_model: HashMap::from([("orch".to_string(), vec![w("alpha")])]),
            default_model: None,
            routes_any_model: false,
        };
        assert_eq!(single.resolve(Some("orch")).len(), 1);
        assert!(single.resolve(Some("nope")).is_empty());
        // no default advertised -> None resolves nothing, even with one entry
        // (a lone *visible* entry may coexist with hidden configs that make
        // request routing reject a model-less request)
        assert!(single.resolve(None).is_empty());

        // a real default resolves that agent's workers, even among several
        let multi = WorkerOverviews {
            by_model: HashMap::from([
                ("orch".to_string(), vec![w("alpha"), w("beta")]),
                ("solo".to_string(), Vec::new()),
            ]),
            default_model: Some("orch".to_string()),
            routes_any_model: false,
        };
        assert_eq!(multi.resolve(None).len(), 2);
        assert!(multi.resolve(Some("solo")).is_empty());

        // no default -> None resolves nothing
        let ambiguous = WorkerOverviews {
            default_model: None,
            ..multi
        };
        assert!(ambiguous.resolve(None).is_empty());
    }

    #[test]
    fn resolve_ignores_model_under_single_config_passthrough() {
        // an unmatched --model still resolves the default's workers
        let passthrough = WorkerOverviews {
            by_model: HashMap::from([("orch".to_string(), vec![w("alpha")])]),
            default_model: Some("orch".to_string()),
            routes_any_model: true,
        };
        assert_eq!(passthrough.resolve(Some("gpt-4o")).len(), 1);
        assert_eq!(passthrough.resolve(None).len(), 1);
    }
}
