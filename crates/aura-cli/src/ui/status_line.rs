//! Bottom status line: the segments it can show, the values they render
//! from, and width-fitted rendering.
//!
//! Everything here is pure string work — capturing live values into a
//! [`Snapshot`] is the caller's job (see `status_bar`).

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::api::mcp_status::McpCounts;
use crate::theme::{AuraStyle, Themed};
use crate::ui::text::strip_control_chars;

/// One unit of the status line, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    Model,
    Cwd,
    Git,
    Context,
    Tokens,
    Scratchpad,
    Mcp,
}

impl Segment {
    pub const ALL: [Segment; 7] = [
        Segment::Model,
        Segment::Cwd,
        Segment::Git,
        Segment::Context,
        Segment::Tokens,
        Segment::Scratchpad,
        Segment::Mcp,
    ];

    /// The `cli.toml` spelling of this segment.
    pub fn name(self) -> &'static str {
        match self {
            Segment::Model => "model",
            Segment::Cwd => "cwd",
            Segment::Git => "git",
            Segment::Context => "context",
            Segment::Tokens => "tokens",
            Segment::Scratchpad => "scratchpad",
            Segment::Mcp => "mcp",
        }
    }
}

/// Segments shown when `cli.toml` has no `[status_line] segments`.
pub const DEFAULT_SEGMENTS: &[Segment] = &Segment::ALL;

fn valid_segment_names() -> String {
    Segment::ALL
        .iter()
        .map(|s| s.name())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, thiserror::Error)]
#[error("unknown status line segment `{name}` (expected one of: {valid})", valid = valid_segment_names())]
pub struct UnknownSegment {
    name: String,
}

impl FromStr for Segment {
    type Err = UnknownSegment;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Segment::ALL
            .iter()
            .copied()
            .find(|seg| seg.name() == s)
            .ok_or_else(|| UnknownSegment { name: s.to_owned() })
    }
}

/// Tokens in the model's context, and the window they count against when
/// the server reported one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    pub used: u64,
    pub limit: Option<NonZeroU64>,
}

/// Values the status line renders from.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub model: Option<String>,
    /// Working directory, already home-abbreviated for display.
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub context: Option<ContextUsage>,
    /// Cumulative prompt tokens across the conversation.
    pub prompt_tokens: u64,
    /// Cumulative completion tokens across the conversation.
    pub completion_tokens: u64,
    /// Tokens of tool output diverted to the scratchpad.
    pub scratchpad_intercepted: u64,
    /// Tokens read back into context from the scratchpad.
    pub scratchpad_extracted: u64,
    pub mcp: Option<McpCounts>,
}

const SEPARATOR: &str = " │ ";
const METER_CELLS: u64 = 8;
const CONTEXT_WARN_PCT: u64 = 70;
const CONTEXT_ERROR_PCT: u64 = 90;
/// Narrowest the cwd segment is squeezed to before whole segments are dropped.
const MIN_CWD_WIDTH: usize = 12;
/// Minimum spacing between the segments and the right-aligned text.
const MIN_GAP: usize = 2;

struct Piece {
    segment: Segment,
    text: String,
    style: AuraStyle,
}

/// Render `segments` from `snapshot` as one styled line that fits within
/// `width` columns, with `right` right-aligned when there is room left over.
///
/// Segments with nothing to show are omitted. When the line is too wide,
/// the cwd is tail-truncated first, then trailing segments are dropped;
/// `right` is only added once every remaining segment fits. A `width` of 0
/// means the terminal size is unknown; the segments are then rendered
/// unconstrained and `right` is omitted.
pub fn render(snapshot: &Snapshot, segments: &[Segment], width: usize, right: &str) -> String {
    let mut pieces: Vec<Piece> = segments
        .iter()
        .filter_map(|&segment| piece(segment, snapshot))
        .collect();
    if width > 0 {
        fit(&mut pieces, width);
    }

    let left_width = joined_width(&pieces);
    let mut out = String::new();
    for (i, piece) in pieces.iter().enumerate() {
        if i > 0 {
            out.push_str(&SEPARATOR.themed(AuraStyle::Muted).to_string());
        }
        out.push_str(&piece.text.as_str().themed(piece.style).to_string());
    }
    let right_width = right.width();
    if width > 0 && !right.is_empty() && left_width + MIN_GAP + right_width <= width {
        out.push_str(&" ".repeat(width - left_width - right_width));
        out.push_str(&right.themed(AuraStyle::Muted).to_string());
    }
    out
}

