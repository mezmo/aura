//! Local, user-readable record of every event the telemetry layer
//! processed.
//!
//! This log is written **even when telemetry is disabled**. That is the
//! property that lets a user prove to themselves that the kill switch
//! is honored: they see the event was processed, with `sent: false` and
//! a `disable_reason` field naming the rule that silenced it, while the
//! wiremock / network sink receives nothing.
//!
//! Format: JSON Lines (one event per line). Rotation: at 1000 lines the
//! current file is renamed to `events.jsonl.1` (overwriting any
//! previous `.1`) and a fresh file is started. Cadence and shape mirror
//! OpenSRE's `posthog_events.txt` so users coming from that project
//! find a familiar log.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::disable::DisableReason;

/// Default rotation cadence (mirrors OpenSRE).
pub const DEFAULT_ROTATION_LINES: usize = 1000;

/// One record on disk. The schema is part of the user-facing contract;
/// changes require a doc update in `docs/telemetry.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InspectedEvent {
    pub ts: DateTime<Utc>,
    pub event: String,
    pub properties: Value,
    pub sent: bool,
    /// `null` when telemetry is active.
    pub disable_reason: Option<String>,
}

/// Render a [`DisableReason`] for the inspection log. Stable strings so
/// users (and downstream filtering) can rely on them.
pub fn disable_reason_label(reason: &DisableReason) -> String {
    match reason {
        DisableReason::DoNotTrack => "DoNotTrack".into(),
        DisableReason::AuraDisabled => "AuraDisabled".into(),
        DisableReason::Ci(name) => format!("Ci({name})"),
        DisableReason::CargoTest => "CargoTest".into(),
        DisableReason::ConfigDisabled => "ConfigDisabled".into(),
    }
}

/// The on-disk inspection log. Cheap to `Clone` — the inner state lives
/// behind an `Arc<Mutex<…>>`.
#[derive(Clone)]
pub struct InspectionLog {
    inner: std::sync::Arc<Mutex<Inner>>,
}

struct Inner {
    path: PathBuf,
    rotation_lines: usize,
    current_lines: usize,
}

impl InspectionLog {
    /// Open (or prepare) the inspection log at `path`. Counts existing
    /// lines so the rotation budget accounts for prior runs.
    pub fn open(path: PathBuf, rotation_lines: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let current_lines = count_lines(&path)?;
        Ok(Self {
            inner: std::sync::Arc::new(Mutex::new(Inner {
                path,
                rotation_lines,
                current_lines,
            })),
        })
    }

    /// Append one record. Rotates if the file would exceed the line
    /// budget. Errors are surfaced to the caller because the inspection
    /// log is the user's audit trail; the caller decides whether to
    /// also swallow them (the production background task does).
    pub fn append(&self, event: &InspectedEvent) -> io::Result<()> {
        let mut state = self.inner.lock().expect("inspection log poisoned");
        if state.current_lines >= state.rotation_lines {
            rotate(&state.path)?;
            state.current_lines = 0;
        }
        let line = serde_json::to_string(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&state.path)?;
        writeln!(file, "{line}")?;
        state.current_lines += 1;
        Ok(())
    }

    /// Return the last `n` records, oldest-first. Reads the rotated
    /// `.1` file if the current file does not have enough lines.
    ///
    /// `n` is silently clamped to `rotation_lines * 2` — the upper
    /// bound of events the log can hold at any instant (current file
    /// up to `rotation_lines`, plus the rotated `.1` file of the same
    /// size). Without this, a caller supplying
    /// `?limit=10000000000` from the HTTP endpoint or
    /// `/telemetry recent 99999999999` from the REPL would force a
    /// huge `Vec::with_capacity` allocation and abort the process
    /// before reading the tiny on-disk log. Tests cover the clamp
    /// engaging at `usize::MAX`.
    pub fn recent(&self, n: usize) -> io::Result<Vec<InspectedEvent>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let state = self.inner.lock().expect("inspection log poisoned");
        let max_returnable = state.rotation_lines.saturating_mul(2);
        let n = n.min(max_returnable);
        let mut buf: Vec<InspectedEvent> = Vec::with_capacity(n);
        push_tail(&rotated_path(&state.path), n, &mut buf)?;
        // We over-read from .1 and trim from the front later if needed.
        push_tail(&state.path, n, &mut buf)?;
        drop(state);

