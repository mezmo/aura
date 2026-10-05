//! Formatter for Prometheus HTTP API query results.
//!
//! Accepts the `data` object (`resultType` + `result`), the full
//! `{status, data}` envelope, or either encoded as a JSON string, and renders
//! it as plain text: labels shared by every series printed once, and for
//! range results the returned resolution, missing points and run-length
//! encoded values per series. Anything else returns `None` so the caller can
//! pass the original output through.
//!
//! See <https://prometheus.io/docs/prometheus/latest/querying/api/#expression-query-result-formats>.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use aura_config::{PrometheusFormatOptions, glob_match};
use chrono::{DateTime, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Prometheus's own limit on points per series in a range query.
const MAX_GRID_POINTS: i64 = 11_000;

/// Maximum missing-point ranges listed per series.
const MAX_MISSING_RANGES: usize = 10;

/// The step, start and end a range query requested, in milliseconds.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RequestedRange {
    pub step_ms: Option<i64>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
}

impl RequestedRange {
    /// Read the tool arguments named by `step_arg`, `start_arg` and `end_arg`.
    /// Unnamed or unparseable arguments are left as `None`.
    pub(crate) fn from_args(args: &Value, opts: &PrometheusFormatOptions) -> Self {
        let arg = |name: &Option<String>| name.as_deref().and_then(|n| args.get(n));
        Self {
            step_ms: arg(&opts.step_arg).and_then(parse_duration_ms),
            start_ms: arg(&opts.start_arg).and_then(parse_time_ms),
            end_ms: arg(&opts.end_arg).and_then(parse_time_ms),
        }
    }
}

/// Format a Prometheus query result. Returns `None` when `content` is not one.
pub(crate) fn format(
    content: &str,
    opts: &PrometheusFormatOptions,
    requested: &RequestedRange,
) -> Option<String> {
    let data = parse_data(content)?;
    let result_type = data.get("resultType")?.as_str()?;
    let result = data.get("result")?;
    match result_type {
        "vector" => format_vector(&parse_vector(result)?, opts),
        "matrix" => format_matrix(&parse_matrix(result)?, opts, requested),
        "scalar" | "string" => format_scalar(result_type, result),
        _ => None,
    }
}

fn header(result_type: &str, series: usize) -> String {
    format!(
        "Prometheus {result_type} result: {series} series. Formatted by aura; call again with \
         \"_aura_raw\": true to skip formatting.\n"
    )
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Unwrap a JSON-string encoding and the `{status, data}` envelope, returning
/// the `data` object.
fn parse_data(content: &str) -> Option<Value> {
    let mut value: Value = serde_json::from_str(content.trim()).ok()?;
    if let Value::String(inner) = value {
        value = serde_json::from_str(inner.trim()).ok()?;
    }
    let data = match value.get("status") {
        Some(status) => {
            if status.as_str()? != "success" {
                return None;
            }
            value.get_mut("data")?.take()
        }
        None => value,
    };
    data.get("resultType")?;
    Some(data)
}

#[derive(Debug)]
struct Sample {
    ts_ms: i64,
    value: String,
}

#[derive(Debug)]
struct Series {
    labels: BTreeMap<String, String>,
    samples: Vec<Sample>,
}

fn parse_labels(metric: Option<&Value>) -> Option<BTreeMap<String, String>> {
    match metric {
        None | Some(Value::Null) => Some(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect(),
        Some(_) => None,
    }
}

/// A `[<unix seconds>, "<value>"]` pair.
fn parse_sample(pair: &Value) -> Option<Sample> {
    let pair = pair.as_array()?;
    if pair.len() != 2 {
        return None;
    }
    Some(Sample {
        ts_ms: (pair[0].as_f64()? * 1000.0).round() as i64,
        value: pair[1].as_str()?.to_string(),
    })
}

fn parse_vector(result: &Value) -> Option<Vec<Series>> {
    result
        .as_array()?
        .iter()
        .map(|item| {
            Some(Series {
                labels: parse_labels(item.get("metric"))?,
                samples: vec![parse_sample(item.get("value")?)?],
            })
        })
        .collect()
}

fn parse_matrix(result: &Value) -> Option<Vec<Series>> {
    result
        .as_array()?
        .iter()
        .map(|item| {
            let mut samples = item
                .get("values")?
                .as_array()?
                .iter()
                .map(parse_sample)
                .collect::<Option<Vec<_>>>()?;
            samples.sort_by_key(|s| s.ts_ms);
            Some(Series {
                labels: parse_labels(item.get("metric"))?,
                samples,
            })
        })
        .collect()
}

/// Seconds as a number or numeric string, or a Prometheus duration
/// (`1h30m`, `500ms`).
fn parse_duration_ms(value: &Value) -> Option<i64> {
    let ms = match value {
        Value::Number(n) => n.as_f64()? * 1000.0,
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(secs) => secs * 1000.0,
            Err(_) => parse_prometheus_duration_ms(s.trim())? as f64,
        },
        _ => return None,
    };
    (ms > 0.0).then_some(ms.round() as i64)
}

fn parse_prometheus_duration_ms(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut total: i64 = 0;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let n: i64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let unit_ms = match &rest[..unit_len] {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "w" => 604_800_000,
            "y" => 31_536_000_000,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total = total.checked_add(n.checked_mul(unit_ms)?)?;
    }
    Some(total)
}

/// Unix seconds as a number or numeric string, or an RFC 3339 timestamp.
fn parse_time_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => Some((n.as_f64()? * 1000.0).round() as i64),
        Value::String(s) => match s.trim().parse::<f64>() {
            Ok(secs) => Some((secs * 1000.0).round() as i64),
            Err(_) => DateTime::parse_from_rfc3339(s.trim())
                .ok()
                .map(|t| t.timestamp_millis()),
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn fmt_ts(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::AutoSi, true))
        .unwrap_or_else(|| ms.to_string())
}

