//! Offline evaluation of the decision-model skill router.
//!
//! Scores a labelled prompt set against the skill catalog and router of an
//! AURA config, reports precision/recall/F1 for the final selection, and
//! sweeps both stage thresholds offline from the captured probabilities so
//! thresholds can be fitted without re-querying the models.
//!
//! ```text
//! cargo run -p aura --example skill_router_eval -- <config.toml> <cases.jsonl>
//! ```
//!
//! Each case is one JSON object per line: `{"prompt": "...", "skills": ["a", "b"]}`
//! where `skills` is the ground-truth set of skills that prompt needs (empty
//! when it needs none).

use aura::skill_router::{SkillRouter, SkillRoutingOutcome, SkillRoutingSubject};
use aura_config::skills::SkillName;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct Case {
    prompt: String,
    skills: Vec<SkillName>,
}

#[derive(Default)]
struct Counts {
    tp: usize,
    fp: usize,
    fn_: usize,
}

impl Counts {
    fn add(&mut self, expected: &BTreeSet<&SkillName>, got: &BTreeSet<&SkillName>) {
        self.tp += expected.intersection(got).count();
        self.fp += got.difference(expected).count();
        self.fn_ += expected.difference(got).count();
    }

    fn prf(&self) -> (f64, f64, f64) {
        let p = ratio(self.tp, self.tp + self.fp);
        let r = ratio(self.tp, self.tp + self.fn_);
        let f1 = if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        };
        (p, r, f1)
    }
}

fn ratio(num: usize, den: usize) -> f64 {
    if den == 0 {
        1.0
    } else {
        num as f64 / den as f64
    }
}

fn names(list: &[SkillName]) -> String {
    if list.is_empty() {
        "-".to_string()
    } else {
        list.iter()
            .map(SkillName::as_str)
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, config_path, cases_path] = args.as_slice() else {
        eprintln!("usage: skill_router_eval <config.toml> <cases.jsonl>");
        std::process::exit(2);
    };

    let configs = aura_config::load_config(config_path).expect("load config");
    let config = configs
        .into_iter()
        .next()
        .expect("config file defines an agent");
    let router_cfg = config
        .agent
        .skill_router
        .clone()
        .expect("config needs an [agent.skill_router] table");
    let skills =
        aura_config::skills::discover_skills(&config.agent.skills.local).expect("discover skills");
    let router = SkillRouter::new(router_cfg.clone());

    let cases: Vec<Case> = std::fs::read_to_string(cases_path)
        .expect("read cases")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| serde_json::from_str(l).expect("parse case line"))
        .collect();

    println!(
        "catalog: {} skills | cases: {} | stage1: {} @ {} | stage2: {}",
        skills.len(),
        cases.len(),
        router_cfg.stage1.url,
        router_cfg.stage1.threshold,
        router_cfg
            .stage2
            .as_ref()
            .map(|s| format!("{} @ {}", s.url, s.threshold))
            .unwrap_or_else(|| "none".into()),
    );
    for case in &cases {
        for name in &case.skills {
            assert!(
                skills.iter().any(|s| &s.name == name),
                "case expects unknown skill '{name}': {}",
                case.prompt
            );
        }
    }

    let mut final_counts = Counts::default();
    let mut stage1_counts = Counts::default();
    let mut decisions = Vec::new();
    let mut total_ms = 0u64;

    println!();
    for (i, case) in cases.iter().enumerate() {
        let outcome = router
            .route(SkillRoutingSubject::Agent, None, &case.prompt, &skills)
            .await;
        let decision = match outcome {
            SkillRoutingOutcome::Routed(d) => d,
            SkillRoutingOutcome::Unavailable { reason } => {
                eprintln!("case {i}: router unavailable: {reason}");
                std::process::exit(1);
            }
        };
        let expected: BTreeSet<&SkillName> = case.skills.iter().collect();
        let got: BTreeSet<&SkillName> = decision.selected.iter().collect();
        let shortlist: BTreeSet<&SkillName> = decision.stage1.selected.iter().collect();
        final_counts.add(&expected, &got);
        stage1_counts.add(&expected, &shortlist);
        total_ms += decision.total_latency_ms;

        let missed: Vec<_> = expected.difference(&got).map(|s| (*s).clone()).collect();
        let extra: Vec<_> = got.difference(&expected).map(|s| (*s).clone()).collect();
        let mark = if missed.is_empty() && extra.is_empty() {
            "ok  "
        } else {
            "MISS"
        };
        let preview: String = case.prompt.chars().take(60).collect();
        println!(
            "{mark} [{i:>2}] {:>5}ms shortlist={} selected={} missed={} extra={} | {preview}",
            decision.total_latency_ms,
            decision.stage1.selected.len(),
            names(&decision.selected),
            names(&missed),
            names(&extra),
        );
        decisions.push(decision);
    }

    let (p, r, f1) = final_counts.prf();
    let (p1, r1, _) = stage1_counts.prf();
    println!();
    println!(
        "final     precision={p:.2} recall={r:.2} f1={f1:.2}   mean latency {} ms",
        total_ms / cases.len().max(1) as u64
    );
    println!(
        "stage1    precision={p1:.2} recall={r1:.2} mean shortlist {:.1}",
        decisions
            .iter()
            .map(|d| d.stage1.selected.len())
            .sum::<usize>() as f64
            / cases.len().max(1) as f64
    );

    // Offline threshold sweeps from the captured probabilities.
    println!();
    println!("stage1 sweep (recall stage: want zero drops, small shortlist)");
    println!("  thr   drops  mean_shortlist");
    for t in (10..=70).step_by(5) {
        let thr = t as f64 / 100.0;
        let mut drops = 0;
        let mut shortlist = 0;
        for (case, d) in cases.iter().zip(&decisions) {
            for (name, p) in &d.stage1.probabilities {
                if *p >= thr {
                    shortlist += 1;
                } else if case.skills.contains(name) {
                    drops += 1;
                }
            }
        }
        println!(
            "  {thr:.2}  {drops:>5}  {:.1}",
            shortlist as f64 / cases.len().max(1) as f64
        );
    }

    if decisions.iter().any(|d| d.stage2.is_some()) {
        println!();
        println!("stage2 sweep (precision stage, over the captured stage1 shortlist)");
        println!("  thr   precision  recall  f1");
        for t in (20..=80).step_by(5) {
            let thr = t as f64 / 100.0;
            let mut counts = Counts::default();
            for (case, d) in cases.iter().zip(&decisions) {
                let expected: BTreeSet<&SkillName> = case.skills.iter().collect();
                let got: BTreeSet<&SkillName> = match &d.stage2 {
                    Some(s2) => s2
                        .probabilities
                        .iter()
                        .filter(|(_, p)| **p >= thr)
                        .map(|(n, _)| n)
                        .collect(),
                    None => d.stage1.selected.iter().collect(),
                };
                counts.add(&expected, &got);
            }
            let (p, r, f1) = counts.prf();
            println!("  {thr:.2}  {p:>9.2}  {r:>6.2}  {f1:.2}");
        }
    }
}