        // If we now have more than n, drop oldest entries from the front.
        if buf.len() > n {
            let to_drop = buf.len() - n;
            buf.drain(0..to_drop);
        }
        Ok(buf)
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

fn rotate(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let rotated = rotated_path(path);
    if rotated.exists() {
        fs::remove_file(&rotated)?;
    }
    fs::rename(path, &rotated)?;
    Ok(())
}

fn count_lines(path: &Path) -> io::Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut n = 0;
    for line in reader.lines() {
        line?;
        n += 1;
    }
    Ok(n)
}

fn push_tail(path: &Path, n: usize, out: &mut Vec<InspectedEvent>) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut window: std::collections::VecDeque<InspectedEvent> =
        std::collections::VecDeque::with_capacity(n);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // A malformed line should not crash the inspection-log reader;
        // skip it so the user can still see surrounding context.
        if let Ok(event) = serde_json::from_str::<InspectedEvent>(&line) {
            if window.len() == n {
                window.pop_front();
            }
            window.push_back(event);
        }
    }
    out.extend(window);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use tempfile::tempdir;

    fn sample(name: &str, sent: bool, reason: Option<&str>) -> InspectedEvent {
        InspectedEvent {
            ts: Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, 0).unwrap(),
            event: name.into(),
            properties: json!({"k": "v"}),
            sent,
            disable_reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn append_creates_file_and_parent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("telemetry").join("events.jsonl");
        let log = InspectionLog::open(path.clone(), DEFAULT_ROTATION_LINES).unwrap();
        log.append(&sample("server_started", true, None)).unwrap();
        assert!(path.exists());
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
    }

    #[test]
    fn each_append_writes_one_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path.clone(), DEFAULT_ROTATION_LINES).unwrap();
        for i in 0..5 {
            log.append(&sample(&format!("evt_{i}"), true, None)).unwrap();
        }
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 5);
        for (i, line) in contents.lines().enumerate() {
            let parsed: InspectedEvent = serde_json::from_str(line).unwrap();
            assert_eq!(parsed.event, format!("evt_{i}"));
        }
    }

    #[test]
    fn disabled_record_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path.clone(), DEFAULT_ROTATION_LINES).unwrap();
        log.append(&sample("server_started", false, Some("DoNotTrack")))
            .unwrap();
        let line = fs::read_to_string(&path).unwrap();
        let parsed: InspectedEvent = serde_json::from_str(line.trim()).unwrap();
        assert!(!parsed.sent);
        assert_eq!(parsed.disable_reason.as_deref(), Some("DoNotTrack"));
    }

    #[test]
    fn rotates_at_line_budget() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path.clone(), 3).unwrap();
        for i in 0..7 {
            log.append(&sample(&format!("evt_{i}"), true, None)).unwrap();
        }
        // After the 4th write we rotated once, after the 7th we rotated
        // again. So the .1 file contains entries from the most recent
        // pre-rotation cycle (3 lines) and the main file contains the
        // post-rotation tail (1 line).
        let main = fs::read_to_string(&path).unwrap();
        let rotated = fs::read_to_string(rotated_path(&path)).unwrap();
        assert_eq!(main.lines().count(), 1);
        assert_eq!(rotated.lines().count(), 3);
    }

    #[test]
    fn rotation_overwrites_previous_rotated_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path.clone(), 2).unwrap();
        for i in 0..6 {
            log.append(&sample(&format!("evt_{i}"), true, None)).unwrap();
        }
        // Three rotations happened; we still have only one .1 backup.
        assert!(rotated_path(&path).exists());
        assert!(!rotated_path(&rotated_path(&path)).exists());
    }

    #[test]
    fn open_counts_existing_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        // Seed three lines from a prior "run".
        {
            let log = InspectionLog::open(path.clone(), 5).unwrap();
            for i in 0..3 {
                log.append(&sample(&format!("seed_{i}"), true, None)).unwrap();
            }
        }
        // Reopen with budget 5 — only 2 more writes allowed before rotation.
        let log = InspectionLog::open(path.clone(), 5).unwrap();
        log.append(&sample("a", true, None)).unwrap();
        log.append(&sample("b", true, None)).unwrap();
        assert!(!rotated_path(&path).exists());
        log.append(&sample("c", true, None)).unwrap();
        // Now the 6th line forces rotation.
        assert!(rotated_path(&path).exists());
    }

    #[test]
    fn recent_returns_last_n_oldest_first() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path, DEFAULT_ROTATION_LINES).unwrap();
        for i in 0..10 {
            log.append(&sample(&format!("e_{i}"), true, None)).unwrap();
        }
        let last3 = log.recent(3).unwrap();
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0].event, "e_7");
        assert_eq!(last3[1].event, "e_8");
        assert_eq!(last3[2].event, "e_9");
    }

    #[test]
    fn recent_reads_across_rotation_boundary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path, 3).unwrap();
        for i in 0..5 {
            log.append(&sample(&format!("e_{i}"), true, None)).unwrap();
        }
        // After 5 appends with rotation_lines=3: .1 holds e_0,e_1,e_2;
        // main holds e_3,e_4.
        let last4 = log.recent(4).unwrap();
        let names: Vec<_> = last4.iter().map(|e| e.event.clone()).collect();
        assert_eq!(names, vec!["e_1", "e_2", "e_3", "e_4"]);
    }

    #[test]
    fn recent_zero_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path, DEFAULT_ROTATION_LINES).unwrap();
        log.append(&sample("e", true, None)).unwrap();
        assert!(log.recent(0).unwrap().is_empty());
    }

    /// Regression: `Vec::with_capacity(usize::MAX)` aborts the process.
    /// `recent` must clamp `n` to a bound the log could actually return
    /// before any allocation. Verified by passing a deliberately huge
    /// `n` against a small log; the call must succeed and return what
    /// is on disk (not panic or OOM).
    #[test]
    fn recent_clamps_huge_limit_without_panic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path, 8).unwrap();
        for i in 0..3 {
            log.append(&sample(&format!("e_{i}"), true, None)).unwrap();
        }
        // A naive `Vec::with_capacity(usize::MAX)` would abort. The
        // clamp brings n down to 2 * rotation_lines = 16, which is the
        // worst case the inspection log could ever hold.
        let got = log.recent(usize::MAX).expect("must not panic");
        let names: Vec<_> = got.iter().map(|e| e.event.clone()).collect();
        assert_eq!(names, vec!["e_0", "e_1", "e_2"]);
    }

    /// When the log is full (rotation_lines in current + rotation_lines
    /// in the rotated `.1`), the clamp caps at 2 * rotation_lines, not
    /// the caller's larger value. The returned set is exactly the
    /// retained on-disk events.
    #[test]
    fn recent_clamps_to_rotation_bound() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path, 4).unwrap();
        // Write 9 events with rotation_lines=4: after 5th append .1
        // holds the first 4, after 9th append .1 holds 5..8 and main
        // holds e_8. The log can never report more than 8 events even
        // if the caller asks for 1_000_000_000.
        for i in 0..9 {
            log.append(&sample(&format!("e_{i}"), true, None)).unwrap();
        }
        let got = log.recent(1_000_000_000).expect("must not panic");
        assert!(
            got.len() <= 8,
            "expected at most 2*rotation_lines events, got {}",
            got.len()
        );
    }

    #[test]
    fn malformed_lines_are_skipped_in_recent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = InspectionLog::open(path.clone(), DEFAULT_ROTATION_LINES).unwrap();
        log.append(&sample("first", true, None)).unwrap();
        // Inject garbage from outside the API.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "not a json object").unwrap();
        drop(file);
        log.append(&sample("third", true, None)).unwrap();
        let all = log.recent(10).unwrap();
        let names: Vec<_> = all.iter().map(|e| e.event.clone()).collect();
        assert_eq!(names, vec!["first", "third"]);
    }

    #[test]
    fn disable_reason_label_stable_strings() {
        assert_eq!(
            disable_reason_label(&DisableReason::DoNotTrack),
            "DoNotTrack"
        );
        assert_eq!(
            disable_reason_label(&DisableReason::AuraDisabled),
            "AuraDisabled"
        );
        assert_eq!(
            disable_reason_label(&DisableReason::Ci("GITHUB_ACTIONS")),
            "Ci(GITHUB_ACTIONS)"
        );
        assert_eq!(
            disable_reason_label(&DisableReason::CargoTest),
            "CargoTest"
        );
        assert_eq!(
            disable_reason_label(&DisableReason::ConfigDisabled),
            "ConfigDisabled"
        );
    }
}