/// Prometheus-style duration: `30s`, `5m`, `1h30m`, `250ms`.
fn fmt_duration(ms: i64) -> String {
    if ms % 1000 != 0 {
        return format!("{ms}ms");
    }
    let mut secs = ms / 1000;
    if secs == 0 {
        return "0s".to_string();
    }
    let mut out = String::new();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        if secs >= size {
            let _ = write!(out, "{}{unit}", secs / size);
            secs %= size;
        }
    }
    out
}

/// Labels shared by the whole result, and the label keys shown per series.
struct LabelView {
    common: BTreeMap<String, String>,
    per_series: Vec<String>,
}

/// Split labels into those with one value on every series (printed once) and
/// those printed per series: `keep_labels` first, in config order, then the
/// varying labels sorted. Labels matching `drop_labels` are left out.
fn label_view(series: &[Series], opts: &PrometheusFormatOptions) -> LabelView {
    let dropped = |k: &str| opts.drop_labels.iter().any(|p| glob_match(p, k));
    let keys: BTreeSet<&String> = series.iter().flat_map(|s| s.labels.keys()).collect();

    let mut common = BTreeMap::new();
    let mut varying = Vec::new();
    for key in keys {
        if dropped(key) {
            continue;
        }
        let first = series[0].labels.get(key);
        let same = first.is_some() && series.iter().all(|s| s.labels.get(key) == first);
        if same && !opts.keep_labels.contains(key) {
            common.insert(key.clone(), first.cloned().unwrap_or_default());
        } else if !opts.keep_labels.contains(key) {
            varying.push(key.clone());
        }
    }

    let mut per_series: Vec<String> = opts
        .keep_labels
        .iter()
        .filter(|k| series.iter().any(|s| s.labels.contains_key(*k)))
        .cloned()
        .collect();
    per_series.extend(varying);
    LabelView { common, per_series }
}

fn fmt_labels<'a>(pairs: impl Iterator<Item = (&'a String, &'a String)>) -> String {
    let body: Vec<String> = pairs.map(|(k, v)| format!("{k}={v:?}")).collect();
    format!("{{{}}}", body.join(", "))
}

fn series_labels(series: &Series, view: &LabelView) -> String {
    fmt_labels(
        view.per_series
            .iter()
            .filter_map(|k| series.labels.get_key_value(k)),
    )
}

