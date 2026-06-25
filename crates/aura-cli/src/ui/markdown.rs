use termimad::crossterm::style::{Attribute, Color};
use termimad::{MadSkin, StyledChar};

use crate::theme::{Theme, theme};

/// Left margin to align assistant body text with the text after "● ".
const INDENT: &str = "  ";

/// Columns reserved on the right so wrapped text never butts against the very
/// edge of the window.
const RIGHT_MARGIN: usize = 1;

/// Build a `MadSkin` from a `Theme`. Reads role styles + the code background
/// fields (`inline_code_bg`, `code_block_bg`) — backgrounds on inline-code
/// and code-block spans are the documented exception to the "no per-token
/// bgs" rule.
fn skin_from_theme(t: &Theme) -> MadSkin {
    let mut skin = MadSkin::default();

    // Headings (h1-h3) share the Heading role.
    let heading = t.heading;
    for level in 0..3 {
        skin.headers[level].set_fg(heading.fg);
        if heading.bold {
            skin.headers[level].add_attr(Attribute::Bold);
        }
        if heading.italic {
            skin.headers[level].add_attr(Attribute::Italic);
        }
    }

    // Bold body text -> Primary role (preserves visual weight without
    // double-applying Bold inside termimad's own bold renderer).
    skin.bold.set_fg(t.primary.fg);

    // Italic -> Emphasis role.
    skin.italic.set_fg(t.emphasis.fg);
    if t.emphasis.italic {
        skin.italic.add_attr(Attribute::Italic);
    }

    // Inline code: Code fg + inline_code_bg (per-token bg, special-cased).
    skin.inline_code.set_fg(t.code.fg);
    if !matches!(t.inline_code_bg, Color::Reset) {
        skin.inline_code.set_bg(t.inline_code_bg);
    }

    // Code blocks: Code fg + code_block_bg.
    skin.code_block.set_fg(t.code.fg);
    if !matches!(t.code_block_bg, Color::Reset) {
        skin.code_block.set_bg(t.code_block_bg);
    }

    // Bullets -> Bullet role.
    skin.bullet = StyledChar::from_fg_char(t.bullet.fg, '•');

    skin
}

/// Build a customized `MadSkin` from the active theme.
fn make_skin() -> MadSkin {
    skin_from_theme(theme())
}

/// Render markdown text to stdout using termimad, with a left indent on each line
/// so the body aligns with the text after the "● " marker.
pub fn render_markdown(text: &str) {
    let skin = make_skin();
    let trimmed = text.trim_start_matches('\n');
    let width = crossterm::terminal::size()
        .map(|(w, _)| (w as usize).saturating_sub(INDENT.len() + RIGHT_MARGIN))
        .unwrap_or(78);
    let rendered = format!("{}", skin.text(trimmed, Some(width)));
    for line in rendered.lines() {
        println!("{INDENT}{line}");
    }
}

/// Available text columns for a wrapped summary line: terminal width minus the
/// "● "/"  " 2-column gutter and the right margin (at least 1).
fn summary_text_width(term_w: usize) -> usize {
    term_w.saturating_sub(INDENT.len() + RIGHT_MARGIN).max(1)
}

/// Print the assistant summary headline: a colored "● " marker followed by the
/// summary in bold, word-wrapped to the terminal width so it never breaks
/// mid-word. Continuation lines use a 2-space hanging indent so they align
/// under the headline text (matching the markdown body's indent). A
/// [`RIGHT_MARGIN`] column is reserved on the right.
///
/// The marker and indent are both 2 columns wide, so every line shares the
/// same available text width.
pub fn render_summary(summary: &str) {
    use crate::ui::state::random_bullet_color;
    use crate::ui::wrap::wrap_plain;
    use crossterm::style::{Attribute, Stylize};

    let term_w = crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80);
    let text_w = summary_text_width(term_w);

    let color = random_bullet_color();
    for (i, line) in wrap_plain(summary, text_w).iter().enumerate() {
        if i == 0 {
            println!(
                "{} {}",
                "●".with(color).attribute(Attribute::Bold),
                line.as_str().attribute(Attribute::Bold),
            );
        } else {
            println!("{INDENT}{}", line.as_str().attribute(Attribute::Bold));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::wrap::wrap_plain;
    use unicode_width::UnicodeWidthStr;

    /// The screenshot bug: at 80 cols this summary used to char-wrap mid-word
    /// ("routin" / "g"). Verify the width `render_summary` wraps to keeps whole
    /// words and never reaches the right edge once the gutter is added back.
    #[test]
    fn summary_wraps_without_mid_word_break_at_80_cols() {
        let summary = "I'm an SRE orchestration assistant. I help triage operational \
issues by routing work to specialized agents for incidents, metrics, and logs.";
        let text_w = summary_text_width(80);
        assert_eq!(text_w, 80 - 2 - 1);

        let lines = wrap_plain(summary, text_w);
        for line in &lines {
            // Add the 2-column gutter back: rendered line must leave the
            // right-margin column free.
            let rendered_w = INDENT.len() + UnicodeWidthStr::width(line.as_str());
            assert!(
                rendered_w <= 80 - RIGHT_MARGIN,
                "line touches margin: {rendered_w}"
            );
        }
        // "routing" is never split.
        assert!(lines.iter().all(|l| !l.ends_with("routin")));
        assert!(lines.iter().any(|l| l.contains("routing")));
    }
}
