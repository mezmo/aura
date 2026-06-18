//! Theming for the CLI. Semantic role tokens (`AuraStyle`) resolve through
//! the active `Theme` to a `Style` (fg + per-token attrs). Region backgrounds
//! and a couple of code-style backgrounds live on the `Theme` directly.
//!
//! Active theme is held in an `AtomicPtr<Theme>`; reads are lock-free and
//! `set_theme` is a single atomic store. No external deps.

use std::fmt::Display;
use std::sync::atomic::{AtomicPtr, Ordering};

use crossterm::style::{Attribute, Color, ContentStyle, StyledContent};

// ---------------------------------------------------------------------------
// Style — fg + per-token attributes. No background.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub fg: Color,
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub underline: bool,
}

impl Style {
    pub const fn fg(fg: Color) -> Self {
        Self {
            fg,
            bold: false,
            italic: false,
            dim: false,
            underline: false,
        }
    }
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub(crate) fn to_content_style(self) -> ContentStyle {
        let mut cs = ContentStyle {
            foreground_color: Some(self.fg),
            ..Default::default()
        };
        if self.bold {
            cs.attributes.set(Attribute::Bold);
        }
        if self.italic {
            cs.attributes.set(Attribute::Italic);
        }
        if self.dim {
            cs.attributes.set(Attribute::Dim);
        }
        if self.underline {
            cs.attributes.set(Attribute::Underlined);
        }
        cs
    }
}

// ---------------------------------------------------------------------------
// AuraStyle — semantic role tokens. Use these everywhere instead of
// raw crossterm Color constants.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum AuraStyle {
    Primary,       // tool display names, headers in trees, markdown bold
    Muted,         // hints, durations, "completed", paths, dim metadata
    Connector,     // └─ ├─ │ ⎿ ▸ ▲ ▼ and frame ─ borders
    Heading,       // markdown headers, banner titles, prompt subject
    Selected,      // tab-completion winner, focused stream-panel row
    Success,       // ✓ approval, "Resumed conversation"
    Warning,       // "Warning:", reason hints, "Allow X?" prompt subject
    Error,         // "Error" label, ✗ deny messages
    Emphasis,      // markdown italic
    Code,          // markdown inline_code + code_block fg
    Identifier,    // UUIDs, short hashes, conversation IDs
    Prompt,        // ❯ input arrow
    DiffAdded,     // diff + lines
    DiffRemoved,   // diff - lines
    UserEchoFg,    // text on the dark echo bar after submit
    Bullet,        // markdown • glyph
    KeyLabel,      // tool argument key labels in trees
    StreamAura,    // stream panel: aura.* event lines
    StreamMessage, // stream panel: 'message' event lines
}

// ---------------------------------------------------------------------------
// Theme — one Style per role, region + code backgrounds, indexed task palette.
// ---------------------------------------------------------------------------

pub struct Theme {
    pub name: &'static str,

    // Per-role token styles.
    pub primary: Style,
    pub muted: Style,
    pub connector: Style,
    pub heading: Style,
    pub selected: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub emphasis: Style,
    pub code: Style,
    pub identifier: Style,
    pub prompt: Style,
    pub diff_added: Style,
    pub diff_removed: Style,
    pub user_echo_fg: Style,
    pub bullet: Style,
    pub key_label: Style,
    pub stream_aura: Style,
    pub stream_message: Style,

    // Region backgrounds — applied to large output zones.
    pub default_output_bg: Color,
    pub markdown_bg: Color,
    pub diff_bg: Color,
    pub user_echo_bg: Color,

    // Code backgrounds — special-cased per-token bgs for code spans.
    pub inline_code_bg: Color,
    pub code_block_bg: Color,

    // Animation shimmer base color (white-ish in dark theme).
    pub shimmer_base: Color,

    // Indexed accent palette. Always go through `task_accent(idx)`.
    pub task_palette: &'static [Style],
}

impl Theme {
    pub fn style(&self, role: AuraStyle) -> Style {
        match role {
            AuraStyle::Primary => self.primary,
            AuraStyle::Muted => self.muted,
            AuraStyle::Connector => self.connector,
            AuraStyle::Heading => self.heading,
            AuraStyle::Selected => self.selected,
            AuraStyle::Success => self.success,
            AuraStyle::Warning => self.warning,
            AuraStyle::Error => self.error,
            AuraStyle::Emphasis => self.emphasis,
            AuraStyle::Code => self.code,
            AuraStyle::Identifier => self.identifier,
            AuraStyle::Prompt => self.prompt,
            AuraStyle::DiffAdded => self.diff_added,
            AuraStyle::DiffRemoved => self.diff_removed,
            AuraStyle::UserEchoFg => self.user_echo_fg,
            AuraStyle::Bullet => self.bullet,
            AuraStyle::KeyLabel => self.key_label,
            AuraStyle::StreamAura => self.stream_aura,
            AuraStyle::StreamMessage => self.stream_message,
        }
    }