fn write_common_labels(out: &mut String, view: &LabelView) {
    if !view.common.is_empty() {
        let _ = writeln!(out, "common labels: {}", fmt_labels(view.common.iter()));
    }
}

// ---------------------------------------------------------------------------
// vector, scalar, string
// ---------------------------------------------------------------------------

fn format_vector(series: &[Series], opts: &PrometheusFormatOptions) -> Option<String> {
    let mut out = header("vector", series.len());
    if series.is_empty() {
        return Some(out);
    }
    let view = label_view(series, opts);
    let first_ts = series[0].samples[0].ts_ms;
    let same_time = series.iter().all(|s| s.samples[0].ts_ms == first_ts);
    if same_time {
        let _ = writeln!(out, "evaluated at {}", fmt_ts(first_ts));
    }
    write_common_labels(&mut out, &view);

    // Response order is kept: it carries meaning for topk/bottomk/sort.
    for s in series.iter().take(opts.max_series) {
        let sample = &s.samples[0];
        let _ = write!(out, "{} {}", series_labels(s, &view), sample.value);
        if !same_time {
            let _ = write!(out, " at {}", fmt_ts(sample.ts_ms));
        }
        out.push('\n');
    }
    write_omitted(&mut out, series.len(), opts.max_series);
    Some(out)
}

fn format_scalar(result_type: &str, result: &Value) -> Option<String> {
    let sample = parse_sample(result)?;
    Some(format!(
        "Prometheus {result_type} result: {} at {}. Formatted by aura; call again with \
         \"_aura_raw\": true to skip formatting.\n",
        sample.value,
        fmt_ts(sample.ts_ms)
    ))
}

fn write_omitted(out: &mut String, total: usize, shown: usize) {
    if total > shown {
        let _ = writeln!(
            out,
            "{} more series not shown (max_series = {shown}).",
            total - shown
        );
    }
}

// ---------------------------------------------------------------------------
// matrix
// ---------------------------------------------------------------------------

/// The most common spacing between consecutive timestamps across all
/// series; ties go to the smaller spacing.
fn returned_resolution(series: &[Series]) -> Option<i64> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for s in series {
        for pair in s.samples.windows(2) {
            let delta = pair[1].ts_ms - pair[0].ts_ms;
            if delta > 0 {
                *counts.entry(delta).or_default() += 1;
            }
        }
    }
    counts
        .into_iter()
        .max_by(|(da, ca), (db, cb)| ca.cmp(cb).then(db.cmp(da)))
        .map(|(delta, _)| delta)
}

/// Evenly spaced expected evaluation timestamps.
#[derive(Debug, Clone, Copy)]
struct Grid {
    first: i64,
    step: i64,
    count: i64,
}

impl Grid {
    fn last(&self) -> i64 {
        self.first + (self.count - 1) * self.step
    }

    fn at(&self, i: i64) -> i64 {
        self.first + i * self.step
    }

    fn contains(&self, t: i64) -> bool {
        t >= self.first && t <= self.last() && (t - self.first) % self.step == 0
    }

    /// Grid points with no sample in `series`. Samples are sorted, so a
    /// repeated timestamp is counted once.
    fn missing_in(&self, series: &Series) -> usize {
        let mut on_grid = 0usize;
        let mut previous = None;
        for sample in &series.samples {
            if previous != Some(sample.ts_ms) && self.contains(sample.ts_ms) {
                on_grid += 1;
            }
            previous = Some(sample.ts_ms);
        }
        self.count as usize - on_grid
    }
}

/// The expected grid at `step`: aligned to the observed samples, spanning the
/// requested window when known, else the observed span. `None` when there
/// are no samples, or when the grid exceeds [`MAX_GRID_POINTS`].
///
/// A gap shared by every series at regular intervals is indistinguishable
/// here from a coarser resolution: `format_matrix` states both readings when
/// the requested step is known.
fn expected_grid(series: &[Series], step: i64, requested: &RequestedRange) -> Option<Grid> {
    let observed = series
        .iter()
        .flat_map(|s| s.samples.iter().map(|x| x.ts_ms));
    let (min_ts, max_ts) = observed.fold(None, |acc: Option<(i64, i64)>, t| match acc {
        None => Some((t, t)),
        Some((lo, hi)) => Some((lo.min(t), hi.max(t))),
    })?;
    let lo = requested.start_ms.unwrap_or(min_ts).min(min_ts);
    let hi = requested.end_ms.unwrap_or(max_ts).max(max_ts);
    let first = min_ts - ((min_ts - lo) / step) * step;
    let count = (hi - first) / step + 1;
    (count <= MAX_GRID_POINTS).then_some(Grid { first, step, count })
}