fn piece(segment: Segment, snapshot: &Snapshot) -> Option<Piece> {
    // Model, cwd, and branch come from a server or the filesystem: strip
    // control characters so they cannot carry escape sequences to the terminal.
    let (text, style) = match segment {
        Segment::Model => (
            strip_control_chars(snapshot.model.as_deref()?),
            AuraStyle::StatusModel,
        ),
        Segment::Cwd => (
            strip_control_chars(snapshot.cwd.as_deref()?),
            AuraStyle::StatusPath,
        ),
        Segment::Git => (
            format!("⎇ {}", strip_control_chars(snapshot.git_branch.as_deref()?)),
            AuraStyle::StatusGit,
        ),
        Segment::Context => context_piece(snapshot.context?),
        Segment::Tokens => {
            if snapshot.prompt_tokens == 0 && snapshot.completion_tokens == 0 {
                return None;
            }
            (
                format!(
                    "in {} / out {}",
                    compact(snapshot.prompt_tokens),
                    compact(snapshot.completion_tokens)
                ),
                AuraStyle::StatusTokens,
            )
        }
        Segment::Scratchpad => {
            if snapshot.scratchpad_intercepted == 0 {
                return None;
            }
            (
                format!(
                    "scratchpad in {} / out {}",
                    compact(snapshot.scratchpad_intercepted),
                    compact(snapshot.scratchpad_extracted)
                ),
                AuraStyle::StatusScratch,
            )
        }
        Segment::Mcp => {
            let McpCounts { connected, total } = snapshot.mcp?;
            let style = if total == 0 || connected == total {
                AuraStyle::StatusMcp
            } else if connected == 0 {
                AuraStyle::Error
            } else {
                AuraStyle::Warning
            };
            (format!("mcp {connected}/{total}"), style)
        }
    };
    Some(Piece {
        segment,
        text,
        style,
    })
}

fn context_piece(ContextUsage { used, limit }: ContextUsage) -> (String, AuraStyle) {
    let Some(limit) = limit else {
        return (format!("ctx {}", compact(used)), AuraStyle::Success);
    };
    let limit = limit.get();
    let pct = (used.saturating_mul(100) + limit / 2) / limit;
    let style = if pct >= CONTEXT_ERROR_PCT {
        AuraStyle::Error
    } else if pct >= CONTEXT_WARN_PCT {
        AuraStyle::Warning
    } else {
        AuraStyle::Success
    };
    (
        format!(
            "ctx {} {pct}% {}/{}",
            meter(pct),
            compact(used),
            compact(limit)
        ),
        style,
    )
}

fn meter(pct: u64) -> String {
    let filled = ((pct.min(100) * METER_CELLS + 50) / 100) as usize;
    let empty = METER_CELLS as usize - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn joined_width(pieces: &[Piece]) -> usize {
    let text: usize = pieces.iter().map(|p| p.text.width()).sum();
    text + SEPARATOR.width() * pieces.len().saturating_sub(1)
}

/// Shrink `pieces` until they fit in `available` columns.
fn fit(pieces: &mut Vec<Piece>, available: usize) {
    let over = joined_width(pieces).saturating_sub(available);
    if over > 0
        && let Some(cwd) = pieces.iter_mut().find(|p| p.segment == Segment::Cwd)
    {
        let target = cwd.text.width().saturating_sub(over).max(MIN_CWD_WIDTH);
        cwd.text = truncate_path(&cwd.text, target);
    }
    while joined_width(pieces) > available && pieces.len() > 1 {
        pieces.pop();
    }
    if let Some(only) = pieces.first_mut()
        && only.text.width() > available
    {
        only.text = truncate_end(&only.text, available);
    }
}

/// Keep the tail of a `/`-separated path within `max` columns, replacing
/// dropped leading components with `…/`.
fn truncate_path(path: &str, max: usize) -> String {
    if path.width() <= max {
        return path.to_owned();
    }
    let components: Vec<&str> = path.split('/').collect();
    for start in 1..components.len() {
        let candidate = format!("…/{}", components[start..].join("/"));
        if candidate.width() <= max {
            return candidate;
        }
    }
    truncate_start(components.last().copied().unwrap_or(path), max)
}

/// Keep the last `max - 1` columns of `s`, prefixed with `…`.
fn truncate_start(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    let budget = max.saturating_sub(1);
    let mut kept = 0;
    let mut tail = String::new();
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if kept + w > budget {
            break;
        }
        kept += w;
        tail.insert(0, c);
    }
    format!("…{tail}")
}

