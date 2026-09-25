//! Decision-model skill routing.
//!
//! Chooses which skills to preload into an agent's context by asking a System
//! One decision model (TypeSafe Jev, or a local Kev server exposing the same
//! `POST /v1/systemone` API) one independent yes/no `noul` question per skill.
//! Skill selection is multi-label, so the request fans out one `noul` per
//! skill and thresholds each calibrated `P(yes)`; a `choice` question would
//! be a softmax whose options compete and always names exactly one winner.
//!
//! Two stages form a funnel: stage 1 scores the whole catalog and is tuned
//! for recall, stage 2 rescores only the stage-1 shortlist and is tuned for
//! precision. The stages make different mistakes, so the funnel only fails
//! where both agree.
//!
//! The router never fails the agent. A stage-1 failure yields
//! [`SkillRoutingOutcome::Unavailable`] and the caller keeps the on-demand
//! `load_skill` path; a stage-2 failure degrades to the stage-1 shortlist.

use aura_config::skills::SkillName;
use aura_config::{SkillConfig, SkillRouterConfig, SkillRouterMode, SkillRouterStage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which agent a routing decision was made for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillRoutingSubject {
    Coordinator,
    Worker {
        task_id: usize,
        worker_name: Option<String>,
    },
    /// The single-agent (non-orchestrated) path.
    Agent,
}

/// The scored result of one decision-model stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageDecision {
    pub url: String,
    pub model: String,
    pub threshold: f64,
    /// Calibrated `P(load)` for every skill this stage scored.
    pub probabilities: BTreeMap<SkillName, f64>,
    /// Skills whose probability met the threshold, catalog order.
    pub selected: Vec<SkillName>,
    /// Wall-clock time of the HTTP round trip.
    pub latency_ms: u64,
    /// Latency the model server reported for its own inference.
    pub server_latency_ms: Option<f64>,
}

/// One complete routing decision, also the JSONL decision-log record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRoutingDecision {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub request_id: Option<String>,
    pub subject: SkillRoutingSubject,
    pub mode: SkillRouterMode,
    /// The text the skills were scored against.
    pub prompt: String,
    pub catalog: Vec<SkillName>,
    pub stage1: StageDecision,
    pub stage2: Option<StageDecision>,
    /// Why stage 2 was configured but did not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage2_skipped: Option<String>,
    /// The final selection.
    pub selected: Vec<SkillName>,
    pub total_latency_ms: u64,
}

/// What routing produced for one prompt.
#[derive(Debug, Clone)]
pub enum SkillRoutingOutcome {
    Routed(Box<SkillRoutingDecision>),
    /// No decision was made.
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
enum StageError {
    #[error("request to {url} failed: {source}")]
    Http { url: String, source: reqwest::Error },
    #[error("{url} returned HTTP {status}: {body}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },
    #[error("{url} returned no noul answer for skill '{skill}'")]
    MissingAnswer { url: String, skill: SkillName },
}

// ---------------------------------------------------------------------------
// System One wire types (the subset the router uses)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SystemOneRequest<'a> {
    model: &'a str,
    state: SystemOneState<'a>,
    questions: BTreeMap<&'a str, NoulQuestion>,
}

#[derive(Serialize)]
struct SystemOneState<'a> {
    user_prompt: &'a str,
}

#[derive(Serialize)]
struct NoulQuestion {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
}

#[derive(Deserialize)]
struct SystemOneResponse {
    answers: BTreeMap<String, NoulAnswer>,
    #[serde(default)]
    latency_ms: Option<f64>,
}

#[derive(Deserialize)]
struct NoulAnswer {
    #[serde(default)]
    noul: Option<f64>,
}

/// The per-skill question sent to the decision model.
///
/// The trailing "only if the prompt actually requires" clause measurably
/// reduces keyword-triggered false loads; keep it when editing.
pub fn question_for(skill: &SkillConfig) -> String {
    format!(
        "Should the '{}' skill be loaded for this prompt? It covers: {} \
         Load it only if the prompt actually requires that capability, \
         not merely because related words appear.",
        skill.name, skill.description
    )
}