    pub fn task_accent(&self, idx: usize) -> Style {
        if self.task_palette.is_empty() {
            self.primary
        } else {
            self.task_palette[idx % self.task_palette.len()]
        }
    }
}

// ---------------------------------------------------------------------------
// Themed — call-site ergonomics:
//   "completed".themed(AuraStyle::Muted)
//   "●".themed_with(theme().task_accent(idx))
// ---------------------------------------------------------------------------

pub trait Themed: Sized + Display {
    fn themed(self, role: AuraStyle) -> StyledContent<Self>;
    fn themed_with(self, style: Style) -> StyledContent<Self>;
}

impl Themed for &str {
    fn themed(self, role: AuraStyle) -> StyledContent<Self> {
        StyledContent::new(theme().style(role).to_content_style(), self)
    }
    fn themed_with(self, style: Style) -> StyledContent<Self> {
        StyledContent::new(style.to_content_style(), self)
    }
}

impl Themed for String {
    fn themed(self, role: AuraStyle) -> StyledContent<Self> {
        StyledContent::new(theme().style(role).to_content_style(), self)
    }
    fn themed_with(self, style: Style) -> StyledContent<Self> {
        StyledContent::new(style.to_content_style(), self)
    }
}

// ---------------------------------------------------------------------------
// Active theme storage — AtomicPtr<Theme>, std-only.
// ---------------------------------------------------------------------------

static ACTIVE_THEME: AtomicPtr<Theme> = AtomicPtr::new(&NORMAL as *const Theme as *mut Theme);

pub fn theme() -> &'static Theme {
    // Safety: ACTIVE_THEME only ever holds pointers to &'static Theme
    // installed by `set_theme`. Initial value is &NORMAL.
    unsafe { &*ACTIVE_THEME.load(Ordering::Acquire) }
}

pub fn set_theme(t: &'static Theme) {
    ACTIVE_THEME.store(t as *const Theme as *mut Theme, Ordering::Release);
}

/// Canonical user-facing name for a theme — what to write back into
/// `cli.toml`. The persistence options are exactly `"normal"`,
/// `"high-contrast"`, and `"no-color"` (singular). [`theme_by_name`]
/// accepts a wider set of aliases on the read path.
pub fn theme_public_name(t: &Theme) -> &'static str {
    match t.name {
        "normal" => "normal",
        "high-contrast" => "high-contrast",
        "no-colors" => "no-color",
        other => other,
    }
}

pub fn theme_by_name(name: &str) -> Option<&'static Theme> {
    match name.to_ascii_lowercase().as_str() {
        "normal" => Some(&NORMAL),
        "high-contrast" | "hc" => Some(&HIGH_CONTRAST),
        "no-colors" | "no-color" | "none" | "plain" => Some(&NO_COLORS),
        _ => None,
    }
}

pub const STYLE_NAMES: &[&str] = &["normal", "high-contrast", "no-colors"];

// ---------------------------------------------------------------------------
// Baked themes
// ---------------------------------------------------------------------------

const TASK_PALETTE_NORMAL: &[Style] = &[
    Style::fg(Color::Cyan),    // Cyan
    Style::fg(Color::Magenta), // Magenta
    Style::fg(Color::Yellow),  // Yellow
    Style::fg(Color::Green),   // Green
    Style::fg(Color::Rgb {
        r: 100,
        g: 149,
        b: 237,
    }), // Cornflower blue
    Style::fg(Color::Rgb {
        r: 255,
        g: 165,
        b: 0,
    }), // Orange
    Style::fg(Color::Rgb {
        r: 147,
        g: 112,
        b: 219,
    }), // Purple
    Style::fg(Color::Rgb {
        r: 0,
        g: 255,
        b: 127,
    }), // Spring green
    Style::fg(Color::Rgb {
        r: 255,
        g: 105,
        b: 180,
    }), // Hot pink
    Style::fg(Color::Rgb {
        r: 64,
        g: 224,
        b: 208,
    }), // Turquoise
];

static BABY_BLUE: Color = Color::Rgb {
    r: 94,
    g: 147,
    b: 251,
};

