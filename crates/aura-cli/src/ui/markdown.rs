use termimad::crossterm::style::{Attribute, Color};
use termimad::{MadSkin, StyledChar};

use crate::theme::{Theme, theme};

/// Left margin to align assistant body text with the text after "● ".
const INDENT: &str = "  ";

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
        .map(|(w, _)| (w as usize).saturating_sub(INDENT.len()))
        .unwrap_or(78);
    let rendered = format!("{}", skin.text(trimmed, Some(width)));
    for line in rendered.lines() {
        println!("{INDENT}{line}");
    }
}