#[derive(Debug, Clone, PartialEq)]
enum Point<'a> {
    Value(&'a str),
    Gap,
}

impl Point<'_> {
    fn label(&self) -> &str {
        match self {
            Point::Value(v) => v,
            Point::Gap => "gap",
        }
    }
}

struct SeriesSummary<'a> {
    series: &'a Series,
    timeline: Vec<Point<'a>>,
    /// `(first, last, count)` of each run of consecutive missing grid points.
    missing: Vec<(i64, i64, usize)>,
    missing_points: usize,
}

fn summarise<'a>(series: &'a Series, grid: Option<Grid>) -> SeriesSummary<'a> {
    let Some(grid) = grid else {
        return SeriesSummary {
            series,
            timeline: series
                .samples
                .iter()
                .map(|s| Point::Value(&s.value))
                .collect(),
            missing: Vec::new(),
            missing_points: 0,
        };
    };

    let present: HashSet<i64> = series.samples.iter().map(|s| s.ts_ms).collect();
    let mut events: Vec<(i64, Point<'a>)> = series
        .samples
        .iter()
        .map(|s| (s.ts_ms, Point::Value(&s.value)))
        .collect();

    let mut missing: Vec<(i64, i64, usize)> = Vec::new();
    let mut missing_points = 0;
    let mut previous_missing = false;
    for t in (0..grid.count).map(|i| grid.at(i)) {
        if present.contains(&t) {
            previous_missing = false;
            continue;
        }
        missing_points += 1;
        events.push((t, Point::Gap));
        match missing.last_mut() {
            Some(run) if previous_missing => {
                run.1 = t;
                run.2 += 1;
            }
            _ => missing.push((t, t, 1)),
        }
        previous_missing = true;
    }
    events.sort_by_key(|(t, _)| *t);

    SeriesSummary {
        series,
        timeline: events.into_iter().map(|(_, p)| p).collect(),
        missing,
        missing_points,
    }
}