/// Two-stage decision-model skill router. Cheap to share behind an `Arc`.
pub struct SkillRouter {
    config: SkillRouterConfig,
    client: reqwest::Client,
    decision_log: Option<Mutex<std::fs::File>>,
}

impl std::fmt::Debug for SkillRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillRouter")
            .field("mode", &self.config.mode)
            .field("stage1", &self.config.stage1.url)
            .field("stage2", &self.config.stage2.as_ref().map(|s| &s.url))
            .field("decision_log", &self.config.decision_log)
            .finish()
    }
}

impl SkillRouter {
    /// A decision log that cannot be opened is reported and skipped; routing
    /// itself is unaffected.
    pub fn new(config: SkillRouterConfig) -> Self {
        let decision_log = config.decision_log.as_ref().and_then(|path| {
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                Ok(file) => Some(Mutex::new(file)),
                Err(e) => {
                    tracing::warn!(
                        "Skill router: cannot open decision log {}: {e}; decisions will \
                         only be traced",
                        path.display()
                    );
                    None
                }
            }
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .unwrap_or_default();
        tracing::info!(
            "Skill router enabled (mode={:?}, stage1={} @ {}, stage2={})",
            config.mode,
            config.stage1.url,
            config.stage1.threshold,
            config
                .stage2
                .as_ref()
                .map(|s| format!("{} @ {}", s.url, s.threshold))
                .unwrap_or_else(|| "none".to_string()),
        );
        Self {
            config,
            client,
            decision_log,
        }
    }

    pub fn mode(&self) -> SkillRouterMode {
        self.config.mode
    }

    /// Score `skills` against `prompt`, log the decision, and return it.
    pub async fn route(
        &self,
        subject: SkillRoutingSubject,
        request_id: Option<&str>,
        prompt: &str,
        skills: &[SkillConfig],
    ) -> SkillRoutingOutcome {
        if skills.is_empty() {
            return SkillRoutingOutcome::Unavailable {
                reason: "no skills configured".to_string(),
            };
        }
        let started = Instant::now();

        let stage1 = match self.score_stage(&self.config.stage1, prompt, skills).await {
            Ok(stage) => stage,
            Err(e) => {
                tracing::warn!(
                    subject = ?subject,
                    "Skill router: stage 1 unavailable, keeping on-demand skill loading: {e}"
                );
                return SkillRoutingOutcome::Unavailable {
                    reason: e.to_string(),
                };
            }
        };

        let shortlist: Vec<SkillConfig> = skills
            .iter()
            .filter(|s| stage1.selected.contains(&s.name))
            .cloned()
            .collect();

        let (stage2, stage2_skipped) = match &self.config.stage2 {
            None => (None, None),
            Some(_) if shortlist.is_empty() => {
                (None, Some("stage 1 shortlist is empty".to_string()))
            }
            Some(cfg) => match self.score_stage(cfg, prompt, &shortlist).await {
                Ok(stage) => (Some(stage), None),
                Err(e) => {
                    tracing::warn!(
                        subject = ?subject,
                        "Skill router: stage 2 unavailable, using the stage 1 shortlist: {e}"
                    );
                    (None, Some(e.to_string()))
                }
            },
        };

        // Stage 2 owns the final selection when it ran; otherwise the stage-1
        // shortlist is the selection.
        let selected = stage2
            .as_ref()
            .map(|s| s.selected.clone())
            .unwrap_or_else(|| stage1.selected.clone());

        let decision = SkillRoutingDecision {
            timestamp: chrono::Utc::now(),
            request_id: request_id.map(String::from),
            subject,
            mode: self.config.mode,
            prompt: prompt.to_string(),
            catalog: skills.iter().map(|s| s.name.clone()).collect(),
            stage1,
            stage2,
            stage2_skipped,
            selected,
            total_latency_ms: started.elapsed().as_millis() as u64,
        };
        self.record(&decision);
        SkillRoutingOutcome::Routed(Box::new(decision))
    }

    /// Route and, in inject mode, render the preamble section that preloads
    /// the selected skills. `None` in shadow mode, when routing is
    /// unavailable, or when nothing was selected.
    pub async fn preload_section(
        &self,
        subject: SkillRoutingSubject,
        request_id: Option<&str>,
        prompt: &str,
        skills: &[SkillConfig],
    ) -> Option<String> {
        let SkillRoutingOutcome::Routed(decision) =
            self.route(subject, request_id, prompt, skills).await
        else {
            return None;
        };
        if self.config.mode != SkillRouterMode::Inject {
            return None;
        }
        render_preloaded_skills(&decision.selected, skills).await
    }

    async fn score_stage(
        &self,
        stage: &SkillRouterStage,
        prompt: &str,
        skills: &[SkillConfig],
    ) -> Result<StageDecision, StageError> {
        let url = format!("{}/v1/systemone", stage.url.trim_end_matches('/'));
        let questions = skills
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    NoulQuestion {
                        kind: "noul",
                        instructions: question_for(s),
                    },
                )
            })
            .collect();
        let body = SystemOneRequest {
            model: &stage.model,
            state: SystemOneState {
                user_prompt: prompt,
            },
            questions,
        };

        let mut request = self.client.post(&url).json(&body);
        if let Some(key) = &stage.api_key {
            request = request.bearer_auth(key);
        }

        let started = Instant::now();
        let response = request.send().await.map_err(|source| StageError::Http {
            url: url.clone(),
            source,
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let (body, _) = crate::string_utils::safe_truncate(&body, 300);
            return Err(StageError::Status {
                url,
                status: status.as_u16(),
                body: body.to_string(),
            });
        }
        let parsed: SystemOneResponse =
            response.json().await.map_err(|source| StageError::Http {
                url: url.clone(),
                source,
            })?;
        let latency_ms = started.elapsed().as_millis() as u64;

        let mut probabilities = BTreeMap::new();
        let mut selected = Vec::new();
        for skill in skills {
            let p = parsed
                .answers
                .get(skill.name.as_str())
                .and_then(|a| a.noul)
                .ok_or_else(|| StageError::MissingAnswer {
                    url: url.clone(),
                    skill: skill.name.clone(),
                })?;
            probabilities.insert(skill.name.clone(), p);
            if p >= stage.threshold {
                selected.push(skill.name.clone());
            }
        }

        Ok(StageDecision {
            url: stage.url.clone(),
            model: stage.model.clone(),
            threshold: stage.threshold,
            probabilities,
            selected,
            latency_ms,
            server_latency_ms: parsed.latency_ms,
        })
    }

    fn record(&self, decision: &SkillRoutingDecision) {
        let (prompt_preview, _) = crate::string_utils::safe_truncate(&decision.prompt, 120);
        tracing::info!(
            subject = ?decision.subject,
            mode = ?decision.mode,
            selected = ?decision.selected,
            shortlist = ?decision.stage1.selected,
            stage1_ms = decision.stage1.latency_ms,
            stage2_ms = decision.stage2.as_ref().map(|s| s.latency_ms),
            total_ms = decision.total_latency_ms,
            prompt = prompt_preview,
            "Skill router decision"
        );
        tracing::debug!(
            stage1 = ?decision.stage1.probabilities,
            stage2 = ?decision.stage2.as_ref().map(|s| &s.probabilities),
            "Skill router probabilities"
        );
        if let Some(log) = &self.decision_log
            && let Ok(line) = serde_json::to_string(decision)
            && let Ok(mut file) = log.lock()
            && let Err(e) = writeln!(file, "{line}")
        {
            tracing::warn!("Skill router: failed to append decision log: {e}");
        }
    }
}

