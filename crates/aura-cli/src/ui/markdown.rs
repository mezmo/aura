use termimad::crossterm::style::Color;
use termimad::{MadSkin, StyledChar};

/// Left margin to align assistant body text with the text after "● ".
const INDENT: &str = "  ";

/// Build a customized MadSkin for rendering markdown.
fn make_skin() -> MadSkin {
    let mut skin = MadSkin::default();

    // Headers in bold yellow
    skin.headers[0].set_fg(Color::Yellow);
    skin.headers[1].set_fg(Color::Yellow);
    skin.headers[2].set_fg(Color::Yellow);

    // Bold text
    skin.bold.set_fg(Color::White);

    // Italic text
    skin.italic.set_fg(Color::Magenta);

    // Inline code
    skin.inline_code.set_fg(Color::Green);
    skin.inline_code.set_bg(Color::DarkGrey);

    // Code blocks
    skin.code_block.set_fg(Color::Green);

    // Bullet points
    skin.bullet = StyledChar::from_fg_char(Color::Cyan, '•');

    skin
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