fn run_length(timeline: &[Point<'_>]) -> Vec<(String, usize)> {
    let mut runs: Vec<(String, usize)> = Vec::new();
    for point in timeline {
        match runs.last_mut() {
            Some((label, count)) if label == point.label() => *count += 1,
            _ => runs.push((point.label().to_string(), 1)),
        }
    }
    runs
}

/// Run-length encode the timeline. With more than `max_runs` runs, print
/// `max_runs` evenly spaced points instead, so the trend stays visible.
fn fmt_values(timeline: &[Point<'_>], max_runs: usize) -> String {
    let runs = run_length(timeline);
    if runs.len() <= max_runs {
        let parts: Vec<String> = runs
            .iter()
            .map(|(label, n)| {
                if *n == 1 {
                    label.clone()
                } else {
                    format!("{label}×{n}")
                }
            })
            .collect();
        return format!("values: {}", parts.join(" "));
    }
    let take = max_runs.max(2);
    let last = timeline.len() - 1;
    let parts: Vec<&str> = (0..take)
        .map(|i| timeline[i * last / (take - 1)].label())
        .collect();
    format!(
        "values ({take} evenly spaced of {}, in time order): {}",
        timeline.len(),
        parts.join(" ")
    )
}

fn fmt_missing(runs: &[(i64, i64, usize)]) -> String {
    let mut parts: Vec<String> = runs
        .iter()
        .take(MAX_MISSING_RANGES)
        .map(|&(first, last, n)| {
            if n == 1 {
                fmt_ts(first)
            } else {
                format!("{} to {} ({n})", fmt_ts(first), fmt_ts(last))
            }
        })
        .collect();
    if runs.len() > MAX_MISSING_RANGES {
        parts.push(format!(
            "and {} more ranges",
            runs.len() - MAX_MISSING_RANGES
        ));
    }
    parts.join(", ")
}

/// Print the returned resolution (and the requested step, when `step_arg`
/// is configured), the expected window (over the requested start and end,
/// when configured), common labels, then up to `max_series` series, those
/// with the most missing points first.
fn format_matrix(
    series: &[Series],
    opts: &PrometheusFormatOptions,
    requested: &RequestedRange,
) -> Option<String> {
    let mut out = header("matrix", series.len());
    if series.is_empty() {
        return Some(out);
    }

    let resolution = returned_resolution(series);
    let grid_step = resolution.or(requested.step_ms);
    let grid = grid_step.and_then(|step| expected_grid(series, step, requested));

    match resolution {
        Some(res) => {
            let _ = write!(out, "resolution: {}", fmt_duration(res));
            if let Some(step) = requested.step_ms {
                let _ = write!(out, " (requested step {})", fmt_duration(step));
            }
            out.push('\n');
            // Coarser than requested: either the backend aggregated, or every
            // series lacks the same points at regular intervals. The data
            // cannot tell these apart, so state both with the counts.
            if let Some(step) = requested.step_ms.filter(|&step| res > step)
                && let Some(g) = &grid
            {
                let at_step = (g.last() - g.first) / step + 1;
                let most = series.iter().map(|s| s.samples.len()).max().unwrap_or(0);
                let _ = writeln!(
                    out,
                    "The returned resolution {} is coarser than the requested step {}: the backend \
                     aggregated the data, or every series is missing the same points. The requested \
                     step gives {at_step} points over this window; the series with the most has {most}.",
                    fmt_duration(res),
                    fmt_duration(step),
                );
            }
        }
        None => out.push_str("resolution: unknown (no series has two or more points)\n"),
    }
    match &grid {
        Some(g) => {
            let _ = writeln!(
                out,
                "window: {} to {}, {} points expected per series",
                fmt_ts(g.first),
                fmt_ts(g.last()),
                g.count
            );
        }
        None if grid_step.is_some() => {
            let _ = writeln!(
                out,
                "missing points: not analysed (more than {MAX_GRID_POINTS} points expected per series)"
            );
        }
        None => {}
    }

    let view = label_view(series, opts);
    write_common_labels(&mut out, &view);

    // Rank every series by a cheap missing-point count, then build full
    // timelines only for the series that are printed.
    let mut ranked: Vec<(usize, &Series)> = series
        .iter()
        .map(|s| (grid.map_or(0, |g| g.missing_in(s)), s))
        .collect();
    ranked.sort_by_key(|(missing, _)| std::cmp::Reverse(*missing));
    let summaries: Vec<SeriesSummary> = ranked
        .iter()
        .take(opts.max_series)
        .map(|(_, s)| summarise(s, grid))
        .collect();

    for summary in &summaries {
        let s = summary.series;
        let _ = writeln!(out, "{}", series_labels(s, &view));

        let _ = write!(out, "  points {}", s.samples.len());
        if let Some(g) = &grid {
            let _ = write!(out, "/{}", g.count);
            if summary.missing_points > 0 {
                let _ = write!(
                    out,
                    ", missing {}: {}",
                    summary.missing_points,
                    fmt_missing(&summary.missing)
                );
            }
        }
        out.push('\n');

        let finite: Vec<f64> = s
            .samples
            .iter()
            .filter_map(|x| x.value.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .collect();
        let non_finite = s.samples.len() - finite.len();
        if let (Some(min), Some(max)) = (
            finite.iter().copied().reduce(f64::min),
            finite.iter().copied().reduce(f64::max),
        ) {
            let _ = write!(out, "  min {min}, max {max}");
        } else {
            out.push_str("  no finite values");
        }
        if let Some(last) = s.samples.last() {
            let _ = write!(out, ", last {}", last.value);
        }
        if non_finite > 0 {
            let _ = write!(out, ", NaN/Inf {non_finite}");
        }
        out.push('\n');

        if !summary.timeline.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                fmt_values(&summary.timeline, opts.max_runs_per_series)
            );
        }
    }
    write_omitted(&mut out, series.len(), opts.max_series);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-10-05T07:20:00Z
    const T0: i64 = 1_791_184_800;

    fn opts() -> PrometheusFormatOptions {
        PrometheusFormatOptions::default()
    }

    fn matrix(series: Vec<(Value, Vec<(i64, &str)>)>) -> String {
        let result: Vec<Value> = series
            .into_iter()
            .map(|(metric, values)| {
                json!({
                    "metric": metric,
                    "values": values.iter().map(|(t, v)| json!([t, v])).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({"resultType": "matrix", "result": result}).to_string()
    }

    fn steady(instance: &str, points: usize, skip: &[usize]) -> (Value, Vec<(i64, &'static str)>) {
        let values = (0..points)
            .filter(|i| !skip.contains(i))
            .map(|i| (T0 + 30 * i as i64, "1"))
            .collect();
        (
            json!({"__name__": "up", "job": "node", "instance": instance}),
            values,
        )
    }

    #[test]
    fn non_prometheus_output_is_not_formatted() {
        assert_eq!(
            format("plain text", &opts(), &RequestedRange::default()),
            None
        );
        assert_eq!(
            format(r#"{"items": []}"#, &opts(), &RequestedRange::default()),
            None
        );
        assert_eq!(
            format(
                r#"{"resultType": "histogram", "result": []}"#,
                &opts(),
                &RequestedRange::default()
            ),
            None
        );
    }

    #[test]
    fn error_status_is_not_formatted() {
        let body = json!({"status": "error", "errorType": "bad_data", "error": "parse error"});
        assert_eq!(
            format(&body.to_string(), &opts(), &RequestedRange::default()),
            None
        );
    }

    #[test]
    fn accepts_envelope_and_json_string_encodings() {
        let data = json!({"resultType": "vector", "result": [
            {"metric": {"instance": "a"}, "value": [T0, "1"]}
        ]});
        let envelope = json!({"status": "success", "data": data.clone()});
        let as_string = Value::String(envelope.to_string()).to_string();
        for content in [data.to_string(), envelope.to_string(), as_string] {
            let out = format(&content, &opts(), &RequestedRange::default())
                .unwrap_or_else(|| panic!("not formatted: {content}"));
            assert!(out.contains("Prometheus vector result: 1 series"), "{out}");
        }
    }

    #[test]
    fn vector_prints_common_labels_once_and_keeps_response_order() {
        let content = json!({"resultType": "vector", "result": [
            {"metric": {"job": "node", "instance": "b"}, "value": [T0, "5"]},
            {"metric": {"job": "node", "instance": "a"}, "value": [T0, "3"]},
        ]})
        .to_string();
        let out = format(&content, &opts(), &RequestedRange::default()).unwrap();
        assert!(out.contains("evaluated at 2026-10-05T07:20:00Z"), "{out}");
        assert!(out.contains(r#"common labels: {job="node"}"#), "{out}");
        let b = out.find(r#"{instance="b"} 5"#).expect(&out);
        let a = out.find(r#"{instance="a"} 3"#).expect(&out);
        assert!(b < a, "response order must be kept:\n{out}");
    }

    #[test]
    fn keep_and_drop_labels_apply() {
        let content = json!({"resultType": "vector", "result": [
            {"metric": {"job": "node", "instance": "a", "agent_id": "1"}, "value": [T0, "1"]},
            {"metric": {"job": "node", "instance": "b", "agent_id": "2"}, "value": [T0, "1"]},
        ]})
        .to_string();
        let options = PrometheusFormatOptions {
            keep_labels: vec!["job".into()],
            drop_labels: vec!["agent_*".into()],
            ..opts()
        };
        let out = format(&content, &options, &RequestedRange::default()).unwrap();
        assert!(out.contains(r#"{job="node", instance="a"} 1"#), "{out}");
        assert!(!out.contains("agent_id"), "{out}");
        assert!(!out.contains("common labels"), "{out}");
    }

    #[test]
    fn matrix_reports_missing_points_and_gaps_in_values() {
        let content = matrix(vec![steady("a", 31, &[]), steady("b", 31, &[15, 16])]);
        let out = format(&content, &opts(), &RequestedRange::default()).unwrap();
        assert!(out.contains("resolution: 30s"), "{out}");
        assert!(out.contains("31 points expected per series"), "{out}");
        assert!(
            out.contains(
                "points 29/31, missing 2: 2026-10-05T07:27:30Z to 2026-10-05T07:28:00Z (2)"
            ),
            "{out}"
        );
        assert!(out.contains("values: 1×15 gap×2 1×14"), "{out}");
        assert!(out.contains("values: 1×31"), "{out}");
        let b = out.find(r#"{instance="b"}"#).unwrap();
        let a = out.find(r#"{instance="a"}"#).unwrap();
        assert!(b < a, "series with missing points sort first:\n{out}");
    }

    #[test]
    fn matrix_counts_gaps_shared_by_every_series_against_the_requested_window() {
        // Both series stop at index 28; the window runs to index 30.
        let content = matrix(vec![steady("a", 29, &[]), steady("b", 29, &[])]);
        let requested = RequestedRange {
            step_ms: Some(30_000),
            start_ms: Some(T0 * 1000),
            end_ms: Some((T0 + 900) * 1000),
        };
        let out = format(&content, &opts(), &requested).unwrap();
        assert!(
            out.contains("resolution: 30s (requested step 30s)"),
            "{out}"
        );
        assert!(out.matches("points 29/31, missing 2").count() == 2, "{out}");
    }

    #[test]
    fn matrix_states_a_coarser_resolution_than_requested() {
        let hourly: Vec<(i64, &str)> = (0..4).map(|i| (T0 + 3600 * i, "7.98")).collect();
        let content = matrix(vec![(json!({}), hourly)]);
        let requested = RequestedRange {
            step_ms: Some(300_000),
            ..Default::default()
        };
        let out = format(&content, &opts(), &requested).unwrap();
        assert!(out.contains("resolution: 1h (requested step 5m)"), "{out}");
        assert!(
            out.contains(
                "The returned resolution 1h is coarser than the requested step 5m: the backend \
                 aggregated the data, or every series is missing the same points. The requested \
                 step gives 37 points over this window; the series with the most has 4."
            ),
            "{out}"
        );
    }

    #[test]
    fn matrix_flags_gaps_shared_by_every_series_at_regular_intervals() {
        // A 1m query where every series has points only at minutes 0, 2, 4, 6.
        let every_other = |instance: &str| {
            (
                json!({"instance": instance}),
                (0..4).map(|i| (T0 + 120 * i, "1")).collect::<Vec<_>>(),
            )
        };
        let content = matrix(vec![every_other("a"), every_other("b")]);
        let requested = RequestedRange {
            step_ms: Some(60_000),
            ..Default::default()
        };
        let out = format(&content, &opts(), &requested).unwrap();
        assert!(
            out.contains("or every series is missing the same points"),
            "{out}"
        );
        assert!(
            out.contains("The requested step gives 7 points over this window; the series with the most has 4."),
            "{out}"
        );
    }

    #[test]
    fn matrix_builds_timelines_only_for_printed_series() {
        // 2,000 sparse series over a full-size grid: ranked by a count, with
        // timelines built for the 2 printed.
        let series: Vec<_> = (0..2_000)
            .map(|i| {
                (
                    json!({"instance": format!("i{i}")}),
                    vec![(T0, "1"), (T0 + 30, "1")],
                )
            })
            .collect();
        let requested = RequestedRange {
            step_ms: Some(30_000),
            start_ms: Some(T0 * 1000),
            end_ms: Some((T0 + 30 * (MAX_GRID_POINTS - 1)) * 1000),
        };
        let options = PrometheusFormatOptions {
            max_series: 2,
            ..opts()
        };
        let out = format(&matrix(series), &options, &requested).unwrap();
        assert!(
            out.contains(&format!("{MAX_GRID_POINTS} points expected per series")),
            "{out}"
        );
        assert!(out.contains("1998 more series not shown"), "{out}");
    }

    #[test]
    fn matrix_skips_missing_analysis_beyond_the_prometheus_point_limit() {
        let content = matrix(vec![steady("a", 2, &[])]);
        let requested = RequestedRange {
            step_ms: Some(30_000),
            start_ms: Some(T0 * 1000),
            end_ms: Some((T0 + 30 * MAX_GRID_POINTS) * 1000),
        };
        let out = format(&content, &opts(), &requested).unwrap();
        assert!(
            out.contains(
                "missing points: not analysed (more than 11000 points expected per series)"
            ),
            "{out}"
        );
    }

    #[test]
    fn matrix_counts_non_finite_values() {
        let content = matrix(vec![(
            json!({"instance": "a"}),
            vec![
                (T0, "1"),
                (T0 + 30, "NaN"),
                (T0 + 60, "+Inf"),
                (T0 + 90, "2"),
            ],
        )]);
        let out = format(&content, &opts(), &RequestedRange::default()).unwrap();
        assert!(out.contains("min 1, max 2, last 2, NaN/Inf 2"), "{out}");
    }

    #[test]
    fn matrix_samples_series_with_too_many_runs() {
        let values: Vec<(i64, String)> = (0..100).map(|i| (T0 + 30 * i, i.to_string())).collect();
        let values: Vec<(i64, &str)> = values.iter().map(|(t, v)| (*t, v.as_str())).collect();
        let content = matrix(vec![(json!({"instance": "a"}), values)]);
        let options = PrometheusFormatOptions {
            max_runs_per_series: 5,
            ..opts()
        };
        let out = format(&content, &options, &RequestedRange::default()).unwrap();
        assert!(
            out.contains("values (5 evenly spaced of 100, in time order): 0 24 49 74 99"),
            "{out}"
        );
    }

    #[test]
    fn matrix_caps_printed_series() {
        let series: Vec<_> = (0..5).map(|i| steady(&format!("i{i}"), 3, &[])).collect();
        let options = PrometheusFormatOptions {
            max_series: 2,
            ..opts()
        };
        let out = format(&matrix(series), &options, &RequestedRange::default()).unwrap();
        assert!(
            out.contains("3 more series not shown (max_series = 2)."),
            "{out}"
        );
    }

    #[test]
    fn empty_results_are_formatted() {
        let content = json!({"resultType": "matrix", "result": []}).to_string();
        let out = format(&content, &opts(), &RequestedRange::default()).unwrap();
        assert!(
            out.starts_with("Prometheus matrix result: 0 series."),
            "{out}"
        );
    }

    #[test]
    fn scalar_and_string_results_are_one_line() {
        let content = json!({"resultType": "scalar", "result": [T0, "42"]}).to_string();
        let out = format(&content, &opts(), &RequestedRange::default()).unwrap();
        assert!(
            out.starts_with("Prometheus scalar result: 42 at 2026-10-05T07:20:00Z."),
            "{out}"
        );
    }

    #[test]
    fn requested_range_reads_configured_arguments() {
        let options = PrometheusFormatOptions {
            step_arg: Some("step".into()),
            start_arg: Some("start".into()),
            end_arg: Some("end".into()),
            ..opts()
        };
        let args = json!({"step": "1m30s", "start": "2026-10-05T07:20:00Z", "end": 1_791_185_700});
        assert_eq!(
            RequestedRange::from_args(&args, &options),
            RequestedRange {
                step_ms: Some(90_000),
                start_ms: Some(T0 * 1000),
                end_ms: Some((T0 + 900) * 1000),
            }
        );
        assert_eq!(
            RequestedRange::from_args(&args, &opts()),
            RequestedRange::default()
        );
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration_ms(&json!(30)), Some(30_000));
        assert_eq!(parse_duration_ms(&json!("15")), Some(15_000));
        assert_eq!(parse_duration_ms(&json!("500ms")), Some(500));
        assert_eq!(parse_duration_ms(&json!("1h30m")), Some(5_400_000));
        assert_eq!(parse_duration_ms(&json!("5x")), None);
        assert_eq!(parse_duration_ms(&json!("m")), None);
        assert_eq!(parse_duration_ms(&json!(0)), None);
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_duration(30_000), "30s");
        assert_eq!(fmt_duration(300_000), "5m");
        assert_eq!(fmt_duration(5_400_000), "1h30m");
        assert_eq!(fmt_duration(86_400_000), "1d");
        assert_eq!(fmt_duration(250), "250ms");
    }
}