/// Render the preamble section carrying the full body of each selected
/// skill, or `None` when nothing was selected or nothing could be read.
pub async fn render_preloaded_skills(
    selected: &[SkillName],
    skills: &[SkillConfig],
) -> Option<String> {
    let mut section = String::new();
    for skill in skills.iter().filter(|s| selected.contains(&s.name)) {
        let path = skill.path.join("SKILL.md");
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(e) => {
                tracing::warn!(
                    "Skill router: cannot preload skill '{}' from {}: {e}",
                    skill.name,
                    path.display()
                );
                continue;
            }
        };
        let body = crate::skill_tool::strip_frontmatter(&content).trim();
        section.push_str(&format!("\n### Skill: {}\n\n{body}\n", skill.name));
        let resources = crate::skill_tool::list_skill_resources(&skill.path).await;
        if !resources.is_empty() {
            section.push_str("\nSkill resources (fetch with `read_skill_file`):\n");
            for resource in resources {
                section.push_str(&format!("- {resource}\n"));
            }
        }
    }
    if section.is_empty() {
        return None;
    }
    Some(format!(
        "\n\n## Preloaded skills\n\n\
         The skills below were selected for this request and are already loaded. \
         Follow them without calling `load_skill` for them; `load_skill` remains \
         available for any other catalog entry.\n{section}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, description: &str, path: PathBuf) -> SkillConfig {
        SkillConfig {
            name: SkillName::new(name).unwrap(),
            description: description.to_string(),
            path,
        }
    }

    fn write_skill(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\n{body}"),
        )
        .unwrap();
        skill_dir
    }

    #[test]
    fn question_keeps_the_precision_clause() {
        let q = question_for(&skill("alpha", "Alpha things.", PathBuf::new()));
        assert!(q.starts_with("Should the 'alpha' skill be loaded"));
        assert!(q.contains("It covers: Alpha things."));
        assert!(q.contains("not merely because related words appear"));
    }

    #[tokio::test]
    async fn preload_renders_only_selected_bodies() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = write_skill(dir.path(), "alpha", "# Alpha\n\nDo alpha.");
        let b = write_skill(dir.path(), "beta", "# Beta\n\nDo beta.");
        std::fs::create_dir_all(a.join("references")).unwrap();
        std::fs::write(a.join("references/REF.md"), "ref").unwrap();
        let skills = vec![skill("alpha", "a", a), skill("beta", "b", b)];

        let section = render_preloaded_skills(&[SkillName::new("alpha").unwrap()], &skills)
            .await
            .unwrap();
        assert!(section.contains("## Preloaded skills"));
        assert!(section.contains("### Skill: alpha"));
        assert!(section.contains("Do alpha."));
        assert!(section.contains("- references/REF.md"));
        assert!(!section.contains("Do beta."));
        assert!(
            !section.contains("name: alpha"),
            "frontmatter must be stripped"
        );
    }

    #[tokio::test]
    async fn preload_is_none_when_nothing_selected() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = write_skill(dir.path(), "alpha", "body");
        let skills = vec![skill("alpha", "a", a)];
        assert!(render_preloaded_skills(&[], &skills).await.is_none());
    }

    #[tokio::test]
    async fn unreachable_stage1_is_unavailable_not_an_error() {
        let router = SkillRouter::new(SkillRouterConfig {
            mode: SkillRouterMode::Inject,
            timeout_ms: 500,
            decision_log: None,
            stage1: SkillRouterStage {
                url: "http://127.0.0.1:1".to_string(),
                model: "kev-latest".to_string(),
                threshold: 0.5,
                api_key: None,
            },
            stage2: None,
        });
        let skills = vec![skill("alpha", "a", PathBuf::new())];
        match router
            .route(SkillRoutingSubject::Agent, None, "hello", &skills)
            .await
        {
            SkillRoutingOutcome::Unavailable { reason } => {
                assert!(reason.contains("127.0.0.1:1"), "{reason}")
            }
            SkillRoutingOutcome::Routed(_) => panic!("unreachable server must not route"),
        }
        assert!(
            router
                .preload_section(SkillRoutingSubject::Agent, None, "hello", &skills)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn empty_catalog_is_unavailable() {
        let router = SkillRouter::new(SkillRouterConfig {
            mode: SkillRouterMode::Shadow,
            timeout_ms: 500,
            decision_log: None,
            stage1: SkillRouterStage {
                url: "http://127.0.0.1:1".to_string(),
                model: "kev-latest".to_string(),
                threshold: 0.5,
                api_key: None,
            },
            stage2: None,
        });
        assert!(matches!(
            router
                .route(SkillRoutingSubject::Coordinator, None, "hello", &[])
                .await,
            SkillRoutingOutcome::Unavailable { .. }
        ));
    }
}