/// Keep the first `max - 1` columns of `s`, suffixed with `…`.
fn truncate_end(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_owned();
    }
    let budget = max.saturating_sub(1);
    let mut kept = 0;
    let mut head = String::new();
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if kept + w > budget {
            break;
        }
        kept += w;
        head.push(c);
    }
    format!("{head}…")
}

/// Format a count compactly: `742`, `1.2k`, `182k`, `1.2M`, `12M`.
pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_499 => scaled(n, 1_000, 'k'),
        _ => scaled(n, 1_000_000, 'M'),
    }
}

/// `n / unit` with one decimal below 10, whole numbers above.
fn scaled(n: u64, unit: u64, suffix: char) -> String {
    let tenths = (n * 10 + unit / 2) / unit;
    if tenths < 100 {
        format!("{}.{}{suffix}", tenths / 10, tenths % 10)
    } else {
        format!("{}{suffix}", (n + unit / 2) / unit)
    }
}

/// `path` with a leading `home` replaced by `~`.
pub fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Upper bound on how much of `HEAD` is read; a well-formed file is under
/// 100 bytes, so anything larger is not a ref worth displaying.
const MAX_HEAD_BYTES: u64 = 4096;

/// Current branch of the git repository containing `start`, read from
/// `HEAD` without spawning `git`. Detached heads yield the short commit id.
pub fn git_branch(start: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = String::new();
    std::fs::File::open(git_head_path(start)?)
        .ok()?
        .take(MAX_HEAD_BYTES)
        .read_to_string(&mut head)
        .ok()?;
    let head = head.trim();
    if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_owned());
    }
    if let Some(reference) = head.strip_prefix("ref: ") {
        return Some(reference.to_owned());
    }
    head.get(..7).map(str::to_owned)
}