/// Default theme
pub static NORMAL: Theme = Theme {
    name: "normal",

    primary: Style::fg(Color::Reset),
    muted: Style::fg(Color::Reset).dim(),
    connector: Style::fg(Color::Reset).dim(),
    heading: Style::fg(Color::Yellow).bold(),
    selected: Style::fg(Color::Reset).underline(),
    success: Style::fg(Color::Green),
    warning: Style::fg(Color::Yellow),
    error: Style::fg(Color::Red),
    emphasis: Style::fg(BABY_BLUE).italic(),
    code: Style::fg(Color::Rgb {
        r: 230,
        g: 230,
        b: 230,
    })
    .bold(),
    identifier: Style::fg(Color::Cyan),
    prompt: Style::fg(Color::Green).bold(),
    diff_added: Style::fg(Color::Blue),
    diff_removed: Style::fg(Color::Red),
    user_echo_fg: Style::fg(Color::Grey),
    bullet: Style::fg(Color::Cyan),
    key_label: Style::fg(Color::Reset),
    stream_aura: Style::fg(Color::Yellow),
    stream_message: Style::fg(Color::Cyan),
    default_output_bg: Color::Reset,
    markdown_bg: Color::Reset,
    diff_bg: Color::Reset,
    user_echo_bg: Color::DarkGrey,
    inline_code_bg: Color::Rgb {
        r: 40,
        g: 42,
        b: 54,
    },
    code_block_bg: Color::Rgb {
        r: 40,
        g: 42,
        b: 54,
    },
    shimmer_base: Color::Reset,
    task_palette: TASK_PALETTE_NORMAL,
};

const TASK_PALETTE_HIGH_CONTRAST: &[Style] = &[
    Style::fg(Color::White).bold(),
    Style::fg(Color::Yellow).bold(),
    Style::fg(Color::Cyan).bold(),
    Style::fg(Color::Green).bold(),
    Style::fg(Color::Magenta).bold(),
    Style::fg(Color::Red).bold(),
];

/// High-contrast theme — bright ANSI base colors with bold attributes.
/// Drops grey/dim tones so every line stays visible on any background.
pub static HIGH_CONTRAST: Theme = Theme {
    name: "high-contrast",
    primary: Style::fg(Color::Reset).bold(),
    muted: Style::fg(Color::Reset),
    connector: Style::fg(Color::Reset),
    heading: Style::fg(Color::Yellow).bold(),
    selected: Style::fg(Color::Yellow).bold().underline(),
    success: Style::fg(Color::Green).bold(),
    warning: Style::fg(Color::Yellow).bold(),
    error: Style::fg(Color::Red).bold(),
    emphasis: Style::fg(Color::White).italic().bold(),
    code: Style::fg(Color::Black).bold(),
    identifier: Style::fg(Color::Cyan).bold(),
    prompt: Style::fg(Color::Green).bold(),
    diff_added: Style::fg(Color::Green).bold(),
    diff_removed: Style::fg(Color::Red).bold(),
    user_echo_fg: Style::fg(Color::White).bold(),
    bullet: Style::fg(Color::White).bold(),
    key_label: Style::fg(Color::Cyan).bold(),
    stream_aura: Style::fg(Color::Yellow).bold(),
    stream_message: Style::fg(Color::Cyan).bold(),
    default_output_bg: Color::Reset,
    markdown_bg: Color::Reset,
    diff_bg: Color::Reset,
    user_echo_bg: Color::Black,
    inline_code_bg: Style::fg(Color::Yellow).dim().dim().fg,
    code_block_bg: Style::fg(Color::Yellow).dim().dim().fg,
    shimmer_base: Color::White,
    task_palette: TASK_PALETTE_HIGH_CONTRAST,
};

/// No-colors theme — Color::Reset everywhere, attributes only.
/// Honors the spirit of `NO_COLOR=1` while preserving emphasis cues.
pub static NO_COLORS: Theme = Theme {
    name: "no-colors",
    primary: Style::fg(Color::Reset),
    muted: Style::fg(Color::Reset).dim(),
    connector: Style::fg(Color::Reset).dim(),
    heading: Style::fg(Color::Reset).bold(),
    selected: Style::fg(Color::Reset).bold().underline(),
    success: Style::fg(Color::Reset).bold(),
    warning: Style::fg(Color::Reset).bold(),
    error: Style::fg(Color::Reset).bold(),
    emphasis: Style::fg(Color::Reset).italic(),
    code: Style::fg(Color::Reset),
    identifier: Style::fg(Color::Reset).bold(),
    prompt: Style::fg(Color::Reset).bold(),
    diff_added: Style::fg(Color::Reset).bold(),
    diff_removed: Style::fg(Color::Reset).dim(),
    user_echo_fg: Style::fg(Color::Reset),
    bullet: Style::fg(Color::Reset),
    key_label: Style::fg(Color::Reset).bold(),
    stream_aura: Style::fg(Color::Reset).dim(),
    stream_message: Style::fg(Color::Reset).dim(),
    default_output_bg: Color::Reset,
    markdown_bg: Color::Reset,
    diff_bg: Color::DarkGrey,
    user_echo_bg: Color::DarkGrey,
    inline_code_bg: Color::DarkGrey,
    code_block_bg: Color::DarkGrey,
    shimmer_base: Color::Grey,
    task_palette: &[],
};