/// Locate the `HEAD` file for the repository containing `start`, following
/// a `.git` *file* (worktree or submodule) to its real git directory.
fn git_head_path(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let dot_git = dir.join(".git");
        let meta = std::fs::metadata(&dot_git).ok()?;
        if meta.is_dir() {
            return Some(dot_git.join("HEAD"));
        }
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = Path::new(pointer.strip_prefix("gitdir:")?.trim());
        let git_dir = if target.is_absolute() {
            target.to_path_buf()
        } else {
            dir.join(target)
        };
        Some(git_dir.join("HEAD"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            model: Some("claude-sonnet-4-5".to_owned()),
            cwd: Some("~/src/aura".to_owned()),
            git_branch: Some("main".to_owned()),
            context: Some(ContextUsage {
                used: 76_000,
                limit: NonZeroU64::new(200_000),
            }),
            prompt_tokens: 182_000,
            completion_tokens: 41_000,
            scratchpad_intercepted: 0,
            scratchpad_extracted: 0,
            mcp: Some(McpCounts {
                connected: 3,
                total: 3,
            }),
        }
    }

    #[test]
    fn segment_names_round_trip() {
        for segment in Segment::ALL {
            assert_eq!(segment.name().parse::<Segment>().unwrap(), segment);
        }
        let err = "nope".parse::<Segment>().unwrap_err();
        assert!(err.to_string().contains("`nope`"));
        assert!(err.to_string().contains("model, cwd, git"));
    }

    #[test]
    fn compact_numbers() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_000), "1.0k");
        assert_eq!(compact(1_234), "1.2k");
        assert_eq!(compact(9_949), "9.9k");
        assert_eq!(compact(9_999), "10k");
        assert_eq!(compact(76_412), "76k");
        assert_eq!(compact(999_499), "999k");
        assert_eq!(compact(999_500), "1.0M");
        assert_eq!(compact(1_250_000), "1.3M");
        assert_eq!(compact(12_000_000), "12M");
    }

    #[test]
    fn meter_fills_proportionally() {
        assert_eq!(meter(0), "░░░░░░░░");
        assert_eq!(meter(38), "███░░░░░");
        assert_eq!(meter(100), "████████");
        assert_eq!(meter(140), "████████");
    }

    #[test]
    fn renders_all_segments_in_order() {
        let line = strip_ansi(&render(&snapshot(), DEFAULT_SEGMENTS, 200, ""));
        assert_eq!(
            line,
            "claude-sonnet-4-5 │ ~/src/aura │ ⎇ main │ ctx ███░░░░░ 38% 76k/200k │ in 182k / out 41k │ mcp 3/3"
        );
    }

    #[test]
    fn control_characters_in_external_text_are_neutralised() {
        let snapshot = Snapshot {
            model: Some("gpt\x1b[2J-4o".to_owned()),
            cwd: Some("~/a\tb".to_owned()),
            git_branch: Some("main\u{9b}31m".to_owned()),
            ..Snapshot::default()
        };
        let line = render(
            &snapshot,
            &[Segment::Model, Segment::Cwd, Segment::Git],
            200,
            "",
        );
        // The raw output carries only our own SGR styling: the injected
        // clear-screen, tab, and C1 CSI never reach the terminal.
        assert!(!line.contains("\x1b[2J"));
        assert!(!line.contains('\t'));
        assert!(!line.contains('\u{9b}'));
        assert_eq!(strip_ansi(&line), "gpt[2J-4o │ ~/ab │ ⎇ main31m");
    }

    #[test]
    fn omits_segments_without_data() {
        let snapshot = Snapshot {
            model: Some("gpt-4o".to_owned()),
            cwd: Some("~/x".to_owned()),
            ..Snapshot::default()
        };
        let line = strip_ansi(&render(&snapshot, DEFAULT_SEGMENTS, 80, ""));
        assert_eq!(line, "gpt-4o │ ~/x");
    }

    #[test]
    fn honors_segment_order_and_subset() {
        let line = strip_ansi(&render(
            &snapshot(),
            &[Segment::Git, Segment::Model],
            80,
            "",
        ));
        assert_eq!(line, "⎇ main │ claude-sonnet-4-5");
    }

    #[test]
    fn context_without_a_window_shows_the_raw_count() {
        let snapshot = Snapshot {
            context: Some(ContextUsage {
                used: 76_412,
                limit: None,
            }),
            ..Snapshot::default()
        };
        let line = strip_ansi(&render(&snapshot, &[Segment::Context], 80, ""));
        assert_eq!(line, "ctx 76k");
    }

    #[test]
    fn context_hidden_when_absent() {
        let snapshot = Snapshot {
            model: Some("m".to_owned()),
            context: None,
            ..Snapshot::default()
        };
        let line = strip_ansi(&render(
            &snapshot,
            &[Segment::Model, Segment::Context],
            80,
            "",
        ));
        assert_eq!(line, "m");
    }

    #[test]
    fn tokens_are_labelled_in_and_out() {
        let snapshot = Snapshot {
            prompt_tokens: 182_000,
            completion_tokens: 41_000,
            ..Snapshot::default()
        };
        let line = strip_ansi(&render(&snapshot, &[Segment::Tokens], 80, ""));
        assert_eq!(line, "in 182k / out 41k");
    }

    #[test]
    fn scratchpad_shows_when_intercepted() {
        let snapshot = Snapshot {
            scratchpad_intercepted: 48_000,
            scratchpad_extracted: 3_100,
            ..Snapshot::default()
        };
        let line = strip_ansi(&render(&snapshot, &[Segment::Scratchpad], 80, ""));
        assert_eq!(line, "scratchpad in 48k / out 3.1k");
    }

    #[test]
    fn right_text_is_right_aligned() {
        let snapshot = Snapshot {
            model: Some("m".to_owned()),
            ..Snapshot::default()
        };
        let line = strip_ansi(&render(&snapshot, &[Segment::Model], 24, "esc to stop"));
        assert_eq!(line, "m            esc to stop");
        assert_eq!(line.width(), 24);
    }

    #[test]
    fn segments_take_priority_over_right_text() {
        let snapshot = Snapshot {
            model: Some("claude-sonnet-4-5".to_owned()),
            git_branch: Some("main".to_owned()),
            ..Snapshot::default()
        };
        let segments = [Segment::Model, Segment::Git];
        // 26 columns of segments fit in 30; the 11-column hint does not.
        let line = strip_ansi(&render(&snapshot, &segments, 30, "esc to stop"));
        assert_eq!(line, "claude-sonnet-4-5 │ ⎇ main");
        // Widen until the hint plus a two-column gap fits as well.
        let line = strip_ansi(&render(&snapshot, &segments, 39, "esc to stop"));
        assert_eq!(line, "claude-sonnet-4-5 │ ⎇ main  esc to stop");
    }

    #[test]
    fn right_text_dropped_when_too_narrow() {
        let snapshot = Snapshot {
            model: Some("model-name".to_owned()),
            ..Snapshot::default()
        };
        let line = strip_ansi(&render(&snapshot, &[Segment::Model], 16, "AURA, by Mezmo!"));
        assert_eq!(line, "model-name");
    }

    #[test]
    fn narrow_width_truncates_cwd_then_drops_trailing_segments() {
        let snapshot = Snapshot {
            model: Some("gpt-4o".to_owned()),
            cwd: Some("~/src/github.com/mezmo/aura/crates/aura-cli".to_owned()),
            git_branch: Some("main".to_owned()),
            ..Snapshot::default()
        };
        let segments = [Segment::Model, Segment::Cwd, Segment::Git];

        let line = strip_ansi(&render(&snapshot, &segments, 40, ""));
        assert_eq!(line, "gpt-4o │ …/aura/crates/aura-cli │ ⎇ main");
        assert!(line.width() <= 40);

        let line = strip_ansi(&render(&snapshot, &segments, 24, ""));
        assert_eq!(line, "gpt-4o │ …/aura-cli");
        assert!(line.width() <= 24);

        let line = strip_ansi(&render(&snapshot, &segments, 4, ""));
        assert_eq!(line, "gpt…");
    }

    #[test]
    fn unknown_width_renders_unconstrained() {
        let line = strip_ansi(&render(&snapshot(), DEFAULT_SEGMENTS, 0, "AURA, by Mezmo!"));
        assert!(line.starts_with("claude-sonnet-4-5 │ ~/src/aura"));
        assert!(line.ends_with("mcp 3/3"));
    }

    #[test]
    fn context_thresholds_pick_style() {
        let usage = |used| ContextUsage {
            used,
            limit: NonZeroU64::new(100),
        };
        assert!(matches!(context_piece(usage(69)), (_, AuraStyle::Success)));
        assert!(matches!(context_piece(usage(70)), (_, AuraStyle::Warning)));
        assert!(matches!(context_piece(usage(90)), (_, AuraStyle::Error)));
        let (text, _) = context_piece(usage(120));
        assert_eq!(text, "ctx ████████ 120% 120/100");
    }

    #[test]
    fn mcp_degraded_styles() {
        let mcp = |connected, total| Snapshot {
            mcp: Some(McpCounts { connected, total }),
            ..Snapshot::default()
        };
        assert!(matches!(
            piece(Segment::Mcp, &mcp(3, 3)).map(|p| p.style),
            Some(AuraStyle::StatusMcp)
        ));
        assert!(matches!(
            piece(Segment::Mcp, &mcp(1, 3)).map(|p| p.style),
            Some(AuraStyle::Warning)
        ));
        assert!(matches!(
            piece(Segment::Mcp, &mcp(0, 3)).map(|p| p.style),
            Some(AuraStyle::Error)
        ));
        assert!(matches!(
            piece(Segment::Mcp, &mcp(0, 0)).map(|p| p.style),
            Some(AuraStyle::StatusMcp)
        ));
    }

    #[test]
    fn abbreviates_home() {
        let home = Path::new("/Users/me");
        assert_eq!(
            abbreviate_home(Path::new("/Users/me/src/aura"), Some(home)),
            "~/src/aura"
        );
        assert_eq!(abbreviate_home(Path::new("/Users/me"), Some(home)), "~");
        assert_eq!(
            abbreviate_home(Path::new("/opt/work"), Some(home)),
            "/opt/work"
        );
        assert_eq!(abbreviate_home(Path::new("/opt/work"), None), "/opt/work");
    }

    #[test]
    fn git_branch_from_head_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();

        assert_eq!(git_branch(&repo).as_deref(), Some("feature/x"));
        assert_eq!(
            git_branch(&repo.join("src/deep")).as_deref(),
            Some("feature/x")
        );

        std::fs::write(
            repo.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(git_branch(&repo).as_deref(), Some("0123456"));

        assert_eq!(git_branch(tmp.path()), None);
    }

    #[test]
    fn git_branch_follows_worktree_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let main_git = tmp.path().join("main/.git/worktrees/wt");
        std::fs::create_dir_all(&main_git).unwrap();
        std::fs::write(main_git.join("HEAD"), "ref: refs/heads/wt-branch\n").unwrap();

        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", main_git.display()),
        )
        .unwrap();

        assert_eq!(git_branch(&worktree).as_deref(), Some("wt-branch"));
    }
}
